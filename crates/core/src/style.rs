//! Cell visual styles, composable style patches and per-sheet style interning
//! (`14-m1b.md` 5.4).

use alloc::vec;
use alloc::vec::Vec;
use core::hash::BuildHasherDefault;

use hashbrown::HashMap;
use rustc_hash::FxHasher;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::digest::Digest64;

/// Horizontal text alignment inside a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HorizontalAlign {
    /// Type-driven alignment: numbers right, booleans/errors centered, text left.
    #[default]
    Auto,
    /// Left-aligned regardless of value type.
    Left,
    /// Horizontally centered.
    Center,
    /// Right-aligned regardless of value type.
    Right,
}

impl HorizontalAlign {
    const fn tag(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::Left => 1,
            Self::Center => 2,
            Self::Right => 3,
        }
    }
}

/// Number of built-in light/dark-adaptive semantic palette slots (`0..8`).
pub const PALETTE_SLOT_COUNT: u8 = 8;

/// Foreground text or background fill color specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColorValue {
    /// Theme-adaptive palette index (`0..PALETTE_SLOT_COUNT`).
    Palette(u8),
    /// Explicit 24-bit sRGB color (`0x00RRGGBB`).
    Rgb(u32),
}

impl ColorValue {
    /// Validates color range invariants (palette slot `< 8`, RGB `<= 0xFF_FFFF`).
    #[must_use]
    pub const fn is_valid(self) -> bool {
        match self {
            Self::Palette(slot) => slot < PALETTE_SLOT_COUNT,
            Self::Rgb(rgb) => rgb <= 0x00FF_FFFF,
        }
    }

    fn write_digest(self, d: &mut Digest64) {
        match self {
            Self::Palette(slot) => d.write_bytes(&[1, slot]),
            Self::Rgb(rgb) => {
                d.write_bytes(&[2]);
                d.write_u32(rgb);
            }
        }
    }
}

/// Stroke weight of a single cell edge border.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BorderWeight {
    /// No border drawn (gridline shows through).
    #[default]
    None,
    /// 1 CSS pixel solid border.
    Thin,
    /// 2 CSS pixel solid border.
    Medium,
}

impl BorderWeight {
    const fn tag(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Thin => 1,
            Self::Medium => 2,
        }
    }
}

/// Four-edge border configuration of a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BorderSet {
    /// Top border stroke.
    pub top: BorderWeight,
    /// Right border stroke.
    pub right: BorderWeight,
    /// Bottom border stroke.
    pub bottom: BorderWeight,
    /// Left border stroke.
    pub left: BorderWeight,
}

/// Resolved visual style of a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellStyle {
    /// Bold font weight.
    pub bold: bool,
    /// Italic font posture.
    pub italic: bool,
    /// Strikethrough decoration.
    pub strikethrough: bool,
    /// Horizontal text alignment.
    pub align: HorizontalAlign,
    /// Optional foreground text color.
    pub text_color: Option<ColorValue>,
    /// Optional background fill color.
    pub fill_color: Option<ColorValue>,
    /// Edge borders.
    pub borders: BorderSet,
}

impl CellStyle {
    /// Validates internal color bounds.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        let text_ok = match self.text_color {
            None => true,
            Some(c) => c.is_valid(),
        };
        let fill_ok = match self.fill_color {
            None => true,
            Some(c) => c.is_valid(),
        };
        text_ok && fill_ok
    }

    /// Feeds a deterministic canonical encoding into `d`.
    pub fn write_digest(&self, d: &mut Digest64) {
        let mut flags: u8 = 0;
        if self.bold {
            flags |= 1;
        }
        if self.italic {
            flags |= 2;
        }
        if self.strikethrough {
            flags |= 4;
        }
        d.write_bytes(&[
            flags,
            self.align.tag(),
            self.borders.top.tag(),
            self.borders.right.tag(),
            self.borders.bottom.tag(),
            self.borders.left.tag(),
        ]);
        match self.text_color {
            None => d.write_bytes(&[0]),
            Some(c) => c.write_digest(d),
        }
        match self.fill_color {
            None => d.write_bytes(&[0]),
            Some(c) => c.write_digest(d),
        }
    }
}

/// Tri-state patch for an optional color attribute.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColorPatch {
    /// Leave the attribute untouched.
    #[default]
    Keep,
    /// Set a specific color.
    Set(ColorValue),
    /// Explicitly clear the color to transparent/theme default.
    Clear,
}

impl ColorPatch {
    const fn is_valid(self) -> bool {
        match self {
            Self::Keep | Self::Clear => true,
            Self::Set(c) => c.is_valid(),
        }
    }

    fn write_digest(self, d: &mut Digest64) {
        match self {
            Self::Keep => d.write_bytes(&[0]),
            Self::Clear => d.write_bytes(&[1]),
            Self::Set(c) => {
                d.write_bytes(&[2]);
                c.write_digest(d);
            }
        }
    }
}

/// Composable attribute-level style override stored on a cell, row or column,
/// and carried in [`crate::ops::Op::SetFormat`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StylePatch {
    /// Bold override (`None` keeps inherited/existing setting).
    pub bold: Option<bool>,
    /// Italic override.
    pub italic: Option<bool>,
    /// Strikethrough override.
    pub strikethrough: Option<bool>,
    /// Horizontal alignment override.
    pub align: Option<HorizontalAlign>,
    /// Foreground text color patch.
    pub text_color: ColorPatch,
    /// Background fill color patch.
    pub fill_color: ColorPatch,
    /// Top border override.
    pub border_top: Option<BorderWeight>,
    /// Right border override.
    pub border_right: Option<BorderWeight>,
    /// Bottom border override.
    pub border_bottom: Option<BorderWeight>,
    /// Left border override.
    pub border_left: Option<BorderWeight>,
}

impl StylePatch {
    /// Whether every field is `None` / `Keep` (i.e. a no-op patch).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Validates color attributes inside this patch.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.text_color.is_valid() && self.fill_color.is_valid()
    }

    /// Returns a new [`StylePatch`] representing `self` layered on top of
    /// `prior`.
    #[must_use]
    pub const fn merged_over(self, prior: Self) -> Self {
        Self {
            bold: match self.bold {
                Some(v) => Some(v),
                None => prior.bold,
            },
            italic: match self.italic {
                Some(v) => Some(v),
                None => prior.italic,
            },
            strikethrough: match self.strikethrough {
                Some(v) => Some(v),
                None => prior.strikethrough,
            },
            align: match self.align {
                Some(v) => Some(v),
                None => prior.align,
            },
            text_color: match self.text_color {
                ColorPatch::Keep => prior.text_color,
                other => other,
            },
            fill_color: match self.fill_color {
                ColorPatch::Keep => prior.fill_color,
                other => other,
            },
            border_top: match self.border_top {
                Some(v) => Some(v),
                None => prior.border_top,
            },
            border_right: match self.border_right {
                Some(v) => Some(v),
                None => prior.border_right,
            },
            border_bottom: match self.border_bottom {
                Some(v) => Some(v),
                None => prior.border_bottom,
            },
            border_left: match self.border_left {
                Some(v) => Some(v),
                None => prior.border_left,
            },
        }
    }

    /// Applies this override layer onto a resolved [`CellStyle`].
    #[must_use]
    pub const fn apply_to(self, mut style: CellStyle) -> CellStyle {
        if let Some(v) = self.bold {
            style.bold = v;
        }
        if let Some(v) = self.italic {
            style.italic = v;
        }
        if let Some(v) = self.strikethrough {
            style.strikethrough = v;
        }
        if let Some(v) = self.align {
            style.align = v;
        }
        match self.text_color {
            ColorPatch::Keep => {}
            ColorPatch::Set(c) => style.text_color = Some(c),
            ColorPatch::Clear => style.text_color = None,
        }
        match self.fill_color {
            ColorPatch::Keep => {}
            ColorPatch::Set(c) => style.fill_color = Some(c),
            ColorPatch::Clear => style.fill_color = None,
        }
        if let Some(w) = self.border_top {
            style.borders.top = w;
        }
        if let Some(w) = self.border_right {
            style.borders.right = w;
        }
        if let Some(w) = self.border_bottom {
            style.borders.bottom = w;
        }
        if let Some(w) = self.border_left {
            style.borders.left = w;
        }
        style
    }

    /// Feeds a deterministic encoding of this patch into `d`.
    pub fn write_digest(&self, d: &mut Digest64) {
        let opt_bool = |b: Option<bool>| match b {
            None => 0_u8,
            Some(false) => 1_u8,
            Some(true) => 2_u8,
        };
        let opt_border = |w: Option<BorderWeight>| match w {
            None => 0_u8,
            Some(bw) => bw.tag().saturating_add(1),
        };
        let opt_align = match self.align {
            None => 0_u8,
            Some(a) => a.tag().saturating_add(1),
        };
        d.write_bytes(&[
            opt_bool(self.bold),
            opt_bool(self.italic),
            opt_bool(self.strikethrough),
            opt_align,
            opt_border(self.border_top),
            opt_border(self.border_right),
            opt_border(self.border_bottom),
            opt_border(self.border_left),
        ]);
        self.text_color.write_digest(d);
        self.fill_color.write_digest(d);
    }
}

/// 16-bit style table identifier carried in viewport v2 cell records (`0` is
/// always [`CellStyle::default`]).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct StyleId(pub u16);

type StyleBuckets = HashMap<u64, SmallVec<[u16; 1]>, BuildHasherDefault<FxHasher>>;

/// Per-worksheet deduplicated table of resolved [`CellStyle`] entries.
#[derive(Clone, Debug)]
pub struct StyleTable {
    entries: Vec<CellStyle>,
    buckets: StyleBuckets,
}

impl Default for StyleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl StyleTable {
    /// Creates a table containing `StyleId(0) = CellStyle::default()`.
    #[must_use]
    pub fn new() -> Self {
        let default_style = CellStyle::default();
        let mut buckets = StyleBuckets::default();
        buckets
            .entry(hash_style(&default_style))
            .or_default()
            .push(0);
        Self {
            entries: vec![default_style],
            buckets,
        }
    }

    /// Reconstructs a [`StyleTable`] from persisted entries, validating that
    /// entry `0` is [`CellStyle::default`] and all entries are distinct and valid.
    #[must_use]
    pub fn from_entries(entries: Vec<CellStyle>) -> Option<Self> {
        if entries.is_empty() || entries.len() > usize::from(u16::MAX) {
            return None;
        }
        if entries.first().copied() != Some(CellStyle::default()) {
            return None;
        }
        let mut table = Self::new();
        for style in entries.into_iter().skip(1) {
            if !style.is_valid() {
                return None;
            }
            let prev_len = table.entries.len();
            let _ = table.intern(style)?;
            if table.entries.len() == prev_len {
                // Duplicate entry in persisted table.
                return None;
            }
        }
        Some(table)
    }

    /// Looks up the existing [`StyleId`] of `style` without mutating the table.
    #[must_use]
    pub fn lookup(&self, style: &CellStyle) -> Option<StyleId> {
        if *style == CellStyle::default() {
            return Some(StyleId(0));
        }
        let h = hash_style(style);
        let candidates = self.buckets.get(&h)?;
        for &id in candidates {
            if self.entries.get(usize::from(id)) == Some(style) {
                return Some(StyleId(id));
            }
        }
        None
    }

    /// Interns `style`, returning its [`StyleId`] (or `None` if `65,536` styles
    /// are already stored).
    pub fn intern(&mut self, style: CellStyle) -> Option<StyleId> {
        if let Some(existing) = self.lookup(&style) {
            return Some(existing);
        }
        let id = u16::try_from(self.entries.len()).ok()?;
        let h = hash_style(&style);
        self.entries.push(style);
        self.buckets.entry(h).or_default().push(id);
        Some(StyleId(id))
    }

    /// Resolves a [`StyleId`] back to its [`CellStyle`] (out-of-bounds falls back
    /// to default).
    #[must_use]
    pub fn get(&self, id: StyleId) -> CellStyle {
        self.entries
            .get(usize::from(id.0))
            .copied()
            .unwrap_or_default()
    }

    /// All interned styles in index order `0..len`.
    #[must_use]
    pub fn entries(&self) -> &[CellStyle] {
        &self.entries
    }

    /// Approximate heap bytes owned by this table.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.entries
            .capacity()
            .saturating_mul(size_of::<CellStyle>())
            .saturating_add(
                self.buckets
                    .capacity()
                    .saturating_mul(size_of::<(u64, SmallVec<[u16; 1]>)>()),
            )
    }
}

fn hash_style(style: &CellStyle) -> u64 {
    let mut d = Digest64::new();
    style.write_digest(&mut d);
    d.finish()
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{
        BorderWeight, CellStyle, ColorPatch, ColorValue, HorizontalAlign, StyleId, StylePatch,
        StyleTable,
    };

    #[test]
    fn patches_layer_cleanly_and_style_table_deduplicates() {
        let col_patch = StylePatch {
            fill_color: ColorPatch::Set(ColorValue::Palette(2)),
            ..StylePatch::default()
        };
        let row_patch = StylePatch {
            bold: Some(true),
            border_bottom: Some(BorderWeight::Thin),
            ..StylePatch::default()
        };
        let cell_patch = StylePatch {
            align: Some(HorizontalAlign::Center),
            ..StylePatch::default()
        };

        let resolved =
            cell_patch.apply_to(row_patch.apply_to(col_patch.apply_to(CellStyle::default())));
        assert!(resolved.bold);
        assert_eq!(resolved.align, HorizontalAlign::Center);
        assert_eq!(resolved.fill_color, Some(ColorValue::Palette(2)));
        assert_eq!(resolved.borders.bottom, BorderWeight::Thin);

        let mut table = StyleTable::new();
        assert_eq!(table.intern(CellStyle::default()), Some(StyleId(0)));
        let id1 = table.intern(resolved);
        assert_eq!(id1, Some(StyleId(1)));
        assert_eq!(table.intern(resolved), Some(StyleId(1)));
        assert_eq!(table.get(StyleId(1)), resolved);
    }
}
