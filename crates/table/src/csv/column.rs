//! Reading the fields of one column.

use alloc::string::String;
use alloc::vec::Vec;
use core::iter::{Skip, StepBy};

use super::infer::Seen;
use super::split::Field;
use super::value::{self, Class, Number, SAFE_LIMIT, classify, digit_count, power};
use crate::types::ColumnType;
use crate::writer::{ColumnBuilder, ColumnSpec};

/// Some fields, row by row.
#[derive(Clone, Copy)]
pub(super) struct Fields<'a> {
    pub(super) input: &'a str,
    pub(super) joined: &'a str,
    pub(super) all: &'a [Field],
    /// The column of the first one.
    pub(super) first: usize,
    /// Number of columns.
    pub(super) width: usize,
    /// Whether some texts stand for blank cells.
    pub(super) markers: bool,
}

type Places<'a> = StepBy<Skip<core::slice::Iter<'a, Field>>>;

impl<'a> Fields<'a> {
    fn of(&self, column: usize) -> Places<'a> {
        let skip = match column.checked_sub(self.first) {
            Some(skip) => skip,
            None => column.saturating_add(self.width).saturating_sub(self.first),
        };
        self.all.iter().skip(skip).step_by(self.width.max(1))
    }

    fn bytes(&self, field: Field) -> &'a [u8] {
        field.bytes(self.input, self.joined)
    }

    fn is_marker(&self, text: &[u8]) -> bool {
        self.markers && value::is_blank_marker(text)
    }
}

/// A column that is being imported.
#[derive(Debug)]
pub(super) struct Column {
    pub(super) name: String,
    /// What it has to hold.
    pub(super) forced: Option<ColumnType>,
    /// What its cells are read as.
    pub(super) kind: Option<ColumnType>,
    /// Whether texts should go into a dictionary.
    pub(super) dictionary: bool,
    /// What its values were in this pass.
    pub(super) seen: Seen,
    /// Blank cells, with the values that did not fit.
    pub(super) blanks: u64,
    /// Values that did not fit the type the column has to hold.
    pub(super) misfits: u64,
    /// Texts that were cut.
    pub(super) cut_texts: u64,
}

impl Column {
    /// Starts over, with cells of another type.
    pub(super) fn restart(&mut self, kind: Option<ColumnType>) {
        self.kind = self.forced.or(kind);
        self.seen = Seen::default();
        self.blanks = 0;
        self.misfits = 0;
        self.cut_texts = 0;
    }

    /// Whether the cells are read as the narrowest type there is for them.
    /// Whether all decimals are shown can still be said at the end.
    pub(super) fn is_right(&self) -> bool {
        let scale = |kind: Option<ColumnType>| match kind {
            Some(ColumnType::Decimal { scale, .. }) => Some(ColumnType::Decimal {
                scale,
                fixed: false,
            }),
            other => other,
        };
        self.forced.is_some() || scale(self.kind) == scale(self.seen.kind())
    }

    /// Whether every value of a decimal column has all of its decimals.
    pub(super) fn is_fixed(&self) -> bool {
        match self.kind {
            Some(ColumnType::Decimal { scale, .. }) => {
                self.seen.is_fixed() && self.seen.most_decimals() == u32::from(scale)
            }
            _ => false,
        }
    }

    /// Only looks at what the values are.
    pub(super) fn look(&mut self, fields: &Fields<'_>, index: usize) {
        if self.forced.is_none() {
            self.look_at(fields, fields.of(index));
        }
    }

    fn look_at(&mut self, fields: &Fields<'_>, mut places: Places<'_>) {
        while !self.seen.is_text()
            && let Some(field) = places.next()
        {
            self.seen.look(fields.bytes(*field), fields.markers);
        }
    }

    /// Gives the cells to the builder. Gives whether the column can hold
    /// them all; when it cannot, the builder has not got the rest.
    pub(super) fn read(
        &mut self,
        fields: &Fields<'_>,
        index: usize,
        builder: &mut ColumnBuilder,
    ) -> bool {
        let mut places = fields.of(index);
        let mut known = self.kind;
        let mut decided = self.forced.is_some() || self.seen.is_text();
        // Of numbers: the largest one, and the fewest and the most decimals.
        let (mut largest, mut least, mut most) = (0_u64, u32::MAX, 0_u32);
        let mut surprise = None;
        for field in places.by_ref() {
            let text = fields.bytes(*field);
            if text.is_empty() {
                self.blanks = self.blanks.wrapping_add(1);
                builder.push_blank();
                continue;
            }
            let Some(kind) = known.or_else(|| self.first_kind(fields, text, builder)) else {
                continue;
            };
            known = Some(kind);
            // What the column holds is the same for every cell, so that it is
            // known beforehand which way this goes.
            let value = match kind {
                ColumnType::Integer => value::integer(text).inspect(|value| {
                    largest = largest.max(value.unsigned_abs());
                    least = 0;
                }),
                ColumnType::Decimal { scale, .. } => {
                    value::decimal(text, u32::from(scale)).map(|(value, decimals)| {
                        largest = largest.max(value.unsigned_abs());
                        least = least.min(decimals);
                        most = most.max(decimals);
                        value
                    })
                }
                ColumnType::Date => value::date(text).inspect(|_| self.seen.dates = true),
                ColumnType::DateTime => value::date_time(text)
                    .inspect(|_| self.seen.times = true)
                    .or_else(|| value::start_of_day(text).inspect(|_| self.seen.dates = true)),
                ColumnType::Boolean => value::boolean(text).inspect(|_| self.seen.booleans = true),
                ColumnType::Float => {
                    if let Some((value, class)) = lossless_float(text) {
                        self.seen.add(class);
                        builder.push_float(value);
                        continue;
                    }
                    None
                }
                ColumnType::Text => {
                    if !fields.is_marker(text) {
                        if !decided {
                            self.seen.add(classify(text));
                            decided = self.seen.is_text();
                        }
                        if builder.push_text(field.text(fields.input, fields.joined)) {
                            self.cut_texts = self.cut_texts.wrapping_add(1);
                        }
                        continue;
                    }
                    None
                }
            };
            if let Some(value) = value {
                if value.unsigned_abs() >= SAFE_LIMIT && !self.seen.lossy && kind.is_number() {
                    self.check_lossless(text);
                }
                builder.push_int(value);
            } else if !self.take_odd(fields, text, kind, builder) {
                surprise = Some(text);
                break;
            }
        }
        if least <= most {
            let scale = match known {
                Some(ColumnType::Decimal { scale, .. }) => u32::from(scale),
                _ => 0,
            };
            let whole = largest.checked_div(power(scale)).unwrap_or(0);
            self.seen.numbers(digit_count(whole), least, most);
        }
        let Some(text) = surprise else {
            return true;
        };
        self.seen.add(classify(text));
        self.look_at(fields, places);
        false
    }

    /// Decides what the column holds by its first value. `None` when that is
    /// blank after all.
    #[cold]
    #[inline(never)]
    fn first_kind(
        &mut self,
        fields: &Fields<'_>,
        text: &[u8],
        builder: &mut ColumnBuilder,
    ) -> Option<ColumnType> {
        let mut seen = Seen::default();
        seen.look(text, fields.markers);
        self.kind = seen.kind();
        match self.kind {
            Some(kind) => builder.set_kind(kind),
            None => self.blank(builder),
        }
        self.kind
    }

    fn blank(&mut self, builder: &mut ColumnBuilder) {
        self.blanks = self.blanks.wrapping_add(1);
        builder.push_blank();
    }

    /// Notes when a number with many digits does not survive the conversion
    /// to floating point. One that does not is enough, so this is done often
    /// only where all of them do.
    #[cold]
    #[inline(never)]
    fn check_lossless(&mut self, text: &[u8]) {
        self.seen.lossy = !Number::read(text).is_some_and(|number| number.is_lossless(text));
    }

    /// Takes a cell that is not what the column holds. Gives whether the
    /// column can have it.
    #[cold]
    #[inline(never)]
    fn take_odd(
        &mut self,
        fields: &Fields<'_>,
        text: &[u8],
        kind: ColumnType,
        builder: &mut ColumnBuilder,
    ) -> bool {
        if fields.is_marker(text) {
            self.blank(builder);
            return true;
        }
        if self.forced.is_none() {
            return false;
        }
        // Numbers may then be written in other ways, and lose digits that
        // floating-point numbers do not have.
        let number = Number::read(text).filter(|_| kind.is_number());
        match (number, kind) {
            (Some(number), ColumnType::Float) if let Some(value) = number.to_float(text) => {
                builder.push_float(value);
            }
            (Some(number), _) if let Some(value) = number.to_kind(kind) => {
                self.seen.numbers(0, number.fraction, number.fraction);
                builder.push_int(value);
            }
            _ => {
                self.misfits = self.misfits.saturating_add(1);
                self.blank(builder);
            }
        }
        true
    }
}

/// A number that survives the conversion to floating point, and what it is.
fn lossless_float(text: &[u8]) -> Option<(f64, Class)> {
    let number = Number::read(text)?;
    let class = number.class(text);
    match class {
        Class::Integer { lossless: true, .. }
        | Class::Decimal { lossless: true, .. }
        | Class::Exponent { lossless: true } => Some((number.to_float(text)?, class)),
        _ => None,
    }
}

/// The specs of the columns, for writing them.
pub(super) fn specs(columns: &[Column]) -> Vec<ColumnSpec> {
    let mut specs = Vec::new();
    for column in columns {
        specs.push(ColumnSpec {
            name: column.name.clone(),
            kind: column.kind,
            dictionary: column.dictionary,
        });
    }
    specs
}
