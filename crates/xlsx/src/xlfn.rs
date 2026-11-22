//! OOXML `_xlfn.` / `_xlfn._xlws.` prefix stripping and restoration, plus AST
//! built-in function verification (`17-m3a.md` §7).

use reticula_formula::ast::Expr;
use reticula_formula::functions::catalog::lookup_function_meta;

const XLWS_PREFIX: &str = "_xlfn._xlws.";
const XLFN_PREFIX: &str = "_xlfn.";
const SINGLE_PREFIX: &str = "_xlfn.SINGLE(";
const ANCHORARRAY_PREFIX: &str = "_xlfn.ANCHORARRAY(";

/// Functions requiring the `_xlfn._xlws.` worksheet-function prefix in OOXML.
const XLWS_FUNCTIONS: &[&str] = &["FILTER", "SORT"];

/// Modern Excel 2010–365 functions requiring the `_xlfn.` prefix in OOXML `<f>` text.
const XLFN_FUNCTIONS: &[&str] = &[
    "ACOT",
    "ACOTH",
    "AGGREGATE",
    "ARABIC",
    "ARRAYTOTEXT",
    "BASE",
    "BETA.DIST",
    "BETA.INV",
    "BINOM.DIST",
    "BINOM.DIST.RANGE",
    "BINOM.INV",
    "BITAND",
    "BITLSHIFT",
    "BITOR",
    "BITRSHIFT",
    "BITXOR",
    "BYCOL",
    "BYROW",
    "CEILING.MATH",
    "CEILING.PRECISE",
    "CHISQ.DIST",
    "CHISQ.DIST.RT",
    "CHISQ.INV",
    "CHISQ.INV.RT",
    "CHISQ.TEST",
    "CHOOSECOLS",
    "CHOOSEROWS",
    "COMBINA",
    "CONCAT",
    "CONFIDENCE.NORM",
    "CONFIDENCE.T",
    "COT",
    "COTH",
    "COVARIANCE.P",
    "COVARIANCE.S",
    "CSC",
    "CSCH",
    "DAYS",
    "DECIMAL",
    "DROP",
    "ENCODEURL",
    "ERF.PRECISE",
    "ERFC.PRECISE",
    "EXPAND",
    "EXPON.DIST",
    "F.DIST",
    "F.DIST.RT",
    "F.INV",
    "F.INV.RT",
    "F.TEST",
    "FLOOR.MATH",
    "FLOOR.PRECISE",
    "FORECAST.LINEAR",
    "FORMULATEXT",
    "GAMMA",
    "GAMMA.DIST",
    "GAMMA.INV",
    "GAMMALN.PRECISE",
    "GAUSS",
    "HSTACK",
    "HYPGEOM.DIST",
    "IFNA",
    "IFS",
    "ISFORMULA",
    "ISO.CEILING",
    "ISOMITTED",
    "ISOWEEKNUM",
    "LAMBDA",
    "LET",
    "LOGNORM.DIST",
    "LOGNORM.INV",
    "MAKEARRAY",
    "MAP",
    "MAXIFS",
    "MINIFS",
    "MODE.MULT",
    "MODE.SNGL",
    "MUNIT",
    "NEGBINOM.DIST",
    "NETWORKDAYS.INTL",
    "NORM.DIST",
    "NORM.INV",
    "NORM.S.DIST",
    "NORM.S.INV",
    "NUMBERVALUE",
    "PDURATION",
    "PERCENTILE.EXC",
    "PERCENTILE.INC",
    "PERCENTRANK.EXC",
    "PERCENTRANK.INC",
    "PERMUTATIONA",
    "PHI",
    "POISSON.DIST",
    "QUARTILE.EXC",
    "QUARTILE.INC",
    "RANDARRAY",
    "RANK.AVG",
    "RANK.EQ",
    "REDUCE",
    "RRI",
    "SCAN",
    "SEC",
    "SECH",
    "SEQUENCE",
    "SHEET",
    "SHEETS",
    "SKEW.P",
    "SORTBY",
    "STDEV.P",
    "STDEV.S",
    "SWITCH",
    "T.DIST",
    "T.DIST.2T",
    "T.DIST.RT",
    "T.INV",
    "T.INV.2T",
    "T.TEST",
    "TAKE",
    "TEXTAFTER",
    "TEXTBEFORE",
    "TEXTJOIN",
    "TEXTSPLIT",
    "TOCOL",
    "TOROW",
    "UNICHAR",
    "UNICODE",
    "UNIQUE",
    "VALUETOTEXT",
    "VAR.P",
    "VAR.S",
    "VSTACK",
    "WEIBULL.DIST",
    "WORKDAY.INTL",
    "WRAPCOLS",
    "WRAPROWS",
    "XLOOKUP",
    "XMATCH",
    "XOR",
    "Z.TEST",
];

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.'
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn starts_with_ignore_ascii_case(haystack: &[u8], prefix: &[u8]) -> bool {
    haystack
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn copy_quoted(bytes: &[u8], pos: usize, quote: u8, out: &mut String) -> usize {
    out.push(char::from(quote));
    let mut i = pos.saturating_add(1);
    while i < bytes.len() {
        let Some(&b) = bytes.get(i) else {
            break;
        };
        if b == quote {
            out.push(char::from(quote));
            i = i.saturating_add(1);
            if bytes.get(i) == Some(&quote) {
                out.push(char::from(quote));
                i = i.saturating_add(1);
                continue;
            }
            return i;
        }
        if let Some(ch) = slice_next_char(bytes, i) {
            out.push(ch);
            i = i.saturating_add(ch.len_utf8());
        } else {
            i = i.saturating_add(1);
        }
    }
    i
}

fn slice_next_char(bytes: &[u8], pos: usize) -> Option<char> {
    let tail = bytes.get(pos..)?;
    std::str::from_utf8(tail)
        .ok()
        .and_then(|s| s.chars().next())
        .or_else(|| {
            // Only examine valid UTF-8 prefix around `pos`.
            let max = tail.len().min(4);
            for len in 1..=max {
                if let Ok(s) = std::str::from_utf8(tail.get(..len)?) {
                    return s.chars().next();
                }
            }
            None
        })
}

fn find_matching_paren(bytes: &[u8], open_paren_end: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut i = open_paren_end;
    while i < bytes.len() {
        let b = *bytes.get(i)?;
        if b == b'"' || b == b'\'' {
            let quote = b;
            i = i.saturating_add(1);
            while i < bytes.len() {
                let qb = *bytes.get(i)?;
                i = i.saturating_add(1);
                if qb == quote {
                    if bytes.get(i) == Some(&quote) {
                        i = i.saturating_add(1);
                        continue;
                    }
                    break;
                }
            }
            continue;
        }
        if b == b'(' {
            depth = depth.saturating_add(1);
        } else if b == b')' {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(i);
            }
        }
        i = i.saturating_add(1);
    }
    None
}

fn preceding_spill_ref_start(out: &[u8]) -> Option<usize> {
    let last = *out.last()?;
    if !(last.is_ascii_alphanumeric() || last == b'$' || last == b'_') {
        return None;
    }
    let mut pos = out.len();
    while pos > 0
        && out
            .get(pos.saturating_sub(1))
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(*b, b'$' | b'_' | b'.'))
    {
        pos = pos.saturating_sub(1);
    }
    if pos == out.len() {
        return None;
    }
    if pos > 0 && out.get(pos.saturating_sub(1)) == Some(&b'!') {
        let mut s_pos = pos.saturating_sub(1);
        if s_pos > 0 && out.get(s_pos.saturating_sub(1)) == Some(&b'\'') {
            s_pos = s_pos.saturating_sub(1);
            while s_pos > 0 {
                s_pos = s_pos.saturating_sub(1);
                if out.get(s_pos) == Some(&b'\'') {
                    if s_pos > 0 && out.get(s_pos.saturating_sub(1)) == Some(&b'\'') {
                        s_pos = s_pos.saturating_sub(1);
                        continue;
                    }
                    return Some(s_pos);
                }
            }
        } else {
            while s_pos > 0
                && out
                    .get(s_pos.saturating_sub(1))
                    .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(*b, b'_' | b'.'))
            {
                s_pos = s_pos.saturating_sub(1);
            }
            return Some(s_pos);
        }
    }
    Some(pos)
}

/// Strips OOXML `_xlfn._xlws.` and `_xlfn.` prefixes outside quotes and sheet
/// identifiers, translating `_xlfn.SINGLE(` to Reticula `@(` and
/// `_xlfn.ANCHORARRAY(Ref)` to `Ref#`.
#[must_use]
pub fn strip_xlfn_prefixes(formula: &str) -> String {
    let bytes = formula.as_bytes();
    let mut out = String::with_capacity(formula.len());
    let mut i = 0;

    while i < bytes.len() {
        let Some(&b) = bytes.get(i) else {
            break;
        };
        if b == b'"' || b == b'\'' {
            i = copy_quoted(bytes, i, b, &mut out);
            continue;
        }
        let prev_is_ident = i
            .checked_sub(1)
            .and_then(|idx| bytes.get(idx))
            .is_some_and(|prev| is_ident_continue(*prev));
        if !prev_is_ident {
            let tail = bytes.get(i..).unwrap_or_default();
            if starts_with_ignore_ascii_case(tail, ANCHORARRAY_PREFIX.as_bytes())
                && let Some(close_idx) =
                    find_matching_paren(bytes, i.saturating_add(ANCHORARRAY_PREFIX.len()))
                && let Some(inner) =
                    formula.get(i.saturating_add(ANCHORARRAY_PREFIX.len())..close_idx)
            {
                out.push_str(&strip_xlfn_prefixes(inner.trim()));
                out.push('#');
                i = close_idx.saturating_add(1);
                continue;
            }
            if starts_with_ignore_ascii_case(tail, SINGLE_PREFIX.as_bytes()) {
                out.push_str("@(");
                i = i.saturating_add(SINGLE_PREFIX.len());
                continue;
            }
            if starts_with_ignore_ascii_case(tail, XLWS_PREFIX.as_bytes())
                && tail
                    .get(XLWS_PREFIX.len())
                    .is_some_and(|c| is_ident_start(*c))
            {
                i = i.saturating_add(XLWS_PREFIX.len());
                continue;
            }
            if starts_with_ignore_ascii_case(tail, XLFN_PREFIX.as_bytes())
                && tail
                    .get(XLFN_PREFIX.len())
                    .is_some_and(|c| is_ident_start(*c))
            {
                i = i.saturating_add(XLFN_PREFIX.len());
                continue;
            }
        }
        if let Some(ch) = slice_next_char(bytes, i) {
            out.push(ch);
            i = i.saturating_add(ch.len_utf8());
        } else {
            i = i.saturating_add(1);
        }
    }

    out
}

/// Restores OOXML `_xlfn.` / `_xlfn._xlws.` prefixes on function calls that
/// require them (plus `_xlfn.ANCHORARRAY(Ref)` for `Ref#`) without
/// double-prefixing or mutating literals.
#[must_use]
pub fn add_xlfn_prefixes(formula: &str) -> String {
    let bytes = formula.as_bytes();
    let mut out = String::with_capacity(formula.len().saturating_add(16));
    let mut i = 0;

    while i < bytes.len() {
        let Some(&b) = bytes.get(i) else {
            break;
        };
        if b == b'"' || b == b'\'' {
            i = copy_quoted(bytes, i, b, &mut out);
            continue;
        }
        if b == b'#'
            && let Some(ref_start) = preceding_spill_ref_start(out.as_bytes())
            && let Some(anchor_ref) = out.get(ref_start..).map(str::to_owned)
        {
            out.truncate(ref_start);
            out.push_str(ANCHORARRAY_PREFIX);
            out.push_str(&anchor_ref);
            out.push(')');
            i = i.saturating_add(1);
            continue;
        }
        let prev_is_ident = i
            .checked_sub(1)
            .and_then(|idx| bytes.get(idx))
            .is_some_and(|prev| is_ident_continue(*prev));
        if !prev_is_ident && is_ident_start(b) {
            let mut end = i.saturating_add(1);
            while end < bytes.len() && bytes.get(end).is_some_and(|c| is_ident_continue(*c)) {
                end = end.saturating_add(1);
            }
            let ident = formula.get(i..end).unwrap_or_default();
            let mut after = end;
            while after < bytes.len()
                && bytes.get(after).is_some_and(|c| matches!(*c, b' ' | b'\t'))
            {
                after = after.saturating_add(1);
            }
            if bytes.get(after) == Some(&b'(') {
                let upper = ident.to_ascii_uppercase();
                if !upper.starts_with("_XLFN.") {
                    if XLWS_FUNCTIONS.contains(&upper.as_str()) {
                        out.push_str(XLWS_PREFIX);
                    } else if XLFN_FUNCTIONS.contains(&upper.as_str()) {
                        out.push_str(XLFN_PREFIX);
                    }
                }
            }
            out.push_str(ident);
            i = end;
            continue;
        }
        if let Some(ch) = slice_next_char(bytes, i) {
            out.push(ch);
            i = i.saturating_add(ch.len_utf8());
        } else {
            i = i.saturating_add(1);
        }
    }

    out
}

/// Returns `true` if `expr` invokes any function name missing from the 193-function built-in catalog.
#[must_use]
pub fn contains_unknown_function(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(_)
        | Expr::ArrayLiteral { .. }
        | Expr::CellRef { .. }
        | Expr::AreaRef { .. }
        | Expr::ColRange { .. }
        | Expr::RowRange { .. }
        | Expr::Name(_)
        | Expr::QualifiedName { .. }
        | Expr::MissingArg => false,
        Expr::SpillRef(inner) | Expr::Paren(inner) | Expr::Unary { expr: inner, .. } => {
            contains_unknown_function(inner)
        }
        Expr::Binary { left, right, .. } => {
            contains_unknown_function(left) || contains_unknown_function(right)
        }
        Expr::Call { name, args } => {
            lookup_function_meta(name).is_none() || args.iter().any(contains_unknown_function)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{add_xlfn_prefixes, contains_unknown_function, strip_xlfn_prefixes};
    use reticula_formula::parser::parse_formula;

    #[test]
    fn xlfn_strip_and_restore_preserves_quotes_and_never_doubles() {
        let src = r#"_xlfn._xlws.FILTER(A1:A5,_xlfn.XLOOKUP(B1,C1:C5,D1:D5)="_xlfn.XLOOKUP")"#;
        let stripped = strip_xlfn_prefixes(src);
        assert_eq!(
            stripped,
            r#"FILTER(A1:A5,XLOOKUP(B1,C1:C5,D1:D5)="_xlfn.XLOOKUP")"#
        );
        let restored = add_xlfn_prefixes(&stripped);
        assert_eq!(restored, src);
        assert_eq!(add_xlfn_prefixes(&restored), src);
        assert_eq!(
            add_xlfn_prefixes("SORTBY(A1:A5,B1:B5,1)"),
            "_xlfn.SORTBY(A1:A5,B1:B5,1)"
        );
    }

    #[test]
    fn strip_handles_single_prefix_and_sheet_quotes() {
        assert_eq!(strip_xlfn_prefixes("_xlfn.SINGLE(A1:A5)"), "@(A1:A5)");
        assert_eq!(
            strip_xlfn_prefixes("'_xlfn.Sheet'!A1+_xlfn.SUMSQ_UNKNOWN(1)"),
            "'_xlfn.Sheet'!A1+SUMSQ_UNKNOWN(1)"
        );
    }

    #[test]
    fn anchorarray_and_spill_hash_round_trip() {
        assert_eq!(
            strip_xlfn_prefixes("SUM(_xlfn.ANCHORARRAY(A1),_xlfn.ANCHORARRAY('Sheet 2'!$W$5))"),
            "SUM(A1#,'Sheet 2'!$W$5#)"
        );
        let restored = add_xlfn_prefixes("SUM(A1#,'Sheet 2'!$W$5#)");
        assert_eq!(
            restored,
            "SUM(_xlfn.ANCHORARRAY(A1),_xlfn.ANCHORARRAY('Sheet 2'!$W$5))"
        );
        assert_eq!(add_xlfn_prefixes(&restored), restored);
        assert_eq!(add_xlfn_prefixes("IFERROR(#N/A,0)"), "IFERROR(#N/A,0)");
    }

    #[test]
    fn unknown_function_detection_matches_catalog() {
        let known = parse_formula("=XLOOKUP(A1,B1:B5,C1:C5)").expect("parses");
        assert!(!contains_unknown_function(&known));
        let unknown = parse_formula("=SUM(1,MY_CUSTOM_UDF(A1))").expect("parses");
        assert!(contains_unknown_function(&unknown));
    }
}
