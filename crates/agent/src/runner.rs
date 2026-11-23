//! Persistent multi-step agent loop with guardrails, repeat-call detection,
//! cancellation, and crash-recovery resume from persisted `agent_steps` (`18-m4.md` §6.3).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tokio::sync::watch;
use utoipa::ToSchema;

use crate::llm::{LlmDelta, LlmMessage, LlmProvider, LlmToolCall, LlmTurnRequest, LlmUsage};
use crate::tools::{
    AgentPresenceHint, AgentWorkbookSink, BoxFuture, FinishReport, execute_tool, tool_definitions,
};
use crate::{AgentError, AgentRunConfig};

pub const SYSTEM_PROMPT: &str = "\
You are Reticula AI Analyst, a precise spreadsheet engineer operating on an isolated draft branch.
Workflow rules:
1. Start with `workbook_overview` and `read_range` to inspect headers, existing formulas, and layout before editing.
2. Prefer Excel formulas over hardcoded derived numbers. Use `evaluate_formula` to dry-run non-trivial formulas before writing them.
3. Use `fill_formula` for repeated formula columns, `sort_range` for sorting, `edit_structure` for inserting/deleting rows, columns, sheets, or named ranges, and `format_range` for styling/number formats.
4. Before finishing, run `list_errors` to verify you did not introduce `#REF!`, `#DIV/0!`, `#VALUE!`, `#NAME?`, `#N/A`, `#SPILL!`, or `#CYCLE!`.
5. End by calling `finish` with a concise summary and bullet highlights.";

/// Persisted step kinds matching `agent_steps.kind` CHECK constraint in migration 0004.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentStepKind {
    UserPrompt,
    AssistantThought,
    ToolCall,
    ToolResult,
    TxnCommit,
    Error,
    Finish,
}

impl AgentStepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserPrompt => "user_prompt",
            Self::AssistantThought => "assistant_thought",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::TxnCommit => "txn_commit",
            Self::Error => "error",
            Self::Finish => "finish",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "user_prompt" => Some(Self::UserPrompt),
            "assistant_thought" => Some(Self::AssistantThought),
            "tool_call" => Some(Self::ToolCall),
            "tool_result" => Some(Self::ToolResult),
            "txn_commit" => Some(Self::TxnCommit),
            "error" => Some(Self::Error),
            "finish" => Some(Self::Finish),
            _ => None,
        }
    }
}

/// Durable step record persisted before advancing the agent loop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedStep {
    pub idx: i32,
    pub kind: AgentStepKind,
    pub payload: JsonValue,
    pub prompt_tokens: i32,
    pub cached_prompt_tokens: i32,
    pub completion_tokens: i32,
    pub latency_ms: i32,
}

/// Live streaming event forwarded over SSE or captured during recording.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentStreamEvent {
    Step { step: PersistedStep },
    TextDelta { text: String },
    Presence { presence: AgentPresenceHint },
    Status { status: String, summary: String },
}

/// Persistence interface for recording steps before progressing.
pub trait StepStore: Send + Sync {
    fn append_step<'a>(&'a self, step: PersistedStep) -> BoxFuture<'a, Result<(), String>>;
    fn list_steps<'a>(&'a self) -> BoxFuture<'a, Result<Vec<PersistedStep>, String>>;
}

/// In-memory step store for unit tests and replay bundling.
#[derive(Clone, Default)]
pub struct MemoryStepStore {
    steps: Arc<Mutex<Vec<PersistedStep>>>,
}

impl MemoryStepStore {
    pub fn snapshot(&self) -> Vec<PersistedStep> {
        self.steps.lock().expect("step store mutex").clone()
    }
}

impl StepStore for MemoryStepStore {
    fn append_step<'a>(&'a self, step: PersistedStep) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut guard = self.steps.lock().expect("step store mutex");
            guard.push(step);
            Ok(())
        })
    }

    fn list_steps<'a>(&'a self) -> BoxFuture<'a, Result<Vec<PersistedStep>, String>> {
        Box::pin(async move {
            let guard = self.steps.lock().expect("step store mutex");
            Ok(guard.clone())
        })
    }
}

/// JSONL file-backed step store for headless eval & S6 traces.
#[derive(Clone)]
pub struct JsonlFileStepStore {
    path: PathBuf,
}

impl JsonlFileStepStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl StepStore for JsonlFileStepStore {
    fn append_step<'a>(&'a self, step: PersistedStep) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            use tokio::io::AsyncWriteExt;
            if let Some(parent) = self.path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await
                .map_err(|e| e.to_string())?;
            let mut line = serde_json::to_string(&step).map_err(|e| e.to_string())?;
            line.push('\n');
            file.write_all(line.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            file.flush().await.map_err(|e| e.to_string())?;
            Ok(())
        })
    }

    fn list_steps<'a>(&'a self) -> BoxFuture<'a, Result<Vec<PersistedStep>, String>> {
        Box::pin(async move {
            let Ok(text) = tokio::fs::read_to_string(&self.path).await else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(step) = serde_json::from_str::<PersistedStep>(trimmed) {
                    out.push(step);
                }
            }
            Ok(out)
        })
    }
}

/// Aggregate telemetry returned when an agent run completes cleanly.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunOutcome {
    pub summary: String,
    pub highlights: Vec<String>,
    pub steps_executed: u32,
    pub commits_written: u32,
    pub total_usage: LlmUsage,
    pub ttft_samples_ms: Vec<u64>,
    pub step_latencies_ms: Vec<u64>,
    pub retries: u32,
    pub rate_limited_hits: u32,
    pub schema_violations: u32,
    pub resolved_model: Option<String>,
}

/// Rebuilds the LLM conversation message history from previously persisted
/// `agent_steps` (supporting crash-recovery resume without re-running completed tools).
pub fn rebuild_messages_from_steps(prompt: &str, steps: &[PersistedStep]) -> Vec<LlmMessage> {
    let mut messages = vec![LlmMessage::system(SYSTEM_PROMPT)];
    let mut has_user = false;
    let mut pending_thought: Option<String> = None;
    let mut pending_calls: Vec<LlmToolCall> = Vec::new();

    let flush_assistant =
        |thought: &mut Option<String>, calls: &mut Vec<LlmToolCall>, out: &mut Vec<LlmMessage>| {
            if thought.is_some() || !calls.is_empty() {
                out.push(LlmMessage::assistant(thought.take(), std::mem::take(calls)));
            }
        };

    for step in steps {
        match step.kind {
            AgentStepKind::UserPrompt => {
                let text = step
                    .payload
                    .get("prompt")
                    .and_then(JsonValue::as_str)
                    .unwrap_or(prompt);
                messages.push(LlmMessage::user(text));
                has_user = true;
            }
            AgentStepKind::AssistantThought => {
                flush_assistant(&mut pending_thought, &mut pending_calls, &mut messages);
                let text = step
                    .payload
                    .get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("")
                    .to_owned();
                if !text.is_empty() {
                    pending_thought = Some(text);
                }
            }
            AgentStepKind::ToolCall => {
                let id = step
                    .payload
                    .get("callId")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("call_0")
                    .to_owned();
                let name = step
                    .payload
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("")
                    .to_owned();
                let arguments = step
                    .payload
                    .get("arguments")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        step.payload
                            .get("args")
                            .map(JsonValue::to_string)
                            .unwrap_or_else(|| "{}".to_owned())
                    });
                if !name.is_empty() {
                    pending_calls.push(LlmToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
            }
            AgentStepKind::ToolResult => {
                flush_assistant(&mut pending_thought, &mut pending_calls, &mut messages);
                let call_id = step
                    .payload
                    .get("callId")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("call_0");
                let name = step
                    .payload
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("tool");
                let output = step
                    .payload
                    .get("output")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                messages.push(LlmMessage::tool_result(call_id, name, output));
            }
            AgentStepKind::TxnCommit | AgentStepKind::Error | AgentStepKind::Finish => {}
        }
    }
    flush_assistant(&mut pending_thought, &mut pending_calls, &mut messages);
    if !has_user {
        messages.insert(1, LlmMessage::user(prompt));
    }
    messages
}

/// Executes (or resumes) the persistent multi-step agent loop.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_loop(
    prompt: &str,
    provider: &dyn LlmProvider,
    sink: &mut dyn AgentWorkbookSink,
    store: &dyn StepStore,
    config: AgentRunConfig,
    cancel_rx: Option<watch::Receiver<bool>>,
    mut on_event: impl FnMut(AgentStreamEvent) + Send,
) -> Result<AgentRunOutcome, AgentError> {
    let started = Instant::now();
    let existing = store.list_steps().await.map_err(AgentError::Store)?;

    let mut next_idx: i32 = existing.last().map_or(0, |s| s.idx + 1);
    let mut commits_written: u32 = existing
        .iter()
        .filter(|s| s.kind == AgentStepKind::TxnCommit)
        .count() as u32;
    let mut steps_executed: u32 = existing
        .iter()
        .filter(|s| s.kind == AgentStepKind::AssistantThought || s.kind == AgentStepKind::ToolCall)
        .count() as u32;
    let mut total_usage = LlmUsage::default();
    for s in &existing {
        total_usage.prompt_tokens += s.prompt_tokens.max(0) as u64;
        total_usage.cached_prompt_tokens += s.cached_prompt_tokens.max(0) as u64;
        total_usage.completion_tokens += s.completion_tokens.max(0) as u64;
    }

    if existing.is_empty() {
        let user_step = PersistedStep {
            idx: next_idx,
            kind: AgentStepKind::UserPrompt,
            payload: json!({ "prompt": prompt }),
            prompt_tokens: 0,
            cached_prompt_tokens: 0,
            completion_tokens: 0,
            latency_ms: 0,
        };
        next_idx += 1;
        store
            .append_step(user_step.clone())
            .await
            .map_err(AgentError::Store)?;
        on_event(AgentStreamEvent::Step { step: user_step });
    }

    let mut messages = rebuild_messages_from_steps(prompt, &existing);
    let tools = tool_definitions();
    let mut ttft_samples_ms = Vec::new();
    let mut step_latencies_ms = Vec::new();
    let mut retries = 0_u32;
    let mut rate_limited_hits = 0_u32;
    let mut schema_violations = 0_u32;
    let mut resolved_model: Option<String> = None;

    let mut last_tool_sig: Option<String> = None;
    let mut identical_repeat_count: u32 = 0;

    loop {
        if let Some(ref rx) = cancel_rx
            && *rx.borrow()
        {
            return Err(AgentError::Cancelled);
        }
        if started.elapsed() >= config.timeout {
            return Err(AgentError::TimedOut(config.timeout.as_secs()));
        }
        if steps_executed >= config.max_steps {
            return Err(AgentError::BudgetExceeded(format!(
                "reached max_steps ({})",
                config.max_steps
            )));
        }
        if total_usage.total() >= config.max_tokens {
            return Err(AgentError::BudgetExceeded(format!(
                "reached max_tokens ({})",
                config.max_tokens
            )));
        }

        let req = LlmTurnRequest {
            messages: messages.clone(),
            tools: tools.clone(),
        };

        let mut delta_handler = |delta: LlmDelta| {
            if let LlmDelta::Text(text) = delta {
                on_event(AgentStreamEvent::TextDelta { text });
            }
        };

        let remaining = config.timeout.saturating_sub(started.elapsed());
        let turn_fut = provider.stream_turn(req, &mut delta_handler);
        let turn = tokio::time::timeout(remaining, turn_fut)
            .await
            .map_err(|_| AgentError::TimedOut(config.timeout.as_secs()))??;

        if let Some(ref rx) = cancel_rx
            && *rx.borrow()
        {
            return Err(AgentError::Cancelled);
        }

        steps_executed += 1;
        ttft_samples_ms.push(turn.ttft_ms);
        step_latencies_ms.push(turn.latency_ms);
        retries += turn.retries;
        rate_limited_hits += turn.rate_limited_hits;
        if resolved_model.is_none() {
            resolved_model = turn.resolved_model.clone();
        }
        total_usage.prompt_tokens += turn.usage.prompt_tokens;
        total_usage.cached_prompt_tokens += turn.usage.cached_prompt_tokens;
        total_usage.completion_tokens += turn.usage.completion_tokens;

        // Persist assistant thought step BEFORE running any tools.
        let thought_text = if !turn.text.trim().is_empty() {
            turn.text.clone()
        } else if !turn.reasoning.trim().is_empty() {
            turn.reasoning.chars().take(400).collect()
        } else {
            String::new()
        };
        let thought_step = PersistedStep {
            idx: next_idx,
            kind: AgentStepKind::AssistantThought,
            payload: json!({ "text": thought_text }),
            prompt_tokens: turn.usage.prompt_tokens.min(i32::MAX as u64) as i32,
            cached_prompt_tokens: turn.usage.cached_prompt_tokens.min(i32::MAX as u64) as i32,
            completion_tokens: turn.usage.completion_tokens.min(i32::MAX as u64) as i32,
            latency_ms: turn.latency_ms.min(i32::MAX as u64) as i32,
        };
        next_idx += 1;
        store
            .append_step(thought_step.clone())
            .await
            .map_err(AgentError::Store)?;
        on_event(AgentStreamEvent::Step { step: thought_step });

        if turn.tool_calls.is_empty() {
            // Model returned plain text without calling `finish`.
            let summary = if !turn.text.trim().is_empty() {
                turn.text.trim().to_owned()
            } else {
                "Completed analysis with no workbook changes.".to_owned()
            };
            let finish_step = PersistedStep {
                idx: next_idx,
                kind: AgentStepKind::Finish,
                payload: json!({
                    "summary": summary,
                    "highlights": [],
                    "commitsWritten": commits_written,
                }),
                prompt_tokens: 0,
                cached_prompt_tokens: 0,
                completion_tokens: 0,
                latency_ms: 0,
            };
            store
                .append_step(finish_step.clone())
                .await
                .map_err(AgentError::Store)?;
            on_event(AgentStreamEvent::Step { step: finish_step });
            return Ok(AgentRunOutcome {
                summary,
                highlights: Vec::new(),
                steps_executed,
                commits_written,
                total_usage,
                ttft_samples_ms,
                step_latencies_ms,
                retries,
                rate_limited_hits,
                schema_violations,
                resolved_model,
            });
        }

        messages.push(LlmMessage::assistant(
            (!turn.text.is_empty()).then_some(turn.text.clone()),
            turn.tool_calls.clone(),
        ));

        let mut finish_report: Option<FinishReport> = None;

        for tc in &turn.tool_calls {
            let parsed_args: Result<JsonValue, _> = serde_json::from_str(&tc.arguments);
            let args_val = match parsed_args {
                Ok(v) => v,
                Err(err) => {
                    schema_violations += 1;
                    let call_step = PersistedStep {
                        idx: next_idx,
                        kind: AgentStepKind::ToolCall,
                        payload: json!({
                            "callId": tc.id,
                            "name": tc.name,
                            "arguments": tc.arguments,
                        }),
                        prompt_tokens: 0,
                        cached_prompt_tokens: 0,
                        completion_tokens: 0,
                        latency_ms: 0,
                    };
                    next_idx += 1;
                    store
                        .append_step(call_step.clone())
                        .await
                        .map_err(AgentError::Store)?;
                    on_event(AgentStreamEvent::Step { step: call_step });

                    let err_msg = format!("Invalid JSON arguments for `{}`: {err}", tc.name);
                    let res_step = PersistedStep {
                        idx: next_idx,
                        kind: AgentStepKind::ToolResult,
                        payload: json!({
                            "callId": tc.id,
                            "name": tc.name,
                            "ok": false,
                            "output": err_msg,
                        }),
                        prompt_tokens: 0,
                        cached_prompt_tokens: 0,
                        completion_tokens: 0,
                        latency_ms: 0,
                    };
                    next_idx += 1;
                    store
                        .append_step(res_step.clone())
                        .await
                        .map_err(AgentError::Store)?;
                    on_event(AgentStreamEvent::Step { step: res_step });
                    messages.push(LlmMessage::tool_result(&tc.id, &tc.name, err_msg));
                    continue;
                }
            };

            // Repeat-call loop guard.
            let sig = format!("{}:{}", tc.name, args_val);
            if last_tool_sig.as_deref() == Some(sig.as_str()) {
                identical_repeat_count += 1;
            } else {
                last_tool_sig = Some(sig);
                identical_repeat_count = 1;
            }
            if identical_repeat_count >= config.max_identical_tool_repeats {
                return Err(AgentError::BudgetExceeded(format!(
                    "aborted after {} identical consecutive `{}` calls",
                    identical_repeat_count, tc.name
                )));
            }

            // Persist tool_call BEFORE executing.
            let call_step = PersistedStep {
                idx: next_idx,
                kind: AgentStepKind::ToolCall,
                payload: json!({
                    "callId": tc.id,
                    "name": tc.name,
                    "arguments": tc.arguments,
                    "args": args_val,
                }),
                prompt_tokens: 0,
                cached_prompt_tokens: 0,
                completion_tokens: 0,
                latency_ms: 0,
            };
            next_idx += 1;
            store
                .append_step(call_step.clone())
                .await
                .map_err(AgentError::Store)?;
            on_event(AgentStreamEvent::Step { step: call_step });

            let t0 = Instant::now();
            let exec_res = execute_tool(&tc.name, &args_val, sink).await;
            let tool_ms = t0.elapsed().as_millis().min(i32::MAX as u128) as i32;

            match exec_res {
                Ok(outcome) => {
                    if let Some(p) = outcome.presence {
                        on_event(AgentStreamEvent::Presence { presence: p });
                    }
                    if let Some(seq) = outcome.committed_seq {
                        commits_written += 1;
                        let commit_step = PersistedStep {
                            idx: next_idx,
                            kind: AgentStepKind::TxnCommit,
                            payload: json!({
                                "callId": tc.id,
                                "tool": tc.name,
                                "seq": seq.to_string(),
                            }),
                            prompt_tokens: 0,
                            cached_prompt_tokens: 0,
                            completion_tokens: 0,
                            latency_ms: 0,
                        };
                        next_idx += 1;
                        store
                            .append_step(commit_step.clone())
                            .await
                            .map_err(AgentError::Store)?;
                        on_event(AgentStreamEvent::Step { step: commit_step });
                    }
                    let mut tool_out = outcome.output;
                    if identical_repeat_count == config.max_identical_tool_repeats.saturating_sub(1)
                    {
                        tool_out.push_str(
                            "\n[Guardrail warning: you called this tool with identical arguments 3 times. Change approach or call `finish`.]",
                        );
                    }
                    let res_step = PersistedStep {
                        idx: next_idx,
                        kind: AgentStepKind::ToolResult,
                        payload: json!({
                            "callId": tc.id,
                            "name": tc.name,
                            "ok": true,
                            "output": tool_out,
                        }),
                        prompt_tokens: 0,
                        cached_prompt_tokens: 0,
                        completion_tokens: 0,
                        latency_ms: tool_ms,
                    };
                    next_idx += 1;
                    store
                        .append_step(res_step.clone())
                        .await
                        .map_err(AgentError::Store)?;
                    on_event(AgentStreamEvent::Step { step: res_step });
                    messages.push(LlmMessage::tool_result(&tc.id, &tc.name, tool_out));

                    if let Some(report) = outcome.finished {
                        finish_report = Some(report);
                    }
                }
                Err(err_text) => {
                    schema_violations += 1;
                    let res_step = PersistedStep {
                        idx: next_idx,
                        kind: AgentStepKind::ToolResult,
                        payload: json!({
                            "callId": tc.id,
                            "name": tc.name,
                            "ok": false,
                            "output": err_text,
                        }),
                        prompt_tokens: 0,
                        cached_prompt_tokens: 0,
                        completion_tokens: 0,
                        latency_ms: tool_ms,
                    };
                    next_idx += 1;
                    store
                        .append_step(res_step.clone())
                        .await
                        .map_err(AgentError::Store)?;
                    on_event(AgentStreamEvent::Step { step: res_step });
                    messages.push(LlmMessage::tool_result(&tc.id, &tc.name, err_text));
                }
            }
        }

        if let Some(report) = finish_report {
            let finish_step = PersistedStep {
                idx: next_idx,
                kind: AgentStepKind::Finish,
                payload: json!({
                    "summary": report.summary,
                    "highlights": report.highlights,
                    "commitsWritten": commits_written,
                }),
                prompt_tokens: 0,
                cached_prompt_tokens: 0,
                completion_tokens: 0,
                latency_ms: 0,
            };
            store
                .append_step(finish_step.clone())
                .await
                .map_err(AgentError::Store)?;
            on_event(AgentStreamEvent::Step { step: finish_step });
            return Ok(AgentRunOutcome {
                summary: report.summary,
                highlights: report.highlights,
                steps_executed,
                commits_written,
                total_usage,
                ttft_samples_ms,
                step_latencies_ms,
                retries,
                rate_limited_hits,
                schema_violations,
                resolved_model,
            });
        }
    }
}

/// Evaluates whether a run interrupted by a server crash can safely resume from
/// its persisted `agent_steps` (`18-m4.md` §2.6):
/// - Safe to resume iff `resume_count < 2` and the last persisted step is
///   `UserPrompt` or `ToolResult` (no half-completed mutating tool call).
pub fn can_safely_resume(steps: &[PersistedStep], resume_count: i32) -> bool {
    if resume_count >= 2 {
        return false;
    }
    match steps.last().map(|s| s.kind) {
        None | Some(AgentStepKind::UserPrompt | AgentStepKind::ToolResult) => true,
        Some(
            AgentStepKind::AssistantThought
            | AgentStepKind::ToolCall
            | AgentStepKind::TxnCommit
            | AgentStepKind::Error
            | AgentStepKind::Finish,
        ) => false,
    }
}

#[cfg(test)]
mod tests {
    use reticula_core::id::{ColId, RowId, SheetId, pack_replica_id};
    use reticula_core::value::Value;
    use reticula_formula::engine::FormulaEngine;

    use super::*;
    use crate::llm::{ScriptedMockProvider, ScriptedTurn};
    use crate::tools::HeadlessEvalSink;

    #[tokio::test]
    async fn multi_step_loop_executes_all_tools_and_resumes_from_persisted_steps() {
        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        eng.edit_cell_raw(s0, RowId(0), ColId(0), "Region").unwrap();
        eng.edit_cell_raw(s0, RowId(0), ColId(1), "Sales").unwrap();
        eng.edit_cell_raw(s0, RowId(1), ColId(0), "North").unwrap();
        eng.edit_cell_raw(s0, RowId(1), ColId(1), "100").unwrap();
        eng.edit_cell_raw(s0, RowId(2), ColId(0), "South").unwrap();
        eng.edit_cell_raw(s0, RowId(2), ColId(1), "350").unwrap();
        eng.recalc_all();

        let rep = pack_replica_id(0, 2).unwrap();
        let mut sink = HeadlessEvalSink::new(eng.workbook().clone(), 0, rep).unwrap();
        let store = MemoryStepStore::default();

        // Phase 1: run 2 steps then simulate crash via cancellation watch.
        let (tx, rx) = watch::channel(false);
        let provider_part1 = ScriptedMockProvider::new(vec![
            ScriptedTurn::tool("Overview", "t1", "workbook_overview", json!({})),
            ScriptedTurn::tool(
                "Dry run sum",
                "t2",
                "evaluate_formula",
                json!({ "cell": "B4", "formula": "=SUM(B2:B3)" }),
            ),
            ScriptedTurn::tool(
                "Should be cancelled before executing",
                "t3",
                "write_cells",
                json!({ "edits": [{ "cell": "B4", "formula": "=SUM(B2:B3)" }] }),
            ),
        ]);
        let mut seen_results = 0_usize;
        let res1 = run_agent_loop(
            "Summarize sales",
            &provider_part1,
            &mut sink,
            &store,
            AgentRunConfig::default(),
            Some(rx),
            |ev| {
                if let AgentStreamEvent::Step { step } = ev
                    && step.kind == AgentStepKind::ToolResult
                {
                    seen_results += 1;
                    if seen_results == 2 {
                        let _ = tx.send(true);
                    }
                }
            },
        )
        .await;
        assert!(matches!(res1, Err(AgentError::Cancelled)));

        let persisted = store.snapshot();
        assert!(can_safely_resume(&persisted, 0));

        // Phase 2: resume from persisted steps without redoing overview/eval!
        let provider_part2 = ScriptedMockProvider::new(vec![
            ScriptedTurn::tool(
                "Writing total and sorting",
                "t3",
                "write_cells",
                json!({
                    "edits": [
                        { "cell": "A4", "value": "Total" },
                        { "cell": "B4", "formula": "=SUM(B2:B3)", "number_format": "currency" }
                    ]
                }),
            ),
            ScriptedTurn::tool(
                "Sorting rows by sales desc",
                "t4",
                "sort_range",
                json!({ "range": "A1:B3", "has_header": true, "keys": [{ "column": 1, "descending": true }] }),
            ),
            ScriptedTurn::tool(
                "Formatting header bold",
                "t5",
                "format_range",
                json!({ "range": "A1:B1", "style": { "bold": true } }),
            ),
            ScriptedTurn::tool(
                "Done",
                "t6",
                "finish",
                json!({ "summary": "Sorted and summed sales.", "highlights": ["Total = 450"] }),
            ),
        ]);
        let outcome = run_agent_loop(
            "Summarize sales",
            &provider_part2,
            &mut sink,
            &store,
            AgentRunConfig::default(),
            None,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(outcome.summary, "Sorted and summed sales.");
        assert_eq!(outcome.commits_written, 3);
        let sheet = sink.engine().workbook().sheet(s0).unwrap();
        // After descending sort on B2:B3, South (350) is now row 1 (A2:B2), total B4 is 450!
        assert_eq!(sheet.value(1, 1), Value::Number(350.0));
        assert_eq!(sheet.value(3, 1), Value::Number(450.0));
    }

    #[tokio::test]
    async fn repeat_guard_aborts_stuck_identical_calls() {
        let rep = pack_replica_id(0, 2).unwrap();
        let mut sink = HeadlessEvalSink::new(reticula_core::ops::Workbook::new(), 0, rep).unwrap();
        let store = MemoryStepStore::default();
        let turns = (0..5)
            .map(|i| ScriptedTurn::tool("loop", format!("c{i}"), "workbook_overview", json!({})))
            .collect();
        let provider = ScriptedMockProvider::new(turns);
        let err = run_agent_loop(
            "loop test",
            &provider,
            &mut sink,
            &store,
            AgentRunConfig::default(),
            None,
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AgentError::BudgetExceeded(_)));
    }
}
