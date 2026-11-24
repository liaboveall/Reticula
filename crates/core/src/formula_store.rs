//! Identity-affine 1D vertical column strip storage for workbook formulas.
//!
//! [`FormulaStore`] stores contiguous vertical runs of identically-templated
//! formulas in a column as a single compact [`TemplateStrip`] instead of
//! repeating per-cell AST allocations across hundreds of thousands of rows.
//! Singletons and non-strippable formulas remain in a sorted `singles` map.

use alloc::borrow::Cow;
use alloc::collections::btree_map::Range as BTreeRange;
use alloc::collections::{BTreeMap, BTreeSet, BinaryHeap};
use alloc::vec::Vec;
use core::cmp::Reverse;
use core::iter::Peekable;
use core::mem::size_of;
use core::ops::{Bound, RangeBounds};

use crate::id::{ColId, IdRun, RowId, SheetId, id_counter, id_stream};
use crate::ops::{FormulaPayload, WorkbookCellKey};

/// A contiguous vertical run (`len >= 2`) of formulas in `(sheet, col)` sharing
/// an identity-affine [`FormulaPayload::StripTemplate`].
///
/// The strip owns every key `(sheet, RowId(start_row.0 + offset), col)` for
/// `0 <= offset < len`, all guaranteed by construction to belong to the same
/// identity stream (`IdRun::new(start_row.0, len).is_some()`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateStrip<T> {
    /// Owning worksheet identity.
    pub sheet: SheetId,
    /// Owning column identity.
    pub col: ColId,
    /// First row identity covered by the strip (`offset == 0`).
    pub start_row: RowId,
    /// Number of contiguous row identities in this strip (`>= 2`).
    pub len: u32,
    /// Identity-affine template describing every cell in `[0..len)`.
    pub template: T,
}

impl<T> TemplateStrip<T> {
    /// Last inclusive row identity covered by this strip.
    #[must_use]
    pub const fn end_row_inclusive(&self) -> RowId {
        RowId(
            self.start_row
                .0
                .saturating_add((self.len as u64).saturating_sub(1)),
        )
    }

    /// Exclusive upper row identity (`start_row.0 + len`).
    #[must_use]
    pub const fn end_row_exclusive_raw(&self) -> u64 {
        self.start_row.0.saturating_add(self.len as u64)
    }

    /// Returns the `0`-based offset within this strip if `row` lies inside it.
    #[must_use]
    pub fn offset_of_row(&self, row: RowId) -> Option<u32> {
        if row.0 < self.start_row.0 || id_stream(row.0) != id_stream(self.start_row.0) {
            return None;
        }
        let diff = row.0.saturating_sub(self.start_row.0);
        if diff >= u64::from(self.len) {
            return None;
        }
        u32::try_from(diff).ok()
    }
}

/// Compressed formula collection keyed by [`WorkbookCellKey`] `(SheetId, RowId, ColId)`.
///
/// Contiguous same-column formulas matching an identity-affine template coalesce
/// automatically into [`TemplateStrip`] entries in `col_strips` while singleton or
/// unstrippable formulas live in `singles`. All public lookup and ordered iteration
/// methods present the exact same logical `(SheetId, RowId, ColId)` map semantics as
/// a flat `BTreeMap<WorkbookCellKey, F>`.
#[derive(Clone, Debug)]
pub struct FormulaStore<F: FormulaPayload> {
    singles: BTreeMap<WorkbookCellKey, F>,
    col_strips: BTreeMap<(SheetId, ColId, RowId), TemplateStrip<F::StripTemplate>>,
    single_row_endpoints: BTreeMap<(SheetId, RowId), BTreeSet<WorkbookCellKey>>,
    single_col_endpoints: BTreeMap<(SheetId, ColId), BTreeSet<WorkbookCellKey>>,
    total_len: usize,
    estimated_bytes: usize,
}

impl<F: FormulaPayload> Default for FormulaStore<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: FormulaPayload> PartialEq for FormulaStore<F> {
    fn eq(&self, other: &Self) -> bool {
        self.total_len == other.total_len && self.iter().eq(other.iter())
    }
}

impl<F: FormulaPayload> From<BTreeMap<WorkbookCellKey, F>> for FormulaStore<F> {
    fn from(map: BTreeMap<WorkbookCellKey, F>) -> Self {
        let mut builder = FormulaStoreBuilder::new();
        for (k, f) in map {
            let _ = builder.insert_unique(k, f);
        }
        builder.finish()
    }
}

enum OpenColTail<F: FormulaPayload> {
    Single {
        row: RowId,
        formula: F,
        bytes: usize,
    },
    Strip {
        start_row: RowId,
        len: u32,
        template: F::StripTemplate,
        bytes: usize,
    },
}

/// Incremental builder for [`FormulaStore`] optimised for strictly ascending
/// `(SheetId, RowId, ColId)` streams such as snapshot decoding.
///
/// Tracks an open vertical tail per active column (`(SheetId, ColId)`) plus a
/// single-entry hot column cache so contiguous affine formulas coalesce into
/// [`TemplateStrip`]s with minimal map lookups, falling back transparently to
/// general insertion if an out-of-order key arrives.
pub struct FormulaStoreBuilder<F: FormulaPayload> {
    store: FormulaStore<F>,
    last_key: Option<WorkbookCellKey>,
    monotonic: bool,
    hot_col: Option<((SheetId, ColId), OpenColTail<F>)>,
    open_cols: BTreeMap<(SheetId, ColId), OpenColTail<F>>,
    open_bytes: usize,
    open_len: usize,
    r_eps: Vec<(SheetId, RowId)>,
    c_eps: Vec<(SheetId, ColId)>,
}

impl<F: FormulaPayload> Default for FormulaStoreBuilder<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: FormulaPayload> FormulaStoreBuilder<F> {
    /// Creates an empty [`FormulaStoreBuilder`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            store: FormulaStore::new(),
            last_key: None,
            monotonic: true,
            hot_col: None,
            open_cols: BTreeMap::new(),
            open_bytes: 0,
            open_len: 0,
            r_eps: Vec::new(),
            c_eps: Vec::new(),
        }
    }

    /// Total logical formula cell count buffered and committed so far.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.store.len().saturating_add(self.open_len)
    }

    /// Returns `true` when zero formulas have been inserted.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Approximate heap bytes of committed and open formulas combined.
    #[must_use]
    pub const fn estimated_bytes(&self) -> usize {
        self.store.estimated_bytes().saturating_add(self.open_bytes)
    }

    /// Attempts to extend an already-open vertical strip for `(key.0, key.2)`
    /// in place when `key` advances strictly in sorted order and matches the
    /// open strip's validated template at the next contiguous row identity.
    pub fn try_match_open_strip(&mut self, key: WorkbookCellKey, formula: &F) -> bool {
        if !self.monotonic {
            return false;
        }
        if let Some(prev) = self.last_key
            && key <= prev
        {
            return false;
        }
        let col_key = (key.0, key.2);
        let is_hot = self.hot_col.as_ref().is_some_and(|&(hk, _)| hk == col_key);
        if !is_hot {
            let Some(tail) = self.open_cols.remove(&col_key) else {
                return false;
            };
            if let Some((prev_k, prev_tail)) = self.hot_col.replace((col_key, tail)) {
                self.open_cols.insert(prev_k, prev_tail);
            }
        }
        let Some((
            _,
            OpenColTail::Strip {
                start_row,
                len,
                template,
                ..
            },
        )) = self.hot_col.as_mut()
        else {
            return false;
        };
        let Some(next_len) = len.checked_add(1) else {
            return false;
        };
        if start_row.0.checked_add(u64::from(*len)) != Some(key.1.0)
            || IdRun::new(start_row.0, next_len).is_none()
            || !formula.matches_strip_template(template, *len)
        {
            return false;
        }
        *len = next_len;
        self.open_len = self.open_len.saturating_add(1);
        self.last_key = Some(key);
        true
    }

    /// Inserts a formula whose key must be unique across the stream, coalescing
    /// contiguous vertical strips along the monotonic fast path.
    #[expect(
        clippy::result_unit_err,
        reason = "unit error acts as a lightweight duplicate-key sentinel"
    )]
    pub fn insert_unique(&mut self, key: WorkbookCellKey, formula: F) -> Result<(), ()> {
        if !self.monotonic {
            return if self.store.insert(key, formula).is_some() {
                Err(())
            } else {
                Ok(())
            };
        }

        if let Some(prev) = self.last_key {
            if key == prev {
                return Err(());
            }
            if key < prev {
                self.flush_open_tails();
                self.monotonic = false;
                return if self.store.insert(key, formula).is_some() {
                    Err(())
                } else {
                    Ok(())
                };
            }
        }

        let col_key = (key.0, key.2);
        let prev_tail = if self.hot_col.as_ref().is_some_and(|&(hk, _)| hk == col_key) {
            self.hot_col.take().map(|(_, tail)| tail)
        } else {
            if let Some((prev_k, prev_tail)) = self.hot_col.take() {
                self.open_cols.insert(prev_k, prev_tail);
            }
            self.open_cols.remove(&col_key)
        };

        let new_tail = match prev_tail {
            None => {
                let bytes = single_entry_bytes(&formula);
                self.open_bytes = self.open_bytes.saturating_add(bytes);
                self.open_len = self.open_len.saturating_add(1);
                OpenColTail::Single {
                    row: key.1,
                    formula,
                    bytes,
                }
            }
            Some(OpenColTail::Single {
                row: prev_row,
                formula: prev_f,
                bytes: prev_bytes,
            }) => {
                let contiguous = prev_row.0.checked_add(1) == Some(key.1.0)
                    && IdRun::new(prev_row.0, 2).is_some();
                if contiguous && let Some(template) = F::try_pair_strip_template(&prev_f, &formula)
                {
                    let strip_bytes = strip_template_entry_bytes::<F>(&template);
                    self.open_bytes = self
                        .open_bytes
                        .saturating_sub(prev_bytes)
                        .saturating_add(strip_bytes);
                    self.open_len = self.open_len.saturating_add(1);
                    OpenColTail::Strip {
                        start_row: prev_row,
                        len: 2,
                        template,
                        bytes: strip_bytes,
                    }
                } else {
                    self.open_bytes = self.open_bytes.saturating_sub(prev_bytes);
                    self.open_len = self.open_len.saturating_sub(1);
                    self.store.insert_single_fresh(
                        (key.0, prev_row, key.2),
                        prev_f,
                        &mut self.r_eps,
                        &mut self.c_eps,
                    );
                    let bytes = single_entry_bytes(&formula);
                    self.open_bytes = self.open_bytes.saturating_add(bytes);
                    self.open_len = self.open_len.saturating_add(1);
                    OpenColTail::Single {
                        row: key.1,
                        formula,
                        bytes,
                    }
                }
            }
            Some(OpenColTail::Strip {
                start_row,
                len,
                template,
                bytes: strip_bytes,
            }) => {
                let next_len = len.checked_add(1);
                let extends = next_len.is_some_and(|nl| {
                    start_row.0.checked_add(u64::from(len)) == Some(key.1.0)
                        && IdRun::new(start_row.0, nl).is_some()
                        && formula.matches_strip_template(&template, len)
                });
                if extends && let Some(nl) = next_len {
                    self.open_len = self.open_len.saturating_add(1);
                    OpenColTail::Strip {
                        start_row,
                        len: nl,
                        template,
                        bytes: strip_bytes,
                    }
                } else {
                    self.open_bytes = self.open_bytes.saturating_sub(strip_bytes);
                    self.open_len = self.open_len.saturating_sub(len as usize);
                    self.store.insert_strip_raw(TemplateStrip {
                        sheet: key.0,
                        col: key.2,
                        start_row,
                        len,
                        template,
                    });
                    let bytes = single_entry_bytes(&formula);
                    self.open_bytes = self.open_bytes.saturating_add(bytes);
                    self.open_len = self.open_len.saturating_add(1);
                    OpenColTail::Single {
                        row: key.1,
                        formula,
                        bytes,
                    }
                }
            }
        };

        self.hot_col = Some((col_key, new_tail));
        self.last_key = Some(key);
        Ok(())
    }

    /// Flushes all remaining buffered column tails and returns the completed [`FormulaStore`].
    #[must_use]
    pub fn finish(mut self) -> FormulaStore<F> {
        self.flush_open_tails();
        self.store
    }

    fn flush_open_tails(&mut self) {
        if let Some(((sheet, col), tail)) = self.hot_col.take() {
            self.flush_tail(sheet, col, tail);
        }
        while let Some(((sheet, col), tail)) = self.open_cols.pop_first() {
            self.flush_tail(sheet, col, tail);
        }
    }

    fn flush_tail(&mut self, sheet: SheetId, col: ColId, tail: OpenColTail<F>) {
        match tail {
            OpenColTail::Single {
                row,
                formula,
                bytes,
            } => {
                self.open_bytes = self.open_bytes.saturating_sub(bytes);
                self.open_len = self.open_len.saturating_sub(1);
                self.store.insert_single_fresh(
                    (sheet, row, col),
                    formula,
                    &mut self.r_eps,
                    &mut self.c_eps,
                );
            }
            OpenColTail::Strip {
                start_row,
                len,
                template,
                bytes,
            } => {
                self.open_bytes = self.open_bytes.saturating_sub(bytes);
                self.open_len = self.open_len.saturating_sub(len as usize);
                self.store.insert_strip_raw(TemplateStrip {
                    sheet,
                    col,
                    start_row,
                    len,
                    template,
                });
            }
        }
    }
}

impl<F: FormulaPayload> FormulaStore<F> {
    /// Creates an empty [`FormulaStore`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            singles: BTreeMap::new(),
            col_strips: BTreeMap::new(),
            single_row_endpoints: BTreeMap::new(),
            single_col_endpoints: BTreeMap::new(),
            total_len: 0,
            estimated_bytes: 0,
        }
    }

    /// Total logical formula cell count across strips and singletons.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.total_len
    }

    /// Returns `true` when zero formulas are stored.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total_len == 0
    }

    /// Approximate heap bytes owned by this store (singles plus compact strips).
    #[must_use]
    pub const fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    /// Number of unmerged singleton formulas.
    #[must_use]
    pub fn single_count(&self) -> usize {
        self.singles.len()
    }

    /// Number of compressed vertical column strips (`len >= 2` each).
    #[must_use]
    pub fn strip_count(&self) -> usize {
        self.col_strips.len()
    }

    /// Read-only view of unmerged singleton formulas.
    #[must_use]
    pub const fn singles(&self) -> &BTreeMap<WorkbookCellKey, F> {
        &self.singles
    }

    /// Read-only view of vertical column strips keyed by `(sheet, col, start_row)`.
    #[must_use]
    pub const fn col_strips(
        &self,
    ) -> &BTreeMap<(SheetId, ColId, RowId), TemplateStrip<F::StripTemplate>> {
        &self.col_strips
    }

    /// Removes all formulas and strips.
    pub fn clear(&mut self) {
        self.singles.clear();
        self.col_strips.clear();
        self.single_row_endpoints.clear();
        self.single_col_endpoints.clear();
        self.total_len = 0;
        self.estimated_bytes = 0;
    }

    /// Returns `true` if a formula exists at `key`.
    #[must_use]
    pub fn contains_key(&self, key: &WorkbookCellKey) -> bool {
        self.singles.contains_key(key) || self.get_strip_covering(*key).is_some()
    }

    /// Looks up the formula at `key`, borrowing singletons or materialising from
    /// a covering [`TemplateStrip`].
    #[must_use]
    pub fn get(&self, key: &WorkbookCellKey) -> Option<Cow<'_, F>> {
        if let Some(single) = self.singles.get(key) {
            return Some(Cow::Borrowed(single));
        }
        let (strip, offset) = self.get_strip_covering(*key)?;
        Some(Cow::Owned(F::instantiate_strip_template(
            &strip.template,
            offset,
        )))
    }

    /// Returns `(&strip, offset)` if `key` falls inside a compressed strip.
    #[must_use]
    pub fn get_strip_covering(
        &self,
        key: WorkbookCellKey,
    ) -> Option<(&TemplateStrip<F::StripTemplate>, u32)> {
        if self.col_strips.is_empty() {
            return None;
        }
        let (sheet, row, col) = key;
        let (_, strip) = self
            .col_strips
            .range((sheet, col, RowId(0))..=(sheet, col, row))
            .next_back()?;
        let offset = strip.offset_of_row(row)?;
        Some((strip, offset))
    }

    /// Returns `true` if any formula (strip or singleton) resides in `(sheet, col)`.
    #[must_use]
    pub fn col_has_any(&self, sheet: SheetId, col: ColId) -> bool {
        if self
            .col_strips
            .range((sheet, col, RowId(0))..=(sheet, col, RowId(u64::MAX)))
            .next()
            .is_some()
        {
            return true;
        }
        if self.singles.is_empty() {
            return false;
        }
        self.singles
            .range((sheet, RowId(0), ColId(0))..=(sheet, RowId(u64::MAX), ColId(u64::MAX)))
            .any(|(&(_, _, fc), _)| fc == col)
    }

    /// Inserts or replaces the formula at `key`, automatically coalescing
    /// contiguous vertical formulas with matching identity-affine templates.
    pub fn insert(&mut self, key: WorkbookCellKey, formula: F) -> Option<F> {
        let (sheet, row, col) = key;
        if let Some((strip_key, offset)) = self.covering_strip_key(key)
            && let Some(strip) = self.col_strips.get(&strip_key)
            && formula.matches_strip_template(&strip.template, offset)
        {
            return Some(F::instantiate_strip_template(&strip.template, offset));
        }

        let prev = self.remove(&key);
        let ctr = id_counter(row.0);

        // Try extending the tail of a strip at `(sheet, row - 1, col)`.
        if ctr > 0 {
            let prev_row = RowId(row.0.saturating_sub(1));
            let prev_key = (sheet, prev_row, col);
            if let Some((left_key, left_off)) = self.covering_strip_key(prev_key) {
                let can_extend = self.col_strips.get(&left_key).is_some_and(|strip| {
                    left_off.saturating_add(1) == strip.len
                        && IdRun::new(strip.start_row.0, strip.len.saturating_add(1)).is_some()
                        && formula.matches_strip_template(&strip.template, strip.len)
                });
                if can_extend {
                    self.extend_strip_by_one_raw(left_key);
                    self.try_merge_adjacent_strips(left_key, sheet, row, col);
                    return prev;
                }
            } else if let Some(prev_f) = self.singles.get(&prev_key)
                && let Some(tmpl) = F::try_pair_strip_template(prev_f, &formula)
                && IdRun::new(prev_row.0, 2).is_some()
            {
                let _ = self.remove_single_raw(&prev_key);
                let strip_key = (sheet, col, prev_row);
                self.insert_strip_raw(TemplateStrip {
                    sheet,
                    col,
                    start_row: prev_row,
                    len: 2,
                    template: tmpl,
                });
                self.try_merge_adjacent_strips(strip_key, sheet, row, col);
                return prev;
            }
        }

        // Try prepending to a strip or pairing with a singleton at `(sheet, row + 1, col)`.
        if ctr < u32::MAX {
            let next_row = RowId(row.0.saturating_add(1));
            let right_strip_key = (sheet, col, next_row);
            if let Some(right_strip) = self.col_strips.get(&right_strip_key) {
                if IdRun::new(row.0, right_strip.len.saturating_add(1)).is_some() {
                    let right_first = F::instantiate_strip_template(&right_strip.template, 0);
                    if let Some(new_tmpl) = F::try_pair_strip_template(&formula, &right_first)
                        && F::strips_can_merge(&new_tmpl, 1, &right_strip.template)
                    {
                        let right = self.remove_strip_raw(&right_strip_key);
                        if let Some(r) = right {
                            self.insert_strip_raw(TemplateStrip {
                                sheet,
                                col,
                                start_row: row,
                                len: r.len.saturating_add(1),
                                template: new_tmpl,
                            });
                            return prev;
                        }
                    }
                }
            } else {
                let next_cell_key = (sheet, next_row, col);
                if let Some(next_f) = self.singles.get(&next_cell_key)
                    && let Some(tmpl) = F::try_pair_strip_template(&formula, next_f)
                    && IdRun::new(row.0, 2).is_some()
                {
                    let _ = self.remove_single_raw(&next_cell_key);
                    self.insert_strip_raw(TemplateStrip {
                        sheet,
                        col,
                        start_row: row,
                        len: 2,
                        template: tmpl,
                    });
                    return prev;
                }
            }
        }

        self.insert_single_raw(key, formula);
        prev
    }

    /// Inserts a pre-validated contiguous vertical strip (`len >= 2`) directly,
    /// splitting/evicting any prior formulas overlapping `[start_row .. start_row + len)`
    /// in `(sheet, col)` and coalescing with adjacent neighbours when identical.
    pub fn insert_strip(
        &mut self,
        sheet: SheetId,
        start_row: RowId,
        col: ColId,
        len: u32,
        template: F::StripTemplate,
    ) {
        if len < 2 || IdRun::new(start_row.0, len).is_none() {
            for off in 0..len {
                let row = RowId(start_row.0.saturating_add(u64::from(off)));
                let _ = self.insert(
                    (sheet, row, col),
                    F::instantiate_strip_template(&template, off),
                );
            }
            return;
        }
        let end_excl = start_row.0.saturating_add(u64::from(len));
        self.clear_column_interval(sheet, col, start_row.0, end_excl);

        let key = (sheet, col, start_row);
        self.insert_strip_raw(TemplateStrip {
            sheet,
            col,
            start_row,
            len,
            template,
        });

        let last_row = RowId(end_excl.saturating_sub(1));
        self.try_merge_adjacent_strips(key, sheet, last_row, col);
        self.try_merge_with_left_neighbour(key);
    }

    /// Removes the formula at `key`, splitting any enclosing [`TemplateStrip`]
    /// into non-overlapping left/right fragments and demoting length-1 pieces.
    pub fn remove(&mut self, key: &WorkbookCellKey) -> Option<F> {
        if let Some(old) = self.remove_single_raw(key) {
            return Some(old);
        }
        let (strip_key, offset) = self.covering_strip_key(*key)?;
        let strip = self.remove_strip_raw(&strip_key)?;
        let old = F::instantiate_strip_template(&strip.template, offset);

        if offset == 1 {
            let left_f = F::instantiate_strip_template(&strip.template, 0);
            self.insert_single_raw((strip.sheet, strip.start_row, strip.col), left_f);
        } else if offset >= 2 {
            self.insert_strip_raw(TemplateStrip {
                sheet: strip.sheet,
                col: strip.col,
                start_row: strip.start_row,
                len: offset,
                template: strip.template.clone(),
            });
        }

        let r_off = offset.saturating_add(1);
        let r_len = strip.len.saturating_sub(r_off);
        if r_len == 1 {
            let r_row = RowId(strip.start_row.0.saturating_add(u64::from(r_off)));
            let right_f = F::instantiate_strip_template(&strip.template, r_off);
            self.insert_single_raw((strip.sheet, r_row, strip.col), right_f);
        } else if r_len >= 2 {
            let r_row = RowId(strip.start_row.0.saturating_add(u64::from(r_off)));
            let r_tmpl = F::shift_strip_template(&strip.template, r_off);
            self.insert_strip_raw(TemplateStrip {
                sheet: strip.sheet,
                col: strip.col,
                start_row: r_row,
                len: r_len,
                template: r_tmpl,
            });
        }

        Some(old)
    }

    /// Returns an ordered iterator over all `(WorkbookCellKey, Cow<'_, F>)`
    /// entries in ascending `(SheetId, RowId, ColId)` order.
    #[must_use]
    pub fn iter(&self) -> FormulaStoreIter<'_, F> {
        self.range(..)
    }

    /// Returns an ordered iterator over all `WorkbookCellKey`s in ascending
    /// `(SheetId, RowId, ColId)` order.
    pub fn keys(&self) -> impl Iterator<Item = WorkbookCellKey> + '_ {
        self.iter().map(|(k, _)| k)
    }

    /// Returns an ordered iterator over `(WorkbookCellKey, Cow<'_, F>)` pairs
    /// within `bounds` in strictly ascending `(SheetId, RowId, ColId)` order.
    pub fn range<R>(&self, bounds: R) -> FormulaStoreIter<'_, F>
    where
        R: RangeBounds<WorkbookCellKey>,
    {
        let start = cloned_bound(bounds.start_bound());
        let end = cloned_bound(bounds.end_bound());
        if bounds_inverted(&start, &end) {
            return FormulaStoreIter::empty();
        }

        let singles = Some(self.singles.range((start, end)).peekable());
        if self.col_strips.is_empty() {
            return FormulaStoreIter {
                singles,
                cursors: Vec::new(),
                heap: BinaryHeap::new(),
                end_bound: end,
            };
        }

        let (sheet_lo, sheet_hi) = sheet_window(&start, &end);
        let mut cursors = Vec::new();
        let mut heap = BinaryHeap::new();

        let mut probe = (sheet_lo, ColId(0), RowId(0));
        let max_probe = (sheet_hi, ColId(u64::MAX), RowId(u64::MAX));
        while probe <= max_probe {
            let Some((&(s, c, _), _)) = self.col_strips.range(probe..=max_probe).next() else {
                break;
            };
            if let Some(min_row) = min_row_for_col(s, c, &start)
                && let Some(cursor) = self.seek_col_cursor(s, c, min_row)
            {
                let first_key = cursor.current_key();
                if in_upper_bound(first_key, &end) {
                    let idx = cursors.len();
                    cursors.push(cursor);
                    heap.push(Reverse(HeapItem {
                        sheet: first_key.0,
                        row: first_key.1,
                        col: first_key.2,
                        cursor_idx: idx,
                    }));
                }
            }
            if c.0 < u64::MAX {
                probe = (s, ColId(c.0.saturating_add(1)), RowId(0));
            } else if s.0 < u32::MAX {
                probe = (SheetId(s.0.saturating_add(1)), ColId(0), RowId(0));
            } else {
                break;
            }
        }

        FormulaStoreIter {
            singles,
            cursors,
            heap,
            end_bound: end,
        }
    }

    fn covering_strip_key(&self, key: WorkbookCellKey) -> Option<((SheetId, ColId, RowId), u32)> {
        let (strip, offset) = self.get_strip_covering(key)?;
        Some(((strip.sheet, strip.col, strip.start_row), offset))
    }

    fn clear_column_interval(&mut self, sheet: SheetId, col: ColId, start_raw: u64, end_excl: u64) {
        if !self.singles.is_empty() {
            let end_incl = end_excl.saturating_sub(1);
            let single_keys: Vec<WorkbookCellKey> = self
                .singles
                .range((sheet, RowId(start_raw), col)..=(sheet, RowId(end_incl), col))
                .filter_map(|(&k @ (_, _, fc), _)| (fc == col).then_some(k))
                .collect();
            for sk in single_keys {
                let _ = self.remove_single_raw(&sk);
            }
        }

        if self.col_strips.is_empty() {
            return;
        }
        let end_incl = end_excl.saturating_sub(1);
        let mut overlapping: Vec<(SheetId, ColId, RowId)> = Vec::new();
        if let Some((&k_prev, prev_strip)) = self
            .col_strips
            .range((sheet, col, RowId(0))..=(sheet, col, RowId(start_raw)))
            .next_back()
            && id_stream(prev_strip.start_row.0) == id_stream(start_raw)
            && prev_strip.end_row_exclusive_raw() > start_raw
        {
            overlapping.push(k_prev);
        }
        for (&k, _) in self.col_strips.range((
            Bound::Excluded((sheet, col, RowId(start_raw))),
            Bound::Included((sheet, col, RowId(end_incl))),
        )) {
            overlapping.push(k);
        }

        for k in overlapping {
            let Some(old) = self.remove_strip_raw(&k) else {
                continue;
            };
            let old_end = old.end_row_exclusive_raw();
            if old.start_row.0 < start_raw {
                let left_len =
                    u32::try_from(start_raw.saturating_sub(old.start_row.0)).unwrap_or(0);
                if left_len == 1 {
                    self.insert_single_raw(
                        (old.sheet, old.start_row, old.col),
                        F::instantiate_strip_template(&old.template, 0),
                    );
                } else if left_len >= 2 {
                    self.insert_strip_raw(TemplateStrip {
                        sheet: old.sheet,
                        col: old.col,
                        start_row: old.start_row,
                        len: left_len,
                        template: old.template.clone(),
                    });
                }
            }
            if old_end > end_excl {
                let skip = u32::try_from(end_excl.saturating_sub(old.start_row.0)).unwrap_or(0);
                let right_len = u32::try_from(old_end.saturating_sub(end_excl)).unwrap_or(0);
                let right_start = RowId(end_excl);
                if right_len == 1 {
                    self.insert_single_raw(
                        (old.sheet, right_start, old.col),
                        F::instantiate_strip_template(&old.template, skip),
                    );
                } else if right_len >= 2 {
                    self.insert_strip_raw(TemplateStrip {
                        sheet: old.sheet,
                        col: old.col,
                        start_row: right_start,
                        len: right_len,
                        template: F::shift_strip_template(&old.template, skip),
                    });
                }
            }
        }
    }

    fn try_merge_adjacent_strips(
        &mut self,
        left_key: (SheetId, ColId, RowId),
        sheet: SheetId,
        last_inserted_row: RowId,
        col: ColId,
    ) {
        let next_ctr = id_counter(last_inserted_row.0);
        if next_ctr == u32::MAX {
            return;
        }
        let next_row = RowId(last_inserted_row.0.saturating_add(1));
        let right_key = (sheet, col, next_row);
        let can_merge = match (
            self.col_strips.get(&left_key),
            self.col_strips.get(&right_key),
        ) {
            (Some(left), Some(right)) => {
                left.end_row_exclusive_raw() == right.start_row.0
                    && left
                        .len
                        .checked_add(right.len)
                        .and_then(|total| IdRun::new(left.start_row.0, total))
                        .is_some()
                    && F::strips_can_merge(&left.template, left.len, &right.template)
            }
            _ => false,
        };
        if can_merge
            && let Some(right) = self.remove_strip_raw(&right_key)
            && let Some(left) = self.col_strips.get_mut(&left_key)
        {
            left.len = left.len.saturating_add(right.len);
            self.total_len = self.total_len.saturating_add(right.len as usize);
        }
    }

    fn try_merge_with_left_neighbour(&mut self, right_key: (SheetId, ColId, RowId)) {
        let Some(right_start) = self.col_strips.get(&right_key).map(|s| s.start_row) else {
            return;
        };
        if id_counter(right_start.0) == 0 {
            return;
        }
        let prev_row = RowId(right_start.0.saturating_sub(1));
        let sheet = right_key.0;
        let col = right_key.1;
        if let Some((left_key, _)) = self.covering_strip_key((sheet, prev_row, col)) {
            self.try_merge_adjacent_strips(left_key, sheet, prev_row, col);
        } else {
            let prev_cell_key = (sheet, prev_row, col);
            let can_prepend = match (
                self.singles.get(&prev_cell_key),
                self.col_strips.get(&right_key),
            ) {
                (Some(prev_f), Some(right)) => {
                    IdRun::new(prev_row.0, right.len.saturating_add(1)).is_some()
                        && F::try_pair_strip_template(
                            prev_f,
                            &F::instantiate_strip_template(&right.template, 0),
                        )
                        .filter(|t| F::strips_can_merge(t, 1, &right.template))
                        .is_some()
                }
                _ => false,
            };
            if can_prepend {
                let prev_f = self.remove_single_raw(&prev_cell_key);
                let right = self.remove_strip_raw(&right_key);
                if let (Some(pf), Some(r)) = (prev_f, right)
                    && let Some(tmpl) = F::try_pair_strip_template(
                        &pf,
                        &F::instantiate_strip_template(&r.template, 0),
                    )
                {
                    self.insert_strip_raw(TemplateStrip {
                        sheet,
                        col,
                        start_row: prev_row,
                        len: r.len.saturating_add(1),
                        template: tmpl,
                    });
                }
            }
        }
    }

    fn seek_col_cursor(
        &self,
        sheet: SheetId,
        col: ColId,
        min_row: u64,
    ) -> Option<StripCursor<'_, F::StripTemplate>> {
        if let Some((&k_prev, strip_prev)) = self
            .col_strips
            .range((sheet, col, RowId(0))..=(sheet, col, RowId(min_row)))
            .next_back()
            && let Some(off) = strip_prev.offset_of_row(RowId(min_row))
        {
            let rest = self.col_strips.range((
                Bound::Excluded(k_prev),
                Bound::Included((sheet, col, RowId(u64::MAX))),
            ));
            return Some(StripCursor {
                current_strip: strip_prev,
                offset: off,
                rest_strips: rest,
            });
        }
        let mut rest = self
            .col_strips
            .range((sheet, col, RowId(min_row))..=(sheet, col, RowId(u64::MAX)));
        let (_, first_strip) = rest.next()?;
        Some(StripCursor {
            current_strip: first_strip,
            offset: 0,
            rest_strips: rest,
        })
    }

    /// Collects all formula cell keys touching any row identity in `ids` on
    /// `sheet_id` as a range endpoint (`O(affected)` for singles and `O(strips)`).
    #[must_use]
    pub fn collect_row_endpoint_candidates(
        &self,
        sheet_id: SheetId,
        ids: &[IdRun],
    ) -> BTreeSet<WorkbookCellKey> {
        let mut out = BTreeSet::new();
        let total_moved: usize = ids
            .iter()
            .fold(0usize, |acc, r| acc.saturating_add(r.len as usize));
        let sheet_eps_bound = self
            .single_row_endpoints
            .range((sheet_id, RowId(0))..=(sheet_id, RowId(u64::MAX)))
            .take(total_moved.saturating_add(1))
            .count();
        if total_moved < sheet_eps_bound {
            for run in ids {
                for raw_id in run.iter() {
                    if let Some(keys) = self.single_row_endpoints.get(&(sheet_id, RowId(raw_id))) {
                        out.extend(keys.iter().copied());
                    }
                }
            }
        } else {
            for (&(_, r), keys) in self
                .single_row_endpoints
                .range((sheet_id, RowId(0))..=(sheet_id, RowId(u64::MAX)))
            {
                if ids.iter().any(|run| run.contains(r.0)) {
                    out.extend(keys.iter().copied());
                }
            }
        }
        for strip in self.col_strips.values() {
            F::collect_strip_row_endpoints(strip, sheet_id, ids, &mut out);
        }
        out
    }

    /// Collects all formula cell keys touching any column identity in `ids` on
    /// `sheet_id` as a range endpoint (`O(affected)` for singles and `O(strips)`).
    #[must_use]
    pub fn collect_col_endpoint_candidates(
        &self,
        sheet_id: SheetId,
        ids: &[IdRun],
    ) -> BTreeSet<WorkbookCellKey> {
        let mut out = BTreeSet::new();
        let total_moved: usize = ids
            .iter()
            .fold(0usize, |acc, r| acc.saturating_add(r.len as usize));
        let sheet_eps_bound = self
            .single_col_endpoints
            .range((sheet_id, ColId(0))..=(sheet_id, ColId(u64::MAX)))
            .take(total_moved.saturating_add(1))
            .count();
        if total_moved < sheet_eps_bound {
            for run in ids {
                for raw_id in run.iter() {
                    if let Some(keys) = self.single_col_endpoints.get(&(sheet_id, ColId(raw_id))) {
                        out.extend(keys.iter().copied());
                    }
                }
            }
        } else {
            for (&(_, c), keys) in self
                .single_col_endpoints
                .range((sheet_id, ColId(0))..=(sheet_id, ColId(u64::MAX)))
            {
                if ids.iter().any(|run| run.contains(c.0)) {
                    out.extend(keys.iter().copied());
                }
            }
        }
        for strip in self.col_strips.values() {
            F::collect_strip_col_endpoints(strip, sheet_id, ids, &mut out);
        }
        out
    }

    fn insert_single_raw(&mut self, key: WorkbookCellKey, formula: F) {
        let _ = self.remove_single_raw(&key);
        let mut r_eps = Vec::new();
        let mut c_eps = Vec::new();
        self.insert_single_fresh(key, formula, &mut r_eps, &mut c_eps);
    }

    fn insert_single_fresh(
        &mut self,
        key: WorkbookCellKey,
        formula: F,
        r_eps: &mut Vec<(SheetId, RowId)>,
        c_eps: &mut Vec<(SheetId, ColId)>,
    ) {
        r_eps.clear();
        c_eps.clear();
        formula.collect_range_endpoints(key.0, r_eps, c_eps);
        for &ep in r_eps.iter() {
            self.single_row_endpoints.entry(ep).or_default().insert(key);
        }
        for &ep in c_eps.iter() {
            self.single_col_endpoints.entry(ep).or_default().insert(key);
        }
        let added = single_entry_bytes(&formula);
        self.singles.insert(key, formula);
        self.estimated_bytes = self.estimated_bytes.saturating_add(added);
        self.total_len = self.total_len.saturating_add(1);
    }

    fn remove_single_raw(&mut self, key: &WorkbookCellKey) -> Option<F> {
        let old = self.singles.remove(key)?;
        let mut r_eps = Vec::new();
        let mut c_eps = Vec::new();
        old.collect_range_endpoints(key.0, &mut r_eps, &mut c_eps);
        for ep in r_eps {
            let mut empty = false;
            if let Some(set) = self.single_row_endpoints.get_mut(&ep) {
                set.remove(key);
                empty = set.is_empty();
            }
            if empty {
                self.single_row_endpoints.remove(&ep);
            }
        }
        for ep in c_eps {
            let mut empty = false;
            if let Some(set) = self.single_col_endpoints.get_mut(&ep) {
                set.remove(key);
                empty = set.is_empty();
            }
            if empty {
                self.single_col_endpoints.remove(&ep);
            }
        }
        self.estimated_bytes = self
            .estimated_bytes
            .saturating_sub(single_entry_bytes(&old));
        self.total_len = self.total_len.saturating_sub(1);
        Some(old)
    }

    fn insert_strip_raw(&mut self, strip: TemplateStrip<F::StripTemplate>) {
        let added = strip_entry_bytes::<F>(&strip);
        let len_usize = strip.len as usize;
        let key = (strip.sheet, strip.col, strip.start_row);
        if let Some(old) = self.col_strips.insert(key, strip) {
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_sub(strip_entry_bytes::<F>(&old));
            self.total_len = self.total_len.saturating_sub(old.len as usize);
        }
        self.estimated_bytes = self.estimated_bytes.saturating_add(added);
        self.total_len = self.total_len.saturating_add(len_usize);
    }

    fn remove_strip_raw(
        &mut self,
        key: &(SheetId, ColId, RowId),
    ) -> Option<TemplateStrip<F::StripTemplate>> {
        let old = self.col_strips.remove(key)?;
        self.estimated_bytes = self
            .estimated_bytes
            .saturating_sub(strip_entry_bytes::<F>(&old));
        self.total_len = self.total_len.saturating_sub(old.len as usize);
        Some(old)
    }

    fn extend_strip_by_one_raw(&mut self, key: (SheetId, ColId, RowId)) {
        if let Some(strip) = self.col_strips.get_mut(&key) {
            strip.len = strip.len.saturating_add(1);
            self.total_len = self.total_len.saturating_add(1);
        }
    }
}

impl<'a, F: FormulaPayload> IntoIterator for &'a FormulaStore<F> {
    type Item = (WorkbookCellKey, Cow<'a, F>);
    type IntoIter = FormulaStoreIter<'a, F>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Ordered iterator over [`FormulaStore`] entries in strictly ascending
/// `(SheetId, RowId, ColId)` order.
pub struct FormulaStoreIter<'a, F: FormulaPayload> {
    singles: Option<Peekable<BTreeRange<'a, WorkbookCellKey, F>>>,
    cursors: Vec<StripCursor<'a, F::StripTemplate>>,
    heap: BinaryHeap<Reverse<HeapItem>>,
    end_bound: Bound<WorkbookCellKey>,
}

impl<'a, F: FormulaPayload> FormulaStoreIter<'a, F> {
    const fn empty() -> Self {
        Self {
            singles: None,
            cursors: Vec::new(),
            heap: BinaryHeap::new(),
            end_bound: Bound::Unbounded,
        }
    }
}

impl<'a, F: FormulaPayload> Iterator for FormulaStoreIter<'a, F> {
    type Item = (WorkbookCellKey, Cow<'a, F>);

    fn next(&mut self) -> Option<Self::Item> {
        let single_key = self
            .singles
            .as_mut()
            .and_then(|it| it.peek().map(|&(&k, _)| k));
        let strip_key = self.heap.peek().map(|r| (r.0.sheet, r.0.row, r.0.col));

        let take_single = match (single_key, strip_key) {
            (None, None) => return None,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(sk), Some(hk)) => sk <= hk,
        };

        if take_single {
            let (k, f) = self.singles.as_mut()?.next()?;
            return Some((*k, Cow::Borrowed(f)));
        }

        let Reverse(item) = self.heap.pop()?;
        let cursor = self.cursors.get_mut(item.cursor_idx)?;
        let out_key = cursor.current_key();
        let out_formula =
            F::instantiate_strip_template(&cursor.current_strip.template, cursor.offset);

        if cursor.advance() {
            let next_key = cursor.current_key();
            if in_upper_bound(next_key, &self.end_bound) {
                self.heap.push(Reverse(HeapItem {
                    sheet: next_key.0,
                    row: next_key.1,
                    col: next_key.2,
                    cursor_idx: item.cursor_idx,
                }));
            }
        }

        Some((out_key, Cow::Owned(out_formula)))
    }
}

struct StripCursor<'a, T> {
    current_strip: &'a TemplateStrip<T>,
    offset: u32,
    rest_strips: BTreeRange<'a, (SheetId, ColId, RowId), TemplateStrip<T>>,
}

impl<T> StripCursor<'_, T> {
    const fn current_key(&self) -> WorkbookCellKey {
        (
            self.current_strip.sheet,
            RowId(
                self.current_strip
                    .start_row
                    .0
                    .saturating_add(self.offset as u64),
            ),
            self.current_strip.col,
        )
    }

    fn advance(&mut self) -> bool {
        let next_off = self.offset.saturating_add(1);
        if next_off < self.current_strip.len {
            self.offset = next_off;
            return true;
        }
        if let Some((_, next_strip)) = self.rest_strips.next() {
            self.current_strip = next_strip;
            self.offset = 0;
            return true;
        }
        false
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct HeapItem {
    sheet: SheetId,
    row: RowId,
    col: ColId,
    cursor_idx: usize,
}

fn single_entry_bytes<F: FormulaPayload>(formula: &F) -> usize {
    formula
        .estimated_bytes()
        .saturating_add(size_of::<WorkbookCellKey>().saturating_mul(2))
}

fn strip_template_entry_bytes<F: FormulaPayload>(template: &F::StripTemplate) -> usize {
    size_of::<(SheetId, ColId, RowId)>()
        .saturating_add(size_of::<TemplateStrip<F::StripTemplate>>())
        .saturating_add(F::strip_template_bytes(template))
        .saturating_add(size_of::<u64>().saturating_mul(2))
}

fn strip_entry_bytes<F: FormulaPayload>(strip: &TemplateStrip<F::StripTemplate>) -> usize {
    strip_template_entry_bytes::<F>(&strip.template)
}

const fn cloned_bound(bound: Bound<&WorkbookCellKey>) -> Bound<WorkbookCellKey> {
    match bound {
        Bound::Included(&k) => Bound::Included(k),
        Bound::Excluded(&k) => Bound::Excluded(k),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn bounds_inverted(start: &Bound<WorkbookCellKey>, end: &Bound<WorkbookCellKey>) -> bool {
    match (start, end) {
        (Bound::Included(a), Bound::Included(b)) => a > b,
        (Bound::Included(a), Bound::Excluded(b))
        | (Bound::Excluded(a), Bound::Included(b))
        | (Bound::Excluded(a), Bound::Excluded(b)) => a >= b,
        _ => false,
    }
}

const fn sheet_window(
    start: &Bound<WorkbookCellKey>,
    end: &Bound<WorkbookCellKey>,
) -> (SheetId, SheetId) {
    let lo = match start {
        Bound::Included((s, _, _)) | Bound::Excluded((s, _, _)) => *s,
        Bound::Unbounded => SheetId(0),
    };
    let hi = match end {
        Bound::Included((s, _, _)) | Bound::Excluded((s, _, _)) => *s,
        Bound::Unbounded => SheetId(u32::MAX),
    };
    (lo, hi)
}

fn min_row_for_col(sheet: SheetId, col: ColId, start: &Bound<WorkbookCellKey>) -> Option<u64> {
    match start {
        Bound::Unbounded => Some(0),
        Bound::Included((bs, br, bc)) => {
            if sheet > *bs {
                Some(0)
            } else if sheet < *bs {
                None
            } else if col >= *bc {
                Some(br.0)
            } else {
                br.0.checked_add(1)
            }
        }
        Bound::Excluded((bs, br, bc)) => {
            if sheet > *bs {
                Some(0)
            } else if sheet < *bs {
                None
            } else if col > *bc {
                Some(br.0)
            } else {
                br.0.checked_add(1)
            }
        }
    }
}

fn in_upper_bound(key: WorkbookCellKey, end: &Bound<WorkbookCellKey>) -> bool {
    match end {
        Bound::Unbounded => true,
        Bound::Included(b) => key <= *b,
        Bound::Excluded(b) => key < *b,
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::ops::Bound;

    use serde::{Deserialize, Serialize};

    use crate::digest::Digest64;
    use crate::id::{ColId, RowId, SheetId, pack_id};
    use crate::ops::{FormulaMode, FormulaPayload, OpError};
    use crate::rng::Rng;

    use super::{FormulaStore, FormulaStoreBuilder};

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct TestFormula {
        base_row: u64,
        fixed_row: u64,
        tag: u32,
        mode: FormulaMode,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestAffine {
        sliding: bool,
        base_row: u64,
        fixed_row: u64,
        tag: u32,
        mode: FormulaMode,
    }

    impl FormulaPayload for TestFormula {
        type Template = Self;
        type StripTemplate = TestAffine;

        fn estimated_bytes(&self) -> usize {
            64
        }

        fn validate(&self) -> Result<(), OpError> {
            Ok(())
        }

        fn write_digest(&self, digest: &mut Digest64) {
            digest.write_u64(self.base_row);
            digest.write_u64(self.fixed_row);
            digest.write_u32(self.tag);
        }

        fn validate_template(template: &Self::Template) -> Result<usize, OpError> {
            Ok(template.estimated_bytes())
        }

        fn expand_template(template: &Self::Template, _row: RowId, _col: ColId) -> Self {
            template.clone()
        }

        fn try_pair_strip_template(f0: &Self, f1: &Self) -> Option<Self::StripTemplate> {
            if f0.fixed_row != f1.fixed_row || f0.tag != f1.tag || f0.mode != f1.mode {
                return None;
            }
            if f0.base_row == f1.base_row {
                Some(TestAffine {
                    sliding: false,
                    base_row: f0.base_row,
                    fixed_row: f0.fixed_row,
                    tag: f0.tag,
                    mode: f0.mode,
                })
            } else if crate::id::id_stream(f0.base_row) == crate::id::id_stream(f1.base_row)
                && f1.base_row == f0.base_row.saturating_add(1)
            {
                Some(TestAffine {
                    sliding: true,
                    base_row: f0.base_row,
                    fixed_row: f0.fixed_row,
                    tag: f0.tag,
                    mode: f0.mode,
                })
            } else {
                None
            }
        }

        fn matches_strip_template(&self, template: &Self::StripTemplate, offset: u32) -> bool {
            let expected = Self::instantiate_strip_template(template, offset);
            self == &expected
        }

        fn instantiate_strip_template(template: &Self::StripTemplate, offset: u32) -> Self {
            let base_row = if template.sliding {
                template.base_row.saturating_add(u64::from(offset))
            } else {
                template.base_row
            };
            Self {
                base_row,
                fixed_row: template.fixed_row,
                tag: template.tag,
                mode: template.mode,
            }
        }

        fn shift_strip_template(
            template: &Self::StripTemplate,
            offset: u32,
        ) -> Self::StripTemplate {
            let mut next = template.clone();
            if next.sliding {
                next.base_row = next.base_row.saturating_add(u64::from(offset));
            }
            next
        }

        fn strips_can_merge(
            left: &Self::StripTemplate,
            left_len: u32,
            right: &Self::StripTemplate,
        ) -> bool {
            left.sliding == right.sliding
                && left.fixed_row == right.fixed_row
                && left.tag == right.tag
                && left.mode == right.mode
                && if left.sliding {
                    left.base_row.saturating_add(u64::from(left_len)) == right.base_row
                        && crate::id::id_stream(left.base_row)
                            == crate::id::id_stream(right.base_row)
                } else {
                    left.base_row == right.base_row
                }
        }

        fn strip_template_bytes(_template: &Self::StripTemplate) -> usize {
            64
        }
    }

    #[test]
    fn coalescing_splitting_merging_and_kway_order_match_btree() {
        let mut store = FormulaStore::<TestFormula>::new();
        let mut model = BTreeMap::new();
        let s0 = SheetId(0);
        let s1 = SheetId(1);

        // Insert out of order, bridge gaps, overwrite middle, insert on two sheets & two cols.
        let mut rng = Rng::from_seed(0x5555_1234_9876_CAFE);
        let sheets = [s0, s1];
        let cols = [ColId(0), ColId(2), ColId(5)];
        for step in 0..800_u32 {
            let s =
                sheets[usize::try_from(rng.next_u64().checked_rem(2).unwrap_or(0)).unwrap_or(0)];
            let c = cols[usize::try_from(rng.next_u64().checked_rem(3).unwrap_or(0)).unwrap_or(0)];
            let r_raw = rng.next_u64().checked_rem(40).unwrap_or(0);
            let r = RowId(r_raw);
            if rng.next_u64().is_multiple_of(5) {
                assert_eq!(store.remove(&(s, r, c)), model.remove(&(s, r, c)));
            } else {
                let tag = u32::try_from(rng.next_u64().checked_rem(2).unwrap_or(0)).unwrap_or(0);
                let f = TestFormula {
                    base_row: r_raw.saturating_add(1),
                    fixed_row: 99,
                    tag,
                    mode: FormulaMode::Dynamic,
                };
                assert_eq!(
                    store.insert((s, r, c), f.clone()),
                    model.insert((s, r, c), f)
                );
            }
            if step.is_multiple_of(40) || step == 799 {
                assert_eq!(store.len(), model.len());
                let store_vec: Vec<_> = store.iter().map(|(k, f)| (k, f.into_owned())).collect();
                let model_vec: Vec<_> = model.iter().map(|(&k, f)| (k, f.clone())).collect();
                assert_eq!(store_vec, model_vec);
            }
        }

        // Check excluded/included range queries match BTreeMap across boundary shapes.
        let k_lo = (s0, RowId(7), ColId(2));
        let k_hi = (s1, RowId(12), ColId(2));
        let r_store: Vec<_> = store
            .range((Bound::Excluded(k_lo), Bound::Included(k_hi)))
            .map(|(k, f)| (k, f.into_owned()))
            .collect();
        let r_model: Vec<_> = model
            .range((Bound::Excluded(k_lo), Bound::Included(k_hi)))
            .map(|(&k, f)| (k, f.clone()))
            .collect();
        assert_eq!(r_store, r_model);

        // Inverted / empty ranges must never panic and must yield empty.
        assert_eq!(
            store
                .range((Bound::Excluded(k_hi), Bound::Included(k_lo)))
                .count(),
            0
        );
        assert_eq!(
            store
                .range((Bound::Excluded(k_lo), Bound::Included(k_lo)))
                .count(),
            0
        );

        // Bridge gap forward + backward across `insert_strip`.
        store.insert_strip(
            s0,
            RowId(100),
            ColId(0),
            100,
            TestAffine {
                sliding: true,
                base_row: 100,
                fixed_row: 1,
                tag: 0,
                mode: FormulaMode::Dynamic,
            },
        );
        let _ = store.remove(&(s0, RowId(150), ColId(0)));
        assert_eq!(
            store.get(&(s0, RowId(149), ColId(0))).as_deref(),
            Some(&TestFormula {
                base_row: 149,
                fixed_row: 1,
                tag: 0,
                mode: FormulaMode::Dynamic,
            })
        );
        assert!(store.get(&(s0, RowId(150), ColId(0))).is_none());
        assert_eq!(
            store.get(&(s0, RowId(151), ColId(0))).as_deref(),
            Some(&TestFormula {
                base_row: 151,
                fixed_row: 1,
                tag: 0,
                mode: FormulaMode::Dynamic,
            })
        );
        let _ = (format!("{:?}", String::new()), pack_id(0, 1, 0));
    }

    #[test]
    fn builder_matches_sequential_insert_across_strips_interleaving_fallback_and_dups() {
        let s0 = SheetId(0);

        // 1. 1,000-row single-column strip via try_match_open_strip + insert_unique.
        let mut b1 = FormulaStoreBuilder::new();
        let mut ref1 = FormulaStore::new();
        for r in 0_u64..1_000 {
            let key = (s0, RowId(r), ColId(3));
            let f = TestFormula {
                base_row: r,
                fixed_row: 7,
                tag: 1,
                mode: FormulaMode::Dynamic,
            };
            if !b1.try_match_open_strip(key, &f) {
                b1.insert_unique(key, f.clone()).unwrap();
            }
            ref1.insert(key, f);
            assert_eq!(b1.len(), ref1.len());
            assert_eq!(b1.estimated_bytes(), ref1.estimated_bytes());
        }
        let built1 = b1.finish();
        assert_eq!(built1.strip_count(), 1);
        assert_eq!(built1.single_count(), 0);
        assert_eq!(built1, ref1);
        assert_eq!(built1.estimated_bytes(), ref1.estimated_bytes());

        // 2. Row-major interleaved multi-column strips + singletons + strip breaks.
        let mut b2 = FormulaStoreBuilder::new();
        let mut ref2 = FormulaStore::new();
        for r in 0_u64..40 {
            for c in [1_u64, 4_u64, 9_u64] {
                if c == 4 && r == 17 {
                    continue; // row gap splits column 4 strip
                }
                let tag = u32::from(c == 9 && (20..25).contains(&r));
                let key = (s0, RowId(r), ColId(c));
                let f = TestFormula {
                    base_row: r.saturating_add(c),
                    fixed_row: 99,
                    tag,
                    mode: FormulaMode::Dynamic,
                };
                if !b2.try_match_open_strip(key, &f) {
                    b2.insert_unique(key, f.clone()).unwrap();
                }
                ref2.insert(key, f);
                assert_eq!(b2.len(), ref2.len());
                assert_eq!(b2.estimated_bytes(), ref2.estimated_bytes());
            }
        }
        let built2 = b2.finish();
        assert_eq!(built2, ref2);
        assert_eq!(built2.single_count(), ref2.single_count());
        assert_eq!(built2.strip_count(), ref2.strip_count());

        // 3. Duplicate key rejection.
        let mut b3 = FormulaStoreBuilder::new();
        let k_dup = (s0, RowId(5), ColId(2));
        let f_dup = TestFormula {
            base_row: 5,
            fixed_row: 1,
            tag: 0,
            mode: FormulaMode::Dynamic,
        };
        assert!(b3.insert_unique(k_dup, f_dup.clone()).is_ok());
        assert_eq!(b3.insert_unique(k_dup, f_dup.clone()), Err(()));

        // 4. Out-of-order key fallback + duplicate rejection in fallback mode.
        let k_after = (s0, RowId(6), ColId(2));
        let k_earlier = (s0, RowId(2), ColId(0));
        assert!(!b3.try_match_open_strip(k_after, &f_dup));
        assert!(b3.insert_unique(k_after, f_dup.clone()).is_ok());
        assert!(b3.insert_unique(k_earlier, f_dup.clone()).is_ok());
        assert!(!b3.try_match_open_strip((s0, RowId(7), ColId(2)), &f_dup));
        assert_eq!(b3.insert_unique(k_earlier, f_dup.clone()), Err(()));
        let mut ref3 = FormulaStore::new();
        ref3.insert(k_dup, f_dup.clone());
        ref3.insert(k_after, f_dup.clone());
        ref3.insert(k_earlier, f_dup);
        let built3 = b3.finish();
        assert_eq!(built3, ref3);
        assert_eq!(built3.len(), 3);
    }
}
