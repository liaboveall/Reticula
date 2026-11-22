//! OOXML `_xlfn.` / `_xlfn._xlws.` prefix stripping and restoration, plus AST
//! built-in function verification (`17-m3a.md` §7).

use reticula_formula::ast::Expr;
use reticula_formula::functions::catalog::lookup_function_meta;

const XLWS_PREFIX: &str = "_xlfn._xlws.";
const XLFN_PREFIX: &str = "_xlfn.";
const SINGLE_PREFIX: &str = "_xlfn.SINGLE(";

/// Functions requiring the `_xlfn._xlws.` worksheet-function prefix in OOXML.
const XLWS_FUNCTIONS: &[&str] = &["FILTER", "SORT", "SORTBY"];

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

/// Strips OOXML `_xlfn._xlws.` and `_xlfn.` prefixes outside quotes and sheet
/// identifiers, translating `_xlfn.SINGLE(` to Reticula `@(`.
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
/// require them without double-prefixing or mutating literals.
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
    fn unknown_function_detection_matches_catalog() {
        let known = parse_formula("=XLOOKUP(A1,B1:B5,C1:C5)").expect("parses");
        assert!(!contains_unknown_function(&known));
        let unknown = parse_formula("=SUM(1,MY_CUSTOM_UDF(A1))").expect("parses");
        assert!(contains_unknown_function(&unknown));
    }
}
