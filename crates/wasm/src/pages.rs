//! The pages a grid has subscribed to, and which of them it still has to be sent.

use std::collections::{BTreeSet, HashMap};

use reticula_core::id::SheetId;
use reticula_core::viewport::{self, CellSpan, OVERFLOW_PAGE_COLS, Page, PageRange};
use reticula_formula::engine::FormulaEngine;

use crate::{EngineEvent, WireCellStyle};

/// What the grid asks for: the cells to hold, and those of them on screen.
#[derive(Clone, Copy, Debug, Default)]
pub struct PageRequest {
    /// `(first, count)` of the rows to hold.
    pub rows: (u32, u32),
    /// `(first, count)` of the columns to hold.
    pub cols: (u32, u32),
    /// The unfrozen cells on screen; empty when the grid did not say.
    pub visible: CellSpan,
}

/// The subscribed pages in the order they are sent, and the ones that are due.
#[derive(Debug, Default)]
pub struct PageQueue {
    request: PageRequest,
    pages: Vec<Page>,
    /// Position of each page in `pages`.
    order: HashMap<Page, usize>,
    /// Positions of the due pages: the first one is the next to send.
    due: BTreeSet<usize>,
}

impl PageQueue {
    /// Forgets the subscription and everything that was sent for it.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Whether nothing is subscribed to.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Whether `page` is subscribed to.
    #[must_use]
    pub fn contains(&self, page: Page) -> bool {
        self.order.contains_key(&page)
    }

    /// Replaces the subscription. Pages the last one did not hold become due;
    /// those it held are only due if they already were. With `resend` the grid
    /// may miss any of them, and every page is due.
    pub fn subscribe(
        &mut self,
        engine: &FormulaEngine,
        sheet: SheetId,
        request: PageRequest,
        resend: bool,
    ) {
        self.request = request;
        self.rebuild(engine, sheet);
        if resend {
            self.mark_all();
        }
    }

    /// Applies the same request to a sheet whose extent or frozen panes changed.
    pub fn refresh(&mut self, engine: &FormulaEngine, sheet: SheetId) {
        if !self.is_empty() {
            self.rebuild(engine, sheet);
        }
    }

    /// Makes `page` due, if it is subscribed to.
    pub fn mark(&mut self, page: Page) {
        if let Some(&position) = self.order.get(&page) {
            self.due.insert(position);
        }
    }

    /// Makes every page due that can show something of a changed cell on `page`:
    /// that page, and the ones its text may overflow into.
    pub fn mark_changed(&mut self, page: Page) {
        for right in 0..=OVERFLOW_PAGE_COLS {
            self.mark(Page {
                row: page.row,
                col: page.col.saturating_add(right),
            });
        }
    }

    /// Makes every subscribed page due.
    pub fn mark_all(&mut self) {
        self.due = (0..self.pages.len()).collect();
    }

    /// Takes the next due page.
    pub fn pop(&mut self) -> Option<Page> {
        let position = self.due.pop_first()?;
        self.pages.get(position).copied()
    }

    fn rebuild(&mut self, engine: &FormulaEngine, sheet: SheetId) {
        let PageRequest {
            rows,
            cols,
            visible,
        } = self.request;
        let (dims, frozen) = engine
            .workbook()
            .sheet(sheet)
            .map_or(((0, 0), (0, 0)), |target| {
                (
                    (engine.visible_row_count(sheet), target.cols()),
                    (target.frozen_rows(), target.frozen_cols()),
                )
            });
        let range = PageRange::covering(dims, rows, cols, visible);
        let pages = viewport::collect_wanted_pages_with_freeze(range, visible, frozen.0, frozen.1);
        let due = pages
            .iter()
            .enumerate()
            .filter(|(_, page)| {
                self.order
                    .get(page)
                    .is_none_or(|position| self.due.contains(position))
            })
            .map(|(position, _)| position)
            .collect();
        self.order = pages
            .iter()
            .enumerate()
            .map(|(position, &page)| (page, position))
            .collect();
        self.pages = pages;
        self.due = due;
    }
}

/// Encodes `page`. A style the grid has not been told about goes into
/// `styles`, which has to reach it before the page does.
pub fn encode_due_page(
    engine: &mut FormulaEngine,
    sheet_id: SheetId,
    page: Page,
    epoch: u32,
    text: &mut String,
    known_styles: &mut usize,
    styles: &mut Option<EngineEvent>,
) -> Option<Vec<u8>> {
    engine.prepare_page_styles(sheet_id, page);
    let entries = engine.workbook().sheet(sheet_id)?.styles().entries();
    if entries.len() > *known_styles {
        *known_styles = entries.len();
        *styles = Some(EngineEvent::StylesUpdated {
            sheet: sheet_id.0,
            styles: entries.iter().copied().map(WireCellStyle::from).collect(),
        });
    }
    engine.encode_viewport_page(sheet_id, page, epoch, text)
}

#[cfg(test)]
mod tests {
    use super::{PageQueue, PageRequest};
    use reticula_core::id::SheetId;
    use reticula_core::viewport::{CellSpan, Page};
    use reticula_formula::engine::FormulaEngine;

    const SHEET: SheetId = SheetId(0);

    fn request(rows: (u32, u32), cols: (u32, u32)) -> PageRequest {
        PageRequest {
            rows,
            cols,
            visible: CellSpan::default(),
        }
    }

    fn drain(queue: &mut PageQueue) -> Vec<(u32, u32)> {
        std::iter::from_fn(|| queue.pop())
            .map(|page| (page.row, page.col))
            .collect()
    }

    #[test]
    fn a_new_subscription_sends_only_what_the_last_one_did_not_hold() {
        let engine = FormulaEngine::new();
        let mut queue = PageQueue::default();
        queue.subscribe(&engine, SHEET, request((0, 128), (0, 16)), false);
        assert_eq!(drain(&mut queue), [(0, 0), (1, 0)]);
        queue.subscribe(&engine, SHEET, request((64, 128), (0, 16)), false);
        assert_eq!(drain(&mut queue), [(2, 0)]);
        // Page (0, 0) was let go, so it is new again.
        queue.subscribe(&engine, SHEET, request((0, 128), (0, 16)), false);
        assert_eq!(drain(&mut queue), [(0, 0)]);
        // A grid that dropped its pages gets all of them.
        queue.subscribe(&engine, SHEET, request((0, 128), (0, 16)), true);
        assert_eq!(drain(&mut queue), [(0, 0), (1, 0)]);
        queue.clear();
        queue.subscribe(&engine, SHEET, request((0, 64), (0, 16)), false);
        assert_eq!(drain(&mut queue), [(0, 0)]);
    }

    #[test]
    fn pages_that_were_due_stay_due_in_the_order_of_the_new_subscription() {
        let engine = FormulaEngine::new();
        let mut queue = PageQueue::default();
        queue.subscribe(&engine, SHEET, request((0, 256), (0, 16)), false);
        assert_eq!(queue.pop(), Some(Page { row: 0, col: 0 }));
        let scrolled = PageRequest {
            rows: (0, 384),
            cols: (0, 16),
            visible: CellSpan {
                first_row: 200,
                end_row: 250,
                first_col: 0,
                end_col: 10,
            },
        };
        queue.subscribe(&engine, SHEET, scrolled, false);
        // Page row 3 is on screen now, and the rest follows by distance.
        assert_eq!(drain(&mut queue), [(3, 0), (2, 0), (4, 0), (1, 0), (5, 0)]);
    }

    #[test]
    fn a_changed_page_jumps_the_queue_if_it_is_on_screen() {
        let engine = FormulaEngine::new();
        let mut queue = PageQueue::default();
        let shown = PageRequest {
            rows: (0, 192),
            cols: (0, 16),
            visible: CellSpan {
                first_row: 0,
                end_row: 40,
                first_col: 0,
                end_col: 10,
            },
        };
        queue.subscribe(&engine, SHEET, shown, false);
        assert_eq!(queue.pop(), Some(Page { row: 0, col: 0 }));
        queue.mark(Page { row: 0, col: 0 });
        queue.mark(Page { row: 0, col: 0 });
        queue.mark(Page { row: 9, col: 0 });
        assert_eq!(drain(&mut queue), [(0, 0), (1, 0), (2, 0)]);
    }

    #[test]
    fn a_change_reaches_the_pages_its_text_may_overflow_into() {
        let engine = FormulaEngine::new();
        let mut queue = PageQueue::default();
        queue.subscribe(&engine, SHEET, request((0, 128), (0, 160)), false);
        assert_eq!(drain(&mut queue).len(), 20);
        queue.mark_changed(Page { row: 1, col: 2 });
        assert_eq!(drain(&mut queue), [(1, 2), (1, 3), (1, 4), (1, 5), (1, 6)]);
    }
}
