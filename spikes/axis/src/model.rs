//! A small, slow, obviously-correct model of one sheet: rows only, a handful
//! of columns, three kinds of cell content. It exists to pin down semantics.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The stable identity of a row.
pub type RowId = u64;
/// A column, by index. Columns are not edited in this prototype.
pub type Col = u8;

/// A row identity: the replica that created the row, and that replica's counter.
/// Replica 0 is the sheet's initial grid.
#[must_use]
pub const fn row_id(replica: u32, counter: u64) -> RowId {
    ((replica as u64) << 40) | counter
}

/// What a cell holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Number(f64),
    /// A reference to one cell, like `=A5`.
    Cell {
        row: RowId,
        col: Col,
    },
    /// A sum over a range of one column, like `=SUM(A5:A9)`, bound to the rows
    /// that were its first and last when it was written.
    Sum {
        col: Col,
        first: RowId,
        last: RowId,
    },
    /// A reference that was pushed off the sheet.
    RefError,
}

/// One entry of the row axis. A deleted row stays on the axis as a tombstone;
/// the count is how many deletions are in effect, so that two users deleting
/// the same row and one of them undoing leaves it deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AxisRow {
    pub id: RowId,
    pub deleted: u32,
}

/// The value of a cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Empty,
    Number(f64),
    RefError,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sheet {
    /// Every row ever created, in axis order, tombstones included.
    pub rows: Vec<AxisRow>,
    pub cells: BTreeMap<(RowId, Col), Content>,
    /// How [`Op::Move`] treats a range whose first or last row is moved.
    pub hand_off_anchors: bool,
}

/// A change to a sheet. Every target is an explicit identity.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Set {
        row: RowId,
        col: Col,
        content: Option<Content>,
    },
    /// Writes only if the cell still holds `expected`. Undo in a shared sheet.
    SetIf {
        row: RowId,
        col: Col,
        expected: Option<Content>,
        content: Option<Content>,
    },
    /// Puts new rows directly after `after`, or at the top.
    Insert {
        after: Option<RowId>,
        ids: Vec<RowId>,
    },
    Delete {
        ids: Vec<RowId>,
    },
    /// Takes back one deletion of each row.
    Undelete {
        ids: Vec<RowId>,
    },
    /// Moves rows, keeping their relative order, to directly after `after`.
    Move {
        ids: Vec<RowId>,
        after: Option<RowId>,
    },
    /// Sorting: `rows[i]` receives the content that `sources[i]` held.
    Permute {
        rows: Vec<RowId>,
        sources: Vec<RowId>,
    },
}

impl Sheet {
    /// A sheet with `count` initial rows, identified as (replica 0, 1..=count).
    #[must_use]
    pub fn with_rows(count: u64) -> Self {
        Self {
            rows: (1..=count)
                .map(|counter| AxisRow {
                    id: row_id(0, counter),
                    deleted: 0,
                })
                .collect(),
            cells: BTreeMap::new(),
            hand_off_anchors: true,
        }
    }

    fn axis_index(&self, id: RowId) -> Option<usize> {
        self.rows.iter().position(|row| row.id == id)
    }

    /// The identities of the visible rows, top to bottom.
    #[must_use]
    pub fn live_rows(&self) -> Vec<RowId> {
        self.rows
            .iter()
            .filter(|row| row.deleted == 0)
            .map(|row| row.id)
            .collect()
    }

    /// The 1-based position of a visible row, as a user would number it.
    #[must_use]
    pub fn position(&self, id: RowId) -> Option<usize> {
        self.live_rows()
            .iter()
            .position(|live| *live == id)
            .map(|index| index + 1)
    }

    /// The identity of the visible row at a 1-based position.
    #[must_use]
    pub fn row_at(&self, position: usize) -> Option<RowId> {
        position
            .checked_sub(1)
            .and_then(|index| self.live_rows().get(index).copied())
    }

    /// The visible rows a range covers: everything between its two anchors,
    /// whichever of them comes first.
    #[must_use]
    pub fn range_rows(&self, first: RowId, last: RowId) -> Vec<RowId> {
        let (Some(a), Some(b)) = (self.axis_index(first), self.axis_index(last)) else {
            return Vec::new();
        };
        let (low, high) = if a <= b { (a, b) } else { (b, a) };
        self.rows[low..=high]
            .iter()
            .filter(|row| row.deleted == 0)
            .map(|row| row.id)
            .collect()
    }

    #[must_use]
    pub fn value(&self, row: RowId, col: Col) -> Value {
        self.value_at_depth(row, col, 0)
    }

    fn value_at_depth(&self, row: RowId, col: Col, depth: u32) -> Value {
        if depth > 64 {
            return Value::RefError;
        }
        match self.cells.get(&(row, col)) {
            None => Value::Empty,
            Some(Content::Number(number)) => Value::Number(*number),
            Some(Content::RefError) => Value::RefError,
            Some(Content::Cell {
                row: target,
                col: target_col,
            }) => {
                if self.position(*target).is_none() {
                    return Value::RefError;
                }
                match self.value_at_depth(*target, *target_col, depth + 1) {
                    Value::Empty => Value::Number(0.0),
                    other => other,
                }
            }
            Some(Content::Sum {
                col: target_col,
                first,
                last,
            }) => {
                let rows = self.range_rows(*first, *last);
                if rows.is_empty() {
                    return Value::RefError;
                }
                let mut sum = 0.0;
                for target in rows {
                    match self.value_at_depth(target, *target_col, depth + 1) {
                        Value::Number(number) => sum += number,
                        Value::Empty => {}
                        Value::RefError => return Value::RefError,
                    }
                }
                Value::Number(sum)
            }
        }
    }

    /// The formula as a user would read it, with identities turned back into
    /// the positions they have right now.
    #[must_use]
    pub fn formula_text(&self, row: RowId, col: Col) -> String {
        let letter = |col: Col| char::from(b'A' + col);
        match self.cells.get(&(row, col)) {
            None => String::new(),
            Some(Content::Number(number)) => number.to_string(),
            Some(Content::RefError) => "=#REF!".to_owned(),
            Some(Content::Cell {
                row: target,
                col: target_col,
            }) => match self.position(*target) {
                Some(position) => format!("={}{position}", letter(*target_col)),
                None => "=#REF!".to_owned(),
            },
            Some(Content::Sum {
                col: target_col,
                first,
                last,
            }) => {
                let rows = self.range_rows(*first, *last);
                match (
                    rows.first().and_then(|id| self.position(*id)),
                    rows.last().and_then(|id| self.position(*id)),
                ) {
                    (Some(from), Some(to)) => {
                        format!("=SUM({0}{from}:{0}{to})", letter(*target_col))
                    }
                    _ => "=SUM(#REF!)".to_owned(),
                }
            }
        }
    }

    /// The values of one column down the visible rows, for reading a test.
    #[must_use]
    pub fn column(&self, col: Col) -> String {
        let mut text = String::new();
        for row in self.live_rows() {
            let _ = match self.value(row, col) {
                Value::Empty => write!(text, "· "),
                Value::Number(number) => write!(text, "{number} "),
                Value::RefError => write!(text, "#REF! "),
            };
        }
        text.trim_end().to_owned()
    }

    /// Applies an operation. An operation that names an unknown row, or
    /// re-uses an identity, is rejected and changes nothing.
    pub fn apply(&mut self, op: &Op) -> Result<(), &'static str> {
        match op {
            Op::Set { row, col, content } => {
                self.axis_index(*row).ok_or("unknown row")?;
                self.write(*row, *col, content.clone());
            }
            Op::SetIf {
                row,
                col,
                expected,
                content,
            } => {
                self.axis_index(*row).ok_or("unknown row")?;
                if self.cells.get(&(*row, *col)) == expected.as_ref() {
                    self.write(*row, *col, content.clone());
                }
            }
            Op::Insert { after, ids } => {
                if ids.iter().any(|id| self.axis_index(*id).is_some()) {
                    return Err("row identity already in use");
                }
                let at = match after {
                    Some(anchor) => self.axis_index(*anchor).ok_or("unknown anchor")? + 1,
                    None => 0,
                };
                let new_rows = ids.iter().map(|id| AxisRow {
                    id: *id,
                    deleted: 0,
                });
                self.rows.splice(at..at, new_rows);
            }
            Op::Delete { ids } => {
                let indexes = self.indexes_of(ids)?;
                for index in indexes {
                    self.rows[index].deleted += 1;
                }
            }
            Op::Undelete { ids } => {
                let indexes = self.indexes_of(ids)?;
                for index in indexes {
                    self.rows[index].deleted = self.rows[index].deleted.saturating_sub(1);
                }
            }
            Op::Move { ids, after } => self.move_rows(ids, *after)?,
            Op::Permute { rows, sources } => self.permute(rows, sources)?,
        }
        Ok(())
    }

    fn write(&mut self, row: RowId, col: Col, content: Option<Content>) {
        match content {
            Some(content) => {
                self.cells.insert((row, col), content);
            }
            None => {
                self.cells.remove(&(row, col));
            }
        }
    }

    fn indexes_of(&self, ids: &[RowId]) -> Result<Vec<usize>, &'static str> {
        ids.iter()
            .map(|id| self.axis_index(*id).ok_or("unknown row"))
            .collect()
    }

    fn move_rows(&mut self, ids: &[RowId], after: Option<RowId>) -> Result<(), &'static str> {
        self.indexes_of(ids)?;
        if let Some(anchor) = after {
            self.axis_index(anchor).ok_or("unknown anchor")?;
            if ids.contains(&anchor) {
                return Err("rows cannot be moved to after themselves");
            }
        }
        if self.hand_off_anchors {
            self.hand_off(ids, after);
        }
        let moved: Vec<AxisRow> = self
            .rows
            .iter()
            .filter(|row| ids.contains(&row.id))
            .cloned()
            .collect();
        self.rows.retain(|row| !ids.contains(&row.id));
        let at = match after {
            Some(anchor) => self.axis_index(anchor).ok_or("unknown anchor")? + 1,
            None => 0,
        };
        self.rows.splice(at..at, moved);
        Ok(())
    }

    /// Before rows move: a range that is about to lose exactly one of its two
    /// anchor rows, in a direction other than outward, hands that anchor to
    /// the neighbouring row that stays. Without this a range would follow its
    /// first or last row wherever it goes, and dragging the last row of a
    /// summed range up by two would silently drop two rows from the sum.
    ///
    /// "Outward" is the last row moving further down or the first row moving
    /// further up; then the range stretches to follow it. This is what Excel
    /// was recorded to do.
    fn hand_off(&mut self, moved: &[RowId], after: Option<RowId>) {
        let destination = after.and_then(|anchor| self.axis_index(anchor));
        let mut rewrites = Vec::new();
        for (key, content) in &self.cells {
            let Content::Sum { col, first, last } = content else {
                continue;
            };
            let (Some(a), Some(b)) = (self.axis_index(*first), self.axis_index(*last)) else {
                continue;
            };
            // Normalise so that `top` is the anchor nearer the top of the sheet.
            let ((top, top_at), (bottom, bottom_at)) = if a <= b {
                ((*first, a), (*last, b))
            } else {
                ((*last, b), (*first, a))
            };
            let (top_moves, bottom_moves) = (moved.contains(&top), moved.contains(&bottom));
            if top_moves == bottom_moves {
                continue; // neither anchor moves, or the range moves as a whole
            }
            let staying = |index: &usize| !moved.contains(&self.rows[*index].id);
            let (mut new_top, mut new_bottom) = (top, bottom);
            if bottom_moves {
                let outward = destination.is_some_and(|at| at >= bottom_at);
                if !outward && let Some(index) = (top_at..bottom_at).rev().find(staying) {
                    new_bottom = self.rows[index].id;
                }
            } else {
                let outward = destination.is_none_or(|at| at < top_at);
                if !outward && let Some(index) = (top_at + 1..=bottom_at).find(staying) {
                    new_top = self.rows[index].id;
                }
            }
            if (new_top, new_bottom) != (top, bottom) {
                rewrites.push((
                    *key,
                    Content::Sum {
                        col: *col,
                        first: new_top,
                        last: new_bottom,
                    },
                ));
            }
        }
        for (key, content) in rewrites {
            self.cells.insert(key, content);
        }
    }

    /// Sorting moves content, not rows: afterwards `rows[i]` holds what
    /// `sources[i]` held. A cell reference inside moved content keeps its
    /// distance in rows, as it does in Excel; one that would leave the sheet
    /// becomes a reference error.
    fn permute(&mut self, rows: &[RowId], sources: &[RowId]) -> Result<(), &'static str> {
        if rows.len() != sources.len() {
            return Err("a permutation needs as many sources as rows");
        }
        let mut sorted_rows = rows.to_vec();
        let mut sorted_sources = sources.to_vec();
        sorted_rows.sort_unstable();
        sorted_sources.sort_unstable();
        sorted_rows.dedup();
        if sorted_rows.len() != rows.len() || sorted_rows != sorted_sources {
            return Err("not a permutation");
        }
        self.indexes_of(rows)?;

        let columns: Vec<Col> = {
            let mut columns: Vec<Col> = self
                .cells
                .keys()
                .filter(|(row, _)| rows.contains(row))
                .map(|(_, col)| *col)
                .collect();
            columns.sort_unstable();
            columns.dedup();
            columns
        };
        let mut moved = Vec::new();
        for (row, source) in rows.iter().zip(sources) {
            let shift = self
                .position(*row)
                .zip(self.position(*source))
                .map(|(to, from)| to as i64 - from as i64);
            for col in &columns {
                let content = self.cells.get(&(*source, *col)).cloned().map(|content| {
                    match (content, shift) {
                        (
                            Content::Cell {
                                row: target,
                                col: target_col,
                            },
                            Some(shift),
                        ) => self
                            .position(target)
                            .and_then(|position| usize::try_from(position as i64 + shift).ok())
                            .and_then(|position| self.row_at(position))
                            .map_or(Content::RefError, |row| Content::Cell {
                                row,
                                col: target_col,
                            }),
                        (content, _) => content,
                    }
                });
                moved.push(((*row, *col), content));
            }
        }
        for ((row, col), content) in moved {
            self.write(row, col, content);
        }
        Ok(())
    }
}
