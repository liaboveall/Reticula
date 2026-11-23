//! 10 built-in deterministic AI analyst demo tasks and self-contained client
//! `ReplayBundle` generator.
//!
//! Each bundle runs through the real `FormulaEngine`, `run_agent_loop`,
//! `compute_branch_diff`, `check_reference_closure`, and `plan_merge_ops` so
//! visitors experience authentic engine recalculation, three-way conflict
//! resolution, and static reference closure checks purely in the browser.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use reticula_core::id::{ColId, RowId, SheetId, pack_replica_id};
use reticula_core::ops::{Op, Workbook};
use reticula_formula::binder::BoundFormula;
use reticula_formula::engine::FormulaEngine;
use reticula_wire::convert_v3::ops_to_wire_txn_v3;
use reticula_wire::snapshot::encode_snapshot_v1;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::diff::{
    BranchDiff, ClosureIssue, ConflictResolution, DiffHunkKind, MergeSelection,
    check_reference_closure, compute_branch_diff, plan_merge_ops,
};
use crate::llm::{ScriptedMockProvider, ScriptedTurn};
use crate::runner::{AgentStepKind, MemoryStepStore, run_agent_loop};
use crate::tools::{AgentPresenceHint, AgentWorkbookSink, HeadlessEvalSink};
use crate::{AgentError, AgentRunConfig};

/// Metadata entry written to `apps/web/public/data/ai-replays/index.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayIndexEntry {
    pub id: String,
    pub title: String,
    pub description: String,
    pub prompt: String,
    pub tags: Vec<String>,
    pub has_conflict_demo: bool,
    pub has_closure_demo: bool,
    pub step_count: u32,
}

/// Top-level index manifest loaded lazily when a visitor opens the AI panel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayIndex {
    pub version: u32,
    pub generated_with: String,
    pub items: Vec<ReplayIndexEntry>,
}

/// Single step in a client-side recorded replay timeline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayStep {
    pub idx: i32,
    pub kind: AgentStepKind,
    pub title: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence: Option<AgentPresenceHint>,
    /// Base64 postcard `WireTxnV3` bytes to apply into the local draft preview engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txn_b64: Option<String>,
}

/// Precomputed merge payloads for one hunk so the browser never duplicates Rust diff logic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayHunkOps {
    /// WireTxnV3 Base64 payloads applying this hunk cleanly onto `main`.
    pub apply_txns_b64: Vec<String>,
    /// Optional `#REF!`-sanitized payloads when user accepts this hunk while rejecting a referenced structural insert.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ref_sanitized_txns_b64: Vec<String>,
}

/// Complete self-contained replay artifact fetched on demand (`<id>.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayBundle {
    pub version: u32,
    pub id: String,
    pub title: String,
    pub description: String,
    pub prompt: String,
    pub tags: Vec<String>,
    pub summary: String,
    pub highlights: Vec<String>,
    pub base_snapshot_b64: String,
    /// Optional concurrent main edits applied on `main` (demonstrated in #10).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub main_concurrent_txns_b64: Vec<String>,
    pub steps: Vec<ReplayStep>,
    pub diff: BranchDiff,
    /// Precomputed closure issues that arise if each structural hunk is unchecked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closure_dependencies: Vec<ReplayClosureDep>,
    /// Per-hunk precomputed merge payloads.
    pub hunk_ops: BTreeMap<String, ReplayHunkOps>,
    /// Full default-accept merge payload.
    pub accept_all_txns_b64: Vec<String>,
}

/// Precomputed dependency edge: accepting `source_hunk_id` while rejecting
/// `target_hunk_id` produces `issue`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayClosureDep {
    pub source_hunk_id: String,
    pub target_hunk_id: String,
    pub issue: ClosureIssue,
}

type ConcurrentMainBuilder =
    fn(&Workbook<BoundFormula>) -> (Workbook<BoundFormula>, Vec<Vec<Op<BoundFormula>>>);

struct DemoSpec {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    prompt: &'static str,
    tags: &'static [&'static str],
    build_base: fn() -> FormulaEngine,
    build_main_concurrent: ConcurrentMainBuilder,
    turns: fn() -> Vec<ScriptedTurn>,
}

fn no_concurrent(
    base: &Workbook<BoundFormula>,
) -> (Workbook<BoundFormula>, Vec<Vec<Op<BoundFormula>>>) {
    (base.clone(), Vec::new())
}

fn set_cells(eng: &mut FormulaEngine, sheet: SheetId, rows: &[(u32, u32, &str)]) {
    for &(r, c, raw) in rows {
        eng.edit_cell_raw(sheet, RowId(u64::from(r)), ColId(u64::from(c)), raw)
            .expect("seed cell");
    }
    eng.recalc_all();
}

fn demo_specs() -> Vec<DemoSpec> {
    vec![
        // 1. regional-sales-summary
        DemoSpec {
            id: "regional-sales-summary",
            title: "Regional Sales Summary & Ranking",
            description: "Aggregate revenue by region with SUMIFS, sort descending, and format currency.",
            prompt: "Summarize total sales by region in E1:F5 using SUMIFS, sort the summary descending by revenue, and format amounts as currency.",
            tags: &["SUMIFS", "sort_range", "format_range"],
            build_base: || {
                let mut e = FormulaEngine::new();
                set_cells(
                    &mut e,
                    SheetId(0),
                    &[
                        (0, 0, "Order"),
                        (0, 1, "Region"),
                        (0, 2, "Sales"),
                        (1, 0, "ORD-01"),
                        (1, 1, "North"),
                        (1, 2, "1200"),
                        (2, 0, "ORD-02"),
                        (2, 1, "South"),
                        (2, 2, "2450"),
                        (3, 0, "ORD-03"),
                        (3, 1, "East"),
                        (3, 2, "1850"),
                        (4, 0, "ORD-04"),
                        (4, 1, "North"),
                        (4, 2, "950"),
                        (5, 0, "ORD-05"),
                        (5, 1, "West"),
                        (5, 2, "3100"),
                        (6, 0, "ORD-06"),
                        (6, 1, "South"),
                        (6, 2, "1400"),
                    ],
                );
                e
            },
            build_main_concurrent: no_concurrent,
            turns: || {
                vec![
                    ScriptedTurn::tool(
                        "Inspecting order ledger schema and regions.",
                        "t1",
                        "workbook_overview",
                        json!({}),
                    ),
                    ScriptedTurn::tool(
                        "Reading order rows A1:C7.",
                        "t2",
                        "read_range",
                        json!({ "range": "A1:C7" }),
                    ),
                    ScriptedTurn::tool(
                        "Dry-running SUMIFS for South region.",
                        "t3",
                        "evaluate_formula",
                        json!({ "cell": "F2", "formula": "=SUMIFS($C$2:$C$7,$B$2:$B$7,\"South\")" }),
                    ),
                    ScriptedTurn::tool(
                        "Writing regional summary table in E1:F5.",
                        "t4",
                        "write_cells",
                        json!({
                            "edits": [
                                { "cell": "E1", "value": "Region" },
                                { "cell": "F1", "value": "Total Sales" },
                                { "cell": "E2", "value": "North" },
                                { "cell": "F2", "formula": "=SUMIFS($C$2:$C$7,$B$2:$B$7,E2)" },
                                { "cell": "E3", "value": "South" },
                                { "cell": "F3", "formula": "=SUMIFS($C$2:$C$7,$B$2:$B$7,E3)" },
                                { "cell": "E4", "value": "East" },
                                { "cell": "F4", "formula": "=SUMIFS($C$2:$C$7,$B$2:$B$7,E4)" },
                                { "cell": "E5", "value": "West" },
                                { "cell": "F5", "formula": "=SUMIFS($C$2:$C$7,$B$2:$B$7,E5)" }
                            ]
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Sorting E1:F5 descending by Total Sales.",
                        "t5",
                        "sort_range",
                        json!({
                            "range": "E1:F5",
                            "has_header": true,
                            "keys": [{ "column": 1, "descending": true }]
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Applying currency formatting and bold headers.",
                        "t6",
                        "format_range",
                        json!({
                            "range": "F2:F5",
                            "number_format": "currency"
                        }),
                    ),
                    ScriptedTurn::tool("Verifying zero errors.", "t7", "list_errors", json!({})),
                    ScriptedTurn::tool(
                        "Done.",
                        "t8",
                        "finish",
                        json!({
                            "summary": "Built regional SUMIFS summary in E1:F5, sorted descending (South $3,850, West $3,100, North $2,150, East $1,850), and formatted as currency.",
                            "highlights": ["Dynamic SUMIFS table in E1:F5", "Sorted descending via single SortHunk", "0 formula errors"]
                        }),
                    ),
                ]
            },
        },
        // 2. gross-margin-audit
        DemoSpec {
            id: "gross-margin-audit",
            title: "Gross Margin Error Audit & Repair",
            description: "Locate #DIV/0! errors via list_errors precedent chains and wrap with IFERROR.",
            prompt: "Audit all formula errors in the gross margin table and guard zero-revenue rows so margins evaluate cleanly.",
            tags: &["list_errors", "IFERROR", "audit"],
            build_base: || {
                let mut e = FormulaEngine::new();
                set_cells(
                    &mut e,
                    SheetId(0),
                    &[
                        (0, 0, "SKU"),
                        (0, 1, "Revenue"),
                        (0, 2, "Cost"),
                        (0, 3, "Margin"),
                        (1, 0, "P-100"),
                        (1, 1, "500"),
                        (1, 2, "300"),
                        (1, 3, "=(B2-C2)/B2"),
                        (2, 0, "P-200"),
                        (2, 1, "0"),
                        (2, 2, "80"),
                        (2, 3, "=(B3-C3)/B3"),
                        (3, 0, "P-300"),
                        (3, 1, "1200"),
                        (3, 2, "780"),
                        (3, 3, "=(B4-C4)/B4"),
                        (4, 0, "Average"),
                        (4, 3, "=AVERAGE(D2:D4)"),
                    ],
                );
                e
            },
            build_main_concurrent: no_concurrent,
            turns: || {
                vec![
                    ScriptedTurn::tool(
                        "Scanning workbook for calculation errors and precedents.",
                        "t1",
                        "list_errors",
                        json!({}),
                    ),
                    ScriptedTurn::tool(
                        "Inspecting rows A1:D5.",
                        "t2",
                        "read_range",
                        json!({ "range": "A1:D5", "include_formulas": true }),
                    ),
                    ScriptedTurn::tool(
                        "Testing IFERROR guard on D3.",
                        "t3",
                        "evaluate_formula",
                        json!({ "cell": "D3", "formula": "=IFERROR((B3-C3)/B3,0)" }),
                    ),
                    ScriptedTurn::tool(
                        "Updating D2:D4 with IFERROR protection and percent formatting.",
                        "t4",
                        "fill_formula",
                        json!({
                            "source_cell": "D2",
                            "formula": "=IFERROR((B2-C2)/B2,0)",
                            "target_range": "D2:D4"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Formatting margin column as percentage.",
                        "t5",
                        "format_range",
                        json!({
                            "range": "D2:D5",
                            "number_format": "percent2"
                        }),
                    ),
                    ScriptedTurn::tool("Re-checking errors.", "t6", "list_errors", json!({})),
                    ScriptedTurn::tool(
                        "Done.",
                        "t7",
                        "finish",
                        json!({
                            "summary": "Repaired #DIV/0! in D3 (SKU P-200) and cascading D5 average by guarding D2:D4 with IFERROR(...,0).",
                            "highlights": ["Fixed 2 cascading #DIV/0! errors", "Formatted D2:D5 as percentages"]
                        }),
                    ),
                ]
            },
        },
        // 3. dynamic-top5-spill
        DemoSpec {
            id: "dynamic-top5-spill",
            title: "Dynamic Array Top-3 Customers",
            description: "Build a dynamic SORT/TAKE spill dashboard and sum with the # spill operator.",
            prompt: "Create a dynamic Top-3 customer leaderboard in D2 using SORT and TAKE, and compute the combined total in E6 using the spill operator D2#.",
            tags: &["SORT", "TAKE", "Spill #"],
            build_base: || {
                let mut e = FormulaEngine::new();
                set_cells(
                    &mut e,
                    SheetId(0),
                    &[
                        (0, 0, "Customer"),
                        (0, 1, "ARR"),
                        (1, 0, "Acme"),
                        (1, 1, "42000"),
                        (2, 0, "Beacon"),
                        (2, 1, "88000"),
                        (3, 0, "Cobalt"),
                        (3, 1, "31000"),
                        (4, 0, "Delta"),
                        (4, 1, "64000"),
                        (5, 0, "Helios"),
                        (5, 1, "95000"),
                    ],
                );
                e
            },
            build_main_concurrent: no_concurrent,
            turns: || {
                vec![
                    ScriptedTurn::tool(
                        "Reading customer ARR table.",
                        "t1",
                        "read_range",
                        json!({ "range": "A1:B6" }),
                    ),
                    ScriptedTurn::tool(
                        "Dry-running dynamic SORT + TAKE formula.",
                        "t2",
                        "evaluate_formula",
                        json!({ "cell": "D2", "formula": "=TAKE(SORT(A2:B6,2,-1),3)" }),
                    ),
                    ScriptedTurn::tool(
                        "Writing leaderboard headers, spill formula, and # aggregate.",
                        "t3",
                        "write_cells",
                        json!({
                            "edits": [
                                { "cell": "D1", "value": "Top Customer" },
                                { "cell": "E1", "value": "Top ARR" },
                                { "cell": "D2", "formula": "=TAKE(SORT(A2:B6,2,-1),3)" },
                                { "cell": "D6", "value": "Top-3 Total" },
                                { "cell": "E6", "formula": "=SUM(INDEX(D2#,0,2))", "number_format": "currency0" }
                            ]
                        }),
                    ),
                    ScriptedTurn::tool("Checking errors.", "t4", "list_errors", json!({})),
                    ScriptedTurn::tool(
                        "Done.",
                        "t5",
                        "finish",
                        json!({
                            "summary": "Created dynamic Top-3 leaderboard spilling from D2 (=TAKE(SORT(A2:B6,2,-1),3)) and summed Top-3 ARR ($247,000) via D2#.",
                            "highlights": ["Live 3x2 dynamic array spill in D2:E4", "Spill operator D2# in E6"]
                        }),
                    ),
                ]
            },
        },
        // 4. monthly-budget-variance
        DemoSpec {
            id: "monthly-budget-variance",
            title: "Budget Variance & Q1 Subtotal Row",
            description: "Insert a Q1 subtotal row with identity-anchored references and compute variance %.",
            prompt: "Insert a Q1 Subtotal row after March (row 4), calculate Budget/Actual subtotals, and add Variance % in column D.",
            tags: &["edit_structure", "insert_rows", "variance"],
            build_base: || {
                let mut e = FormulaEngine::new();
                set_cells(
                    &mut e,
                    SheetId(0),
                    &[
                        (0, 0, "Month"),
                        (0, 1, "Budget"),
                        (0, 2, "Actual"),
                        (0, 3, "Variance %"),
                        (1, 0, "Jan"),
                        (1, 1, "10000"),
                        (1, 2, "10800"),
                        (2, 0, "Feb"),
                        (2, 1, "12000"),
                        (2, 2, "11400"),
                        (3, 0, "Mar"),
                        (3, 1, "11000"),
                        (3, 2, "11880"),
                        (4, 0, "Apr"),
                        (4, 1, "13000"),
                        (4, 2, "12610"),
                    ],
                );
                e
            },
            build_main_concurrent: no_concurrent,
            turns: || {
                vec![
                    ScriptedTurn::tool(
                        "Inspecting monthly budget rows.",
                        "t1",
                        "read_range",
                        json!({ "range": "A1:D5" }),
                    ),
                    ScriptedTurn::tool(
                        "Inserting Q1 Subtotal row at row 5 (0-based index 4).",
                        "t2",
                        "edit_structure",
                        json!({ "action": "insert_rows", "index": 4, "count": 1 }),
                    ),
                    ScriptedTurn::tool(
                        "Writing Q1 subtotal formulas and filling Variance % across D2:D6.",
                        "t3",
                        "write_cells",
                        json!({
                            "edits": [
                                { "cell": "A5", "value": "Q1 Subtotal" },
                                { "cell": "B5", "formula": "=SUM(B2:B4)", "number_format": "currency0" },
                                { "cell": "C5", "formula": "=SUM(C2:C4)", "number_format": "currency0" }
                            ]
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Filling variance formula across D2:D6.",
                        "t4",
                        "fill_formula",
                        json!({
                            "source_cell": "D2",
                            "formula": "=(C2-B2)/B2",
                            "target_range": "D2:D6"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Formatting variance column as percent.",
                        "t5",
                        "format_range",
                        json!({
                            "range": "D2:D6",
                            "number_format": "percent2"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Done.",
                        "t6",
                        "finish",
                        json!({
                            "summary": "Inserted atomic Q1 Subtotal row at row 5 ($33,000 budget vs $34,080 actual, +3.27%) and populated Variance % in D2:D6.",
                            "highlights": ["Atomic StructuralHunk bundles inserted row + subtotal formulas", "Variance % filled in D2:D6"]
                        }),
                    ),
                ]
            },
        },
        // 5. xlookup-price-join
        DemoSpec {
            id: "xlookup-price-join",
            title: "Cross-Sheet XLOOKUP Price Join",
            description: "Join unit prices from a Catalog sheet into Orders via XLOOKUP and fill line totals.",
            prompt: "Add a Catalog price lookup in Orders!C2:C4 using XLOOKUP and compute Line Total in D2:D4.",
            tags: &["XLOOKUP", "cross-sheet", "fill_formula"],
            build_base: || {
                let mut e = FormulaEngine::new();
                set_cells(
                    &mut e,
                    SheetId(0),
                    &[
                        (0, 0, "SKU"),
                        (0, 1, "Qty"),
                        (0, 2, "Unit Price"),
                        (0, 3, "Line Total"),
                        (1, 0, "A10"),
                        (1, 1, "4"),
                        (2, 0, "B20"),
                        (2, 1, "2"),
                        (3, 0, "C30"),
                        (3, 1, "5"),
                    ],
                );
                let s1 = e.alloc_sheet_id();
                e.apply_op(Op::AddSheet {
                    sheet: s1,
                    name: "Catalog".to_owned(),
                    after: Some(SheetId(0)),
                })
                .unwrap();
                set_cells(
                    &mut e,
                    s1,
                    &[
                        (0, 0, "SKU"),
                        (0, 1, "Price"),
                        (1, 0, "A10"),
                        (1, 1, "25.5"),
                        (2, 0, "B20"),
                        (2, 1, "80"),
                        (3, 0, "C30"),
                        (3, 1, "14"),
                    ],
                );
                e
            },
            build_main_concurrent: no_concurrent,
            turns: || {
                vec![
                    ScriptedTurn::tool(
                        "Inspecting both worksheets.",
                        "t1",
                        "workbook_overview",
                        json!({}),
                    ),
                    ScriptedTurn::tool(
                        "Dry-running cross-sheet XLOOKUP.",
                        "t2",
                        "evaluate_formula",
                        json!({ "sheet": "Sheet1", "cell": "C2", "formula": "=XLOOKUP(A2,Catalog!$A$2:$A$4,Catalog!$B$2:$B$4,0)" }),
                    ),
                    ScriptedTurn::tool(
                        "Filling XLOOKUP prices across C2:C4.",
                        "t3",
                        "fill_formula",
                        json!({
                            "sheet": "Sheet1",
                            "source_cell": "C2",
                            "formula": "=XLOOKUP(A2,Catalog!$A$2:$A$4,Catalog!$B$2:$B$4,0)",
                            "target_range": "C2:C4"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Filling Line Total across D2:D4.",
                        "t4",
                        "fill_formula",
                        json!({
                            "sheet": "Sheet1",
                            "source_cell": "D2",
                            "formula": "=B2*C2",
                            "target_range": "D2:D4"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Formatting C2:D4 as currency.",
                        "t5",
                        "format_range",
                        json!({
                            "sheet": "Sheet1",
                            "range": "C2:D4",
                            "number_format": "currency"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Done.",
                        "t6",
                        "finish",
                        json!({
                            "summary": "Joined unit prices from Catalog via XLOOKUP in C2:C4 and computed line totals in D2:D4 ($102, $160, $70).",
                            "highlights": ["Cross-sheet XLOOKUP in C2:C4", "Line totals filled in D2:D4"]
                        }),
                    ),
                ]
            },
        },
        // 6. cohort-kpi-named-range
        DemoSpec {
            id: "cohort-kpi-named-range",
            title: "Named Range NPV & IRR Cashflow Model",
            description: "Define DiscountRate named range and compute project NPV and IRR.",
            prompt: "Define a workbook named range DiscountRate pointing to B1 (8%), then calculate NPV in B7 and IRR in B8.",
            tags: &["NamedRange", "NPV", "IRR"],
            build_base: || {
                let mut e = FormulaEngine::new();
                set_cells(
                    &mut e,
                    SheetId(0),
                    &[
                        (0, 0, "Discount Rate"),
                        (0, 1, "0.08"),
                        (1, 0, "Year 0"),
                        (1, 1, "-10000"),
                        (2, 0, "Year 1"),
                        (2, 1, "3500"),
                        (3, 0, "Year 2"),
                        (3, 1, "4200"),
                        (4, 0, "Year 3"),
                        (4, 1, "4800"),
                    ],
                );
                e
            },
            build_main_concurrent: no_concurrent,
            turns: || {
                vec![
                    ScriptedTurn::tool(
                        "Reading cashflow schedule.",
                        "t1",
                        "read_range",
                        json!({ "range": "A1:B5" }),
                    ),
                    ScriptedTurn::tool(
                        "Defining workbook named range DiscountRate = $B$1.",
                        "t2",
                        "edit_structure",
                        json!({
                            "action": "define_named_range",
                            "name": "DiscountRate",
                            "formula": "=$B$1",
                            "workbook_scope": true
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Writing NPV and IRR summary rows.",
                        "t3",
                        "write_cells",
                        json!({
                            "edits": [
                                { "cell": "A7", "value": "NPV" },
                                { "cell": "B7", "formula": "=NPV(DiscountRate,B3:B5)+B2", "number_format": "currency" },
                                { "cell": "A8", "value": "IRR" },
                                { "cell": "B8", "formula": "=IRR(B2:B5)", "number_format": "percent2" }
                            ]
                        }),
                    ),
                    ScriptedTurn::tool("Verifying errors.", "t4", "list_errors", json!({})),
                    ScriptedTurn::tool(
                        "Done.",
                        "t5",
                        "finish",
                        json!({
                            "summary": "Defined workbook name DiscountRate (=$B$1) and computed NPV ($651.96) and IRR (11.39%).",
                            "highlights": ["MetaHunk defines DiscountRate", "Financial NPV & IRR metrics in B7:B8"]
                        }),
                    ),
                ]
            },
        },
        // 7. messy-text-cleanup
        DemoSpec {
            id: "messy-text-cleanup",
            title: "Messy Customer Name Cleanup",
            description: "Normalize whitespace and casing with TRIM/PROPER and build formatted contact tags with TEXTJOIN.",
            prompt: "Clean customer names into C2:C4 with PROPER(TRIM(...)) and build formatted contact tags in D2:D4 with TEXTJOIN.",
            tags: &["TRIM", "PROPER", "TEXTJOIN"],
            build_base: || {
                let mut e = FormulaEngine::new();
                set_cells(
                    &mut e,
                    SheetId(0),
                    &[
                        (0, 0, "Raw Name"),
                        (0, 1, "Tier"),
                        (0, 2, "Clean Name"),
                        (0, 3, "Tag"),
                        (1, 0, "  alice   smith "),
                        (1, 1, "enterprise"),
                        (2, 0, "  BOB  chen  "),
                        (2, 1, "pro"),
                        (3, 0, "carol   DIAZ"),
                        (3, 1, "starter"),
                    ],
                );
                e
            },
            build_main_concurrent: no_concurrent,
            turns: || {
                vec![
                    ScriptedTurn::tool(
                        "Reading messy contact list.",
                        "t1",
                        "read_range",
                        json!({ "range": "A1:B4" }),
                    ),
                    ScriptedTurn::tool(
                        "Testing PROPER(TRIM(A2)).",
                        "t2",
                        "evaluate_formula",
                        json!({ "cell": "C2", "formula": "=PROPER(TRIM(A2))" }),
                    ),
                    ScriptedTurn::tool(
                        "Filling Clean Name in C2:C4.",
                        "t3",
                        "fill_formula",
                        json!({
                            "source_cell": "C2",
                            "formula": "=PROPER(TRIM(A2))",
                            "target_range": "C2:C4"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Filling formatted Tag in D2:D4.",
                        "t4",
                        "fill_formula",
                        json!({
                            "source_cell": "D2",
                            "formula": "=TEXTJOIN(\" · \",TRUE,C2,UPPER(TRIM(B2)))",
                            "target_range": "D2:D4"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Done.",
                        "t5",
                        "finish",
                        json!({
                            "summary": "Normalized customer names with PROPER(TRIM(...)) in C2:C4 and generated uppercase tier tags via TEXTJOIN in D2:D4.",
                            "highlights": ["Trimmed & title-cased names in C2:C4", "Combined display tags in D2:D4"]
                        }),
                    ),
                ]
            },
        },
        // 8. date-sla-tracking
        DemoSpec {
            id: "date-sla-tracking",
            title: "Workday SLA Due Date & Status",
            description: "Compute business-day SLA due dates with WORKDAY and classify overdue tickets via IFS.",
            prompt: "Compute 5-business-day SLA due dates in D2:D4 using WORKDAY and classify status vs Closed Date in E2:E4 using IFS.",
            tags: &["WORKDAY", "NETWORKDAYS", "IFS"],
            build_base: || {
                let mut e = FormulaEngine::new();
                set_cells(
                    &mut e,
                    SheetId(0),
                    &[
                        (0, 0, "Ticket"),
                        (0, 1, "Opened"),
                        (0, 2, "Closed"),
                        (0, 3, "SLA Due"),
                        (0, 4, "Status"),
                        (1, 0, "TK-1"),
                        (1, 1, "2026-11-02"),
                        (1, 2, "2026-11-06"),
                        (2, 0, "TK-2"),
                        (2, 1, "2026-11-02"),
                        (2, 2, "2026-11-12"),
                        (3, 0, "TK-3"),
                        (3, 1, "2026-11-05"),
                        (3, 2, "2026-11-11"),
                    ],
                );
                e
            },
            build_main_concurrent: no_concurrent,
            turns: || {
                vec![
                    ScriptedTurn::tool(
                        "Reading ticket date columns.",
                        "t1",
                        "read_range",
                        json!({ "range": "A1:C4" }),
                    ),
                    ScriptedTurn::tool(
                        "Filling 5-workday SLA due date in D2:D4.",
                        "t2",
                        "fill_formula",
                        json!({
                            "source_cell": "D2",
                            "formula": "=WORKDAY(B2,5)",
                            "target_range": "D2:D4"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Formatting D2:D4 as ISO dates.",
                        "t3",
                        "format_range",
                        json!({
                            "range": "D2:D4",
                            "number_format": "date"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Filling SLA compliance status in E2:E4.",
                        "t4",
                        "fill_formula",
                        json!({
                            "source_cell": "E2",
                            "formula": "=IFS(C2<=D2,\"On Time\",TRUE,\"Overdue\")",
                            "target_range": "E2:E4"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Done.",
                        "t5",
                        "finish",
                        json!({
                            "summary": "Calculated 5-business-day WORKDAY due dates in D2:D4 and classified SLA status (2 On Time, 1 Overdue) in E2:E4.",
                            "highlights": ["WORKDAY SLA deadlines in D2:D4", "IFS status classification in E2:E4"]
                        }),
                    ),
                ]
            },
        },
        // 9. quarterly-sort-ranking
        DemoSpec {
            id: "quarterly-sort-ranking",
            title: "Quarterly Multi-Key Sort & Percentile Rank",
            description: "Sort reps by Region then Score descending, then add RANK.EQ and PERCENTILE.INC.",
            prompt: "Sort A1:C6 by Region ascending then Score descending, add RANK.EQ in D2:D6, and show the 80th percentile threshold in C8.",
            tags: &["sort_range", "RANK.EQ", "PERCENTILE.INC"],
            build_base: || {
                let mut e = FormulaEngine::new();
                set_cells(
                    &mut e,
                    SheetId(0),
                    &[
                        (0, 0, "Rep"),
                        (0, 1, "Region"),
                        (0, 2, "Score"),
                        (0, 3, "Rank"),
                        (1, 0, "Maya"),
                        (1, 1, "West"),
                        (1, 2, "84"),
                        (2, 0, "Liam"),
                        (2, 1, "East"),
                        (2, 2, "92"),
                        (3, 0, "Noah"),
                        (3, 1, "West"),
                        (3, 2, "96"),
                        (4, 0, "Zoe"),
                        (4, 1, "East"),
                        (4, 2, "78"),
                        (5, 0, "Aria"),
                        (5, 1, "East"),
                        (5, 2, "88"),
                    ],
                );
                e
            },
            build_main_concurrent: no_concurrent,
            turns: || {
                vec![
                    ScriptedTurn::tool(
                        "Inspecting rep score sheet.",
                        "t1",
                        "read_range",
                        json!({ "range": "A1:D6" }),
                    ),
                    ScriptedTurn::tool(
                        "Multi-key sorting A1:C6 by Region ASC, Score DESC.",
                        "t2",
                        "sort_range",
                        json!({
                            "range": "A1:C6",
                            "has_header": true,
                            "keys": [
                                { "column": 1, "descending": false },
                                { "column": 2, "descending": true }
                            ]
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Filling RANK.EQ in D2:D6.",
                        "t3",
                        "fill_formula",
                        json!({
                            "source_cell": "D2",
                            "formula": "=RANK.EQ(C2,$C$2:$C$6,0)",
                            "target_range": "D2:D6"
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Adding P80 threshold in B8:C8.",
                        "t4",
                        "write_cells",
                        json!({
                            "edits": [
                                { "cell": "B8", "value": "P80 Cutoff" },
                                { "cell": "C8", "formula": "=PERCENTILE.INC(C2:C6,0.8)", "number_format": "fixed2" }
                            ]
                        }),
                    ),
                    ScriptedTurn::tool(
                        "Done.",
                        "t5",
                        "finish",
                        json!({
                            "summary": "Sorted A1:C6 by Region then Score descending, added RANK.EQ in D2:D6, and computed the 80th percentile cutoff (92.80) in C8.",
                            "highlights": ["Multi-key SortHunk", "RANK.EQ & PERCENTILE.INC verified"]
                        }),
                    ),
                ]
            },
        },
        // 10. conflict-and-closure-demo
        DemoSpec {
            id: "conflict-and-closure-demo",
            title: "Interactive Conflict & Reference Closure Lab",
            description: "Hands-on walkthrough with a concurrent main-branch cell conflict and an inserted-row reference closure check.",
            prompt: "Insert a Tax Adjustment row at row 2, update Q1 Forecast (B3) to 520, and reference the new Tax row from Net Total (C1).",
            tags: &["3-Way Conflict", "Reference Closure", "No Sign-In"],
            build_base: || {
                let mut e = FormulaEngine::new();
                set_cells(
                    &mut e,
                    SheetId(0),
                    &[
                        (0, 0, "Item"),
                        (0, 1, "Amount"),
                        (0, 2, "Net Total"),
                        (1, 0, "Q1 Forecast"),
                        (1, 1, "400"),
                        (2, 0, "Q2 Forecast"),
                        (2, 1, "450"),
                    ],
                );
                e
            },
            build_main_concurrent: |base| {
                // Simulate a human teammate concurrently editing B2 (Q1 Forecast: 400 -> 480) on main!
                let mut main_eng = FormulaEngine::from_workbook_recalc_all(base.clone());
                let op = main_eng
                    .build_cell_edit(SheetId(0), RowId(1), ColId(1), "480")
                    .expect("concurrent edit");
                main_eng
                    .apply_op(op.clone())
                    .expect("apply concurrent edit");
                main_eng.recalc_all();
                (main_eng.workbook().clone(), vec![vec![op]])
            },
            turns: || {
                vec![
                    ScriptedTurn::tool(
                        "Reading forecast table A1:C3.",
                        "t1",
                        "read_range",
                        json!({ "range": "A1:C3" }),
                    ),
                    ScriptedTurn::tool(
                        "Inserting Tax Adjustment row at row 2 (index 1).",
                        "t2",
                        "edit_structure",
                        json!({ "action": "insert_rows", "index": 1, "count": 1 }),
                    ),
                    ScriptedTurn::tool(
                        "Populating Tax row (A2:B2), updating Q1 Forecast (B3=520), and linking C1 to =B2+B3.",
                        "t3",
                        "write_cells",
                        json!({
                            "edits": [
                                { "cell": "A2", "value": "Tax Adjustment" },
                                { "cell": "B2", "value": "35" },
                                { "cell": "B3", "value": "520" },
                                { "cell": "C1", "formula": "=B2+B3" }
                            ]
                        }),
                    ),
                    ScriptedTurn::tool("Checking formula errors.", "t4", "list_errors", json!({})),
                    ScriptedTurn::tool(
                        "Done.",
                        "t5",
                        "finish",
                        json!({
                            "summary": "Inserted Tax Adjustment row ($35), updated Q1 Forecast to $520 (conflicting with main's $480), and set Net Total C1 = B2+B3.",
                            "highlights": [
                                "Try unchecking 'Insert 1 row' while keeping C1 to trigger the Reference Closure guard!",
                                "Pick 'Keep Main ($480)' or 'Use AI Draft ($520)' on the Q1 Forecast conflict."
                            ]
                        }),
                    ),
                ]
            },
        },
    ]
}

fn encode_ops_b64(label: &str, ops: &[Op<BoundFormula>]) -> Result<String, AgentError> {
    let txn = ops_to_wire_txn_v3(label.to_owned(), ops)
        .map_err(|e| AgentError::Merge(format!("wire txn encode failed: {e:?}")))?;
    let raw = postcard::to_allocvec(&txn)
        .map_err(|e| AgentError::Merge(format!("postcard encode failed: {e}")))?;
    Ok(B64.encode(raw))
}

/// Generates all 10 verified, self-contained `ReplayBundle` artifacts by running
/// each scenario through the real agent runtime and three-way diff/merge planner.
pub async fn build_all_demo_replays() -> Result<(ReplayIndex, Vec<ReplayBundle>), AgentError> {
    let specs = demo_specs();
    let mut index_items = Vec::with_capacity(specs.len());
    let mut bundles = Vec::with_capacity(specs.len());

    for spec in specs {
        let base_eng = (spec.build_base)();
        let base_wb = base_eng.workbook().clone();
        let base_snap = encode_snapshot_v1(&base_wb, &[])
            .map_err(|e| AgentError::Workbook(format!("snapshot encode failed: {e:?}")))?;
        let base_snapshot_b64 = B64.encode(base_snap);

        let (main_wb, concurrent_batches) = (spec.build_main_concurrent)(&base_wb);
        let mut main_concurrent_txns_b64 = Vec::new();
        for batch in &concurrent_batches {
            main_concurrent_txns_b64.push(encode_ops_b64("Concurrent main edit", batch)?);
        }

        let draft_rep = pack_replica_id(0, 10)
            .ok_or_else(|| AgentError::Workbook("draft replica".to_owned()))?;
        let mut sink =
            HeadlessEvalSink::new(base_wb.clone(), 0, draft_rep).map_err(AgentError::Workbook)?;
        let store = MemoryStepStore::default();
        let provider = ScriptedMockProvider::new((spec.turns)());

        let mut presence_by_step: BTreeMap<i32, AgentPresenceHint> = BTreeMap::new();
        let outcome = run_agent_loop(
            spec.prompt,
            &provider,
            &mut sink,
            &store,
            AgentRunConfig::default(),
            None,
            |_| {},
        )
        .await?;

        let raw_steps = store.snapshot();
        let mut txn_by_seq: BTreeMap<u64, String> = BTreeMap::new();
        for committed in &sink.committed_txns {
            let raw = postcard::to_allocvec(&committed.txn)
                .map_err(|e| AgentError::Merge(e.to_string()))?;
            txn_by_seq.insert(committed.seq, B64.encode(raw));
        }

        // Pair presence hints with commit/read steps cleanly.
        let mut pres_iter = sink.presence_history.iter().copied();
        for st in &raw_steps {
            if matches!(st.kind, AgentStepKind::ToolCall)
                && matches!(
                    st.payload.get("name").and_then(|v| v.as_str()),
                    Some(
                        "read_range"
                            | "write_cells"
                            | "fill_formula"
                            | "edit_structure"
                            | "sort_range"
                            | "format_range"
                    )
                )
                && let Some(p) = pres_iter.next()
            {
                presence_by_step.insert(st.idx, p);
            }
        }

        let mut replay_steps = Vec::new();
        for st in &raw_steps {
            let (title, detail, txn_b64) = match st.kind {
                AgentStepKind::UserPrompt => (
                    "Prompt".to_owned(),
                    st.payload
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .unwrap_or(spec.prompt)
                        .to_owned(),
                    None,
                ),
                AgentStepKind::AssistantThought => (
                    "Analyst".to_owned(),
                    st.payload
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned(),
                    None,
                ),
                AgentStepKind::ToolCall => {
                    let tname = st
                        .payload
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool");
                    (
                        format!("Tool · {tname}"),
                        st.payload
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}")
                            .to_owned(),
                        None,
                    )
                }
                AgentStepKind::TxnCommit => {
                    let seq: u64 = st
                        .payload
                        .get("seq")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let tool = st
                        .payload
                        .get("tool")
                        .and_then(|v| v.as_str())
                        .unwrap_or("write");
                    (
                        format!("Draft commit #{seq}"),
                        format!("Applied `{tool}` transaction to isolated AI draft branch"),
                        txn_by_seq.get(&seq).cloned(),
                    )
                }
                AgentStepKind::ToolResult => {
                    let tname = st
                        .payload
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool");
                    let out = st
                        .payload
                        .get("output")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let clipped: String = out.chars().take(180).collect();
                    (format!("Result · {tname}"), clipped, None)
                }
                AgentStepKind::Finish => (
                    "Completed".to_owned(),
                    st.payload
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&outcome.summary)
                        .to_owned(),
                    None,
                ),
                AgentStepKind::Error => ("Error".to_owned(), st.payload.to_string(), None),
            };
            replay_steps.push(ReplayStep {
                idx: st.idx,
                kind: st.kind,
                title,
                detail,
                presence: presence_by_step.get(&st.idx).copied(),
                txn_b64,
            });
        }

        let plan = compute_branch_diff(
            &base_wb,
            sink.engine().workbook(),
            &main_wb,
            0,
            concurrent_batches.len() as u64,
            &sink.committed_txns,
        );

        let merge_rep = pack_replica_id(0, 20)
            .ok_or_else(|| AgentError::Workbook("merge replica".to_owned()))?;

        // Precompute closure dependencies for every non-structural hunk when structural hunks are unchecked.
        let mut closure_dependencies = Vec::new();
        let structural_ids: BTreeSet<&str> = plan
            .diff
            .hunks
            .iter()
            .filter(|h| h.kind == DiffHunkKind::Structural)
            .map(|h| h.id.as_str())
            .collect();

        let mut hunk_ops = BTreeMap::new();
        let mut next_id_counter: u32 = 1;

        for h in &plan.diff.hunks {
            let mut choices = BTreeMap::new();
            for c in &h.cells {
                if c.conflict {
                    choices.insert(c.cell_key.clone(), ConflictResolution::TakeDraft);
                }
            }
            let solo_sel = MergeSelection {
                accepted_hunks: vec![h.id.clone()],
                conflict_choices: choices.clone(),
                allow_ref_errors: false,
                expected_main_seq: None,
            };
            let issues = check_reference_closure(&plan, &solo_sel);
            for issue in &issues {
                closure_dependencies.push(ReplayClosureDep {
                    source_hunk_id: h.id.clone(),
                    target_hunk_id: issue.target_hunk_id.clone(),
                    issue: issue.clone(),
                });
            }

            let mut apply_txns_b64 = Vec::new();
            let mut ref_sanitized_txns_b64 = Vec::new();

            if issues.is_empty() {
                if let Ok(batches) =
                    plan_merge_ops(&plan, &solo_sel, &main_wb, 0, merge_rep, next_id_counter)
                {
                    next_id_counter = next_id_counter.saturating_add(100);
                    for batch in batches {
                        apply_txns_b64.push(encode_ops_b64(&h.label, &batch)?);
                    }
                }
            } else {
                // When accepted together with its required structural hunk(s), build clean merge ops,
                // and also build #REF!-sanitized ops if user chooses allow_ref_errors = true.
                let mut with_deps = vec![h.id.clone()];
                for issue in &issues {
                    if !with_deps.contains(&issue.target_hunk_id) {
                        with_deps.push(issue.target_hunk_id.clone());
                    }
                }
                let combined_sel = MergeSelection {
                    accepted_hunks: with_deps,
                    conflict_choices: choices.clone(),
                    allow_ref_errors: false,
                    expected_main_seq: None,
                };
                if let Ok(batches) = plan_merge_ops(
                    &plan,
                    &combined_sel,
                    &main_wb,
                    0,
                    merge_rep,
                    next_id_counter,
                ) {
                    next_id_counter = next_id_counter.saturating_add(100);
                    for batch in batches {
                        apply_txns_b64.push(encode_ops_b64(&h.label, &batch)?);
                    }
                }
                let ref_sel = MergeSelection {
                    accepted_hunks: vec![h.id.clone()],
                    conflict_choices: choices,
                    allow_ref_errors: true,
                    expected_main_seq: None,
                };
                if let Ok(batches) =
                    plan_merge_ops(&plan, &ref_sel, &main_wb, 0, merge_rep, next_id_counter)
                {
                    next_id_counter = next_id_counter.saturating_add(100);
                    for batch in batches {
                        ref_sanitized_txns_b64.push(encode_ops_b64(&h.label, &batch)?);
                    }
                }
            }

            hunk_ops.insert(
                h.id.clone(),
                ReplayHunkOps {
                    apply_txns_b64,
                    ref_sanitized_txns_b64,
                },
            );
        }

        // Precompute full accept-all merge payload (TakeDraft for conflicts).
        let mut all_choices = BTreeMap::new();
        for h in &plan.diff.hunks {
            for c in &h.cells {
                if c.conflict {
                    all_choices.insert(c.cell_key.clone(), ConflictResolution::TakeDraft);
                }
            }
        }
        let all_sel = MergeSelection {
            accepted_hunks: plan.diff.hunks.iter().map(|h| h.id.clone()).collect(),
            conflict_choices: all_choices,
            allow_ref_errors: false,
            expected_main_seq: None,
        };
        let all_batches = plan_merge_ops(&plan, &all_sel, &main_wb, 0, merge_rep, next_id_counter)
            .map_err(|e| {
                AgentError::Merge(format!("accept-all plan failed for {}: {e:?}", spec.id))
            })?;
        let mut accept_all_txns_b64 = Vec::new();
        for batch in &all_batches {
            accept_all_txns_b64.push(encode_ops_b64(&format!("Merge {}", spec.title), batch)?);
        }

        // Verify accept-all actually applies cleanly to main_wb with zero errors!
        let mut verify_eng = FormulaEngine::from_workbook_recalc_all(main_wb.clone());
        for batch in &all_batches {
            for op in batch {
                verify_eng.apply_op(op.clone()).map_err(|e| {
                    AgentError::Merge(format!("verify merge failed for {}: {e:?}", spec.id))
                })?;
            }
        }
        verify_eng.recalc_all();
        let remaining_errs = verify_eng.list_error_cells(None, 5);
        if !remaining_errs.is_empty() {
            return Err(AgentError::Merge(format!(
                "demo `{}` left {} calculation error(s) after merge: {:?}",
                spec.id,
                remaining_errs.len(),
                remaining_errs
            )));
        }

        let has_conflict_demo = plan.diff.summary.conflict_cells > 0;
        let has_closure_demo = !closure_dependencies.is_empty() && !structural_ids.is_empty();

        index_items.push(ReplayIndexEntry {
            id: spec.id.to_owned(),
            title: spec.title.to_owned(),
            description: spec.description.to_owned(),
            prompt: spec.prompt.to_owned(),
            tags: spec.tags.iter().map(|&t| t.to_owned()).collect(),
            has_conflict_demo,
            has_closure_demo,
            step_count: replay_steps.len() as u32,
        });

        bundles.push(ReplayBundle {
            version: 1,
            id: spec.id.to_owned(),
            title: spec.title.to_owned(),
            description: spec.description.to_owned(),
            prompt: spec.prompt.to_owned(),
            tags: spec.tags.iter().map(|&t| t.to_owned()).collect(),
            summary: outcome.summary,
            highlights: outcome.highlights,
            base_snapshot_b64,
            main_concurrent_txns_b64,
            steps: replay_steps,
            diff: plan.diff,
            closure_dependencies,
            hunk_ops,
            accept_all_txns_b64,
        });
    }

    let index = ReplayIndex {
        version: 1,
        generated_with: "reticula-agent-kernel-deterministic".to_owned(),
        items: index_items,
    };
    Ok((index, bundles))
}

/// Writes `index.json` and all 10 `<id>.json` bundles to `out_dir`.
pub async fn write_demo_replays_to_dir(out_dir: &Path) -> Result<ReplayIndex, AgentError> {
    let (index, bundles) = build_all_demo_replays().await?;
    std::fs::create_dir_all(out_dir)
        .map_err(|e| AgentError::Store(format!("cannot create {}: {e}", out_dir.display())))?;
    let idx_json = serde_json::to_string_pretty(&index)
        .map_err(|e| AgentError::Store(format!("serialize index: {e}")))?;
    std::fs::write(out_dir.join("index.json"), idx_json)
        .map_err(|e| AgentError::Store(format!("write index.json: {e}")))?;
    for bundle in &bundles {
        let json = serde_json::to_string(bundle)
            .map_err(|e| AgentError::Store(format!("serialize {}: {e}", bundle.id)))?;
        std::fs::write(out_dir.join(format!("{}.json", bundle.id)), json)
            .map_err(|e| AgentError::Store(format!("write {}.json: {e}", bundle.id)))?;
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn all_ten_demo_replays_build_cleanly_and_demo_10_has_conflict_and_closure() {
        let (index, bundles) = build_all_demo_replays().await.unwrap();
        assert_eq!(index.items.len(), 10);
        assert_eq!(bundles.len(), 10);
        let d6 = bundles
            .iter()
            .find(|b| b.id == "cohort-kpi-named-range")
            .unwrap();
        let d6_cells: BTreeMap<&str, &str> = d6
            .diff
            .hunks
            .iter()
            .flat_map(|h| h.cells.iter())
            .map(|c| (c.address.as_str(), c.draft_value.as_str()))
            .collect();
        assert_eq!(d6_cells.get("B7").copied(), Some("$651.96"));
        assert_eq!(d6_cells.get("B8").copied(), Some("11.39%"));
        assert!(
            d6.summary.contains("$651.96") && d6.summary.contains("11.39%"),
            "cohort-kpi-named-range summary must match evaluated engine cells: {}",
            d6.summary
        );

        let d10 = bundles
            .iter()
            .find(|b| b.id == "conflict-and-closure-demo")
            .unwrap();
        assert!(d10.diff.summary.conflict_cells >= 1);
        assert!(!d10.closure_dependencies.is_empty());
        assert!(!d10.main_concurrent_txns_b64.is_empty());
    }
}
