//! Times the workloads of `spike_axis::cost` natively. A timing run, so it is
//! ignored by default: `cargo xtask spike axis-cost` runs it in release mode.

use std::hint::black_box;
use std::time::Instant;

use spike_axis::cost::{Positional, ROWS, Shape, Sheet};

/// Milliseconds for one call, best of five.
fn best(mut run: impl FnMut() -> f64) -> f64 {
    (0..5)
        .map(|_| {
            let start = Instant::now();
            black_box(run());
            start.elapsed().as_secs_f64() * 1000.0
        })
        .fold(f64::INFINITY, f64::min)
}

#[test]
#[ignore = "a timing run"]
fn print_costs() {
    const COUNT: u32 = 1_000_000;
    let per_call = |milliseconds: f64, count: u32| milliseconds * 1.0e6 / f64::from(count);

    let mut positional = Positional::new();
    println!("baseline, addressed by position");
    println!(
        "  read at a random position        {:>8.2} ns",
        per_call(best(|| positional.read_at_random_positions(COUNT)), COUNT)
    );
    println!(
        "  sum of one column, 1M rows       {:>8.2} ms",
        best(|| positional.sum_column())
    );
    positional.fill_scaled(100_000);
    println!(
        "  100k formulas =A*E1, recalculate {:>8.2} ms",
        best(|| positional.recalculate(2.0))
    );
    positional.fill_product();
    println!(
        "  1M formulas =B*C+E1, recalculate {:>8.2} ms",
        best(|| positional.recalculate(3.0))
    );
    let start = Instant::now();
    positional.insert_rows(10);
    println!(
        "  insert one row (data moves)      {:>8.2} ms",
        start.elapsed().as_secs_f64() * 100.0
    );

    for (shape, name) in [
        (Shape::Fresh, "as created"),
        (Shape::Edited, "after 1,000 inserts"),
        (Shape::Shuffled, "fully shuffled"),
    ] {
        let mut sheet = Sheet::new(shape);
        let ids = sheet.random_ids(COUNT);
        println!(
            "\nby identity, {name}: {} runs, axis {:.1} KB, {} of {ROWS} rows can share a template",
            sheet.axis.run_count(),
            sheet.axis.bytes() as f64 / 1024.0,
            sheet.rows_sharing_a_template()
        );
        println!(
            "  read at a random position        {:>8.2} ns",
            per_call(best(|| sheet.read_at_random_positions(COUNT)), COUNT)
        );
        println!(
            "  read at consecutive positions    {:>8.2} ns",
            per_call(best(|| sheet.read_at_consecutive_positions(COUNT)), COUNT)
        );
        println!(
            "  read through an identity         {:>8.2} ns",
            per_call(best(|| sheet.read_by_identity(&ids)), COUNT)
        );
        println!(
            "  write at a random position       {:>8.2} ns",
            per_call(best(|| sheet.write_at_random_positions(COUNT)), COUNT)
        );
        println!(
            "  which of two rows comes first    {:>8.2} ns",
            per_call(best(|| sheet.compare_positions(&ids)), COUNT / 2)
        );
        println!(
            "  sum of one column, 1M rows       {:>8.2} ms",
            best(|| sheet.sum_column())
        );
        sheet.fill_scaled(100_000);
        println!(
            "  100k formulas =A*E1, recalculate {:>8.2} ms",
            best(|| sheet.recalculate(2.0))
        );
        sheet.fill_product();
        println!(
            "  1M formulas =B*C+E1, recalculate {:>8.2} ms",
            best(|| sheet.recalculate(3.0))
        );
        let start = Instant::now();
        sheet.insert_rows(10);
        println!(
            "  insert one row (axis rebuilt)    {:>8.2} ms",
            start.elapsed().as_secs_f64() * 100.0
        );
    }
}

/// The two ways of addressing must compute the same thing.
#[test]
fn both_addressings_agree() {
    let mut sheet = Sheet::new(Shape::Fresh);
    let mut positional = Positional::new();
    assert_eq!(sheet.sum_column(), positional.sum_column());
    assert_eq!(
        sheet.read_at_random_positions(1000),
        positional.read_at_random_positions(1000)
    );
    sheet.fill_scaled(1000);
    positional.fill_scaled(1000);
    assert_eq!(sheet.recalculate(2.5), positional.recalculate(2.5));

    // Shuffling the rows changes where they are, not what they hold.
    let shuffled = Sheet::new(Shape::Shuffled);
    assert_eq!(shuffled.sum_column(), positional.sum_column());
    assert_eq!(shuffled.rows_sharing_a_template(), 0);
    assert_eq!(Sheet::new(Shape::Edited).axis.run_count(), 2001);
}
