//! Moving whole rows must do to a range what Excel does. The expectations are
//! the recorded output of spikes/excel/probe-clipboard.ps1 (Excel 16.0, build
//! 20711): results/row-moves-excel-16.0.20711.txt.

use spike_axis::model::{Content, Op, RowId, Sheet, Value, row_id};

const fn row(number: u64) -> RowId {
    row_id(0, number)
}

/// A1:A12 hold 1..12. C1 is =SUM(A5:A9); D1, E1, F1 are =A5, =A7, =A9.
fn fixture() -> Sheet {
    let mut sheet = Sheet::with_rows(12);
    for number in 1..=12_u32 {
        sheet.cells.insert(
            (row(u64::from(number)), 0),
            Content::Number(f64::from(number)),
        );
    }
    sheet.cells.insert(
        (row(1), 2),
        Content::Sum {
            col: 0,
            first: row(5),
            last: row(9),
        },
    );
    for (col, target) in [(3, 5), (4, 7), (5, 9)] {
        sheet.cells.insert(
            (row(1), col),
            Content::Cell {
                row: row(target),
                col: 0,
            },
        );
    }
    sheet
}

/// (rows moved, the row they are inserted above, the range afterwards, its
/// value, and what =A5, =A7, =A9 turned into).
type Case = (&'static [u64], u64, &'static str, f64, [&'static str; 3]);

const RECORDED: [Case; 20] = [
    (&[7], 2, "=SUM(A6:A9)", 28.0, ["=A6", "=A2", "=A9"]),
    (&[7], 12, "=SUM(A5:A8)", 28.0, ["=A5", "=A11", "=A8"]),
    (&[6], 9, "=SUM(A5:A9)", 35.0, ["=A5", "=A6", "=A9"]),
    (&[9], 12, "=SUM(A5:A11)", 56.0, ["=A5", "=A7", "=A11"]),
    (&[9], 7, "=SUM(A5:A9)", 35.0, ["=A5", "=A8", "=A7"]),
    (&[9], 5, "=SUM(A6:A9)", 26.0, ["=A6", "=A8", "=A5"]),
    (&[9], 2, "=SUM(A6:A9)", 26.0, ["=A6", "=A8", "=A2"]),
    (&[5], 2, "=SUM(A2:A9)", 44.0, ["=A2", "=A7", "=A9"]),
    (&[5], 8, "=SUM(A5:A9)", 35.0, ["=A7", "=A6", "=A9"]),
    (&[5], 10, "=SUM(A5:A8)", 30.0, ["=A9", "=A6", "=A8"]),
    (&[5], 12, "=SUM(A5:A8)", 30.0, ["=A11", "=A6", "=A8"]),
    (&[11], 7, "=SUM(A5:A10)", 46.0, ["=A5", "=A8", "=A10"]),
    (&[11], 5, "=SUM(A6:A10)", 35.0, ["=A6", "=A8", "=A10"]),
    (&[11], 10, "=SUM(A5:A9)", 35.0, ["=A5", "=A7", "=A9"]),
    (&[2], 7, "=SUM(A4:A9)", 37.0, ["=A4", "=A7", "=A9"]),
    (&[2], 10, "=SUM(A4:A8)", 35.0, ["=A4", "=A6", "=A8"]),
    (&[5, 6], 12, "=SUM(A5:A7)", 24.0, ["=A10", "=A5", "=A7"]),
    (&[8, 9], 2, "=SUM(A7:A9)", 18.0, ["=A7", "=A9", "=A3"]),
    (
        &[5, 6, 7, 8, 9],
        2,
        "=SUM(A2:A6)",
        35.0,
        ["=A2", "=A4", "=A6"],
    ),
    (
        &[4, 5, 6, 7, 8, 9, 10],
        2,
        "=SUM(A3:A7)",
        35.0,
        ["=A3", "=A5", "=A7"],
    ),
];

fn moved(case: &Case, hand_off_anchors: bool) -> Sheet {
    let (rows, above, ..) = case;
    let mut sheet = fixture();
    sheet.hand_off_anchors = hand_off_anchors;
    let ids: Vec<RowId> = rows.iter().map(|number| row(*number)).collect();
    sheet
        .apply(&Op::Move {
            ids,
            after: Some(row(above - 1)),
        })
        .unwrap();
    sheet
}

#[test]
fn ranges_follow_excel_when_anchors_are_handed_off() {
    for case in &RECORDED {
        let (rows, above, range, value, singles) = case;
        let sheet = moved(case, true);
        let what = format!("rows {rows:?} to above row {above}");
        assert_eq!(sheet.formula_text(row(1), 2), *range, "{what}");
        assert_eq!(sheet.value(row(1), 2), Value::Number(*value), "{what}");
        for (col, expected) in (3..).zip(singles) {
            assert_eq!(sheet.formula_text(row(1), col), *expected, "{what}");
        }
    }
}

/// The design as first proposed: a range is whatever lies between its two
/// anchor rows. Recorded here to show how far that is from Excel.
#[test]
fn plain_anchors_disagree_with_excel_in_eight_of_twenty_moves() {
    let mut agreeing = 0;
    for case in &RECORDED {
        let sheet = moved(case, false);
        if sheet.value(row(1), 2) == Value::Number(case.3) {
            agreeing += 1;
        }
        // Single-cell references follow their row under either rule.
        for (col, expected) in (3..).zip(&case.4) {
            assert_eq!(sheet.formula_text(row(1), col), *expected);
        }
    }
    assert_eq!(agreeing, 12);

    // The one that matters most: drag the last row of a summed range up by two.
    let sheet = moved(&RECORDED[4], false);
    assert_eq!(sheet.formula_text(row(1), 2), "=SUM(A5:A7)");
    assert_eq!(sheet.value(row(1), 2), Value::Number(20.0)); // Excel: 35
}
