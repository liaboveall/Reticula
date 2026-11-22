//! Pratt parser for spreadsheet formulas (`13-m1a.md` 5.3 & 5.4).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::ops::MAX_FORMULA_CHARS;
use reticula_core::value::CalcError;

use crate::address::{parse_cell_addr, parse_col_only, parse_row_only};
use crate::ast::{BinaryOp, Expr, Literal, MAX_FUNCTION_ARGS, MAX_NESTING_DEPTH, UnaryOp};
use crate::lexer::{LexError, Token, tokenize};

/// Syntax or static-limit error produced when parsing a formula.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Formula did not begin with `'='` or was empty after `'='`.
    MissingEquals,
    /// Formula exceeded [`MAX_FORMULA_CHARS`] characters.
    FormulaTooLong,
    /// Parenthesis, function or array nesting exceeded [`MAX_NESTING_DEPTH`].
    NestingTooDeep,
    /// Function call had more than [`MAX_FUNCTION_ARGS`] arguments.
    TooManyArguments,
    /// Lexical error in input text.
    Lex(LexError),
    /// Unexpected token or premature end of expression.
    UnexpectedToken,
    /// Malformed range operand around `':'`.
    InvalidRange,
    /// Empty, non-rectangular, or non-constant `{...}` array literal.
    InvalidArrayLiteral,
}

impl From<LexError> for ParseError {
    fn from(err: LexError) -> Self {
        match err {
            LexError::FormulaTooLong => Self::FormulaTooLong,
            other => Self::Lex(other),
        }
    }
}

/// Parses a formula string starting with `'='` into a coordinate [`Expr`].
pub fn parse_formula(source: &str) -> Result<Expr, ParseError> {
    if source.chars().count() > MAX_FORMULA_CHARS {
        return Err(ParseError::FormulaTooLong);
    }
    let body = source.strip_prefix('=').ok_or(ParseError::MissingEquals)?;
    parse_expr(body)
}

/// Parses a formula body (without leading `'='`) into a coordinate [`Expr`].
pub fn parse_expr(body: &str) -> Result<Expr, ParseError> {
    if body.chars().count() > MAX_FORMULA_CHARS {
        return Err(ParseError::FormulaTooLong);
    }
    let tokens = tokenize(body)?;
    if tokens.is_empty() {
        return Err(ParseError::MissingEquals);
    }
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
    };
    let expr = parser.parse_bp(0, 0)?;
    if parser.pos != parser.tokens.len() {
        return Err(ParseError::UnexpectedToken);
    }
    Ok(expr)
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.pos.saturating_add(offset))
    }

    fn bump(&mut self) -> Option<&Token> {
        let tok = self.tokens.get(self.pos)?;
        self.pos = self.pos.saturating_add(1);
        Some(tok)
    }

    fn parse_bp(&mut self, min_bp: u8, depth: usize) -> Result<Expr, ParseError> {
        if depth > MAX_NESTING_DEPTH {
            return Err(ParseError::NestingTooDeep);
        }

        let mut lhs = self.parse_prefix(depth)?;

        let mut postfix_depth = depth;
        while let Some(tok) = self.peek() {
            // Postfix `#` (spill range) binds tightest (`lbp = 90`), above `%` (`80`) and unary (`70`).
            if matches!(tok, Token::Hash) {
                if 90 < min_bp {
                    break;
                }
                postfix_depth = postfix_depth.saturating_add(1);
                if postfix_depth > MAX_NESTING_DEPTH {
                    return Err(ParseError::NestingTooDeep);
                }
                let _ = self.bump();
                lhs = Expr::SpillRef(Box::new(lhs));
                continue;
            }
            // Postfix `%` binds tighter (`lbp = 80`) than prefix `-` (`rbp = 70`)
            // and exponentiation `^` (`lbp = 60`).
            if matches!(tok, Token::Percent) {
                if 80 < min_bp {
                    break;
                }
                postfix_depth = postfix_depth.saturating_add(1);
                if postfix_depth > MAX_NESTING_DEPTH {
                    return Err(ParseError::NestingTooDeep);
                }
                let _ = self.bump();
                lhs = Expr::Unary {
                    op: UnaryOp::Percent,
                    expr: Box::new(lhs),
                };
                continue;
            }

            let Some((op, lbp, rbp)) = infix_binding_power(tok) else {
                break;
            };
            if lbp < min_bp {
                break;
            }
            let _ = self.bump();
            let rhs = self.parse_bp(rbp, depth)?;
            lhs = Expr::Binary {
                op,
                left: Box::new(lhs),
                right: Box::new(rhs),
            };
        }

        Ok(lhs)
    }

    fn parse_prefix(&mut self, depth: usize) -> Result<Expr, ParseError> {
        let Some(tok) = self.peek().cloned() else {
            return Err(ParseError::UnexpectedToken);
        };

        match tok {
            Token::Plus => {
                let _ = self.bump();
                let next_depth = depth.saturating_add(1);
                let operand = self.parse_bp(70, next_depth)?;
                Ok(Expr::Unary {
                    op: UnaryOp::Plus,
                    expr: Box::new(operand),
                })
            }
            Token::Minus => {
                let _ = self.bump();
                let next_depth = depth.saturating_add(1);
                let operand = self.parse_bp(70, next_depth)?;
                Ok(Expr::Unary {
                    op: UnaryOp::Neg,
                    expr: Box::new(operand),
                })
            }
            Token::At => {
                let _ = self.bump();
                let next_depth = depth.saturating_add(1);
                let operand = self.parse_bp(70, next_depth)?;
                Ok(Expr::Unary {
                    op: UnaryOp::ImplicitIntersection,
                    expr: Box::new(operand),
                })
            }
            Token::LParen => {
                let _ = self.bump();
                let next_depth = depth.saturating_add(1);
                if next_depth > MAX_NESTING_DEPTH {
                    return Err(ParseError::NestingTooDeep);
                }
                let inner = self.parse_bp(0, next_depth)?;
                if !matches!(self.bump(), Some(Token::RParen)) {
                    return Err(ParseError::UnexpectedToken);
                }
                Ok(Expr::Paren(Box::new(inner)))
            }
            Token::LBrace => self.parse_array_literal(depth.saturating_add(1)),
            Token::StringLit(s) => {
                let _ = self.bump();
                Ok(Expr::Literal(Literal::Text(s)))
            }
            Token::ErrorLit(err) => {
                let _ = self.bump();
                if err == CalcError::Ref {
                    let has_coord_suffix = match self.peek() {
                        Some(Token::Ident(_)) => true,
                        Some(Token::Number {
                            raw_digits: Some(_),
                            ..
                        }) => matches!(self.peek_at(1), Some(Token::Colon)),
                        _ => false,
                    };
                    if has_coord_suffix {
                        return self.parse_qualified_ref(String::from("#REF!"));
                    }
                }
                Ok(Expr::Literal(Literal::Error(err)))
            }
            Token::Number { value, raw_digits } => {
                if let Some(d1) = raw_digits
                    && matches!(self.peek_at(1), Some(Token::Colon))
                {
                    let _ = self.bump(); // first row number
                    let _ = self.bump(); // ':'
                    return self.finish_row_range(None, &d1);
                }
                let _ = self.bump();
                Ok(Expr::Literal(Literal::Number(value)))
            }
            Token::SheetPrefix(sheet_name) => {
                let _ = self.bump();
                self.parse_qualified_ref(sheet_name)
            }
            Token::Ident(ident) => {
                let _ = self.bump();
                if matches!(self.peek(), Some(Token::LParen)) {
                    return self.parse_function_call(&ident, depth.saturating_add(1));
                }
                if matches!(self.peek(), Some(Token::Colon)) {
                    let _ = self.bump(); // ':'
                    return self.finish_ident_range(None, &ident);
                }
                if ident.eq_ignore_ascii_case("TRUE") {
                    return Ok(Expr::Literal(Literal::Bool(true)));
                }
                if ident.eq_ignore_ascii_case("FALSE") {
                    return Ok(Expr::Literal(Literal::Bool(false)));
                }
                if let Some(addr) = parse_cell_addr(&ident) {
                    return Ok(Expr::CellRef { sheet: None, addr });
                }
                if ident.starts_with('$') {
                    return Err(ParseError::InvalidRange);
                }
                Ok(Expr::Name(ident.to_ascii_uppercase()))
            }
            _ => Err(ParseError::UnexpectedToken),
        }
    }

    fn parse_qualified_ref(&mut self, sheet: String) -> Result<Expr, ParseError> {
        let Some(first_tok) = self.bump().cloned() else {
            return Err(ParseError::InvalidRange);
        };
        match first_tok {
            Token::Ident(ident) => {
                if matches!(self.peek(), Some(Token::Colon)) {
                    let _ = self.bump();
                    self.finish_ident_range(Some(sheet), &ident)
                } else if let Some(addr) = parse_cell_addr(&ident) {
                    Ok(Expr::CellRef {
                        sheet: Some(sheet),
                        addr,
                    })
                } else if !ident.contains('$')
                    && !ident.eq_ignore_ascii_case("TRUE")
                    && !ident.eq_ignore_ascii_case("FALSE")
                {
                    Ok(Expr::QualifiedName { sheet, name: ident })
                } else {
                    Err(ParseError::InvalidRange)
                }
            }
            Token::Number {
                raw_digits: Some(d1),
                ..
            } => {
                if !matches!(self.bump(), Some(Token::Colon)) {
                    return Err(ParseError::InvalidRange);
                }
                self.finish_row_range(Some(sheet), &d1)
            }
            _ => Err(ParseError::InvalidRange),
        }
    }

    fn finish_ident_range(
        &mut self,
        sheet: Option<String>,
        first_ident: &str,
    ) -> Result<Expr, ParseError> {
        // Case 1: `$1:$5` or `$1:5` where left started with `$` + digits.
        if parse_row_only(first_ident).is_some() {
            return self.finish_row_range(sheet, first_ident);
        }

        let Some(second_tok) = self.bump().cloned() else {
            return Err(ParseError::InvalidRange);
        };
        let Token::Ident(second_ident) = second_tok else {
            return Err(ParseError::InvalidRange);
        };

        if let (Some(first), Some(last)) =
            (parse_cell_addr(first_ident), parse_cell_addr(&second_ident))
        {
            return Ok(Expr::AreaRef { sheet, first, last });
        }

        if let (Some((first_col, abs_first)), Some((last_col, abs_last))) =
            (parse_col_only(first_ident), parse_col_only(&second_ident))
        {
            return Ok(Expr::ColRange {
                sheet,
                first_col,
                abs_first,
                last_col,
                abs_last,
            });
        }

        Err(ParseError::InvalidRange)
    }

    fn finish_row_range(
        &mut self,
        sheet: Option<String>,
        first_raw: &str,
    ) -> Result<Expr, ParseError> {
        let (first_row, abs_first) = parse_row_only(first_raw).ok_or(ParseError::InvalidRange)?;
        let Some(second_tok) = self.bump().cloned() else {
            return Err(ParseError::InvalidRange);
        };
        let second_str = match second_tok {
            Token::Number {
                raw_digits: Some(d2),
                ..
            } => d2,
            Token::Ident(s) => s,
            _ => return Err(ParseError::InvalidRange),
        };
        let (last_row, abs_last) = parse_row_only(&second_str).ok_or(ParseError::InvalidRange)?;
        Ok(Expr::RowRange {
            sheet,
            first_row,
            abs_first,
            last_row,
            abs_last,
        })
    }

    fn parse_function_call(&mut self, name: &str, depth: usize) -> Result<Expr, ParseError> {
        if depth > MAX_NESTING_DEPTH {
            return Err(ParseError::NestingTooDeep);
        }
        let _ = self.bump(); // '('
        let mut args = Vec::new();
        if matches!(self.peek(), Some(Token::RParen)) {
            let _ = self.bump();
            return Ok(Expr::Call {
                name: name.to_ascii_uppercase(),
                args,
            });
        }
        loop {
            if args.len() >= MAX_FUNCTION_ARGS {
                return Err(ParseError::TooManyArguments);
            }
            let arg = self.parse_bp(0, depth)?;
            args.push(arg);
            match self.bump() {
                Some(Token::Comma) => {}
                Some(Token::RParen) => break,
                _ => return Err(ParseError::UnexpectedToken),
            }
        }
        Ok(Expr::Call {
            name: name.to_ascii_uppercase(),
            args,
        })
    }

    fn parse_array_literal(&mut self, depth: usize) -> Result<Expr, ParseError> {
        if depth > MAX_NESTING_DEPTH {
            return Err(ParseError::NestingTooDeep);
        }
        let _ = self.bump(); // '{'
        if matches!(self.peek(), Some(Token::RBrace)) {
            return Err(ParseError::InvalidArrayLiteral);
        }

        let mut elements = Vec::new();
        let mut expected_cols: Option<u32> = None;
        let mut current_cols: u32 = 0;
        let mut rows: u32 = 1;

        loop {
            let lit = self.parse_array_item()?;
            elements.push(lit);
            current_cols = current_cols
                .checked_add(1)
                .ok_or(ParseError::InvalidArrayLiteral)?;

            match self.bump() {
                Some(Token::Comma) => {}
                Some(Token::Semicolon) => {
                    match expected_cols {
                        None => expected_cols = Some(current_cols),
                        Some(cols) if cols != current_cols => {
                            return Err(ParseError::InvalidArrayLiteral);
                        }
                        Some(_) => {}
                    }
                    current_cols = 0;
                    rows = rows.checked_add(1).ok_or(ParseError::InvalidArrayLiteral)?;
                }
                Some(Token::RBrace) => {
                    match expected_cols {
                        None => expected_cols = Some(current_cols),
                        Some(cols) if cols != current_cols => {
                            return Err(ParseError::InvalidArrayLiteral);
                        }
                        Some(_) => {}
                    }
                    break;
                }
                _ => return Err(ParseError::InvalidArrayLiteral),
            }
        }

        let cols = expected_cols.ok_or(ParseError::InvalidArrayLiteral)?;
        if cols == 0 {
            return Err(ParseError::InvalidArrayLiteral);
        }
        Ok(Expr::ArrayLiteral {
            rows,
            cols,
            elements,
        })
    }

    fn parse_array_item(&mut self) -> Result<Literal, ParseError> {
        let Some(tok) = self.bump().cloned() else {
            return Err(ParseError::InvalidArrayLiteral);
        };
        match tok {
            Token::Number { value, .. } => Ok(Literal::Number(value)),
            Token::Plus => match self.bump() {
                Some(Token::Number { value, .. }) => Ok(Literal::Number(*value)),
                _ => Err(ParseError::InvalidArrayLiteral),
            },
            Token::Minus => match self.bump() {
                Some(Token::Number { value, .. }) => {
                    let neg = if *value == 0.0 { 0.0 } else { -*value };
                    Ok(Literal::Number(neg))
                }
                _ => Err(ParseError::InvalidArrayLiteral),
            },
            Token::StringLit(s) => Ok(Literal::Text(s)),
            Token::ErrorLit(err) => Ok(Literal::Error(err)),
            Token::Ident(ident) if ident.eq_ignore_ascii_case("TRUE") => Ok(Literal::Bool(true)),
            Token::Ident(ident) if ident.eq_ignore_ascii_case("FALSE") => Ok(Literal::Bool(false)),
            _ => Err(ParseError::InvalidArrayLiteral),
        }
    }
}

const fn infix_binding_power(tok: &Token) -> Option<(BinaryOp, u8, u8)> {
    match tok {
        // Left-associative in Excel: `=2^3^2` == `(2^3)^2` == 64.
        Token::Caret => Some((BinaryOp::Pow, 60, 61)),
        Token::Star => Some((BinaryOp::Mul, 50, 51)),
        Token::Slash => Some((BinaryOp::Div, 50, 51)),
        Token::Plus => Some((BinaryOp::Add, 40, 41)),
        Token::Minus => Some((BinaryOp::Sub, 40, 41)),
        Token::Ampersand => Some((BinaryOp::Concat, 30, 31)),
        Token::Eq => Some((BinaryOp::Eq, 20, 21)),
        Token::Ne => Some((BinaryOp::Ne, 20, 21)),
        Token::Lt => Some((BinaryOp::Lt, 20, 21)),
        Token::Gt => Some((BinaryOp::Gt, 20, 21)),
        Token::Le => Some((BinaryOp::Le, 20, 21)),
        Token::Ge => Some((BinaryOp::Ge, 20, 21)),
        _ => None,
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{ParseError, parse_formula};
    use crate::ast::{BinaryOp, Expr, UnaryOp};

    #[test]
    fn excel_precedence_for_unary_neg_power_and_percent_holds() {
        // `=-2^2` in Excel parses as `(-2)^2`, NOT `-(2^2)`!
        let neg_pow = parse_formula("=-2^2").unwrap();
        assert!(matches!(
            neg_pow,
            Expr::Binary {
                op: BinaryOp::Pow,
                ref left,
                ..
            } if matches!(**left, Expr::Unary { op: UnaryOp::Neg, .. })
        ));
        assert_eq!(neg_pow.to_formula_string(), "=-2^2");

        // `=2^3^2` in Excel is left-associative `(2^3)^2`.
        let pow_chain = parse_formula("=2^3^2").unwrap();
        assert!(matches!(
            pow_chain,
            Expr::Binary {
                op: BinaryOp::Pow,
                ref left,
                ..
            } if matches!(**left, Expr::Binary { op: BinaryOp::Pow, .. })
        ));
        assert_eq!(pow_chain.to_formula_string(), "=2^3^2");

        // `=2^(3^2)` preserves right grouping via `Expr::Paren`.
        let pow_right = parse_formula("=2^(3^2)").unwrap();
        assert_eq!(pow_right.to_formula_string(), "=2^(3^2)");

        // `=-50%` applies `%` first (`-(50%)`).
        let neg_pct = parse_formula("=-50%").unwrap();
        assert!(matches!(
            neg_pct,
            Expr::Unary {
                op: UnaryOp::Neg,
                ref expr,
            } if matches!(**expr, Expr::Unary { op: UnaryOp::Percent, .. })
        ));
    }

    #[test]
    fn references_arrays_and_round_trip_printing_work() {
        for sample in [
            "=SUM(A1:B10)",
            "=SUM($A$1:$C$5,1:5,$A:$C)",
            "='Sales 2026'!$M2*(1-$V$1)",
            "={1,-2.5,\"hi\";TRUE,FALSE,#N/A}",
            "=@A2:A100+10",
            "=(A1-B1)",
            "=A1#",
            "=-A1#",
            "=A1#%",
            "=SUM('Sales 2026'!A1#)",
            "=Sheet2!TaxRate",
            "='Sales 2026'!TaxRate#",
        ] {
            let ast = parse_formula(sample).unwrap();
            let printed = ast.to_formula_string();
            let reparsed = parse_formula(&printed).unwrap();
            assert_eq!(ast, reparsed, "mismatch on {sample} -> {printed}");
        }

        let neg_spill = parse_formula("=-A1#").unwrap();
        assert!(matches!(
            neg_spill,
            Expr::Unary {
                op: UnaryOp::Neg,
                ref expr,
            } if matches!(**expr, Expr::SpillRef(_))
        ));
    }

    #[test]
    fn static_limits_reject_deep_or_wide_formulas_without_panicking() {
        let too_deep = alloc::format!("={}1{}", "(".repeat(65), ")".repeat(65));
        assert_eq!(parse_formula(&too_deep), Err(ParseError::NestingTooDeep));

        let mut many_args = alloc::string::String::from("=SUM(");
        for i in 0..256 {
            if i > 0 {
                many_args.push(',');
            }
            many_args.push('1');
        }
        many_args.push(')');
        assert_eq!(parse_formula(&many_args), Err(ParseError::TooManyArguments));

        assert_eq!(
            parse_formula("={1,2;3}"),
            Err(ParseError::InvalidArrayLiteral)
        );
    }
}
