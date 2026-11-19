//! Formula lexer.
//!
//! Converts raw formula text (after stripping a leading `'='`) into a flat
//! stream of [`Token`]s with zero panics and strict length bounds.

use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::ops::MAX_FORMULA_CHARS;
use reticula_core::value::CalcError;

/// Lexical token emitted by [`tokenize`].
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    /// Numeric literal; `raw_digits` carries the original digit substring when
    /// the token consisted solely of ASCII digits (used for `1:5` row ranges).
    Number {
        /// Parsed finite `f64` value.
        value: f64,
        /// Raw digit string if the literal had no decimal point or exponent.
        raw_digits: Option<String>,
    },
    /// Double-quoted string literal (`""` already unescaped to `"`).
    StringLit(String),
    /// Standard `#...` error literal.
    ErrorLit(CalcError),
    /// Worksheet qualifier immediately followed by `'!'` (`Sheet1!` or `'My Sheet'!`).
    SheetPrefix(String),
    /// Identifier, cell coordinate, `$`-prefixed row/column token, or function name.
    Ident(String),
    /// `:`
    Colon,
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `^`
    Caret,
    /// `%`
    Percent,
    /// `&`
    Ampersand,
    /// `@`
    At,
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `>=`
    Ge,
}

/// Lexical analysis error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LexError {
    /// Formula exceeded [`MAX_FORMULA_CHARS`] Unicode scalar values.
    FormulaTooLong,
    /// Unterminated `"..."` string literal.
    UnterminatedString,
    /// Unterminated `'Sheet Name'!` quoted worksheet qualifier.
    UnterminatedSheetQuote,
    /// Unrecognised `#...` error literal.
    InvalidErrorLiteral,
    /// Malformed or overflowing numeric literal.
    InvalidNumber,
    /// Unexpected character in formula input.
    UnexpectedChar,
}

/// Tokenises `input` (an expression without its leading `'='`).
pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    if input.chars().count() > MAX_FORMULA_CHARS {
        return Err(LexError::FormulaTooLong);
    }

    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;
    let mut tokens = Vec::new();

    while pos < len {
        let Some(&b) = bytes.get(pos) else {
            break;
        };

        // Skip ASCII whitespace.
        if b == b' ' || b == b'\t' || b == b'\r' || b == b'\n' {
            pos = pos.saturating_add(1);
            continue;
        }

        match b {
            b':' => {
                tokens.push(Token::Colon);
                pos = pos.saturating_add(1);
            }
            b',' => {
                tokens.push(Token::Comma);
                pos = pos.saturating_add(1);
            }
            b';' => {
                tokens.push(Token::Semicolon);
                pos = pos.saturating_add(1);
            }
            b'(' => {
                tokens.push(Token::LParen);
                pos = pos.saturating_add(1);
            }
            b')' => {
                tokens.push(Token::RParen);
                pos = pos.saturating_add(1);
            }
            b'{' => {
                tokens.push(Token::LBrace);
                pos = pos.saturating_add(1);
            }
            b'}' => {
                tokens.push(Token::RBrace);
                pos = pos.saturating_add(1);
            }
            b'+' => {
                tokens.push(Token::Plus);
                pos = pos.saturating_add(1);
            }
            b'-' => {
                tokens.push(Token::Minus);
                pos = pos.saturating_add(1);
            }
            b'*' => {
                tokens.push(Token::Star);
                pos = pos.saturating_add(1);
            }
            b'/' => {
                tokens.push(Token::Slash);
                pos = pos.saturating_add(1);
            }
            b'^' => {
                tokens.push(Token::Caret);
                pos = pos.saturating_add(1);
            }
            b'%' => {
                tokens.push(Token::Percent);
                pos = pos.saturating_add(1);
            }
            b'&' => {
                tokens.push(Token::Ampersand);
                pos = pos.saturating_add(1);
            }
            b'@' => {
                tokens.push(Token::At);
                pos = pos.saturating_add(1);
            }
            b'=' => {
                tokens.push(Token::Eq);
                pos = pos.saturating_add(1);
            }
            b'<' => {
                let next = bytes.get(pos.saturating_add(1)).copied();
                if next == Some(b'>') {
                    tokens.push(Token::Ne);
                    pos = pos.saturating_add(2);
                } else if next == Some(b'=') {
                    tokens.push(Token::Le);
                    pos = pos.saturating_add(2);
                } else {
                    tokens.push(Token::Lt);
                    pos = pos.saturating_add(1);
                }
            }
            b'>' => {
                if bytes.get(pos.saturating_add(1)) == Some(&b'=') {
                    tokens.push(Token::Ge);
                    pos = pos.saturating_add(2);
                } else {
                    tokens.push(Token::Gt);
                    pos = pos.saturating_add(1);
                }
            }
            b'"' => {
                let (lit, next_pos) = lex_string(input, pos)?;
                tokens.push(Token::StringLit(lit));
                pos = next_pos;
            }
            b'\'' => {
                let (sheet_name, next_pos) = lex_quoted_sheet(input, pos)?;
                tokens.push(Token::SheetPrefix(sheet_name));
                pos = next_pos;
            }
            b'#' => {
                let (err, next_pos) = lex_error_literal(input, pos)?;
                tokens.push(Token::ErrorLit(err));
                pos = next_pos;
            }
            b'0'..=b'9' | b'.' => {
                let (tok, next_pos) = lex_number(input, pos)?;
                tokens.push(tok);
                pos = next_pos;
            }
            b'$' | b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let (tok, next_pos) = lex_ident_or_sheet(input, pos)?;
                tokens.push(tok);
                pos = next_pos;
            }
            _ => return Err(LexError::UnexpectedChar),
        }
    }

    Ok(tokens)
}

fn lex_string(input: &str, start_byte: usize) -> Result<(String, usize), LexError> {
    let rest = input
        .get(start_byte.saturating_add(1)..)
        .ok_or(LexError::UnterminatedString)?;
    let mut out = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        if ch == '"' {
            if let Some(&(_, '"')) = chars.peek() {
                let _ = chars.next();
                out.push('"');
            } else {
                let next_pos = start_byte
                    .saturating_add(1)
                    .saturating_add(offset)
                    .saturating_add(1);
                return Ok((out, next_pos));
            }
        } else {
            out.push(ch);
        }
    }
    Err(LexError::UnterminatedString)
}

fn lex_quoted_sheet(input: &str, start_byte: usize) -> Result<(String, usize), LexError> {
    let rest = input
        .get(start_byte.saturating_add(1)..)
        .ok_or(LexError::UnterminatedSheetQuote)?;
    let mut out = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        if ch == '\'' {
            if let Some(&(_, '\'')) = chars.peek() {
                let _ = chars.next();
                out.push('\'');
            } else {
                let after_quote = start_byte
                    .saturating_add(1)
                    .saturating_add(offset)
                    .saturating_add(1);
                if input.as_bytes().get(after_quote) != Some(&b'!') || out.is_empty() {
                    return Err(LexError::UnterminatedSheetQuote);
                }
                return Ok((out, after_quote.saturating_add(1)));
            }
        } else {
            out.push(ch);
        }
    }
    Err(LexError::UnterminatedSheetQuote)
}

fn lex_error_literal(input: &str, start_byte: usize) -> Result<(CalcError, usize), LexError> {
    let bytes = input.as_bytes();
    let mut end = start_byte.saturating_add(1);
    while let Some(&b) = bytes.get(end) {
        if b.is_ascii_alphanumeric() || b == b'/' || b == b'!' || b == b'?' {
            if b == b'/'
                && end.saturating_sub(start_byte) == 4
                && input
                    .get(start_byte..end)
                    .is_some_and(|s| s.eq_ignore_ascii_case("#N/A"))
            {
                break;
            }
            end = end.saturating_add(1);
            if b == b'!' || b == b'?' {
                break;
            }
        } else {
            break;
        }
    }
    let slice = input
        .get(start_byte..end)
        .ok_or(LexError::InvalidErrorLiteral)?;
    let err = CalcError::parse_literal(slice).ok_or(LexError::InvalidErrorLiteral)?;
    Ok((err, end))
}

fn lex_number(input: &str, start_byte: usize) -> Result<(Token, usize), LexError> {
    let bytes = input.as_bytes();
    let mut pos = start_byte;
    let mut int_digits = 0usize;
    while let Some(&b) = bytes.get(pos) {
        if b.is_ascii_digit() {
            int_digits = int_digits.saturating_add(1);
            pos = pos.saturating_add(1);
        } else {
            break;
        }
    }

    let mut has_dot = false;
    let mut frac_digits = 0usize;
    if bytes.get(pos) == Some(&b'.') {
        has_dot = true;
        pos = pos.saturating_add(1);
        while let Some(&b) = bytes.get(pos) {
            if b.is_ascii_digit() {
                frac_digits = frac_digits.saturating_add(1);
                pos = pos.saturating_add(1);
            } else {
                break;
            }
        }
    }

    if int_digits == 0 && frac_digits == 0 {
        return Err(LexError::InvalidNumber);
    }

    let mut has_exp = false;
    if let Some(&e_ch) = bytes.get(pos)
        && (e_ch == b'e' || e_ch == b'E')
    {
        has_exp = true;
        pos = pos.saturating_add(1);
        if let Some(&sign) = bytes.get(pos)
            && (sign == b'+' || sign == b'-')
        {
            pos = pos.saturating_add(1);
        }
        let exp_start = pos;
        while let Some(&b) = bytes.get(pos) {
            if b.is_ascii_digit() {
                pos = pos.saturating_add(1);
            } else {
                break;
            }
        }
        if pos == exp_start {
            return Err(LexError::InvalidNumber);
        }
    }

    let slice = input.get(start_byte..pos).ok_or(LexError::InvalidNumber)?;
    let value = slice.parse::<f64>().map_err(|_| LexError::InvalidNumber)?;
    if !value.is_finite() {
        return Err(LexError::InvalidNumber);
    }
    let canonical = if value == 0.0 { 0.0 } else { value };
    let raw_digits = if !has_dot && !has_exp {
        Some(String::from(slice))
    } else {
        None
    };
    Ok((
        Token::Number {
            value: canonical,
            raw_digits,
        },
        pos,
    ))
}

fn lex_ident_or_sheet(input: &str, start_byte: usize) -> Result<(Token, usize), LexError> {
    let bytes = input.as_bytes();
    let mut pos = start_byte;
    let starts_with_dollar = bytes.get(pos) == Some(&b'$');
    while let Some(&b) = bytes.get(pos) {
        if b.is_ascii_alphanumeric() || b == b'$' || b == b'_' || b == b'.' {
            pos = pos.saturating_add(1);
        } else {
            break;
        }
    }
    let slice = input.get(start_byte..pos).ok_or(LexError::UnexpectedChar)?;
    if !starts_with_dollar && bytes.get(pos) == Some(&b'!') {
        return Ok((
            Token::SheetPrefix(String::from(slice)),
            pos.saturating_add(1),
        ));
    }
    Ok((Token::Ident(String::from(slice)), pos))
}
