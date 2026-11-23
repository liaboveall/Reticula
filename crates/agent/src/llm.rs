//! Streaming OpenAI-compatible LLM client (`Response::chunk()` SSE line parser)
//! and deterministic `ScriptedMockProvider` for tests & E2E.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::tools::BoxFuture;
use crate::{AgentError, LlmConfig};

/// Single chat message exchanged with an LLM provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<LlmToolCall>,
}

impl LlmMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_owned(),
            content: Some(content.into()),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_owned(),
            content: Some(content.into()),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant(content: Option<String>, tool_calls: Vec<LlmToolCall>) -> Self {
        Self {
            role: "assistant".to_owned(),
            content,
            tool_call_id: None,
            name: None,
            tool_calls,
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: "tool".to_owned(),
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
            tool_calls: Vec::new(),
        }
    }
}

/// Completed or scripted tool invocation emitted by the model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string arguments (OpenAI wire representation).
    pub arguments: String,
}

/// Token consumption and prompt-cache hit accounting for one turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmUsage {
    pub prompt_tokens: u64,
    pub cached_prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl LlmUsage {
    pub fn total(&self) -> u64 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }
}

/// Streaming incremental update emitted during a model turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LlmDelta {
    Text(String),
    Reasoning(String),
    ToolCallProgress { index: usize, name: Option<String> },
}

/// Request payload passed to [`LlmProvider::stream_turn`].
#[derive(Clone, Debug)]
pub struct LlmTurnRequest {
    pub messages: Vec<LlmMessage>,
    pub tools: Vec<JsonValue>,
}

/// Aggregated response from one LLM turn.
#[derive(Clone, Debug)]
pub struct LlmTurnResponse {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<LlmToolCall>,
    pub usage: LlmUsage,
    pub ttft_ms: u64,
    pub latency_ms: u64,
    pub retries: u32,
    pub rate_limited_hits: u32,
    pub finish_reason: Option<String>,
    pub resolved_model: Option<String>,
}

/// Callback invoked for streaming deltas during a turn.
pub type DeltaCallback<'a> = &'a mut (dyn FnMut(LlmDelta) + Send);

/// Provider-agnostic async streaming LLM interface.
pub trait LlmProvider: Send + Sync {
    fn provider_name(&self) -> &str;
    fn model_name(&self) -> &str;
    fn stream_turn<'a>(
        &'a self,
        req: LlmTurnRequest,
        on_delta: DeltaCallback<'a>,
    ) -> BoxFuture<'a, Result<LlmTurnResponse, AgentError>>;
}

/// Thin streaming client speaking OpenAI-compatible `/chat/completions` SSE.
pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    config: LlmConfig,
    max_retries: u32,
}

impl OpenAiCompatProvider {
    pub fn new(config: LlmConfig) -> Result<Self, AgentError> {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|e| AgentError::Config(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            client,
            config,
            max_retries: 3,
        })
    }

    fn endpoint(&self) -> String {
        if self.config.base_url.ends_with("/chat/completions") {
            self.config.base_url.clone()
        } else {
            format!("{}/chat/completions", self.config.base_url)
        }
    }

    fn build_request_body(&self, req: &LlmTurnRequest) -> JsonValue {
        let messages: Vec<JsonValue> = req
            .messages
            .iter()
            .map(|m| {
                let mut obj = serde_json::Map::new();
                obj.insert("role".to_owned(), json!(m.role));
                if let Some(ref c) = m.content {
                    obj.insert("content".to_owned(), json!(c));
                } else if m.tool_calls.is_empty() {
                    obj.insert("content".to_owned(), json!(""));
                }
                if let Some(ref id) = m.tool_call_id {
                    obj.insert("tool_call_id".to_owned(), json!(id));
                }
                if let Some(ref name) = m.name {
                    obj.insert("name".to_owned(), json!(name));
                }
                if !m.tool_calls.is_empty() {
                    let calls: Vec<JsonValue> = m
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.arguments,
                                }
                            })
                        })
                        .collect();
                    obj.insert("tool_calls".to_owned(), JsonValue::Array(calls));
                }
                JsonValue::Object(obj)
            })
            .collect();

        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !req.tools.is_empty() {
            body["tools"] = JsonValue::Array(req.tools.clone());
            body["tool_choice"] = json!("auto");
        }
        if let Some(ref effort) = self.config.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        body
    }
}

impl LlmProvider for OpenAiCompatProvider {
    fn provider_name(&self) -> &str {
        &self.config.provider
    }

    fn model_name(&self) -> &str {
        &self.config.model
    }

    fn stream_turn<'a>(
        &'a self,
        req: LlmTurnRequest,
        on_delta: DeltaCallback<'a>,
    ) -> BoxFuture<'a, Result<LlmTurnResponse, AgentError>> {
        Box::pin(async move {
            let url = self.endpoint();
            let body = self.build_request_body(&req);
            let mut retries = 0_u32;
            let mut rate_limited_hits = 0_u32;

            loop {
                let started = Instant::now();
                let resp = self
                    .client
                    .post(&url)
                    .bearer_auth(self.config.api_key())
                    .header("Accept", "text/event-stream")
                    .json(&body)
                    .send()
                    .await;

                match resp {
                    Ok(mut response) => {
                        let status = response.status();
                        if status == StatusCode::TOO_MANY_REQUESTS
                            || status == StatusCode::BAD_GATEWAY
                            || status == StatusCode::SERVICE_UNAVAILABLE
                            || status == StatusCode::GATEWAY_TIMEOUT
                        {
                            if status == StatusCode::TOO_MANY_REQUESTS {
                                rate_limited_hits += 1;
                            }
                            if retries < self.max_retries {
                                let retry_after = response
                                    .headers()
                                    .get("retry-after")
                                    .and_then(|v| v.to_str().ok())
                                    .and_then(|s| s.trim().parse::<u64>().ok())
                                    .unwrap_or(1_u64 << retries)
                                    .clamp(1, 30);
                                retries += 1;
                                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                                continue;
                            }
                            let snippet = response.text().await.unwrap_or_default();
                            return Err(AgentError::Llm(format!(
                                "upstream HTTP {status} after {retries} retries: {}",
                                truncate_err(&snippet)
                            )));
                        }
                        if !status.is_success() {
                            let snippet = response.text().await.unwrap_or_default();
                            return Err(AgentError::Llm(format!(
                                "upstream HTTP {status}: {}",
                                truncate_err(&snippet)
                            )));
                        }

                        // Check if endpoint returned non-streaming JSON despite stream:true.
                        let is_sse = response
                            .headers()
                            .get(reqwest::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .is_some_and(|ct| {
                                ct.to_ascii_lowercase().contains("text/event-stream")
                            });

                        if !is_sse {
                            let text_body = response.text().await.map_err(|e| {
                                AgentError::Llm(format!("failed reading body: {e}"))
                            })?;
                            let parsed: JsonValue =
                                serde_json::from_str(&text_body).map_err(|e| {
                                    AgentError::Llm(format!(
                                        "invalid JSON response: {e}; snippet={}",
                                        truncate_err(&text_body)
                                    ))
                                })?;
                            if let Some(err_obj) = parsed.get("error") {
                                let msg = err_obj
                                    .get("message")
                                    .and_then(JsonValue::as_str)
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| truncate_err(&err_obj.to_string()));
                                return Err(AgentError::Llm(format!("upstream error: {msg}")));
                            }
                            let elapsed = started.elapsed().as_millis() as u64;
                            return parse_non_streaming_json(
                                &parsed,
                                elapsed,
                                retries,
                                rate_limited_hits,
                                on_delta,
                            );
                        }

                        let mut acc = SseAccumulator::new(started);
                        let mut line_buf = Vec::<u8>::new();

                        while let Some(chunk) = response
                            .chunk()
                            .await
                            .map_err(|e| AgentError::Llm(format!("SSE chunk read error: {e}")))?
                        {
                            line_buf.extend_from_slice(&chunk);
                            while let Some(nl) = line_buf.iter().position(|&b| b == b'\n') {
                                let raw_line: Vec<u8> = line_buf.drain(..=nl).collect();
                                let line = String::from_utf8_lossy(&raw_line);
                                acc.feed_line(line.trim(), on_delta)?;
                            }
                        }
                        if !line_buf.is_empty() {
                            let line = String::from_utf8_lossy(&line_buf);
                            acc.feed_line(line.trim(), on_delta)?;
                        }

                        return Ok(acc.finish(retries, rate_limited_hits));
                    }
                    Err(err) => {
                        if retries < self.max_retries {
                            let delay = 1_u64 << retries;
                            retries += 1;
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                            continue;
                        }
                        return Err(AgentError::Llm(format!(
                            "network error after {retries} retries: {err}"
                        )));
                    }
                }
            }
        })
    }
}

struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

struct SseAccumulator {
    started: Instant,
    ttft_ms: Option<u64>,
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, PartialToolCall>,
    usage: LlmUsage,
    finish_reason: Option<String>,
    resolved_model: Option<String>,
}

impl SseAccumulator {
    fn new(started: Instant) -> Self {
        Self {
            started,
            ttft_ms: None,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: BTreeMap::new(),
            usage: LlmUsage::default(),
            finish_reason: None,
            resolved_model: None,
        }
    }

    fn mark_first_token(&mut self) {
        if self.ttft_ms.is_none() {
            self.ttft_ms = Some(self.started.elapsed().as_millis() as u64);
        }
    }

    fn feed_line(&mut self, line: &str, on_delta: DeltaCallback<'_>) -> Result<(), AgentError> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let payload = data.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return Ok(());
        }
        let Ok(val) = serde_json::from_str::<JsonValue>(payload) else {
            return Ok(());
        };
        if let Some(err_obj) = val.get("error")
            && !err_obj.is_null()
        {
            let msg = err_obj
                .get("message")
                .and_then(JsonValue::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| truncate_err(&err_obj.to_string()));
            return Err(AgentError::Llm(format!("SSE upstream error: {msg}")));
        }
        if self.resolved_model.is_none()
            && let Some(m) = val.get("model").and_then(JsonValue::as_str)
        {
            self.resolved_model = Some(m.to_owned());
        }
        if let Some(usage) = val.get("usage")
            && !usage.is_null()
        {
            self.usage = extract_usage(usage);
        }
        let Some(choice) = val
            .get("choices")
            .and_then(JsonValue::as_array)
            .and_then(|arr| arr.first())
        else {
            return Ok(());
        };
        if let Some(fr) = choice.get("finish_reason").and_then(JsonValue::as_str) {
            self.finish_reason = Some(fr.to_owned());
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        if let Some(rc) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(JsonValue::as_str)
            && !rc.is_empty()
        {
            self.mark_first_token();
            self.reasoning.push_str(rc);
            on_delta(LlmDelta::Reasoning(rc.to_owned()));
        }
        if let Some(content) = delta.get("content").and_then(JsonValue::as_str)
            && !content.is_empty()
        {
            self.mark_first_token();
            self.text.push_str(content);
            on_delta(LlmDelta::Text(content.to_owned()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(JsonValue::as_array) {
            for item in calls {
                self.mark_first_token();
                let idx = item.get("index").and_then(JsonValue::as_u64).unwrap_or(0) as usize;
                let entry = self
                    .tool_calls
                    .entry(idx)
                    .or_insert_with(|| PartialToolCall {
                        id: format!("call_{idx}"),
                        name: String::new(),
                        arguments: String::new(),
                    });
                if let Some(id) = item.get("id").and_then(JsonValue::as_str)
                    && !id.is_empty()
                {
                    entry.id = id.to_owned();
                }
                if let Some(func) = item.get("function") {
                    if let Some(name) = func.get("name").and_then(JsonValue::as_str)
                        && !name.is_empty()
                    {
                        entry.name.push_str(name);
                        on_delta(LlmDelta::ToolCallProgress {
                            index: idx,
                            name: Some(entry.name.clone()),
                        });
                    }
                    if let Some(args) = func.get("arguments").and_then(JsonValue::as_str) {
                        entry.arguments.push_str(args);
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(self, retries: u32, rate_limited_hits: u32) -> LlmTurnResponse {
        let latency_ms = self.started.elapsed().as_millis() as u64;
        let tool_calls = self
            .tool_calls
            .into_values()
            .filter(|c| !c.name.is_empty())
            .map(|c| LlmToolCall {
                id: c.id,
                name: c.name,
                arguments: if c.arguments.trim().is_empty() {
                    "{}".to_owned()
                } else {
                    c.arguments
                },
            })
            .collect();
        LlmTurnResponse {
            text: self.text,
            reasoning: self.reasoning,
            tool_calls,
            usage: self.usage,
            ttft_ms: self.ttft_ms.unwrap_or(latency_ms),
            latency_ms,
            retries,
            rate_limited_hits,
            finish_reason: self.finish_reason,
            resolved_model: self.resolved_model,
        }
    }
}

fn parse_non_streaming_json(
    val: &JsonValue,
    elapsed_ms: u64,
    retries: u32,
    rate_limited_hits: u32,
    on_delta: DeltaCallback<'_>,
) -> Result<LlmTurnResponse, AgentError> {
    let usage = val.get("usage").map(extract_usage).unwrap_or_default();
    let resolved_model = val
        .get("model")
        .and_then(JsonValue::as_str)
        .map(str::to_owned);
    let choice = val
        .get("choices")
        .and_then(JsonValue::as_array)
        .and_then(|arr| arr.first())
        .ok_or_else(|| AgentError::Llm("response missing `choices[0]`".to_owned()))?;
    let finish_reason = choice
        .get("finish_reason")
        .and_then(JsonValue::as_str)
        .map(str::to_owned);
    let msg = choice
        .get("message")
        .ok_or_else(|| AgentError::Llm("response missing `choices[0].message`".to_owned()))?;
    let text = msg
        .get("content")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_owned();
    if !text.is_empty() {
        on_delta(LlmDelta::Text(text.clone()));
    }
    let reasoning = msg
        .get("reasoning_content")
        .or_else(|| msg.get("reasoning"))
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_owned();
    let mut tool_calls = Vec::new();
    if let Some(calls) = msg.get("tool_calls").and_then(JsonValue::as_array) {
        for (idx, item) in calls.iter().enumerate() {
            let id = item
                .get("id")
                .and_then(JsonValue::as_str)
                .map_or_else(|| format!("call_{idx}"), str::to_owned);
            let func = item.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(JsonValue::as_str)
                .unwrap_or("")
                .to_owned();
            let arguments = func
                .and_then(|f| f.get("arguments"))
                .and_then(JsonValue::as_str)
                .unwrap_or("{}")
                .to_owned();
            if !name.is_empty() {
                tool_calls.push(LlmToolCall {
                    id,
                    name,
                    arguments,
                });
            }
        }
    }
    Ok(LlmTurnResponse {
        text,
        reasoning,
        tool_calls,
        usage,
        ttft_ms: elapsed_ms,
        latency_ms: elapsed_ms,
        retries,
        rate_limited_hits,
        finish_reason,
        resolved_model,
    })
}

fn extract_usage(u: &JsonValue) -> LlmUsage {
    let prompt_tokens = u
        .get("prompt_tokens")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let completion_tokens = u
        .get("completion_tokens")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let cached_prompt_tokens = u
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| u.get("prompt_cache_hit_tokens"))
        .or_else(|| u.get("cached_tokens"))
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    LlmUsage {
        prompt_tokens,
        cached_prompt_tokens,
        completion_tokens,
    }
}

fn truncate_err(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() <= 240 {
        trimmed.to_owned()
    } else {
        let mut end = 240;
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &trimmed[..end])
    }
}

/// One scripted turn returned by [`ScriptedMockProvider`].
#[derive(Clone, Debug)]
pub struct ScriptedTurn {
    pub text: String,
    pub tool_calls: Vec<LlmToolCall>,
    pub usage: LlmUsage,
    pub delay_ms: u64,
}

impl ScriptedTurn {
    #[allow(clippy::needless_pass_by_value)]
    pub fn tool(
        text: impl Into<String>,
        id: impl Into<String>,
        name: impl Into<String>,
        args: JsonValue,
    ) -> Self {
        Self {
            text: text.into(),
            tool_calls: vec![LlmToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args.to_string(),
            }],
            usage: LlmUsage {
                prompt_tokens: 420,
                cached_prompt_tokens: 280,
                completion_tokens: 95,
            },
            delay_ms: 0,
        }
    }

    pub fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }
}

/// Deterministic scripted provider for unit tests, PG18 SIGKILL recovery tests,
/// replay generation, and three-browser Playwright E2E (zero external network).
#[derive(Clone)]
pub struct ScriptedMockProvider {
    model: String,
    turns: Arc<Mutex<Vec<ScriptedTurn>>>,
    prompt_router: bool,
}

impl ScriptedMockProvider {
    pub fn new(turns: Vec<ScriptedTurn>) -> Self {
        let mut rev = turns;
        rev.reverse();
        Self {
            model: "reticula-scripted-mock-v1".to_owned(),
            turns: Arc::new(Mutex::new(rev)),
            prompt_router: false,
        }
    }

    /// Creates a prompt-aware mock provider used by `RETICULA_LLM_MODE=mock` in
    /// server E2E and restart tests.
    pub fn server_mock() -> Self {
        Self {
            model: "reticula-mock-analyst".to_owned(),
            turns: Arc::new(Mutex::new(Vec::new())),
            prompt_router: true,
        }
    }

    fn route_server_mock(&self, req: &LlmTurnRequest) -> ScriptedTurn {
        let prompt = req
            .messages
            .iter()
            .find(|m| m.role == "user")
            .and_then(|m| m.content.as_deref())
            .unwrap_or("")
            .to_ascii_lowercase();
        let tool_turns_done = req.messages.iter().filter(|m| m.role == "tool").count();

        // Slow multi-step run used by SIGKILL recovery integration tests.
        let step_delay = if prompt.contains("[slow]") { 1200 } else { 15 };

        if prompt.contains("[closure]") || prompt.contains("insert row") {
            return match tool_turns_done {
                0 => ScriptedTurn::tool(
                    "Inspecting the workbook structure before inserting a subtotal row.",
                    "c1",
                    "workbook_overview",
                    json!({}),
                )
                .with_delay(step_delay),
                1 => ScriptedTurn::tool(
                    "Inserting a new row at row 2 for the adjustment factor.",
                    "c2",
                    "edit_structure",
                    json!({ "action": "insert_rows", "index": 1, "count": 1 }),
                )
                .with_delay(step_delay),
                2 => ScriptedTurn::tool(
                    "Populating the newly inserted row A2=25 and referencing it from existing cell B1.",
                    "c3",
                    "write_cells",
                    json!({
                        "edits": [
                            { "cell": "A2", "value": 25 },
                            { "cell": "B1", "formula": "=A2*2" }
                        ]
                    }),
                )
                .with_delay(step_delay),
                _ => ScriptedTurn::tool(
                    "Verified formulas and finishing draft.",
                    "c4",
                    "finish",
                    json!({
                        "summary": "Inserted row 2 with value 25 and updated B1 to =A2*2.",
                        "highlights": ["Inserted 1 row at row 2", "Set B1 = A2*2 (evaluates to 50)"]
                    }),
                ),
            };
        }

        if prompt.contains("[conflict]") {
            return match tool_turns_done {
                0 => ScriptedTurn::tool(
                    "Reading existing cells A1:B4 to compute summary updates.",
                    "c1",
                    "read_range",
                    json!({ "range": "A1:B4", "include_formulas": true }),
                )
                .with_delay(step_delay),
                1 => ScriptedTurn::tool(
                    "Updating A2 and writing a total formula in B2.",
                    "c2",
                    "write_cells",
                    json!({
                        "edits": [
                            { "cell": "A2", "value": 420 },
                            { "cell": "B2", "formula": "=A2*1.1" }
                        ]
                    }),
                )
                .with_delay(step_delay),
                _ => ScriptedTurn::tool(
                    "Draft ready for review.",
                    "c3",
                    "finish",
                    json!({
                        "summary": "Updated A2 to 420 and added 10% uplift formula in B2.",
                        "highlights": ["A2 -> 420", "B2 -> =A2*1.1"]
                    }),
                ),
            };
        }

        // Default general analyst scenario: overview -> eval -> write -> format -> finish.
        match tool_turns_done {
            0 => ScriptedTurn::tool(
                "Inspecting workbook overview to identify data columns and existing formulas.",
                "c1",
                "workbook_overview",
                json!({}),
            )
            .with_delay(step_delay),
            1 => ScriptedTurn::tool(
                "Dry-running summary formula before writing to draft.",
                "c2",
                "evaluate_formula",
                json!({ "cell": "C1", "formula": "=SUM(A1:A10)" }),
            )
            .with_delay(step_delay),
            2 => ScriptedTurn::tool(
                "Writing summary label and formula into C1:D1 on the AI draft branch.",
                "c3",
                "write_cells",
                json!({
                    "edits": [
                        { "cell": "C1", "value": "AI Total" },
                        { "cell": "D1", "formula": "=SUM(A1:A10)", "number_format": "fixed2" }
                    ]
                }),
            )
            .with_delay(step_delay),
            3 => ScriptedTurn::tool(
                "Checking workbook for calculation errors.",
                "c4",
                "list_errors",
                json!({}),
            )
            .with_delay(step_delay),
            _ => ScriptedTurn::tool(
                "All formulas verified with zero errors.",
                "c5",
                "finish",
                json!({
                    "summary": "Added summary total in C1:D1 (=SUM(A1:A10)) with zero formula errors.",
                    "highlights": ["C1: AI Total", "D1: =SUM(A1:A10)"]
                }),
            ),
        }
    }
}

impl LlmProvider for ScriptedMockProvider {
    fn provider_name(&self) -> &str {
        "scripted-mock"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn stream_turn<'a>(
        &'a self,
        req: LlmTurnRequest,
        on_delta: DeltaCallback<'a>,
    ) -> BoxFuture<'a, Result<LlmTurnResponse, AgentError>> {
        Box::pin(async move {
            let started = Instant::now();
            let turn = if self.prompt_router {
                self.route_server_mock(&req)
            } else {
                let mut guard = self.turns.lock().expect("mock mutex");
                guard.pop().unwrap_or_else(|| {
                    ScriptedTurn::tool(
                        "Finalizing scripted run.",
                        "auto_finish",
                        "finish",
                        json!({ "summary": "Completed scripted run." }),
                    )
                })
            };
            if turn.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(turn.delay_ms)).await;
            }
            if !turn.text.is_empty() {
                on_delta(LlmDelta::Text(turn.text.clone()));
            }
            for (idx, tc) in turn.tool_calls.iter().enumerate() {
                on_delta(LlmDelta::ToolCallProgress {
                    index: idx,
                    name: Some(tc.name.clone()),
                });
            }
            let elapsed = started.elapsed().as_millis() as u64;
            Ok(LlmTurnResponse {
                text: turn.text,
                reasoning: String::new(),
                tool_calls: turn.tool_calls,
                usage: turn.usage,
                ttft_ms: elapsed,
                latency_ms: elapsed,
                retries: 0,
                rate_limited_hits: 0,
                finish_reason: Some("tool_calls".to_owned()),
                resolved_model: Some(self.model.clone()),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_accumulator_assembles_incremental_text_tool_calls_and_cached_usage() {
        let mut acc = SseAccumulator::new(Instant::now());
        let mut deltas = Vec::new();
        let mut cb = |d: LlmDelta| deltas.push(d);

        acc.feed_line(
            r#"data: {"model":"gpt-test","choices":[{"delta":{"reasoning_content":"think "}}]}"#,
            &mut cb,
        )
        .unwrap();
        acc.feed_line(
            r#"data: {"choices":[{"delta":{"content":"Checking range."}}]}"#,
            &mut cb,
        )
        .unwrap();
        acc.feed_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_range","arguments":"{\"range\":"}}]}}]}"#,
            &mut cb,
        )
        .unwrap();
        acc.feed_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"A1:C5\"}"}}]}}]}"#,
            &mut cb,
        )
        .unwrap();
        acc.feed_line(
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1200,"completion_tokens":80,"prompt_tokens_details":{"cached_tokens":900}}}"#,
            &mut cb,
        )
        .unwrap();
        acc.feed_line("data: [DONE]", &mut cb).unwrap();

        let resp = acc.finish(0, 0);
        assert_eq!(resp.resolved_model.as_deref(), Some("gpt-test"));
        assert_eq!(resp.reasoning, "think ");
        assert_eq!(resp.text, "Checking range.");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_1");
        assert_eq!(resp.tool_calls[0].name, "read_range");
        assert_eq!(resp.tool_calls[0].arguments, r#"{"range":"A1:C5"}"#);
        assert_eq!(resp.usage.prompt_tokens, 1200);
        assert_eq!(resp.usage.cached_prompt_tokens, 900);
        assert_eq!(resp.usage.completion_tokens, 80);
    }

    #[tokio::test]
    async fn openai_compat_handles_split_utf8_chunks_429_retry_and_sse_errors_over_real_tcp() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempt = Arc::new(AtomicUsize::new(0));
        let attempt_srv = Arc::clone(&attempt);

        let server_task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let n = attempt_srv.fetch_add(1, Ordering::SeqCst);
                let mut req_buf = vec![0_u8; 4096];
                let _ = sock.read(&mut req_buf).await;
                if n == 0 {
                    // First attempt: 429 with Retry-After: 1
                    let resp = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy";
                    let _ = sock.write_all(resp.as_bytes()).await;
                } else if n == 1 {
                    // Second attempt: uppercase Content-Type, split multi-byte UTF-8 across TCP chunks,
                    // prompt_cache_hit_tokens, and [DONE] without trailing newline!
                    let hdr = "HTTP/1.1 200 OK\r\nContent-Type: TEXT/EVENT-STREAM; charset=utf-8\r\nConnection: close\r\n\r\n";
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    let line1 = "data: {\"model\":\"mock-live\",\"choices\":[{\"delta\":{\"content\":\"中文测试\"}}]}\n\n".as_bytes();
                    // Split right inside the 3-byte UTF-8 sequence of '中' (0xE4 0xB8 0xAD)
                    let split = line1.iter().position(|&b| b == 0xE4).unwrap() + 1;
                    let _ = sock.write_all(&line1[..split]).await;
                    let _ = sock.flush().await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    let _ = sock.write_all(&line1[split..]).await;
                    let line2 = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":500,\"completion_tokens\":20,\"prompt_cache_hit_tokens\":350}}\n\ndata: [DONE]";
                    let _ = sock.write_all(line2).await;
                    let _ = sock.flush().await;
                } else {
                    // Third request: mid-stream SSE error object must fail explicitly.
                    let hdr = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    let _ = sock
                        .write_all(
                            b"data: {\"error\":{\"message\":\"context length exceeded\"}}\n\n",
                        )
                        .await;
                    break;
                }
            }
        });

        let cfg = LlmConfig::new(format!("http://{addr}/v1"), "mock-live", "sk-test-local");
        let provider = OpenAiCompatProvider::new(cfg).unwrap();
        let req = LlmTurnRequest {
            messages: vec![LlmMessage::user("hi")],
            tools: Vec::new(),
        };
        let mut streamed = String::new();
        let mut cb = |d: LlmDelta| {
            if let LlmDelta::Text(t) = d {
                streamed.push_str(&t);
            }
        };
        let resp = provider.stream_turn(req.clone(), &mut cb).await.unwrap();
        assert_eq!(resp.retries, 1);
        assert_eq!(resp.rate_limited_hits, 1);
        assert_eq!(resp.text, "中文测试");
        assert_eq!(streamed, "中文测试");
        assert_eq!(resp.usage.prompt_tokens, 500);
        assert_eq!(resp.usage.cached_prompt_tokens, 350);
        assert_eq!(resp.usage.completion_tokens, 20);

        let mut noop = |_: LlmDelta| {};
        let err = provider.stream_turn(req, &mut noop).await.unwrap_err();
        assert!(
            err.to_string().contains("context length exceeded"),
            "unexpected error: {err}"
        );
        server_task.abort();
    }
}
