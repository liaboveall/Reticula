//! What identity addressing costs.
//!
//! A sheet of one million rows whose cells are stored by row identity, next
//! to a baseline that stores them by position. The row axis is the structure
//! planned for the first engine version: runs of consecutive identities with
//! prefix sums, rebuilt on a structural edit.
//!
//! The functions here are the workloads; `tests/cost.rs` times them natively
//! and `time-chromium.mjs` times the same code compiled to WebAssembly.

pub type RowId = u64;

const REPLICA_SHIFT: u32 = 40;
const COUNTER_MASK: u64 = (1 << REPLICA_SHIFT) - 1;
const CHUNK: usize = 4096;

const fn row_id(replica: u64, counter: u64) -> RowId {
    (replica << REPLICA_SHIFT) | counter
}

/// A deterministic stream of numbers for picking rows.
pub struct Picker(u64);

impl Picker {
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn draw(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, bound: u32) -> u32 {
        (((self.draw() >> 32) * u64::from(bound)) >> 32) as u32
    }
}

#[derive(Clone, Copy)]
struct Run {
    first: RowId,
    len: u32,
}

/// The order of the rows: runs of consecutive identities.
pub struct Axis {
    runs: Vec<Run>,
    /// `starts[i]` is the position of the first row of run `i`.
    starts: Vec<u32>,
    /// Runs sorted by first identity, to find the run an identity is in.
    by_id: Vec<(RowId, u32)>,
    len: u32,
}

impl Axis {
    fn from_runs(runs: Vec<Run>) -> Self {
        let mut starts = Vec::with_capacity(runs.len());
        let mut by_id = Vec::with_capacity(runs.len());
        let mut len = 0;
        for (index, run) in runs.iter().enumerate() {
            starts.push(len);
            by_id.push((run.first, index as u32));
            len += run.len;
        }
        by_id.sort_unstable_by_key(|(first, _)| *first);
        Self {
            runs,
            starts,
            by_id,
            len,
        }
    }

    #[must_use]
    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// Bytes held by the axis.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.runs.len() * (size_of::<Run>() + size_of::<u32>() + size_of::<(RowId, u32)>())
    }

    #[must_use]
    pub fn id_at(&self, position: u32) -> RowId {
        let run = self.starts.partition_point(|start| *start <= position) - 1;
        self.runs[run].first + u64::from(position - self.starts[run])
    }

    #[must_use]
    pub fn position_of(&self, id: RowId) -> u32 {
        let entry = self.by_id.partition_point(|(first, _)| *first <= id) - 1;
        let run = self.by_id[entry].1 as usize;
        self.starts[run] + (id - self.runs[run].first) as u32
    }

    /// Inserts one new row before `position` and rebuilds the index, as the
    /// first engine version will on every structural edit.
    pub fn insert(&mut self, position: u32, id: RowId) {
        let run = self.starts.partition_point(|start| *start <= position) - 1;
        let offset = position - self.starts[run];
        let old = self.runs[run];
        let mut runs = Vec::with_capacity(self.runs.len() + 2);
        runs.extend_from_slice(&self.runs[..run]);
        if offset > 0 {
            runs.push(Run {
                first: old.first,
                len: offset,
            });
        }
        runs.push(Run { first: id, len: 1 });
        runs.push(Run {
            first: old.first + u64::from(offset),
            len: old.len - offset,
        });
        runs.extend_from_slice(&self.runs[run + 1..]);
        *self = Self::from_runs(runs);
    }
}

/// One column, stored by slot in fixed-size chunks.
pub struct Column {
    chunks: Vec<Box<[f64; CHUNK]>>,
}

impl Column {
    fn new(slots: usize, value: impl Fn(usize) -> f64) -> Self {
        let chunks = (0..slots.div_ceil(CHUNK))
            .map(|chunk| {
                let mut data = Box::new([0.0; CHUNK]);
                for (offset, cell) in data.iter_mut().enumerate() {
                    *cell = value(chunk * CHUNK + offset);
                }
                data
            })
            .collect();
        Self { chunks }
    }

    #[inline]
    fn get(&self, slot: u32) -> f64 {
        self.chunks[slot as usize / CHUNK][slot as usize % CHUNK]
    }

    #[inline]
    fn set(&mut self, slot: u32, value: f64) {
        self.chunks[slot as usize / CHUNK][slot as usize % CHUNK] = value;
    }
}

/// A node of a bound formula: references are to row identities.
#[derive(Clone, Copy)]
enum Node {
    Cell { row: RowId, col: u8 },
    Mul(u32, u32),
    Add(u32, u32),
}

struct Formula {
    row: RowId,
    root: u32,
}

/// How the rows have been edited before measuring.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// As created: one run.
    Fresh,
    /// After 1,000 single-row inserts at random positions.
    Edited,
    /// The worst case: every row in a random place, each its own run.
    Shuffled,
}

pub const ROWS: u32 = 1_000_000;
const COLUMNS: usize = 5; // A, B, C hold data; D takes results; E1 is the shared operand
const SHARED: (RowId, u8) = (row_id(0, 1), 4);

/// The identity-addressed sheet.
pub struct Sheet {
    pub axis: Axis,
    /// Slot of each row created after the initial grid, by its counter.
    inserted: Vec<u32>,
    columns: Vec<Column>,
    nodes: Vec<Node>,
    formulas: Vec<Formula>,
    /// Formulas that read the shared operand.
    dependents: Vec<u32>,
    dirty: Vec<bool>,
}

impl Sheet {
    #[must_use]
    pub fn new(shape: Shape) -> Self {
        let mut picker = Picker::new(0x5EED);
        let mut axis = Axis::from_runs(vec![Run {
            first: row_id(0, 1),
            len: ROWS,
        }]);
        let mut inserted = Vec::new();
        match shape {
            Shape::Fresh => {}
            Shape::Edited => {
                for counter in 1..=1000 {
                    let position = picker.below(axis.len);
                    inserted.push(ROWS + counter - 1);
                    axis.insert(position, row_id(1, u64::from(counter)));
                }
            }
            Shape::Shuffled => {
                let mut order: Vec<u32> = (1..=ROWS).collect();
                for index in (1..order.len()).rev() {
                    order.swap(index, picker.below(index as u32 + 1) as usize);
                }
                let runs = order
                    .iter()
                    .map(|counter| Run {
                        first: row_id(0, u64::from(*counter)),
                        len: 1,
                    })
                    .collect();
                axis = Axis::from_runs(runs);
            }
        }
        let slots = ROWS as usize + inserted.len();
        let columns = (0..COLUMNS)
            .map(|col| Column::new(slots, |slot| (slot % 1000) as f64 + col as f64))
            .collect();
        Self {
            axis,
            inserted,
            columns,
            nodes: Vec::new(),
            formulas: Vec::new(),
            dependents: Vec::new(),
            dirty: Vec::new(),
        }
    }

    /// Where a row's cells are stored. Rows of the initial grid are stored in
    /// order; later rows are appended, so nothing moves when a row is inserted.
    #[inline]
    fn slot(&self, id: RowId) -> u32 {
        let counter = (id & COUNTER_MASK) as u32;
        if id >> REPLICA_SHIFT == 0 {
            counter - 1
        } else {
            self.inserted[counter as usize - 1]
        }
    }

    #[inline]
    fn read(&self, id: RowId, col: u8) -> f64 {
        self.columns[col as usize].get(self.slot(id))
    }

    fn push(&mut self, node: Node) -> u32 {
        self.nodes.push(node);
        self.nodes.len() as u32 - 1
    }

    /// Fills column D of the first `count` rows with `=A<row> * E1`.
    pub fn fill_scaled(&mut self, count: u32) {
        self.clear_formulas();
        for position in 0..count {
            let row = self.axis.id_at(position);
            let left = self.push(Node::Cell { row, col: 0 });
            let right = self.push(Node::Cell {
                row: SHARED.0,
                col: SHARED.1,
            });
            let root = self.push(Node::Mul(left, right));
            self.dependents.push(self.formulas.len() as u32);
            self.formulas.push(Formula { row, root });
        }
        self.dirty = vec![false; self.formulas.len()];
    }

    /// Fills column D of every row with `=B<row> * C<row> + E1`.
    pub fn fill_product(&mut self) {
        self.clear_formulas();
        for position in 0..self.axis.len {
            let row = self.axis.id_at(position);
            let b = self.push(Node::Cell { row, col: 1 });
            let c = self.push(Node::Cell { row, col: 2 });
            let product = self.push(Node::Mul(b, c));
            let shared = self.push(Node::Cell {
                row: SHARED.0,
                col: SHARED.1,
            });
            let root = self.push(Node::Add(product, shared));
            self.dependents.push(self.formulas.len() as u32);
            self.formulas.push(Formula { row, root });
        }
        self.dirty = vec![false; self.formulas.len()];
    }

    fn clear_formulas(&mut self) {
        self.nodes.clear();
        self.formulas.clear();
        self.dependents.clear();
    }

    fn evaluate(&self, node: u32) -> f64 {
        match self.nodes[node as usize] {
            Node::Cell { row, col } => self.read(row, col),
            Node::Mul(left, right) => self.evaluate(left) * self.evaluate(right),
            Node::Add(left, right) => self.evaluate(left) + self.evaluate(right),
        }
    }

    /// Changes the shared operand, marks every formula that reads it, and
    /// recalculates the marked ones. Returns a checksum.
    pub fn recalculate(&mut self, operand: f64) -> f64 {
        let slot = self.slot(SHARED.0);
        self.columns[SHARED.1 as usize].set(slot, operand);
        for index in 0..self.dependents.len() {
            let formula = self.dependents[index] as usize;
            self.dirty[formula] = true;
        }
        let mut sum = 0.0;
        for index in 0..self.formulas.len() {
            if !self.dirty[index] {
                continue;
            }
            self.dirty[index] = false;
            let value = self.evaluate(self.formulas[index].root);
            let slot = self.slot(self.formulas[index].row);
            self.columns[3].set(slot, value);
            sum += value;
        }
        sum
    }

    /// Reads column A at `count` random positions.
    pub fn read_at_random_positions(&self, count: u32) -> f64 {
        let mut picker = Picker::new(1);
        (0..count)
            .map(|_| self.read(self.axis.id_at(picker.below(self.axis.len)), 0))
            .sum()
    }

    /// Reads column A at `count` positions in order, one lookup per row, as a
    /// naive viewport fill would.
    pub fn read_at_consecutive_positions(&self, count: u32) -> f64 {
        (0..count)
            .map(|index| self.read(self.axis.id_at(index % self.axis.len), 0))
            .sum()
    }

    /// Reads column A through `count` row identities, as formula evaluation does.
    pub fn read_by_identity(&self, ids: &[RowId]) -> f64 {
        ids.iter().map(|id| self.read(*id, 0)).sum()
    }

    /// Writes column D at `count` random positions.
    pub fn write_at_random_positions(&mut self, count: u32) -> f64 {
        let mut picker = Picker::new(2);
        for index in 0..count {
            let slot = self.slot(self.axis.id_at(picker.below(self.axis.len)));
            self.columns[3].set(slot, f64::from(index));
        }
        f64::from(count)
    }

    /// Sums all of column A in row order: what `=SUM(A:A)` does.
    pub fn sum_column(&self) -> f64 {
        let mut sum = 0.0;
        for run in &self.axis.runs {
            let first = self.slot(run.first);
            if run.first >> REPLICA_SHIFT == 0 {
                // Rows of the initial grid are stored consecutively.
                for slot in first..first + run.len {
                    sum += self.columns[0].get(slot);
                }
            } else {
                for offset in 0..u64::from(run.len) {
                    sum += self.read(run.first + offset, 0);
                }
            }
        }
        sum
    }

    /// Decides for `count` pairs of rows which one comes first.
    pub fn compare_positions(&self, ids: &[RowId]) -> f64 {
        ids.as_chunks::<2>()
            .0
            .iter()
            .filter(|[first, second]| {
                self.axis.position_of(*first) < self.axis.position_of(*second)
            })
            .count() as f64
    }

    /// Row identities picked at random from the whole sheet.
    #[must_use]
    pub fn random_ids(&self, count: u32) -> Vec<RowId> {
        let mut picker = Picker::new(3);
        (0..count)
            .map(|_| self.axis.id_at(picker.below(self.axis.len)))
            .collect()
    }

    /// Inserts `count` rows at random positions, rebuilding the axis each time.
    pub fn insert_rows(&mut self, count: u32) -> f64 {
        let mut picker = Picker::new(4);
        let existing = self.inserted.len() as u32;
        for index in 0..count {
            let counter = existing + index + 1;
            self.inserted.push(0); // the prototype does not grow the columns
            self.axis
                .insert(picker.below(self.axis.len), row_id(1, u64::from(counter)));
        }
        f64::from(self.axis.len)
    }

    /// How many rows directly follow their predecessor in identity as well as
    /// in position. A column filled with "the row above" can share one formula
    /// template across exactly these rows.
    #[must_use]
    pub fn rows_sharing_a_template(&self) -> u32 {
        self.axis.runs.iter().map(|run| run.len - 1).sum()
    }
}

/// The baseline: the same data addressed by position.
pub struct Positional {
    columns: Vec<Vec<f64>>,
    /// Formula `index` is `=A<row> * E1` or `=B<row> * C<row> + E1`.
    formula_rows: Vec<u32>,
    product: bool,
}

impl Positional {
    #[must_use]
    pub fn new() -> Self {
        let columns = (0..COLUMNS)
            .map(|col| {
                (0..ROWS as usize)
                    .map(|slot| (slot % 1000) as f64 + col as f64)
                    .collect()
            })
            .collect();
        Self {
            columns,
            formula_rows: Vec::new(),
            product: false,
        }
    }

    pub fn fill_scaled(&mut self, count: u32) {
        self.formula_rows = (0..count).collect();
        self.product = false;
    }

    pub fn fill_product(&mut self) {
        self.formula_rows = (0..ROWS).collect();
        self.product = true;
    }

    pub fn recalculate(&mut self, operand: f64) -> f64 {
        self.columns[4][0] = operand;
        let mut sum = 0.0;
        for index in 0..self.formula_rows.len() {
            let row = self.formula_rows[index] as usize;
            let value = if self.product {
                self.columns[1][row] * self.columns[2][row] + self.columns[4][0]
            } else {
                self.columns[0][row] * self.columns[4][0]
            };
            self.columns[3][row] = value;
            sum += value;
        }
        sum
    }

    pub fn read_at_random_positions(&self, count: u32) -> f64 {
        let mut picker = Picker::new(1);
        (0..count)
            .map(|_| self.columns[0][picker.below(ROWS) as usize])
            .sum()
    }

    pub fn sum_column(&self) -> f64 {
        self.columns[0].iter().sum()
    }

    /// Inserts `count` rows at random positions by moving the data below
    /// them, in every column. Formulas would have to be rewritten as well;
    /// that is not counted.
    pub fn insert_rows(&mut self, count: u32) -> f64 {
        let mut picker = Picker::new(4);
        for _ in 0..count {
            let position = picker.below(ROWS) as usize;
            for column in &mut self.columns {
                column.insert(position, 0.0);
            }
        }
        self.columns[0].len() as f64
    }
}

impl Default for Positional {
    fn default() -> Self {
        Self::new()
    }
}

/// The workloads as plain exports, so that a page can time them inside a
/// browser's WebAssembly engine. One sheet is kept between calls so that
/// building it is not part of any measurement.
#[cfg(target_arch = "wasm32")]
pub mod exported {
    use std::cell::RefCell;

    use super::{Positional, RowId, Shape, Sheet};

    struct State {
        sheet: Sheet,
        positional: Positional,
        ids: Vec<RowId>,
    }

    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    fn with<T>(run: impl FnOnce(&mut State) -> T) -> T {
        STATE.with_borrow_mut(|state| run(state.get_or_insert_with(|| build(Shape::Fresh))))
    }

    fn build(shape: Shape) -> State {
        let sheet = Sheet::new(shape);
        let ids = sheet.random_ids(1_000_000);
        State {
            sheet,
            positional: Positional::new(),
            ids,
        }
    }

    macro_rules! export {
        ($($name:ident($($arg:ident: $ty:ty),*) => |$state:ident| $body:expr;)+) => {
            $(
                #[expect(unsafe_code, reason = "an unmangled export the page can call")]
                #[unsafe(no_mangle)]
                pub extern "C" fn $name($($arg: $ty),*) -> f64 {
                    with(|$state| $body)
                }
            )+
        };
    }

    /// Rebuilds the sheet: 0 as created, 1 after 1,000 inserts, 2 fully shuffled.
    #[expect(unsafe_code, reason = "an unmangled export the page can call")]
    #[unsafe(no_mangle)]
    pub extern "C" fn axis_setup(shape: u32) -> f64 {
        let shape = match shape {
            1 => Shape::Edited,
            2 => Shape::Shuffled,
            _ => Shape::Fresh,
        };
        let state = build(shape);
        let runs = state.sheet.axis.run_count() as f64;
        STATE.with_borrow_mut(|slot| *slot = Some(state));
        runs
    }

    export! {
        axis_fill_scaled(count: u32) => |state| { state.sheet.fill_scaled(count); state.positional.fill_scaled(count); f64::from(count) };
        axis_fill_product() => |state| { state.sheet.fill_product(); state.positional.fill_product(); 0.0 };
        axis_recalculate(operand: f64) => |state| state.sheet.recalculate(operand);
        axis_recalculate_positional(operand: f64) => |state| state.positional.recalculate(operand);
        axis_read_random(count: u32) => |state| state.sheet.read_at_random_positions(count);
        axis_read_random_positional(count: u32) => |state| state.positional.read_at_random_positions(count);
        axis_read_consecutive(count: u32) => |state| state.sheet.read_at_consecutive_positions(count);
        axis_read_by_identity() => |state| state.sheet.read_by_identity(&state.ids);
        axis_write_random(count: u32) => |state| state.sheet.write_at_random_positions(count);
        axis_sum_column() => |state| state.sheet.sum_column();
        axis_sum_column_positional() => |state| state.positional.sum_column();
        axis_compare_positions() => |state| state.sheet.compare_positions(&state.ids);
        axis_insert_rows(count: u32) => |state| state.sheet.insert_rows(count);
        axis_insert_rows_positional(count: u32) => |state| state.positional.insert_rows(count);
        axis_bytes() => |state| state.sheet.axis.bytes() as f64;
        axis_rows_sharing_a_template() => |state| f64::from(state.sheet.rows_sharing_a_template());
    }
}
