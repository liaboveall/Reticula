//! Building a view: the rows that are shown, in the order they are shown in.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;

use super::QueryError;
use super::context::{Context, KeptOrder, Step};
use super::filter::Filtering;
use super::memory::Large;
use super::predicate::Term;
use super::selection::{Selection, get};
use super::session::{Outcome, Work};
use super::sort::{Sorted, Sorting, append_rows};
use super::spec::Spec;

/// Rows in an order.
#[derive(Clone, Debug)]
pub(super) enum View {
    /// All rows, as they are in the file.
    File,
    Rows(Rc<Large<u32>>),
}

#[derive(Debug)]
enum Stage {
    Begin(Vec<Term>),
    Filter(Box<Filtering>),
    Choose,
    Sort(Box<Sorting>, Selection),
    /// The selected rows of an order.
    Through(Rc<Large<u32>>, usize, Large<u32>),
    /// The selected rows.
    List(u32, Large<u32>),
    Done(View),
}

/// A view being built.
#[derive(Debug)]
pub(super) struct Building {
    spec: Spec,
    rows: Selection,
    /// Whether the order that is kept is the one that was worked out for this.
    sorted: bool,
    stage: Stage,
}

impl Building {
    /// Builds a view of some conditions, made ready. `same` are the rows they
    /// hold for, when that is known.
    pub(super) fn new(spec: Spec, terms: Vec<Term>, same: Option<Selection>, rows: u32) -> Self {
        Self {
            spec,
            stage: if same.is_some() || terms.is_empty() {
                Stage::Choose
            } else {
                Stage::Begin(terms)
            },
            rows: same.unwrap_or(Selection::all(rows)),
            sorted: false,
        }
    }

    /// Cells this works on in all, about.
    pub(super) fn size(&self) -> u64 {
        let rows = self.rows.len();
        let filter = match &self.stage {
            Stage::Begin(terms) => Filtering::size(rows, terms.len()),
            _ => 0,
        };
        filter.saturating_add(Sorting::size(rows, self.spec.order.len()))
    }
}

impl Work for Building {
    fn finish(&mut self) -> Outcome {
        let view = match &self.stage {
            Stage::Done(view) => view.clone(),
            _ => View::File,
        };
        let spec = core::mem::take(&mut self.spec);
        Outcome::View(spec, self.rows.clone(), view)
    }

    fn step(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        match &mut self.stage {
            Stage::Begin(terms) => {
                let terms = core::mem::take(terms);
                self.stage = Stage::Filter(Box::new(Filtering::new(cx, terms)?));
                Ok(Step::more(1))
            }
            Stage::Filter(filter) => {
                let step = filter.step(cx)?;
                if step.done {
                    self.rows = filter.finish();
                    self.stage = Stage::Choose;
                }
                Ok(step.part())
            }
            Stage::Choose => self.choose(cx),
            Stage::Sort(sorting, of) => {
                let step = sorting.step(cx)?;
                if step.done {
                    cx.caches.order = Some(KeptOrder {
                        keys: self.spec.order.clone(),
                        of: of.clone(),
                        rows: match sorting.finish() {
                            Sorted::Same => None,
                            Sorted::Rows(rows) => Some(Rc::new(rows)),
                        },
                    });
                    // It is chosen next.
                    self.sorted = true;
                    self.stage = Stage::Choose;
                }
                Ok(step.part())
            }
            Stage::Through(order, at, out) => {
                let to = at.saturating_add(cx.limits.slice).min(order.len());
                let part = order.get(*at..to).unwrap_or(&[]);
                let work = part.len();
                if let Some(bits) = self.rows.bits() {
                    for row in part {
                        if get(bits, *row as usize) {
                            out.push(*row)?;
                        }
                    }
                }
                *at = to;
                if to < order.len() {
                    return Ok(Step::more(work));
                }
                self.stage = Stage::Done(View::Rows(Rc::new(out.take())));
                Ok(Step::done(work))
            }
            Stage::List(group, out) => {
                let directory = cx.reader.directory();
                let mut work = 0_usize;
                while work < cx.limits.slice && *group < directory.groups() {
                    work = work
                        .saturating_add(64)
                        .saturating_add(append_rows(&self.rows, directory, *group, out)?);
                    *group = group.saturating_add(1);
                }
                if *group < directory.groups() {
                    return Ok(Step::more(work));
                }
                self.stage = Stage::Done(View::Rows(Rc::new(out.take())));
                Ok(Step::done(work))
            }
            Stage::Done(_) => Ok(Step::done(1)),
        }
    }
}

impl Building {
    /// The rows are known: in which way they come into order.
    fn choose(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        let all = self.rows.bits().is_none();
        let kept = cx
            .caches
            .order
            .as_ref()
            .filter(|kept| {
                kept.keys == self.spec.order && (self.sorted || self.rows.is_within(&kept.of))
            })
            .map(|kept| (kept.rows.clone(), self.rows.is_same(&kept.of)));
        let as_file = match kept {
            Some((Some(order), same)) => {
                if same {
                    self.stage = Stage::Done(View::Rows(order));
                    return Ok(Step::done(1));
                }
                let out = cx.large(self.rows.len() as usize)?;
                self.stage = Stage::Through(order, 0, out);
                return Ok(Step::more(1));
            }
            Some((None, _)) => true,
            None => self.spec.order.is_empty(),
        };
        if as_file {
            if all {
                self.stage = Stage::Done(View::File);
                return Ok(Step::done(1));
            }
            let out = cx.large(self.rows.len() as usize)?;
            self.stage = Stage::List(0, out);
            return Ok(Step::more(1));
        }
        // Another order is kept from now on, and its room is needed.
        cx.caches.order = None;
        let table = cx.reader.rows();
        let few = self.rows.len() < table.checked_div(cx.limits.few).unwrap_or(0);
        let of = if few {
            self.rows.clone()
        } else {
            Selection::all(table)
        };
        let sorting = Sorting::new(cx, of.clone(), &self.spec.order);
        self.stage = Stage::Sort(Box::new(sorting), of);
        Ok(Step::more(1))
    }
}
