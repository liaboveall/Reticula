//! Built-in spreadsheet functions (`17-m3a.md` 5.1 — 193 unique functions,
//! 200 catalog entries including aliases).

pub mod catalog;
pub mod criteria;
pub mod datetime;
pub mod dynamic_array;
pub mod financial;
pub mod logic_info;
pub mod lookup;
pub mod math_trig;
pub mod stats;
pub mod text_conv;
pub mod wildcard;

use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::decimal15::{
    RoundingMode, excel_int, excel_mod, round_decimal, snap_cancelled_zero,
};
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::num;
use reticula_core::ops::{MAX_CELL_TEXT_UTF16, Workbook};
use reticula_core::rng::Rng;
use reticula_core::sheet::Sheet;
use reticula_core::store::{BLOCK_ROWS, Block};
use reticula_core::value::{CalcError, Value};

use crate::binder::{BoundFormula, live_span_positions};
use crate::lookup_index::LookupIndexCache;
use crate::spill::SpillRegistry;
use crate::value_eval::{
    Array2D, CallerContext, EvalScalar, EvalValue, MAX_ARRAY_CELLS, ResolvedRef, ScalarOrArray,
    coerce_for_scalar_op,
};

pub use logic_info::{combine_ifs_array, finalize_branch_value, switch_scalar_eq};
pub use lookup::{IndirectTarget, combine_choose, eval_offset, resolve_indirect};

/// Default deterministic date serial (`2026-01-01 00:00:00` = `46_023.0`) used
/// by [`EvalEnv::default`] when no host clock has been injected (`14-m1b.md` 5.3).
pub const DEFAULT_NOW_SERIAL: f64 = 46_023.0;

/// Default deterministic PRNG seed used by [`EvalEnv::default`].
pub const DEFAULT_EVAL_SEED: u64 = 0x2026_0101_5EED_CAFE;

/// Pure host-injected environment for volatile functions (`TODAY`, `NOW`,
/// `RAND`, `RANDBETWEEN`; `14-m1b.md` 5.3, D-021).
#[derive(Clone, Debug)]
pub struct EvalEnv {
    /// Current local date+time as an Excel 1900-system serial number.
    pub now_serial: f64,
    /// Deterministic PRNG stepped by `RAND` and `RANDBETWEEN`.
    pub rng: Rng,
}

impl Default for EvalEnv {
    fn default() -> Self {
        Self::new(DEFAULT_NOW_SERIAL, DEFAULT_EVAL_SEED)
    }
}

impl EvalEnv {
    /// Constructs an evaluation environment from a 1900 date serial and 64-bit seed.
    #[must_use]
    pub fn new(now_serial: f64, seed: u64) -> Self {
        Self {
            now_serial,
            rng: Rng::from_seed(seed),
        }
    }
}

/// Evaluates an eager (non-short-circuiting) built-in function `name` on
/// already-evaluated `args`. (`IF`, `IFERROR`, `IFNA`, `IFS`, `SWITCH`, `CHOOSE`,
/// `OFFSET`, and `INDIRECT` are handled directly in `FormulaEngine`.)
#[expect(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "central 192-arm built-in dispatcher"
)]
pub fn eval_eager_function(
    name: &str,
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    env: &mut EvalEnv,
    is_row_hidden: Option<&dyn Fn(SheetId, RowId) -> bool>,
    lookup_cache: &mut LookupIndexCache,
) -> EvalValue {
    match name {
        // --- Layer 0: Aggregations ---
        "SUM" => EvalValue::Scalar(eval_sum(&args, workbook, spills)),
        "AVERAGE" => EvalValue::Scalar(eval_average(&args, workbook, spills)),
        "COUNT" => EvalValue::Scalar(eval_count(&args, workbook, spills)),
        "COUNTA" => EvalValue::Scalar(eval_counta(&args, workbook, spills)),
        "MIN" => EvalValue::Scalar(eval_min_max(&args, workbook, spills, true)),
        "MAX" => EvalValue::Scalar(eval_min_max(&args, workbook, spills, false)),

        // --- Layer 0 & M3a: Logic & Type Information ---
        "AND" => EvalValue::Scalar(eval_and_or(&args, workbook, spills, true)),
        "OR" => EvalValue::Scalar(eval_and_or(&args, workbook, spills, false)),
        "XOR" => logic_info::eval_xor(&args, workbook, spills),
        "NOT" => lift_unary(args, workbook, spills, caller, |x| {
            x.to_bool()
                .map_or_else(EvalScalar::Error, |b| EvalScalar::Bool(!b))
        }),
        "TRUE" => {
            if args.is_empty() {
                EvalValue::Scalar(EvalScalar::Bool(true))
            } else {
                EvalValue::Scalar(EvalScalar::Error(CalcError::Value))
            }
        }
        "FALSE" => {
            if args.is_empty() {
                EvalValue::Scalar(EvalScalar::Bool(false))
            } else {
                EvalValue::Scalar(EvalScalar::Error(CalcError::Value))
            }
        }
        "NA" => {
            if args.is_empty() {
                EvalValue::Scalar(EvalScalar::Error(CalcError::Na))
            } else {
                EvalValue::Scalar(EvalScalar::Error(CalcError::Value))
            }
        }
        "ISERR" => logic_info::eval_iserr(args, workbook, spills, caller),
        "ISNA" => logic_info::eval_isna(args, workbook, spills, caller),
        "ISNONTEXT" => logic_info::eval_isnontext(args, workbook, spills, caller),
        "ISFORMULA" => logic_info::eval_isformula(&args, workbook, caller),
        "ISREF" => logic_info::eval_isref(&args),
        "ISEVEN" => logic_info::eval_iseven_or_isodd(args, workbook, spills, caller, true),
        "ISODD" => logic_info::eval_iseven_or_isodd(args, workbook, spills, caller, false),
        "ERROR.TYPE" => logic_info::eval_error_type(args, workbook, spills, caller),
        "TYPE" => logic_info::eval_type(&args, workbook, spills),

        // --- Layer 0, Layer 1 & M3a: Math, Trig & Volatiles ---
        "ABS" => lift_unary(args, workbook, spills, caller, |x| {
            x.to_number()
                .map_or_else(EvalScalar::Error, |n| EvalScalar::number(n.abs()))
        }),
        "INT" => lift_unary(args, workbook, spills, caller, |x| match x.to_number() {
            Ok(n) => excel_int(n).map_or(EvalScalar::Error(CalcError::Num), EvalScalar::number),
            Err(e) => EvalScalar::Error(e),
        }),
        "SQRT" => lift_unary(args, workbook, spills, caller, |x| match x.to_number() {
            Ok(n) if n < 0.0 => EvalScalar::Error(CalcError::Num),
            Ok(n) => EvalScalar::number(num::sqrt(n)),
            Err(e) => EvalScalar::Error(e),
        }),
        "ROUND" => lift_binary(args, workbook, spills, caller, |x, d| {
            eval_round_scalar(x, d, RoundingMode::HalfAwayFromZero)
        }),
        "ROUNDDOWN" => lift_binary(args, workbook, spills, caller, |x, d| {
            eval_round_scalar(x, d, RoundingMode::TowardZero)
        }),
        "ROUNDUP" => lift_binary(args, workbook, spills, caller, |x, d| {
            eval_round_scalar(x, d, RoundingMode::AwayFromZero)
        }),
        "TRUNC" => eval_trunc(args, workbook, spills, caller),
        "FLOOR" => text_conv::eval_floor_or_ceiling(args, workbook, spills, caller, true),
        "CEILING" => text_conv::eval_floor_or_ceiling(args, workbook, spills, caller, false),
        "MOD" => lift_binary(args, workbook, spills, caller, |a, b| {
            let x = match a.to_number() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let y = match b.to_number() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            if y == 0.0 {
                return EvalScalar::Error(CalcError::Div0);
            }
            excel_mod(x, y).map_or(EvalScalar::Error(CalcError::Num), EvalScalar::number)
        }),
        "POWER" => lift_binary(args, workbook, spills, caller, |a, b| {
            let x = match a.to_number() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let y = match b.to_number() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            if x == 0.0 && y <= 0.0 {
                return EvalScalar::Error(CalcError::Num);
            }
            if x < 0.0 && num::trunc(y) != y {
                return EvalScalar::Error(CalcError::Num);
            }
            EvalScalar::number(num::pow(x, y))
        }),
        "SIGN" => math_trig::eval_sign(args, workbook, spills, caller),
        "EVEN" => math_trig::eval_even(args, workbook, spills, caller),
        "ODD" => math_trig::eval_odd(args, workbook, spills, caller),
        "MROUND" => math_trig::eval_mround(args, workbook, spills, caller),
        "QUOTIENT" => math_trig::eval_quotient(args, workbook, spills, caller),
        "FACT" => math_trig::eval_fact(args, workbook, spills, caller),
        "COMBIN" => math_trig::eval_combin(args, workbook, spills, caller),
        "PERMUT" => math_trig::eval_permut(args, workbook, spills, caller),
        "GCD" => math_trig::eval_gcd(&args, workbook, spills),
        "LCM" => math_trig::eval_lcm(&args, workbook, spills),
        "PI" => math_trig::eval_pi(&args),
        "RADIANS" => math_trig::eval_radians(args, workbook, spills, caller),
        "DEGREES" => math_trig::eval_degrees(args, workbook, spills, caller),
        "EXP" => math_trig::eval_exp(args, workbook, spills, caller),
        "LN" => math_trig::eval_ln(args, workbook, spills, caller),
        "LOG" => math_trig::eval_log(args, workbook, spills, caller),
        "LOG10" => math_trig::eval_log10(args, workbook, spills, caller),
        "SIN" => math_trig::eval_sin(args, workbook, spills, caller),
        "COS" => math_trig::eval_cos(args, workbook, spills, caller),
        "TAN" => math_trig::eval_tan(args, workbook, spills, caller),
        "ASIN" => math_trig::eval_asin(args, workbook, spills, caller),
        "ACOS" => math_trig::eval_acos(args, workbook, spills, caller),
        "ATAN" => math_trig::eval_atan(args, workbook, spills, caller),
        "ATAN2" => math_trig::eval_atan2(args, workbook, spills, caller),
        "SINH" => math_trig::eval_sinh(args, workbook, spills, caller),
        "COSH" => math_trig::eval_cosh(args, workbook, spills, caller),
        "TANH" => math_trig::eval_tanh(args, workbook, spills, caller),
        "PRODUCT" => math_trig::eval_product(&args, workbook, spills),
        "SUMSQ" => math_trig::eval_sumsq(&args, workbook, spills),
        "TODAY" => text_conv::eval_today_or_now(&args, env, false),
        "NOW" => text_conv::eval_today_or_now(&args, env, true),
        "RAND" => text_conv::eval_rand(&args, env),
        "RANDBETWEEN" => text_conv::eval_randbetween(args, workbook, spills, caller, env),

        // --- Layer 0, Layer 1 & M3a: Text & Conversion ---
        "LEN" => lift_unary(args, workbook, spills, caller, |x| match x.to_text() {
            Ok(s) => {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "cell text length <= 32,767 fits in f64 without loss"
                )]
                let len = s.chars().count() as f64;
                EvalScalar::Number(len)
            }
            Err(e) => EvalScalar::Error(e),
        }),
        "LOWER" => lift_unary(args, workbook, spills, caller, |x| match x.to_text() {
            Ok(s) => checked_text(s.to_lowercase()),
            Err(e) => EvalScalar::Error(e),
        }),
        "UPPER" => lift_unary(args, workbook, spills, caller, |x| match x.to_text() {
            Ok(s) => checked_text(s.to_uppercase()),
            Err(e) => EvalScalar::Error(e),
        }),
        "TRIM" => lift_unary(args, workbook, spills, caller, |x| match x.to_text() {
            Ok(s) => EvalScalar::Text(excel_trim(&s)),
            Err(e) => EvalScalar::Error(e),
        }),
        "LEFT" => eval_left_or_right(args, workbook, spills, caller, true),
        "RIGHT" => eval_left_or_right(args, workbook, spills, caller, false),
        "MID" => eval_mid(args, workbook, spills, caller),
        "CONCAT" => EvalValue::Scalar(eval_concat(&args, workbook, spills)),
        "CONCATENATE" => text_conv::eval_concatenate(args, workbook, spills, caller),
        "TEXTJOIN" => text_conv::eval_textjoin(args, workbook, spills, caller),
        "TEXTBEFORE" => text_conv::eval_text_before_or_after(args, workbook, spills, caller, false),
        "TEXTAFTER" => text_conv::eval_text_before_or_after(args, workbook, spills, caller, true),
        "TEXTSPLIT" => text_conv::eval_textsplit(args, workbook, spills, caller),
        "EXACT" => text_conv::eval_exact(args, workbook, spills, caller),
        "PROPER" => text_conv::eval_proper(args, workbook, spills, caller),
        "REPT" => text_conv::eval_rept(args, workbook, spills, caller),
        "CHAR" => text_conv::eval_char(args, workbook, spills, caller),
        "CODE" => text_conv::eval_code(args, workbook, spills, caller),
        "UNICHAR" => text_conv::eval_unichar(args, workbook, spills, caller),
        "UNICODE" => text_conv::eval_unicode(args, workbook, spills, caller),
        "CLEAN" => text_conv::eval_clean(args, workbook, spills, caller),
        "T" => text_conv::eval_t(args, workbook, spills, caller),
        "N" => text_conv::eval_n(args, workbook, spills, caller),
        "FIXED" => text_conv::eval_fixed(args, workbook, spills, caller),
        "DOLLAR" => text_conv::eval_dollar(args, workbook, spills, caller),
        "NUMBERVALUE" => text_conv::eval_numbervalue(args, workbook, spills, caller),
        "FIND" => text_conv::eval_find_or_search(args, workbook, spills, caller, true),
        "SEARCH" => text_conv::eval_find_or_search(args, workbook, spills, caller, false),
        "SUBSTITUTE" => text_conv::eval_substitute(args, workbook, spills, caller),
        "REPLACE" => text_conv::eval_replace(args, workbook, spills, caller),
        "VALUE" => text_conv::eval_value(args, workbook, spills, caller),
        "TEXT" => text_conv::eval_text(args, workbook, spills, caller),

        // --- Layer 0: Basic type predicates ---
        "ISBLANK" => lift_unary(args, workbook, spills, caller, |x| {
            EvalScalar::Bool(matches!(x, EvalScalar::Empty))
        }),
        "ISNUMBER" => lift_unary(args, workbook, spills, caller, |x| {
            EvalScalar::Bool(matches!(x, EvalScalar::Number(_)))
        }),
        "ISTEXT" => lift_unary(args, workbook, spills, caller, |x| {
            EvalScalar::Bool(matches!(x, EvalScalar::Text(_)))
        }),
        "ISLOGICAL" => lift_unary(args, workbook, spills, caller, |x| {
            EvalScalar::Bool(matches!(x, EvalScalar::Bool(_)))
        }),
        "ISERROR" => lift_unary(args, workbook, spills, caller, |x| {
            EvalScalar::Bool(matches!(x, EvalScalar::Error(_)))
        }),

        // --- Layer 0, Layer 1 & M3a: Lookup & Dynamic Arrays ---
        "ROW" => EvalValue::Scalar(eval_row_or_col(&args, workbook, caller, true)),
        "COLUMN" => EvalValue::Scalar(eval_row_or_col(&args, workbook, caller, false)),
        "ROWS" => lookup::eval_rows_or_columns(&args, workbook, true),
        "COLUMNS" => lookup::eval_rows_or_columns(&args, workbook, false),
        "INDEX" => lookup::eval_index(args, workbook, spills, caller),
        "MATCH" => lookup::eval_match(args, workbook, spills, caller, lookup_cache),
        "VLOOKUP" => {
            lookup::eval_vlookup_or_hlookup(args, workbook, spills, caller, true, lookup_cache)
        }
        "HLOOKUP" => {
            lookup::eval_vlookup_or_hlookup(args, workbook, spills, caller, false, lookup_cache)
        }
        "XLOOKUP" => lookup::eval_xlookup(args, workbook, spills, caller, lookup_cache),
        "XMATCH" => lookup::eval_xmatch(args, workbook, spills, caller, lookup_cache),
        "LOOKUP" => lookup::eval_lookup(args, workbook, spills, caller, lookup_cache),
        "ADDRESS" => lookup::eval_address(args, workbook, spills, caller),
        "AREAS" => lookup::eval_areas(&args),
        "TRANSPOSE" => dynamic_array::eval_transpose(args, workbook, spills),
        "FILTER" => dynamic_array::eval_filter(args, workbook, spills),
        "SORT" => dynamic_array::eval_sort(args, workbook, spills, caller),
        "SORTBY" => dynamic_array::eval_sortby(args, workbook, spills, caller),
        "UNIQUE" => dynamic_array::eval_unique(args, workbook, spills, caller),
        "SEQUENCE" => dynamic_array::eval_sequence(args, workbook, spills, caller),
        "RANDARRAY" => dynamic_array::eval_randarray(args, workbook, spills, caller, env),
        "CHOOSECOLS" => dynamic_array::eval_choose_slice(args, workbook, spills, caller, false),
        "CHOOSEROWS" => dynamic_array::eval_choose_slice(args, workbook, spills, caller, true),
        "TAKE" => dynamic_array::eval_take_or_drop(args, workbook, spills, caller, false),
        "DROP" => dynamic_array::eval_take_or_drop(args, workbook, spills, caller, true),
        "HSTACK" => dynamic_array::eval_stack(args, workbook, spills, true),
        "VSTACK" => dynamic_array::eval_stack(args, workbook, spills, false),

        // --- Layer 1 & M3a: Conditional Aggregation ---
        "COUNTBLANK" => criteria::eval_countblank(&args, workbook, spills),
        "COUNTIF" => criteria::eval_countif(args, workbook, spills, caller),
        "SUMIF" => criteria::eval_sumif_or_averageif(args, workbook, spills, caller, false),
        "AVERAGEIF" => criteria::eval_sumif_or_averageif(args, workbook, spills, caller, true),
        "COUNTIFS" => criteria::eval_multi_ifs(
            args,
            workbook,
            spills,
            caller,
            criteria::MultiIfsKind::Count,
        ),
        "SUMIFS" => {
            criteria::eval_multi_ifs(args, workbook, spills, caller, criteria::MultiIfsKind::Sum)
        }
        "AVERAGEIFS" => criteria::eval_multi_ifs(
            args,
            workbook,
            spills,
            caller,
            criteria::MultiIfsKind::Average,
        ),
        "MAXIFS" => {
            criteria::eval_multi_ifs(args, workbook, spills, caller, criteria::MultiIfsKind::Max)
        }
        "MINIFS" => {
            criteria::eval_multi_ifs(args, workbook, spills, caller, criteria::MultiIfsKind::Min)
        }
        "SUMPRODUCT" => criteria::eval_sumproduct(args, workbook, spills),
        "SUBTOTAL" => criteria::eval_subtotal(args, workbook, spills, caller, is_row_hidden),
        "AGGREGATE" => criteria::eval_aggregate(args, workbook, spills, caller, is_row_hidden),

        // --- M3a: Statistical & Distribution (16 + 6 aliases) ---
        "MEDIAN" => stats::eval_median(&args, workbook, spills),
        "MODE.SNGL" | "MODE" => stats::eval_mode_sngl(&args, workbook, spills),
        "LARGE" => stats::eval_large_or_small(args, workbook, spills, caller, true),
        "SMALL" => stats::eval_large_or_small(args, workbook, spills, caller, false),
        "RANK.EQ" | "RANK" => stats::eval_rank(args, workbook, spills, caller, false),
        "RANK.AVG" => stats::eval_rank(args, workbook, spills, caller, true),
        "PERCENTILE.INC" | "PERCENTILE" => {
            stats::eval_percentile(args, workbook, spills, caller, false)
        }
        "PERCENTILE.EXC" => stats::eval_percentile(args, workbook, spills, caller, true),
        "QUARTILE.INC" | "QUARTILE" => stats::eval_quartile(args, workbook, spills, caller, false),
        "QUARTILE.EXC" => stats::eval_quartile(args, workbook, spills, caller, true),
        "STDEV.S" | "STDEV" => stats::eval_var_or_stdev(&args, workbook, spills, true, true),
        "STDEV.P" => stats::eval_var_or_stdev(&args, workbook, spills, false, true),
        "VAR.S" | "VAR" => stats::eval_var_or_stdev(&args, workbook, spills, true, false),
        "VAR.P" => stats::eval_var_or_stdev(&args, workbook, spills, false, false),
        "CORREL" => stats::eval_correl(&args, workbook, spills),
        "FREQUENCY" => stats::eval_frequency(&args, workbook, spills),

        // --- Layer 1 & M3a: Date & Time ---
        "DATE" => datetime::eval_date(args, workbook, spills, caller),
        "YEAR" => datetime::eval_date_part(args, workbook, spills, caller, 0),
        "MONTH" => datetime::eval_date_part(args, workbook, spills, caller, 1),
        "DAY" => datetime::eval_date_part(args, workbook, spills, caller, 2),
        "EDATE" => datetime::eval_edate_or_eomonth(args, workbook, spills, caller, false),
        "EOMONTH" => datetime::eval_edate_or_eomonth(args, workbook, spills, caller, true),
        "DAYS" => datetime::eval_days(args, workbook, spills, caller),
        "TIME" => datetime::eval_time(args, workbook, spills, caller),
        "HOUR" => datetime::eval_time_part(args, workbook, spills, caller, 0),
        "MINUTE" => datetime::eval_time_part(args, workbook, spills, caller, 1),
        "SECOND" => datetime::eval_time_part(args, workbook, spills, caller, 2),
        "DATEVALUE" => datetime::eval_datevalue(args, workbook, spills, caller),
        "TIMEVALUE" => datetime::eval_timevalue(args, workbook, spills, caller),
        "WEEKDAY" => datetime::eval_weekday(args, workbook, spills, caller),
        "WEEKNUM" => datetime::eval_weeknum(args, workbook, spills, caller),
        "ISOWEEKNUM" => datetime::eval_isoweeknum(args, workbook, spills, caller),
        "DATEDIF" => datetime::eval_datedif(args, workbook, spills, caller),
        "DAYS360" => datetime::eval_days360(args, workbook, spills, caller),
        "NETWORKDAYS" => datetime::eval_networkdays(args, workbook, spills, caller),
        "WORKDAY" => datetime::eval_workday(args, workbook, spills, caller),
        "YEARFRAC" => datetime::eval_yearfrac(args, workbook, spills, caller),

        // --- M3a: Financial ---
        "PMT" => financial::eval_pmt(args, workbook, spills, caller),
        "PV" => financial::eval_pv(args, workbook, spills, caller),
        "FV" => financial::eval_fv(args, workbook, spills, caller),
        "NPER" => financial::eval_nper(args, workbook, spills, caller),
        "RATE" => financial::eval_rate(args, workbook, spills, caller),
        "NPV" => financial::eval_npv(args, workbook, spills, caller),
        "IRR" => financial::eval_irr(args, workbook, spills, caller),
        "SLN" => financial::eval_sln(args, workbook, spills, caller),

        _ => EvalValue::Scalar(EvalScalar::Error(CalcError::Name)),
    }
}

/// Evaluates `IF(cond, when_true [, when_false])` once `cond` (and whichever
/// branch(es) were needed) have been evaluated.
pub fn combine_if(
    cond: ScalarOrArray,
    when_true: Option<EvalValue>,
    when_false: Option<EvalValue>,
    has_third_arg: bool,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if let ScalarOrArray::Scalar(c) = cond {
        return match c.to_bool() {
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
            Ok(true) => when_true.unwrap_or(EvalValue::Scalar(EvalScalar::Number(0.0))),
            Ok(false) => {
                if has_third_arg {
                    when_false.unwrap_or(EvalValue::Scalar(EvalScalar::Number(0.0)))
                } else {
                    EvalValue::Scalar(EvalScalar::Bool(false))
                }
            }
        };
    }

    let t_branch = match when_true {
        Some(v) => match coerce_for_scalar_op(v, workbook, spills, caller) {
            Ok(sa) => sa,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        },
        None => ScalarOrArray::Scalar(EvalScalar::Number(0.0)),
    };
    let f_branch = if has_third_arg {
        match when_false {
            Some(v) => match coerce_for_scalar_op(v, workbook, spills, caller) {
                Ok(sa) => sa,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            },
            None => ScalarOrArray::Scalar(EvalScalar::Number(0.0)),
        }
    } else {
        ScalarOrArray::Scalar(EvalScalar::Bool(false))
    };

    let ScalarOrArray::Array(cond_arr) = cond else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    let (rt, ct) = shape_of(&t_branch);
    let (rf, cf) = shape_of(&f_branch);
    let out_rows = cond_arr.rows.max(rt).max(rf);
    let out_cols = cond_arr.cols.max(ct).max(cf);
    let Some(total) = (out_rows as usize).checked_mul(out_cols as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    if total > MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }

    let mut out = Vec::with_capacity(total);
    for r in 0..out_rows {
        for c in 0..out_cols {
            let cs = cond_arr.get_broadcast(r, c);
            let cell = match cs.to_bool() {
                Err(e) => EvalScalar::Error(e),
                Ok(true) => empty_to_zero(get_sa(&t_branch, r, c)),
                Ok(false) => empty_to_zero(get_sa(&f_branch, r, c)),
            };
            out.push(cell);
        }
    }
    match Array2D::new(out_rows, out_cols, out) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates `IFERROR(val, fallback)` or `IFNA(val, fallback)` once needed
/// operands are ready.
pub fn combine_iferror(
    primary: ScalarOrArray,
    fallback: Option<EvalValue>,
    only_na: bool,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    let catches = |err: CalcError| !only_na || err == CalcError::Na;

    if let ScalarOrArray::Scalar(s) = primary {
        if let EvalScalar::Error(e) = s
            && catches(e)
        {
            return fallback.unwrap_or(EvalValue::Scalar(EvalScalar::Number(0.0)));
        }
        return EvalValue::Scalar(empty_to_zero(s));
    }

    let fb = match fallback {
        Some(v) => match coerce_for_scalar_op(v, workbook, spills, caller) {
            Ok(sa) => sa,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        },
        None => ScalarOrArray::Scalar(EvalScalar::Number(0.0)),
    };

    let ScalarOrArray::Array(arr) = primary else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    let (rf, cf) = shape_of(&fb);
    let out_rows = arr.rows.max(rf);
    let out_cols = arr.cols.max(cf);
    let Some(total) = (out_rows as usize).checked_mul(out_cols as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    if total > MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    let mut out = Vec::with_capacity(total);
    for r in 0..out_rows {
        for c in 0..out_cols {
            let s = arr.get_broadcast(r, c);
            if let EvalScalar::Error(e) = s
                && catches(e)
            {
                out.push(empty_to_zero(get_sa(&fb, r, c)));
            } else {
                out.push(empty_to_zero(s));
            }
        }
    }
    match Array2D::new(out_rows, out_cols, out) {
        Ok(res) => EvalValue::Array(res),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

pub(crate) const fn shape_of(sa: &ScalarOrArray) -> (u32, u32) {
    match sa {
        ScalarOrArray::Scalar(_) => (1, 1),
        ScalarOrArray::Array(a) => (a.rows, a.cols),
    }
}

pub(crate) fn get_sa(sa: &ScalarOrArray, r: u32, c: u32) -> EvalScalar {
    match sa {
        ScalarOrArray::Scalar(s) => s.clone(),
        ScalarOrArray::Array(a) => a.get_broadcast(r, c),
    }
}

pub(crate) fn empty_to_zero(s: EvalScalar) -> EvalScalar {
    match s {
        EvalScalar::Empty => EvalScalar::Number(0.0),
        other => other,
    }
}

// ------------------------------------------------------------ aggregations

#[derive(Clone, Copy)]
pub(crate) struct SumAcc {
    pub(crate) sum: f64,
    prev_sum: f64,
    pub(crate) count: u64,
}

impl SumAcc {
    pub(crate) const fn new() -> Self {
        Self {
            sum: 0.0,
            prev_sum: 0.0,
            count: 0,
        }
    }

    pub(crate) fn push(&mut self, val: f64) -> Result<(), CalcError> {
        self.prev_sum = self.sum;
        self.sum += val;
        if !self.sum.is_finite() {
            return Err(CalcError::Num);
        }
        self.count = self.count.saturating_add(1);
        Ok(())
    }

    pub(crate) fn finish(self) -> f64 {
        if self.count >= 2 {
            snap_cancelled_zero(self.prev_sum, self.sum)
        } else if self.sum == 0.0 {
            0.0
        } else {
            self.sum
        }
    }
}

pub(crate) enum RefCellVal<'a> {
    Word(u64),
    Shadow(&'a EvalScalar),
}

pub(crate) fn eval_sum(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalScalar {
    if args.is_empty() {
        return EvalScalar::Error(CalcError::Value);
    }
    let mut acc = SumAcc::new();
    for arg in args {
        if let Err(e) = visit_numeric_arg(arg, workbook, spills, |n| acc.push(n)) {
            return EvalScalar::Error(e);
        }
    }
    EvalScalar::number(acc.finish())
}

pub(crate) fn eval_average(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalScalar {
    if args.is_empty() {
        return EvalScalar::Error(CalcError::Value);
    }
    let mut acc = SumAcc::new();
    for arg in args {
        if let Err(e) = visit_numeric_arg(arg, workbook, spills, |n| acc.push(n)) {
            return EvalScalar::Error(e);
        }
    }
    if acc.count == 0 {
        return EvalScalar::Error(CalcError::Div0);
    }
    let total = acc.finish();
    #[expect(
        clippy::cast_precision_loss,
        reason = "cell count in a workbook fits in 53-bit f64 mantissa"
    )]
    let denom = acc.count as f64;
    EvalScalar::number(total / denom)
}

pub(crate) fn eval_min_max(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    is_min: bool,
) -> EvalScalar {
    if args.is_empty() {
        return EvalScalar::Error(CalcError::Value);
    }
    let mut best: Option<f64> = None;
    for arg in args {
        let res = visit_numeric_arg(arg, workbook, spills, |n| {
            best = Some(match best {
                None => n,
                Some(cur) => {
                    if (is_min && n < cur) || (!is_min && n > cur) {
                        n
                    } else {
                        cur
                    }
                }
            });
            Ok(())
        });
        if let Err(e) = res {
            return EvalScalar::Error(e);
        }
    }
    EvalScalar::number(best.unwrap_or(0.0))
}

fn visit_numeric_arg(
    arg: &EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    mut f: impl FnMut(f64) -> Result<(), CalcError>,
) -> Result<(), CalcError> {
    match arg {
        EvalValue::Missing => Ok(()),
        EvalValue::Scalar(s) => match s {
            EvalScalar::Empty => Ok(()),
            _ => f(s.to_number()?),
        },
        EvalValue::Array(arr) => {
            for item in &arr.values {
                match item {
                    EvalScalar::Error(e) => return Err(*e),
                    EvalScalar::Number(n) => f(*n)?,
                    EvalScalar::Empty | EvalScalar::Text(_) | EvalScalar::Bool(_) => {}
                }
            }
            Ok(())
        }
        EvalValue::Reference(r) => {
            visit_reference_cells(r, workbook, spills, |_sheet, cell| match cell {
                RefCellVal::Word(bits) => match Value::unpack(bits) {
                    Value::Error(e) => Err(e),
                    Value::Number(n) => f(n),
                    Value::Empty | Value::Text(_) | Value::Bool(_) | Value::Array(_) => Ok(()),
                },
                RefCellVal::Shadow(s) => match s {
                    EvalScalar::Error(e) => Err(*e),
                    EvalScalar::Number(n) => f(*n),
                    EvalScalar::Empty | EvalScalar::Text(_) | EvalScalar::Bool(_) => Ok(()),
                },
            })
        }
    }
}

pub(crate) fn eval_count(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalScalar {
    if args.is_empty() {
        return EvalScalar::Error(CalcError::Value);
    }
    let mut count: u64 = 0;
    for arg in args {
        match arg {
            EvalValue::Missing => {}
            EvalValue::Scalar(s) => {
                if !matches!(s, EvalScalar::Empty | EvalScalar::Error(_)) && s.to_number().is_ok() {
                    count = count.saturating_add(1);
                }
            }
            EvalValue::Array(arr) => {
                for item in &arr.values {
                    if matches!(item, EvalScalar::Number(_)) {
                        count = count.saturating_add(1);
                    }
                }
            }
            EvalValue::Reference(r) => {
                let _ = visit_reference_cells(r, workbook, spills, |_sheet, cell| {
                    let is_num = match cell {
                        RefCellVal::Word(bits) => matches!(Value::unpack(bits), Value::Number(_)),
                        RefCellVal::Shadow(s) => matches!(s, EvalScalar::Number(_)),
                    };
                    if is_num {
                        count = count.saturating_add(1);
                    }
                    Ok(())
                });
            }
        }
    }
    #[expect(clippy::cast_precision_loss, reason = "cell count fits in f64")]
    EvalScalar::Number(count as f64)
}

pub(crate) fn eval_counta(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalScalar {
    if args.is_empty() {
        return EvalScalar::Error(CalcError::Value);
    }
    let mut count: u64 = 0;
    for arg in args {
        match arg {
            EvalValue::Missing => {}
            EvalValue::Scalar(s) => {
                if !matches!(s, EvalScalar::Empty) {
                    count = count.saturating_add(1);
                }
            }
            EvalValue::Array(arr) => {
                for item in &arr.values {
                    if !matches!(item, EvalScalar::Empty) {
                        count = count.saturating_add(1);
                    }
                }
            }
            EvalValue::Reference(r) => {
                if matches!(r, ResolvedRef::Invalid(_)) {
                    count = count.saturating_add(1);
                } else {
                    let _ = visit_reference_cells(r, workbook, spills, |_sheet, cell| {
                        let non_empty = match cell {
                            RefCellVal::Word(bits) => bits != Value::EMPTY_BITS,
                            RefCellVal::Shadow(s) => !matches!(s, EvalScalar::Empty),
                        };
                        if non_empty {
                            count = count.saturating_add(1);
                        }
                        Ok(())
                    });
                }
            }
        }
    }
    #[expect(clippy::cast_precision_loss, reason = "cell count fits in f64")]
    EvalScalar::Number(count as f64)
}

pub(crate) fn visit_reference_cells(
    r: &ResolvedRef,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    mut visit: impl FnMut(&Sheet, RefCellVal<'_>) -> Result<(), CalcError>,
) -> Result<(), CalcError> {
    match r {
        ResolvedRef::Invalid(e) => Err(*e),
        ResolvedRef::Cell { sheet, row, col } => {
            let target = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let bits = target.value_by_id(*row, *col).pack();
            if bits != Value::EMPTY_BITS {
                visit(target, RefCellVal::Word(bits))
            } else if let Some(shadow) = spills.shadow_value(*sheet, *row, *col) {
                visit(target, RefCellVal::Shadow(shadow))
            } else {
                visit(target, RefCellVal::Word(Value::EMPTY_BITS))
            }
        }
        ResolvedRef::Area {
            sheet,
            first_row,
            last_row,
            first_col,
            last_col,
        } => {
            let target = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            stream_spans(
                target,
                spills,
                first_row.0,
                last_row.0,
                first_col.0,
                last_col.0,
                &mut visit,
            )
        }
        ResolvedRef::FullCols {
            sheet,
            first_col,
            last_col,
        } => {
            let target = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let (Some(r1), Some(r2)) = (
                target.row_axis().pos_to_id(0),
                target
                    .row_axis()
                    .pos_to_id(target.row_axis().live_len().saturating_sub(1)),
            ) else {
                return Err(CalcError::Ref);
            };
            stream_spans(target, spills, r1, r2, first_col.0, last_col.0, &mut visit)
        }
        ResolvedRef::FullRows {
            sheet,
            first_row,
            last_row,
        } => {
            let target = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let (Some(c1), Some(c2)) = (
                target.col_axis().pos_to_id(0),
                target
                    .col_axis()
                    .pos_to_id(target.col_axis().live_len().saturating_sub(1)),
            ) else {
                return Err(CalcError::Ref);
            };
            stream_spans(target, spills, first_row.0, last_row.0, c1, c2, &mut visit)
        }
    }
}

fn stream_spans(
    sheet: &Sheet,
    spills: &SpillRegistry,
    first_row: u64,
    last_row: u64,
    first_col: u64,
    last_col: u64,
    visit: &mut impl FnMut(&Sheet, RefCellVal<'_>) -> Result<(), CalcError>,
) -> Result<(), CalcError> {
    let mut col_ids = Vec::new();
    let ok_c = sheet
        .col_axis()
        .for_each_live_in_range(first_col, last_col, |run| {
            col_ids.extend(run.iter());
        });
    if !ok_c || col_ids.is_empty() {
        return Err(CalcError::Ref);
    }

    let mut row_runs: Vec<IdRun> = Vec::new();
    let ok_r = sheet
        .row_axis()
        .for_each_live_in_range(first_row, last_row, |run| {
            row_runs.push(run);
        });
    if !ok_r || row_runs.is_empty() {
        return Err(CalcError::Ref);
    }

    let sheet_id = sheet.id();
    let any_shadows = spills.has_spills_on_sheet(sheet_id)
        && col_ids
            .iter()
            .any(|&c| spills.has_shadows_in_col(sheet_id, ColId(c)));

    if !any_shadows
        && col_ids.len() == 1
        && let Some(&col_id) = col_ids.first()
    {
        let Some(col_slot) = sheet.col_slots().get(col_id) else {
            for run in &row_runs {
                for _ in 0..run.len {
                    visit(sheet, RefCellVal::Word(Value::EMPTY_BITS))?;
                }
            }
            return Ok(());
        };
        let Some(column) = sheet.raw_columns().get(col_slot as usize) else {
            for run in &row_runs {
                for _ in 0..run.len {
                    visit(sheet, RefCellVal::Word(Value::EMPTY_BITS))?;
                }
            }
            return Ok(());
        };
        let mut word_visit = |bits: u64| visit(sheet, RefCellVal::Word(bits));
        let base_bound = u64::from(sheet.row_slots().base_len());
        for run in &row_runs {
            let end_excl = run.first_id.saturating_add(u64::from(run.len));
            if end_excl <= base_bound
                && let Ok(start_slot) = u32::try_from(run.first_id)
            {
                stream_contiguous_slots(column, start_slot, run.len, &mut word_visit)?;
            } else {
                for row_id in run.iter() {
                    let bits = sheet
                        .row_slots()
                        .get(row_id)
                        .map_or(Value::EMPTY_BITS, |slot| column.get_packed(slot));
                    word_visit(bits)?;
                }
            }
        }
        return Ok(());
    }

    for run in &row_runs {
        for row_id in run.iter() {
            let row_slot = sheet.row_slots().get(row_id);
            for &col_id in &col_ids {
                let bits = match (row_slot, sheet.col_slots().get(col_id)) {
                    (Some(rs), Some(cs)) => sheet
                        .raw_columns()
                        .get(cs as usize)
                        .map_or(Value::EMPTY_BITS, |col| col.get_packed(rs)),
                    _ => Value::EMPTY_BITS,
                };
                if bits == Value::EMPTY_BITS
                    && any_shadows
                    && let Some(shadow) =
                        spills.shadow_value(sheet_id, RowId(row_id), ColId(col_id))
                {
                    visit(sheet, RefCellVal::Shadow(shadow))?;
                } else {
                    visit(sheet, RefCellVal::Word(bits))?;
                }
            }
        }
    }
    Ok(())
}

fn stream_contiguous_slots(
    column: &reticula_core::store::Column,
    start_slot: u32,
    len: u32,
    visit: &mut impl FnMut(u64) -> Result<(), CalcError>,
) -> Result<(), CalcError> {
    let mut cur = start_slot;
    let end = start_slot.saturating_add(len);
    let blocks = column.blocks();
    while cur < end {
        let block_idx = (cur / BLOCK_ROWS) as usize;
        let offset_in_block = (cur % BLOCK_ROWS) as usize;
        let rem_in_block = (BLOCK_ROWS as usize).saturating_sub(offset_in_block);
        let take = rem_in_block.min((end.saturating_sub(cur)) as usize);
        match blocks.get(block_idx) {
            None | Some(Block::Empty) => {
                for _ in 0..take {
                    visit(Value::EMPTY_BITS)?;
                }
            }
            Some(Block::Dense(words)) => {
                let slice_end = offset_in_block.saturating_add(take);
                if let Some(slice) = words.get(offset_in_block..slice_end) {
                    for &bits in slice {
                        visit(bits)?;
                    }
                }
            }
            Some(Block::Sparse(_)) => {
                let take_u32 = u32::try_from(take).unwrap_or(0);
                for s in cur..cur.saturating_add(take_u32) {
                    visit(column.get_packed(s))?;
                }
            }
        }
        cur = cur.saturating_add(u32::try_from(take).unwrap_or(1));
    }
    Ok(())
}

// ------------------------------------------------------------ logic & math

fn eval_and_or(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    is_and: bool,
) -> EvalScalar {
    if args.is_empty() {
        return EvalScalar::Error(CalcError::Value);
    }
    let mut acc = is_and;
    let mut seen_any = false;

    for arg in args {
        match arg {
            EvalValue::Missing | EvalValue::Scalar(EvalScalar::Empty) => {}
            EvalValue::Scalar(s) => match s.to_bool() {
                Ok(b) => {
                    seen_any = true;
                    acc = if is_and { acc && b } else { acc || b };
                }
                Err(e) => return EvalScalar::Error(e),
            },
            EvalValue::Array(arr) => {
                for item in &arr.values {
                    match item {
                        EvalScalar::Error(e) => return EvalScalar::Error(*e),
                        EvalScalar::Bool(b) => {
                            seen_any = true;
                            acc = if is_and { acc && *b } else { acc || *b };
                        }
                        EvalScalar::Number(n) => {
                            seen_any = true;
                            let b = *n != 0.0;
                            acc = if is_and { acc && b } else { acc || b };
                        }
                        EvalScalar::Empty | EvalScalar::Text(_) => {}
                    }
                }
            }
            EvalValue::Reference(r) => {
                let res = visit_reference_cells(r, workbook, spills, |_sheet, cell| match cell {
                    RefCellVal::Word(bits) => match Value::unpack(bits) {
                        Value::Error(e) => Err(e),
                        Value::Bool(b) => {
                            seen_any = true;
                            acc = if is_and { acc && b } else { acc || b };
                            Ok(())
                        }
                        Value::Number(n) => {
                            seen_any = true;
                            let b = n != 0.0;
                            acc = if is_and { acc && b } else { acc || b };
                            Ok(())
                        }
                        Value::Empty | Value::Text(_) | Value::Array(_) => Ok(()),
                    },
                    RefCellVal::Shadow(s) => match s {
                        EvalScalar::Error(e) => Err(*e),
                        EvalScalar::Bool(b) => {
                            seen_any = true;
                            acc = if is_and { acc && *b } else { acc || *b };
                            Ok(())
                        }
                        EvalScalar::Number(n) => {
                            seen_any = true;
                            let b = *n != 0.0;
                            acc = if is_and { acc && b } else { acc || b };
                            Ok(())
                        }
                        EvalScalar::Empty | EvalScalar::Text(_) => Ok(()),
                    },
                });
                if let Err(e) = res {
                    return EvalScalar::Error(e);
                }
            }
        }
    }

    if !seen_any {
        EvalScalar::Error(CalcError::Value)
    } else {
        EvalScalar::Bool(acc)
    }
}

pub(crate) fn eval_round_scalar(x: &EvalScalar, d: &EvalScalar, mode: RoundingMode) -> EvalScalar {
    let val = match x.to_number() {
        Ok(v) => v,
        Err(e) => return EvalScalar::Error(e),
    };
    let places = match d.to_number().and_then(trunc_to_i32) {
        Ok(p) => p,
        Err(e) => return EvalScalar::Error(e),
    };
    round_decimal(val, places, mode).map_or(EvalScalar::Error(CalcError::Num), EvalScalar::number)
}

fn eval_trunc(
    mut args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if args.len() == 1 {
        args.push(EvalValue::Scalar(EvalScalar::Number(0.0)));
    }
    lift_binary(args, workbook, spills, caller, |x, d| {
        eval_round_scalar(x, d, RoundingMode::TowardZero)
    })
}

pub(crate) fn trunc_to_i32(n: f64) -> Result<i32, CalcError> {
    if !n.is_finite() {
        return Err(CalcError::Num);
    }
    let t = num::trunc(n).clamp(-1_000_000.0, 1_000_000.0);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "clamped to `[-1_000_000, 1_000_000]`"
    )]
    Ok(t as i32)
}

pub(crate) fn trunc_to_isize(n: f64) -> Result<isize, CalcError> {
    if !n.is_finite() {
        return Err(CalcError::Num);
    }
    let t = num::trunc(n);
    if !(-1_000_000_000.0..=1_000_000_000.0).contains(&t) {
        return Err(CalcError::Value);
    }
    #[expect(clippy::cast_possible_truncation, reason = "bounded to `[-1e9, 1e9]`")]
    Ok(t as isize)
}

// ------------------------------------------------------------ text & structural

pub(crate) fn checked_text(s: String) -> EvalScalar {
    if s.encode_utf16().count() > MAX_CELL_TEXT_UTF16 {
        EvalScalar::Error(CalcError::Value)
    } else {
        EvalScalar::Text(s)
    }
}

fn excel_trim(input: &str) -> String {
    let trimmed = input.trim_matches(' ');
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_space = false;
    for ch in trimmed.chars() {
        if ch == ' ' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn eval_left_or_right(
    mut args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    is_left: bool,
) -> EvalValue {
    if args.len() == 1 {
        args.push(EvalValue::Scalar(EvalScalar::Number(1.0)));
    }
    lift_binary(args, workbook, spills, caller, |text_arg, count_arg| {
        let s = match text_arg.to_text() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let n = match count_arg.to_number().and_then(trunc_to_isize) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        if n < 0 {
            return EvalScalar::Error(CalcError::Value);
        }
        #[expect(clippy::cast_sign_loss, reason = "`n >= 0`, checked above")]
        let take = n as usize;
        if is_left {
            EvalScalar::Text(s.chars().take(take).collect())
        } else {
            let total = s.chars().count();
            let skip = total.saturating_sub(take);
            EvalScalar::Text(s.chars().skip(skip).collect())
        }
    })
}

fn eval_mid(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_ternary(
        args,
        workbook,
        spills,
        caller,
        |text_arg, start_arg, len_arg| {
            let s = match text_arg.to_text() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let start = match start_arg.to_number().and_then(trunc_to_isize) {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let count = match len_arg.to_number().and_then(trunc_to_isize) {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            if start < 1 || count < 0 {
                return EvalScalar::Error(CalcError::Value);
            }
            #[expect(clippy::cast_sign_loss, reason = "`start >= 1` and `count >= 0`")]
            let (skip, take) = ((start as usize).saturating_sub(1), count as usize);
            EvalScalar::Text(s.chars().skip(skip).take(take).collect())
        },
    )
}

fn eval_concat(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalScalar {
    if args.is_empty() {
        return EvalScalar::Error(CalcError::Value);
    }
    let mut out = String::new();
    let mut utf16_len = 0usize;
    let mut append = |piece: &str| -> Result<(), CalcError> {
        let added = piece.encode_utf16().count();
        utf16_len = utf16_len.saturating_add(added);
        if utf16_len > MAX_CELL_TEXT_UTF16 {
            return Err(CalcError::Value);
        }
        out.push_str(piece);
        Ok(())
    };

    for arg in args {
        match arg {
            EvalValue::Missing => {}
            EvalValue::Scalar(s) => match s.to_text() {
                Ok(t) => {
                    if let Err(e) = append(&t) {
                        return EvalScalar::Error(e);
                    }
                }
                Err(e) => return EvalScalar::Error(e),
            },
            EvalValue::Array(arr) => {
                for item in &arr.values {
                    match item.to_text() {
                        Ok(t) => {
                            if let Err(e) = append(&t) {
                                return EvalScalar::Error(e);
                            }
                        }
                        Err(e) => return EvalScalar::Error(e),
                    }
                }
            }
            EvalValue::Reference(r) => {
                let res = visit_reference_cells(r, workbook, spills, |sheet, cell| match cell {
                    RefCellVal::Word(bits) => {
                        let s = EvalScalar::from_sheet_value(sheet, Value::unpack(bits));
                        let t = s.to_text()?;
                        append(&t)
                    }
                    RefCellVal::Shadow(s) => {
                        let t = s.to_text()?;
                        append(&t)
                    }
                });
                if let Err(e) = res {
                    return EvalScalar::Error(e);
                }
            }
        }
    }
    EvalScalar::Text(out)
}

fn eval_row_or_col(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    caller: CallerContext,
    is_row: bool,
) -> EvalScalar {
    if args.is_empty() {
        let Some(host) = workbook.sheet(caller.sheet) else {
            return EvalScalar::Error(CalcError::Ref);
        };
        let pos = if is_row {
            host.row_axis().id_to_pos(caller.row.0)
        } else {
            host.col_axis().id_to_pos(caller.col.0)
        };
        return pos.map_or(EvalScalar::Error(CalcError::Ref), |p| {
            EvalScalar::Number(f64::from(p) + 1.0)
        });
    }
    if args.len() > 1 {
        return EvalScalar::Error(CalcError::Value);
    }
    let Some(EvalValue::Reference(r)) = args.first() else {
        return EvalScalar::Error(CalcError::Value);
    };
    first_pos_of_ref(r, workbook, is_row).map_or_else(EvalScalar::Error, |p| {
        EvalScalar::Number(f64::from(p) + 1.0)
    })
}

fn first_pos_of_ref(
    r: &ResolvedRef,
    workbook: &Workbook<BoundFormula>,
    is_row: bool,
) -> Result<u32, CalcError> {
    match r {
        ResolvedRef::Invalid(e) => Err(*e),
        ResolvedRef::Cell { sheet, row, col } => {
            let s = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            if is_row {
                s.row_axis().id_to_pos(row.0).ok_or(CalcError::Ref)
            } else {
                s.col_axis().id_to_pos(col.0).ok_or(CalcError::Ref)
            }
        }
        ResolvedRef::Area {
            sheet,
            first_row,
            last_row,
            first_col,
            last_col,
        } => {
            let s = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            if is_row {
                live_span_positions(s.row_axis(), first_row.0, last_row.0)
                    .map(|(p, _)| p)
                    .ok_or(CalcError::Ref)
            } else {
                live_span_positions(s.col_axis(), first_col.0, last_col.0)
                    .map(|(p, _)| p)
                    .ok_or(CalcError::Ref)
            }
        }
        ResolvedRef::FullCols {
            sheet,
            first_col,
            last_col,
        } => {
            let s = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            if is_row {
                if s.row_axis().live_len() == 0 {
                    Err(CalcError::Ref)
                } else {
                    Ok(0)
                }
            } else {
                live_span_positions(s.col_axis(), first_col.0, last_col.0)
                    .map(|(p, _)| p)
                    .ok_or(CalcError::Ref)
            }
        }
        ResolvedRef::FullRows {
            sheet,
            first_row,
            last_row,
        } => {
            let s = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            if is_row {
                live_span_positions(s.row_axis(), first_row.0, last_row.0)
                    .map(|(p, _)| p)
                    .ok_or(CalcError::Ref)
            } else if s.col_axis().live_len() == 0 {
                Err(CalcError::Ref)
            } else {
                Ok(0)
            }
        }
    }
}

// ------------------------------------------------------------ array lifting helpers

pub(crate) fn lift_unary(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    f: impl Fn(&EvalScalar) -> EvalScalar,
) -> EvalValue {
    if args.len() != 1 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let Some(arg0) = args.into_iter().next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let lifted = match coerce_for_scalar_op(arg0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    match lifted {
        ScalarOrArray::Scalar(s) => EvalValue::Scalar(f(&s)),
        ScalarOrArray::Array(arr) => {
            let values = arr.values.iter().map(&f).collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

pub(crate) fn lift_binary(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    f: impl Fn(&EvalScalar, &EvalScalar) -> EvalScalar,
) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(a0), Some(a1)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let left = match coerce_for_scalar_op(a0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let right = match coerce_for_scalar_op(a1, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if let (ScalarOrArray::Scalar(a), ScalarOrArray::Scalar(b)) = (&left, &right) {
        return EvalValue::Scalar(f(a, b));
    }
    let (r1, c1) = shape_of(&left);
    let (r2, c2) = shape_of(&right);
    let out_rows = r1.max(r2);
    let out_cols = c1.max(c2);
    let Some(total) = (out_rows as usize).checked_mul(out_cols as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    if total > MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    let mut values = Vec::with_capacity(total);
    for r in 0..out_rows {
        for c in 0..out_cols {
            values.push(f(&get_sa(&left, r, c), &get_sa(&right, r, c)));
        }
    }
    match Array2D::new(out_rows, out_cols, values) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

pub(crate) fn lift_ternary(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    f: impl Fn(&EvalScalar, &EvalScalar, &EvalScalar) -> EvalScalar,
) -> EvalValue {
    if args.len() != 3 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(a0), Some(a1), Some(a2)) = (it.next(), it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let s0 = match coerce_for_scalar_op(a0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let s1 = match coerce_for_scalar_op(a1, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let s2 = match coerce_for_scalar_op(a2, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if let (ScalarOrArray::Scalar(x), ScalarOrArray::Scalar(y), ScalarOrArray::Scalar(z)) =
        (&s0, &s1, &s2)
    {
        return EvalValue::Scalar(f(x, y, z));
    }
    let (r0, c0) = shape_of(&s0);
    let (r1, c1) = shape_of(&s1);
    let (r2, c2) = shape_of(&s2);
    let out_rows = r0.max(r1).max(r2);
    let out_cols = c0.max(c1).max(c2);
    let Some(total) = (out_rows as usize).checked_mul(out_cols as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    if total > MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    let mut values = Vec::with_capacity(total);
    for r in 0..out_rows {
        for c in 0..out_cols {
            values.push(f(
                &get_sa(&s0, r, c),
                &get_sa(&s1, r, c),
                &get_sa(&s2, r, c),
            ));
        }
    }
    match Array2D::new(out_rows, out_cols, values) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}
