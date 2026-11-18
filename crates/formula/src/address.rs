//! A1 coordinate conversion and worksheet prefix formatting.

use alloc::string::String;
use core::fmt::Write as _;

use reticula_core::axis::{MAX_COL_AXIS_LEN, MAX_ROW_AXIS_LEN};
use serde::{Deserialize, Serialize};

/// Coordinate-based cell address (`0`-based `row` and `col`) with `$` flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellAddr {
    /// 0-based column index (`0 == A`).
    pub col: u32,
    /// 0-based row index (`0 == row 1`).
    pub row: u32,
    /// Whether the column was prefixed with `$`.
    pub abs_col: bool,
    /// Whether the row was prefixed with `$`.
    pub abs_row: bool,
}

impl CellAddr {
    /// Formats this cell address into `out` in canonical uppercase A1 form.
    pub fn write_a1(self, out: &mut String) {
        if self.abs_col {
            out.push('$');
        }
        write_col_letters(self.col, out);
        if self.abs_row {
            out.push('$');
        }
        let one_based = u64::from(self.row).saturating_add(1);
        let _ = write!(out, "{one_based}");
    }
}

/// Converts a 0-based column index (`0 -> "A"`, `25 -> "Z"`, `26 -> "AA"`,
/// `16_383 -> "XFD"`) and appends the uppercase letters to `out`.
pub fn write_col_letters(col: u32, out: &mut String) {
    let mut buf = [b'A'; 8];
    let mut len = 0usize;
    let mut current = u64::from(col).saturating_add(1);
    while current > 0 {
        let rem = current.saturating_sub(1) % 26;
        let ch = b'A'.saturating_add(u8::try_from(rem).unwrap_or(0));
        if let Some(slot) = buf.get_mut(len) {
            *slot = ch;
        }
        len = len.saturating_add(1);
        current = current.saturating_sub(1) / 26;
    }
    for idx in (0..len).rev() {
        if let Some(&b) = buf.get(idx) {
            out.push(char::from(b));
        }
    }
}

/// Parses 1-to-3 ASCII letters into a 0-based column index in
/// `0..MAX_COL_AXIS_LEN` (`A..=XFD`).
#[must_use]
pub fn letters_to_col(letters: &str) -> Option<u32> {
    if letters.is_empty() || letters.len() > 3 {
        return None;
    }
    let mut acc: u32 = 0;
    for b in letters.bytes() {
        let upper = b.to_ascii_uppercase();
        if !upper.is_ascii_uppercase() {
            return None;
        }
        let digit = u32::from(upper.saturating_sub(b'A')).saturating_add(1);
        acc = acc.checked_mul(26)?.checked_add(digit)?;
    }
    let zero_based = acc.checked_sub(1)?;
    if zero_based < MAX_COL_AXIS_LEN {
        Some(zero_based)
    } else {
        None
    }
}

/// Parses a 1-based row number string (`"1"` ..= `"2000000"`, no leading zeros)
/// into a 0-based row index.
#[must_use]
pub fn digits_to_row(digits: &str) -> Option<u32> {
    if digits.is_empty() || digits.starts_with('0') {
        return None;
    }
    let mut acc: u32 = 0;
    for b in digits.bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        let d = u32::from(b.saturating_sub(b'0'));
        acc = acc.checked_mul(10)?.checked_add(d)?;
    }
    if acc == 0 || acc > MAX_ROW_AXIS_LEN {
        return None;
    }
    acc.checked_sub(1)
}

/// Parses an A1 token (`A1`, `$A$1`, `$A1`, `A$1`) into a [`CellAddr`],
/// returning `None` if it is not a valid in-range cell coordinate.
#[must_use]
pub fn parse_cell_addr(token: &str) -> Option<CellAddr> {
    let bytes = token.as_bytes();
    let mut idx = 0usize;
    let abs_col = if bytes.first() == Some(&b'$') {
        idx = 1;
        true
    } else {
        false
    };

    let col_start = idx;
    while let Some(&b) = bytes.get(idx) {
        if b.is_ascii_alphabetic() {
            idx = idx.saturating_add(1);
        } else {
            break;
        }
    }
    let col_slice = token.get(col_start..idx)?;
    let col = letters_to_col(col_slice)?;

    let abs_row = if bytes.get(idx) == Some(&b'$') {
        idx = idx.saturating_add(1);
        true
    } else {
        false
    };

    let row_slice = token.get(idx..)?;
    let row = digits_to_row(row_slice)?;

    Some(CellAddr {
        col,
        row,
        abs_col,
        abs_row,
    })
}

/// Parses a column-only token (`A`, `$A`, `XFD`, `$XFD`) into `(col, abs_col)`.
#[must_use]
pub fn parse_col_only(token: &str) -> Option<(u32, bool)> {
    let (letters, abs) = match token.strip_prefix('$') {
        Some(rest) => (rest, true),
        None => (token, false),
    };
    let col = letters_to_col(letters)?;
    Some((col, abs))
}

/// Parses a row-only token (`1`, `$1`, `100`, `$100`) into `(row, abs_row)`.
#[must_use]
pub fn parse_row_only(token: &str) -> Option<(u32, bool)> {
    let (digits, abs) = match token.strip_prefix('$') {
        Some(rest) => (rest, true),
        None => (token, false),
    };
    let row = digits_to_row(digits)?;
    Some((row, abs))
}

/// Formats an optional worksheet prefix (`Sheet1!` or `'My Sheet'!`) into `out`.
pub fn write_sheet_prefix(sheet: Option<&str>, out: &mut String) {
    let Some(name) = sheet else {
        return;
    };
    let needs_quotes = name.is_empty()
        || name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_digit() || ch == '\'')
        || name
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '.'))
        || parse_cell_addr(name).is_some();

    if needs_quotes {
        out.push('\'');
        for ch in name.chars() {
            if ch == '\'' {
                out.push('\'');
            }
            out.push(ch);
        }
        out.push('\'');
    } else {
        out.push_str(name);
    }
    out.push('!');
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

    use alloc::string::String;

    use super::{CellAddr, letters_to_col, parse_cell_addr, write_col_letters};

    #[test]
    fn column_letters_and_a1_round_trip() {
        for (col, expected) in [
            (0, "A"),
            (25, "Z"),
            (26, "AA"),
            (51, "AZ"),
            (52, "BA"),
            (701, "ZZ"),
            (702, "AAA"),
            (16_383, "XFD"),
        ] {
            let mut s = String::new();
            write_col_letters(col, &mut s);
            assert_eq!(s, expected);
            assert_eq!(letters_to_col(&s), Some(col));
        }
        assert_eq!(letters_to_col("XFE"), None);
        assert_eq!(
            parse_cell_addr("$V$1"),
            Some(CellAddr {
                col: 21,
                row: 0,
                abs_col: true,
                abs_row: true,
            })
        );
    }
}
