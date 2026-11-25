//! What is asked of a table.

use reticula_table::calendar::{self, DAY_MICROS};
use reticula_table::query::{Condition, NumberOp, SortKey, Spec, TimeOp, Value};
use reticula_table::types::ColumnType;

use crate::json;
use crate::wire::TableTrouble;

/// What a `TableQuery` says, for a table with columns of these types.
pub fn spec(asked: &json::Value, kinds: &[ColumnType]) -> Result<Spec, TableTrouble> {
    let mut spec = Spec {
        filters: Vec::new(),
        order: Vec::new(),
    };
    for (_, filter) in asked.field("filters").items() {
        let column = filter.field("column").number();
        let condition = kinds
            .get(column)
            .and_then(|kind| condition(filter.field("condition"), *kind))
            .ok_or_else(|| TableTrouble::invalid(column, "condition"))?;
        spec.filters.push((column, condition));
    }
    for (_, key) in asked.field("order").items() {
        spec.order.push(SortKey {
            column: key.field("column").number(),
            descending: key.of("descending") == "true",
        });
    }
    Ok(spec)
}

/// What a `TableCondition` says of a column of that type.
fn condition(asked: &json::Value, kind: ColumnType) -> Option<Condition> {
    let (text, op, a) = (asked.of("text"), asked.of("op"), asked.of("a"));
    // Without a second threshold there is none.
    let b = Some(asked.of("b")).filter(|b| !b.is_empty());
    Some(match asked.of("kind") {
        "oneOf" => {
            let mut values: Vec<Value> = asked
                .field("keys")
                .items()
                .iter()
                .map(|(_, key)| value_of(kind, key.text()))
                .collect::<Option<_>>()?;
            if asked.of("blank") == "true" {
                values.push(Value::Blank);
            }
            Condition::OneOf { values }
        }
        "contains" => Condition::TextContains(String::from(text)),
        "startsWith" => Condition::TextStartsWith(String::from(text)),
        "endsWith" => Condition::TextEndsWith(String::from(text)),
        "number" => Condition::Number {
            op: match op {
                "equal" => NumberOp::Equal,
                "notEqual" => NumberOp::NotEqual,
                "greater" => NumberOp::Greater,
                "atLeast" => NumberOp::AtLeast,
                "less" => NumberOp::Less,
                "atMost" => NumberOp::AtMost,
                "between" => NumberOp::Between,
                _ => return None,
            },
            a: String::from(a),
            b: b.map(String::from),
        },
        "time" => Condition::Time {
            op: match op {
                "before" => TimeOp::Before,
                "after" => TimeOp::After,
                "between" => TimeOp::Between,
                "on" => TimeOp::On,
                _ => return None,
            },
            a: time_of(kind, a)?,
            b: match b {
                Some(text) => Some(time_of(kind, text)?),
                None => None,
            },
        },
        "top" => Condition::Top {
            n: asked.of("n").parse().ok()?,
            bottom: asked.of("bottom") == "true",
        },
        "blank" => Condition::Blank,
        "notBlank" => Condition::NotBlank,
        _ => return None,
    })
}

/// A value of a list as a text that stands for it. A blank has none.
pub fn key_of(value: &Value) -> String {
    match value {
        Value::Blank => String::new(),
        Value::Int(number) => number.to_string(),
        Value::Float(bits) => bits.to_string(),
        Value::Text(text) => text.clone(),
    }
}

fn value_of(kind: ColumnType, key: &str) -> Option<Value> {
    Some(match kind {
        ColumnType::Text => Value::Text(String::from(key)),
        ColumnType::Float => Value::Float(key.parse().ok()?),
        _ => Value::Int(key.parse().ok()?),
    })
}

/// What stands for a date, or a date and a time, in a column of such. It is
/// written as cells show it; seconds, or all of the time, may be left out.
fn time_of(kind: ColumnType, text: &str) -> Option<i64> {
    let (date, time) = match text.trim().split_once([' ', 'T']) {
        Some((date, time)) => (date, Some(time)),
        None => (text.trim(), None),
    };
    let mut parts = date.split('-').map(digits);
    let days = calendar::days_from_civil(parts.next()??, parts.next()??, parts.next()??)?;
    if parts.next().is_some() {
        return None;
    }
    match kind {
        ColumnType::Date => time.is_none().then_some(days),
        ColumnType::DateTime => {
            let of_day = match time {
                Some(time) => time_of_day(time)?,
                None => 0,
            };
            Some(days * DAY_MICROS + of_day)
        }
        _ => None,
    }
}

fn time_of_day(text: &str) -> Option<i64> {
    let (text, fraction) = match text.split_once('.') {
        Some((text, fraction)) if fraction.len() <= 6 => (
            text,
            digits(fraction)? * 10_i64.pow(6 - u32::try_from(fraction.len()).ok()?),
        ),
        Some(_) => return None,
        None => (text, 0),
    };
    let mut parts = text.split(':').map(digits);
    let (hour, minute) = (parts.next()??, parts.next()??);
    let second = parts.next().unwrap_or(Some(0))?;
    if hour > 23 || minute > 59 || second > 59 || parts.next().is_some() {
        return None;
    }
    Some(((hour * 60 + minute) * 60 + second) * 1_000_000 + fraction)
}

/// A number of nothing but digits, and not of many.
fn digits(text: &str) -> Option<i64> {
    let fine = !text.is_empty() && text.len() <= 6 && text.bytes().all(|b| b.is_ascii_digit());
    if fine { text.parse().ok() } else { None }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    const KINDS: [ColumnType; 5] = [
        ColumnType::Integer,
        ColumnType::Float,
        ColumnType::Date,
        ColumnType::DateTime,
        ColumnType::Text,
    ];

    /// The conditions in a question, which are on columns.
    fn asked(filters: &str) -> Result<Vec<(usize, Condition)>, TableTrouble> {
        let asked = json::Value::read(&format!(r#"{{"filters":[{filters}],"order":[]}}"#));
        spec(&asked, &KINDS).map(|spec| spec.filters)
    }

    fn of(column: u32, condition: &str) -> Option<Condition> {
        let filter = format!(r#"{{"column":{column},"condition":{{{condition}}}}}"#);
        Some(asked(&filter).ok()?.pop()?.1)
    }

    #[test]
    fn a_question_is_what_it_says_in_order() {
        let text = concat!(
            r#"{"filters":[{"column":4,"condition":{"kind":"oneOf","keys":["x","","y"],"blank":false}},"#,
            r#"{"column":1,"condition":{"kind":"blank"}}],"#,
            r#""order":[{"column":3,"descending":true},{"column":0,"descending":false}]}"#
        );
        let asked = spec(&json::Value::read(text), &KINDS).unwrap();
        let columns: Vec<usize> = asked.filters.iter().map(|filter| filter.0).collect();
        assert_eq!(columns, [4, 1]);
        let values = ["x", "", "y"].map(|text| Value::Text(String::from(text)));
        assert_eq!(
            asked.filters[0].1,
            Condition::OneOf {
                values: values.into()
            }
        );
        let order: Vec<_> = asked
            .order
            .iter()
            .map(|key| (key.column, key.descending))
            .collect();
        assert_eq!(order, [(3, true), (0, false)]);
        let nothing = spec(&json::Value::read("{}"), &KINDS).unwrap();
        assert!(nothing.filters.is_empty() && nothing.order.is_empty());
    }

    #[test]
    fn values_come_back_by_their_keys() {
        for (column, value) in [
            (0, Value::Int(i64::MIN)),
            (1, Value::Float((-0.1_f64).to_bits())),
            (2, Value::Int(-5)),
            (4, Value::Text(String::from("a,b"))),
        ] {
            let key = key_of(&value);
            let one_of = format!(r#""kind":"oneOf","keys":["{key}"],"blank":true"#);
            let values = vec![value, Value::Blank];
            assert_eq!(of(column, &one_of), Some(Condition::OneOf { values }));
        }
        assert_eq!(of(0, r#""kind":"oneOf","keys":["x"],"blank":false"#), None);
    }

    #[test]
    fn every_kind_of_condition_is_known() {
        let text = |kind| of(4, &format!(r#""kind":"{kind}","text":"x""#));
        let x = || String::from("x");
        assert_eq!(text("contains"), Some(Condition::TextContains(x())));
        assert_eq!(text("startsWith"), Some(Condition::TextStartsWith(x())));
        assert_eq!(text("endsWith"), Some(Condition::TextEndsWith(x())));
        assert_eq!(text("blank"), Some(Condition::Blank));
        assert_eq!(text("notBlank"), Some(Condition::NotBlank));
        assert_eq!(text("other"), None);
        let number = |op| {
            let condition = format!(r#""kind":"number","op":"{op}","a":"1.5","b":"2""#);
            of(0, &condition)
        };
        for (name, op) in [
            ("equal", NumberOp::Equal),
            ("notEqual", NumberOp::NotEqual),
            ("greater", NumberOp::Greater),
            ("atLeast", NumberOp::AtLeast),
            ("less", NumberOp::Less),
            ("atMost", NumberOp::AtMost),
            ("between", NumberOp::Between),
        ] {
            let (a, b) = (String::from("1.5"), Some(String::from("2")));
            assert_eq!(number(name), Some(Condition::Number { op, a, b }));
        }
        assert_eq!(number("like"), None);
        for (name, op) in [
            ("before", TimeOp::Before),
            ("after", TimeOp::After),
            ("between", TimeOp::Between),
            ("on", TimeOp::On),
        ] {
            let time = format!(r#""kind":"time","op":"{name}","a":"1970-01-02""#);
            assert_eq!(of(2, &time), Some(Condition::Time { op, a: 1, b: None }));
        }
        let top = |n| of(0, &format!(r#""kind":"top","n":{n},"bottom":true"#));
        assert_eq!(
            top("10"),
            Some(Condition::Top {
                n: 10,
                bottom: true
            })
        );
        assert_eq!(top("-1"), None);
        assert_eq!(of(5, r#""kind":"blank""#), None);
        assert!(asked(r#"{"column":3,"condition":{}}"#).is_err());
        assert!(asked(r#"{"condition":{"kind":"blank"}}"#).is_err());
    }

    #[test]
    fn times_are_written_as_cells_show_them() {
        let day = |text| time_of(ColumnType::Date, text);
        assert_eq!(day("1970-01-01"), Some(0));
        assert_eq!(day(" 1969-12-31 "), Some(-1));
        assert_eq!(day("9999-12-31"), Some(calendar::LAST_DAY));
        for wrong in [
            "",
            "1970",
            "1970-02-30",
            "1970-01-01-01",
            "1970-1-x",
            "+970-01-01",
            "1970-01-01 10:00",
            "19700-01-01",
        ] {
            assert_eq!(day(wrong), None, "{wrong}");
        }
        let time = |text| time_of(ColumnType::DateTime, text);
        assert_eq!(time("1970-01-02"), Some(DAY_MICROS));
        assert_eq!(time("1970-01-01 00:01"), Some(60_000_000));
        assert_eq!(time("1970-01-01T23:59:59.5"), Some(DAY_MICROS - 500_000));
        assert_eq!(time("1969-12-31 23:59:59.999999"), Some(-1));
        for wrong in [
            "1970-01-01 24:00",
            "1970-01-01 10",
            "1970-01-01 10:60",
            "1970-01-01 10:00:00.1234567",
            "1970-01-01 10:00:00:00",
            "1970-01-01 10:00:00.",
        ] {
            assert_eq!(time(wrong), None, "{wrong}");
        }
        assert_eq!(time_of(ColumnType::Integer, "1970-01-01"), None);
    }
}
