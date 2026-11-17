//! What users end up with when they edit at the same time. Each test makes the
//! edits on separate replicas, lets the server order them both ways, and checks
//! the result everyone converges on. Run with `--nocapture` to read the
//! outcomes as a transcript.

use spike_axis::model::{Content, Op, RowId, Sheet, Value, row_id};
use spike_axis::sync::{AfterSort, settle, unique_sheet_name};

const ANN: u32 = 1;
const BEN: u32 = 2;
const A: u8 = 0;
const B: u8 = 1;
const C: u8 = 2;

const fn row(number: u64) -> RowId {
    row_id(0, number)
}

fn number(value: f64) -> Option<Content> {
    Some(Content::Number(value))
}

/// Twelve rows; A1:A12 hold 1..12; C1 is =SUM(A5:A9).
fn sheet() -> Sheet {
    let mut sheet = Sheet::with_rows(12);
    for index in 1..=12_u32 {
        sheet.cells.insert(
            (row(u64::from(index)), A),
            Content::Number(f64::from(index)),
        );
    }
    sheet.cells.insert(
        (row(1), C),
        Content::Sum {
            col: A,
            first: row(5),
            last: row(9),
        },
    );
    sheet
}

/// The outcome when Ann's transaction is ordered first, and when Ben's is.
fn both_orders(start: &Sheet, ann: Vec<Op>, ben: Vec<Op>) -> (Sheet, Sheet) {
    let edits = [(ANN, ann), (BEN, ben)];
    (
        settle(start, AfterSort::SameRow, &edits, &[0, 1]),
        settle(start, AfterSort::SameRow, &edits, &[1, 0]),
    )
}

#[test]
fn the_same_cell_edited_twice_keeps_the_later_edit() {
    let ann = vec![Op::Set {
        row: row(3),
        col: A,
        content: number(100.0),
    }];
    let ben = vec![Op::Set {
        row: row(3),
        col: A,
        content: number(200.0),
    }];
    let (ann_first, ben_first) = both_orders(&sheet(), ann, ben);
    assert_eq!(ann_first.value(row(3), A), Value::Number(200.0));
    assert_eq!(ben_first.value(row(3), A), Value::Number(100.0));
}

#[test]
fn an_edit_in_a_row_deleted_at_the_same_time_waits_on_the_tombstone() {
    let ann = vec![Op::Set {
        row: row(5),
        col: A,
        content: number(50.0),
    }];
    let ben = vec![Op::Delete { ids: vec![row(5)] }];
    let (ann_first, ben_first) = both_orders(&sheet(), ann, ben);
    assert_eq!(ann_first, ben_first);

    let mut sheet = ann_first;
    assert_eq!(sheet.position(row(5)), None);
    assert_eq!(sheet.formula_text(row(1), C), "=SUM(A5:A8)");
    sheet.apply(&Op::Undelete { ids: vec![row(5)] }).unwrap();
    assert_eq!(sheet.value(row(5), A), Value::Number(50.0));
    println!(
        "edit + delete of the same row: the row is gone; undoing the delete shows the edit (A5 = 50)"
    );
}

#[test]
fn two_rows_inserted_at_the_same_place_are_both_kept() {
    let (ann_row, ben_row) = (row_id(ANN, 1), row_id(BEN, 1));
    let ann = vec![
        Op::Insert {
            after: Some(row(6)),
            ids: vec![ann_row],
        },
        Op::Set {
            row: ann_row,
            col: A,
            content: number(61.0),
        },
    ];
    let ben = vec![
        Op::Insert {
            after: Some(row(6)),
            ids: vec![ben_row],
        },
        Op::Set {
            row: ben_row,
            col: A,
            content: number(62.0),
        },
    ];
    let (ann_first, ben_first) = both_orders(&sheet(), ann, ben);
    // The insert ordered later lands directly after the anchor row.
    assert_eq!(ann_first.column(A), "1 2 3 4 5 6 62 61 7 8 9 10 11 12");
    assert_eq!(ben_first.column(A), "1 2 3 4 5 6 61 62 7 8 9 10 11 12");
    assert_eq!(ann_first.formula_text(row(1), C), "=SUM(A5:A11)");
}

#[test]
fn a_range_takes_in_a_row_inserted_inside_it_at_the_same_time() {
    let ben_row = row_id(BEN, 1);
    let ann = vec![Op::Set {
        row: row(2),
        col: C,
        content: Some(Content::Sum {
            col: A,
            first: row(5),
            last: row(9),
        }),
    }];
    let ben = vec![
        Op::Insert {
            after: Some(row(6)),
            ids: vec![ben_row],
        },
        Op::Set {
            row: ben_row,
            col: A,
            content: number(100.0),
        },
    ];
    let (ann_first, ben_first) = both_orders(&sheet(), ann, ben);
    assert_eq!(ann_first, ben_first);
    assert_eq!(ann_first.formula_text(row(2), C), "=SUM(A5:A10)");
    assert_eq!(ann_first.value(row(2), C), Value::Number(135.0));
}

#[test]
fn edits_made_offline_land_on_the_rows_they_were_made_on() {
    let ann = vec![
        Op::Set {
            row: row(8),
            col: A,
            content: number(80.0),
        },
        Op::Set {
            row: row(9),
            col: A,
            content: number(90.0),
        },
    ];
    let ben = vec![
        Op::Insert {
            after: None,
            ids: vec![row_id(BEN, 1), row_id(BEN, 2)],
        },
        Op::Delete { ids: vec![row(3)] },
    ];
    let (ann_first, ben_first) = both_orders(&sheet(), ann, ben);
    assert_eq!(ann_first, ben_first);
    // Two rows appeared above and one disappeared: what was row 8 is now row 9.
    assert_eq!(ann_first.position(row(8)), Some(9));
    assert_eq!(ann_first.value(row(8), A), Value::Number(80.0));
    assert_eq!(ann_first.value(row(9), A), Value::Number(90.0));
}

#[test]
fn an_edit_follows_a_row_moved_at_the_same_time() {
    let ann = vec![Op::Move {
        ids: vec![row(7)],
        after: None,
    }];
    let ben = vec![Op::Set {
        row: row(7),
        col: A,
        content: number(70.0),
    }];
    let (ann_first, ben_first) = both_orders(&sheet(), ann, ben);
    assert_eq!(ann_first, ben_first);
    assert_eq!(ann_first.column(A), "70 1 2 3 4 5 6 8 9 10 11 12");
}

#[test]
fn a_fill_does_not_reach_a_row_inserted_at_the_same_time() {
    let ben_row = row_id(BEN, 1);
    let ann = (5..=9)
        .map(|index| Op::Set {
            row: row(index),
            col: B,
            content: number(1.0),
        })
        .collect();
    let ben = vec![Op::Insert {
        after: Some(row(6)),
        ids: vec![ben_row],
    }];
    let (ann_first, ben_first) = both_orders(&sheet(), ann, ben);
    assert_eq!(ann_first, ben_first);
    // The fill named five rows explicitly; the new row was not one of them.
    assert_eq!(ann_first.value(ben_row, B), Value::Empty);
    assert_eq!(ann_first.column(B), "· · · · 1 1 · 1 1 1 · · ·");
}

/// Five records with keys 5, 3, 1, 4, 2 in column A and ten times the key in
/// column B. Row 6 holds =A2, a reference from outside the sorted range.
fn records() -> Sheet {
    let mut sheet = Sheet::with_rows(6);
    for (index, key) in (1..).zip([5.0, 3.0, 1.0, 4.0, 2.0]) {
        sheet.cells.insert((row(index), A), Content::Number(key));
        sheet
            .cells
            .insert((row(index), B), Content::Number(key * 10.0));
    }
    sheet.cells.insert(
        (row(6), C),
        Content::Cell {
            row: row(2),
            col: A,
        },
    );
    sheet
}

/// Ann sorts the five records by key while Ben changes the payload of the
/// record with key 5 to 555. Where does 555 end up?
#[test]
fn sorting_while_someone_edits_a_record() {
    // After sorting, row n holds the record with key n; these rows held keys 1 to 5.
    let sources = vec![row(3), row(5), row(2), row(4), row(1)];
    let sort_content = vec![Op::Permute {
        rows: (1..=5).map(row).collect(),
        sources,
    }];
    // The same order produced by moving rows instead of content.
    let sort_rows = vec![
        Op::Move {
            ids: vec![row(3)],
            after: None,
        },
        Op::Move {
            ids: vec![row(5)],
            after: Some(row(3)),
        },
        Op::Move {
            ids: vec![row(2)],
            after: Some(row(5)),
        },
        Op::Move {
            ids: vec![row(4)],
            after: Some(row(2)),
        },
        Op::Move {
            ids: vec![row(1)],
            after: Some(row(4)),
        },
    ];
    let edit = vec![Op::Set {
        row: row(1),
        col: B,
        content: number(555.0),
    }];

    let run = |sort: &Vec<Op>, after_sort, order: &[usize]| {
        let edits = [(ANN, sort.clone()), (BEN, edit.clone())];
        let sheet = settle(&records(), after_sort, &edits, order);
        let outside = sheet
            .live_rows()
            .into_iter()
            .find(|id| *id == row(6))
            .map(|id| sheet.formula_text(id, C));
        (
            sheet.column(A),
            sheet.column(B),
            outside.unwrap_or_default(),
            sheet.value(row(6), C),
        )
    };
    let sorted_keys = "1 2 3 4 5 ·";
    let intent_kept = "10 20 30 40 555 ·";
    let intent_lost = "555 20 30 40 50 ·";

    println!(
        "sort + edit of the record with key 5 (payload 50 -> 555); column B afterwards, and the outside reference =A2:"
    );

    // 1. Sorting moves content; an edit goes to the row identity it named.
    let (keys, payload, text, value) = run(&sort_content, AfterSort::SameRow, &[1, 0]);
    assert_eq!(
        (keys.as_str(), payload.as_str()),
        (sorted_keys, intent_kept)
    );
    println!("  content moves, edit ordered first:   {payload}   {text} = {value:?}");
    let (keys, payload, text, value) = run(&sort_content, AfterSort::SameRow, &[0, 1]);
    assert_eq!(
        (keys.as_str(), payload.as_str()),
        (sorted_keys, intent_lost)
    );
    assert_eq!((text.as_str(), value), ("=A2", Value::Number(2.0)));
    println!(
        "  content moves, sort ordered first:   {payload}   {text} = {value:?}   <- 555 landed on the record with key 1"
    );

    // 2. Sorting moves rows; the edit follows its row, but so does every reference.
    for order in [[1, 0], [0, 1]] {
        let (keys, payload, text, value) = run(&sort_rows, AfterSort::SameRow, &order);
        assert_eq!(
            (keys.as_str(), payload.as_str()),
            (sorted_keys, intent_kept)
        );
        assert_eq!((text.as_str(), value), ("=A3", Value::Number(3.0)));
        println!(
            "  rows move, either order:             {payload}   {text} = {value:?}   <- Excel keeps =A2, which would now show 2"
        );
    }

    // 3. Sorting moves content, and an edit made without seeing the sort is
    //    redirected to wherever its record went.
    for order in [[1, 0], [0, 1]] {
        let (keys, payload, text, value) = run(&sort_content, AfterSort::FollowRecord, &order);
        assert_eq!(
            (keys.as_str(), payload.as_str()),
            (sorted_keys, intent_kept)
        );
        assert_eq!((text.as_str(), value), ("=A2", Value::Number(2.0)));
        println!("  content moves, edit follows record:  {payload}   {text} = {value:?}");
    }
}

#[test]
fn a_row_deleted_by_two_users_stays_deleted_until_both_undo() {
    let ann = vec![Op::Delete { ids: vec![row(4)] }];
    let ben = vec![Op::Delete { ids: vec![row(4)] }];
    let (mut sheet, other) = both_orders(&sheet(), ann, ben);
    assert_eq!(sheet, other);
    assert_eq!(sheet.position(row(4)), None);

    sheet.apply(&Op::Undelete { ids: vec![row(4)] }).unwrap(); // Ann undoes hers
    assert_eq!(sheet.position(row(4)), None);
    sheet.apply(&Op::Undelete { ids: vec![row(4)] }).unwrap(); // Ben undoes his
    assert_eq!(sheet.position(row(4)), Some(4));
    println!("both delete row 4, Ann undoes: still deleted; Ben undoes too: the row is back");
}

#[test]
fn undo_skips_a_cell_that_someone_else_has_changed_since() {
    let before = number(3.0);
    let written = number(30.0);
    let undo = Op::SetIf {
        row: row(3),
        col: A,
        expected: written.clone(),
        content: before,
    };

    // Nobody else touched the cell: undo restores it.
    let mut sheet = sheet();
    sheet
        .apply(&Op::Set {
            row: row(3),
            col: A,
            content: written.clone(),
        })
        .unwrap();
    sheet.apply(&undo).unwrap();
    assert_eq!(sheet.value(row(3), A), Value::Number(3.0));

    // Ben changes it while Ann's undo is on its way. Whichever is ordered
    // first, Ben's value stays.
    let mut start = self::sheet();
    start
        .apply(&Op::Set {
            row: row(3),
            col: A,
            content: written,
        })
        .unwrap();
    let ben = vec![Op::Set {
        row: row(3),
        col: A,
        content: number(99.0),
    }];
    let (undo_first, ben_first) = both_orders(&start, vec![undo], ben);
    assert_eq!(undo_first.value(row(3), A), Value::Number(99.0));
    assert_eq!(ben_first.value(row(3), A), Value::Number(99.0));
    println!("Ann undoes 3 -> 30 while Ben writes 99: the cell keeps 99 in both orders");
}

/// A deleted row stays on the axis, so a row inserted "at the same position"
/// has to go on one side of it. With the convention that a new row goes
/// directly after the visible row above the insertion point, one of the two
/// ends of a range differs from Excel; the opposite convention would differ at
/// the other end.
#[test]
fn inserting_next_to_a_deleted_end_of_a_range() {
    // Delete the last row of the range, then insert at position 9.
    let mut sheet = sheet();
    sheet.apply(&Op::Delete { ids: vec![row(9)] }).unwrap();
    assert_eq!(sheet.formula_text(row(1), C), "=SUM(A5:A8)");
    let above = sheet.row_at(8).unwrap();
    let new_row = row_id(ANN, 1);
    sheet
        .apply(&Op::Insert {
            after: Some(above),
            ids: vec![new_row],
        })
        .unwrap();
    sheet
        .apply(&Op::Set {
            row: new_row,
            col: A,
            content: number(100.0),
        })
        .unwrap();
    assert_eq!(sheet.formula_text(row(1), C), "=SUM(A5:A9)"); // Excel: =SUM(A5:A8)
    assert_eq!(sheet.value(row(1), C), Value::Number(126.0)); // Excel: 26
    println!(
        "delete the last row of SUM(A5:A9), insert at 9: {} (Excel: =SUM(A5:A8))",
        sheet.formula_text(row(1), C)
    );

    // Delete the first row of the range, then insert at position 5.
    let mut sheet = self::sheet();
    sheet.apply(&Op::Delete { ids: vec![row(5)] }).unwrap();
    let above = sheet.row_at(4).unwrap();
    sheet
        .apply(&Op::Insert {
            after: Some(above),
            ids: vec![new_row],
        })
        .unwrap();
    sheet
        .apply(&Op::Set {
            row: new_row,
            col: A,
            content: number(100.0),
        })
        .unwrap();
    assert_eq!(sheet.formula_text(row(1), C), "=SUM(A6:A9)"); // the same as Excel
    assert_eq!(sheet.value(row(1), C), Value::Number(30.0));
    println!(
        "delete the first row of SUM(A5:A9), insert at 5: {} (Excel: the same)",
        sheet.formula_text(row(1), C)
    );
}

#[test]
fn two_sheets_created_with_the_same_name_get_distinct_names() {
    let mut names = vec!["Data".to_owned()];
    for _ in 0..2 {
        let name = unique_sheet_name(&names, "Data");
        names.push(name);
    }
    assert_eq!(names, ["Data", "Data (2)", "Data (3)"]);
    assert_eq!(unique_sheet_name(&names, "Report"), "Report");
}

#[test]
fn an_operation_naming_an_unknown_row_changes_nothing() {
    let mut sheet = sheet();
    let before = sheet.clone();
    assert!(
        sheet
            .apply(&Op::Set {
                row: row_id(9, 9),
                col: A,
                content: number(1.0)
            })
            .is_err()
    );
    assert!(
        sheet
            .apply(&Op::Insert {
                after: Some(row(1)),
                ids: vec![row(2)]
            })
            .is_err()
    );
    assert!(
        sheet
            .apply(&Op::Move {
                ids: vec![row(2)],
                after: Some(row(2))
            })
            .is_err()
    );
    assert!(
        sheet
            .apply(&Op::Permute {
                rows: vec![row(1), row(2)],
                sources: vec![row(1), row(1)]
            })
            .is_err()
    );
    assert_eq!(sheet, before);
}
