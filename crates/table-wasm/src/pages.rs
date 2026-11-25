//! Which pages of a table are to be sent, and in which order.

use reticula_core::viewport::{PAGE_COLS, PAGE_ROWS};

/// How far from what is on screen pages are held, at most.
const REACH: u32 = 32;
/// Pages that are held, at most.
const MOST_PAGES: usize = 512;

/// A page by its row and column in the grid of pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub row: u32,
    pub col: u32,
}

/// Cells to hold pages of, and those of them that are on screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cells {
    pub frozen_cols: u32,
    pub first_row: u32,
    pub rows: u32,
    pub first_col: u32,
    pub cols: u32,
    pub seen_row: u32,
    pub seen_row_end: u32,
    pub seen_col: u32,
    pub seen_col_end: u32,
}

impl Cells {
    /// What is on screen, when the first row is the first one there.
    #[must_use]
    pub const fn at_top(self) -> Self {
        let rows = self.seen_row_end.saturating_sub(self.seen_row);
        Self {
            first_row: 0,
            rows,
            first_col: self.seen_col,
            cols: self.seen_col_end.saturating_sub(self.seen_col),
            seen_row: 0,
            seen_row_end: rows,
            ..self
        }
    }
}

/// The pages along one axis: from where to where they are held, and are on screen.
struct Along {
    first: u32,
    end: u32,
    seen: u32,
    seen_end: u32,
}

impl Along {
    fn new(size: u32, page: u32, held: (u32, u32), seen: (u32, u32)) -> Option<Self> {
        let end = held.0.saturating_add(held.1).min(size);
        let seen_end = seen.1.min(size);
        if held.0 >= end {
            return None;
        }
        let (first, end) = (held.0 / page, end.div_ceil(page));
        if seen.0 >= seen_end {
            return Some(Self {
                first,
                end: end.min(first + 2 * REACH),
                seen: first,
                seen_end: end,
            });
        }
        let from = seen.0 / page;
        let to = seen_end.div_ceil(page).min(from + 4 * REACH);
        Some(Self {
            first: first.min(from).max(from.saturating_sub(REACH)),
            end: end.max(to).min(to + REACH),
            seen: from,
            seen_end: to,
        })
    }

    /// How many pages something is away from what is on screen.
    const fn distance(&self, index: u32) -> u32 {
        if index < self.seen {
            self.seen - index
        } else {
            (index + 1).saturating_sub(self.seen_end)
        }
    }
}

/// The pages to hold of a table of that size, in the order the engine sends
/// those of a sheet: the frozen columns that show something, then what is on
/// screen, then the rest in rings around that.
fn order(rows: u32, cols: u32, cells: &Cells) -> Vec<Place> {
    let mut places = Vec::new();
    let seen_rows = (cells.seen_row, cells.seen_row_end);
    let seen_cols = (cells.seen_col, cells.seen_col_end);
    let (Some(down), Some(across)) = (
        Along::new(rows, PAGE_ROWS, (cells.first_row, cells.rows), seen_rows),
        Along::new(cols, PAGE_COLS, (cells.first_col, cells.cols), seen_cols),
    ) else {
        return places;
    };
    let frozen = cells.frozen_cols.min(cols).div_ceil(PAGE_COLS);
    for rank in 0..=REACH + 1 {
        for row in down.first..down.end {
            let below = down.distance(row);
            for col in (0..frozen).chain(across.first.max(frozen)..across.end) {
                // A frozen column does not move sideways, so only its rows count.
                let own = if col >= frozen {
                    below.max(across.distance(col)) + 1
                } else if below == 0 {
                    0
                } else {
                    below + 1
                };
                if own == rank && places.len() < MOST_PAGES {
                    places.push(Place { row, col });
                }
            }
        }
        if places.len() == MOST_PAGES {
            break;
        }
    }
    places
}

/// The pages that whoever subscribed holds, and which of them are still due.
#[derive(Debug, Default)]
pub struct Pages {
    held: Vec<Place>,
    /// The next one to send comes last.
    due: Vec<Place>,
    /// Those that wait for blocks of the file.
    late: Vec<(Place, Vec<u32>)>,
}

impl Pages {
    /// Forgets the subscription, and everything that was sent for it.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Replaces the subscription. Pages the last one did not hold become due;
    /// those it held are due only if they already were. With `resend` all are.
    pub fn subscribe(&mut self, rows: u32, cols: u32, cells: &Cells, resend: bool) {
        self.arrived(|_| true);
        let held = order(rows, cols, cells);
        let mut due = Vec::new();
        for place in held.iter().rev() {
            if resend || !self.held.contains(place) || self.due.contains(place) {
                due.push(*place);
            }
        }
        self.held = held;
        self.due = due;
    }

    /// The next page that is due, which no longer is.
    pub fn pop(&mut self) -> Option<Place> {
        self.due.pop()
    }

    /// Makes a page wait for blocks.
    pub fn wait(&mut self, place: Place, blocks: Vec<u32>) {
        self.late.push((place, blocks));
    }

    /// Makes those due again that wait for nothing any more.
    pub fn arrived(&mut self, is_here: impl Fn(u32) -> bool) {
        for at in (0..self.late.len()).rev() {
            let is_due = self
                .late
                .get(at)
                .is_some_and(|late| late.1.iter().all(|block| is_here(*block)));
            if is_due {
                self.due.push(self.late.remove(at).0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    const fn cells(rows: (u32, u32), cols: (u32, u32), seen: [u32; 4]) -> Cells {
        Cells {
            frozen_cols: 0,
            first_row: rows.0,
            rows: rows.1,
            first_col: cols.0,
            cols: cols.1,
            seen_row: seen[0],
            seen_row_end: seen[1],
            seen_col: seen[2],
            seen_col_end: seen[3],
        }
    }

    const fn frozen(frozen_cols: u32, cells: Cells) -> Cells {
        Cells {
            frozen_cols,
            ..cells
        }
    }

    fn pairs(places: &[Place]) -> Vec<(u32, u32)> {
        places.iter().map(|place| (place.row, place.col)).collect()
    }

    fn ordered(rows: u32, cols: u32, cells: &Cells) -> Vec<(u32, u32)> {
        pairs(&order(rows, cols, cells))
    }

    #[test]
    fn pages_begin_with_what_is_on_screen_and_go_outwards() {
        let held = cells((0, 384), (0, 16), [200, 250, 0, 10]);
        let expected = [(3, 0), (2, 0), (4, 0), (1, 0), (5, 0), (0, 0)];
        assert_eq!(ordered(10_000_000, 16, &held), expected);
        // In both directions.
        let held = cells((0, 192), (0, 48), [64, 128, 16, 32]);
        let expected = [
            (1, 1),
            (0, 0),
            (0, 1),
            (0, 2),
            (1, 0),
            (1, 2),
            (2, 0),
            (2, 1),
            (2, 2),
        ];
        assert_eq!(ordered(1_000, 100, &held), expected);
    }

    #[test]
    fn frozen_columns_come_first_and_once() {
        let held = frozen(2, cells((64, 128), (32, 16), [64, 100, 33, 40]));
        assert_eq!(ordered(1_000, 100, &held), [(1, 0), (1, 2), (2, 0), (2, 2)]);
        let held = frozen(20, cells((0, 64), (0, 32), [0, 40, 20, 30]));
        assert_eq!(ordered(1_000, 100, &held), [(0, 0), (0, 1)]);
    }

    #[test]
    fn pages_go_row_by_row_when_nothing_is_said_to_be_on_screen() {
        let held = cells((0, 128), (0, 32), [0; 4]);
        let expected = [(0, 0), (0, 1), (1, 0), (1, 1)];
        assert_eq!(ordered(1_000, 100, &held), expected);
    }

    #[test]
    fn pages_end_where_the_table_does() {
        let held = cells((0, 64), (0, 16), [0, 10, 0, 3]);
        assert_eq!(ordered(10, 3, &held), [(0, 0)]);
        assert_eq!(ordered(0, 0, &frozen(3, held)), []);
        assert_eq!(
            ordered(10, 3, &cells((64, 64), (0, 16), [64, 70, 0, 3])),
            []
        );
        assert_eq!(ordered(10, 3, &cells((0, 0), (0, 0), [0; 4])), []);
        assert_eq!(
            ordered(10, 3, &cells((u32::MAX, u32::MAX), (0, 1), [0; 4])),
            []
        );
    }

    #[test]
    fn what_is_on_screen_is_held_even_if_it_was_not_asked_for_and_not_too_much() {
        let held = cells(
            (0, 100_000_000),
            (0, 1_024),
            [50_000_000, 50_000_100, 500, 510],
        );
        let all = ordered(100_000_000, 1_024, &held);
        assert_eq!(all.len(), 512);
        assert_eq!(all[..2], [(781_250, 31), (781_251, 31)]);
        let beside = cells((0, 64), (0, 16), [128, 130, 0, 3]);
        assert_eq!(ordered(1_000, 16, &beside), [(2, 0), (1, 0), (0, 0)]);
    }

    #[test]
    fn the_top_is_what_is_on_screen_there() {
        let held = cells((6_400, 320), (0, 32), [6_500, 6_550, 3, 20]);
        let top = cells((0, 50), (3, 17), [0, 50, 3, 20]);
        assert_eq!(held.at_top(), top);
        assert_eq!(ordered(1_000, 40, &held.at_top()), [(0, 0), (0, 1)]);
    }

    /// Holds pages of some rows of a table with one column of pages.
    fn subscribe(pages: &mut Pages, first: u32, end: u32, resend: bool) {
        let rows = (first * PAGE_ROWS, (end - first) * PAGE_ROWS);
        pages.subscribe(6_400, 3, &cells(rows, (0, 3), [0; 4]), resend);
    }

    fn drain(pages: &mut Pages) -> Vec<u32> {
        std::iter::from_fn(|| pages.pop().map(|place| place.row)).collect()
    }

    #[test]
    fn only_what_the_last_subscription_did_not_hold_is_due() {
        let mut pages = Pages::default();
        subscribe(&mut pages, 0, 2, false);
        assert_eq!(drain(&mut pages), [0, 1]);
        subscribe(&mut pages, 1, 3, false);
        assert_eq!(drain(&mut pages), [2]);
        // The first was let go, so it is new again.
        subscribe(&mut pages, 0, 2, false);
        assert_eq!(drain(&mut pages), [0]);
        subscribe(&mut pages, 0, 2, true);
        assert_eq!(drain(&mut pages), [0, 1]);
        pages.clear();
        subscribe(&mut pages, 0, 1, false);
        assert_eq!(drain(&mut pages), [0]);
    }

    #[test]
    fn what_was_due_stays_so_also_when_it_waited() {
        let mut pages = Pages::default();
        subscribe(&mut pages, 0, 4, false);
        assert_eq!(pages.pop(), Some(Place { row: 0, col: 0 }));
        let second = pages.pop().unwrap();
        pages.wait(second, vec![7]);
        subscribe(&mut pages, 0, 5, false);
        assert_eq!(drain(&mut pages), [1, 2, 3, 4]);
    }

    #[test]
    fn a_page_waits_for_all_of_its_blocks() {
        let mut pages = Pages::default();
        subscribe(&mut pages, 0, 3, false);
        for blocks in [vec![1, 2], vec![2], vec![3]] {
            let place = pages.pop().unwrap();
            pages.wait(place, blocks);
        }
        assert_eq!(pages.pop(), None);
        pages.arrived(|block| block == 2);
        assert_eq!(drain(&mut pages), [1]);
        pages.arrived(|block| block < 3);
        assert_eq!(drain(&mut pages), [0]);
        pages.arrived(|_| true);
        assert_eq!(drain(&mut pages), [2]);
    }
}
