//! Measure structured-table filtering at scale.
//!
//! The claim being tested: filtering cost tracks the *cardinality* of the data
//! and the row count as a memory-bandwidth scan, not as per-cell string work.
//! Run it and read the numbers; do not take the claim on faith.
//!
//! Usage: `cargo run --release -p ferrix-bench --bin bench-filter [rows]`

use ferrix_core::{
    CellRef, CmpOp, ColumnType, CompiledPredicate, Predicate, Sheet, Table, TableColumn,
    TableRange, Value,
};

/// The same shape as the 200M-row CSV benchmark: a handful of distinct strings
/// across a huge number of rows.
const REGIONS: [&str; 4] = ["north", "south", "east", "west"];
const STATUSES: [&str; 3] = ["open", "closed", "pending"];

fn main() {
    let rows: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000_000);

    println!("Building {rows} rows x 3 cols...");
    let t = std::time::Instant::now();
    let mut sheet = Sheet::new("bench");
    sheet.set_text(CellRef::new(0, 0), "region");
    sheet.set_text(CellRef::new(0, 1), "status");
    sheet.set_text(CellRef::new(0, 2), "amount");

    // Intern once, then write ids directly — building the fixture must not
    // dominate the thing being measured.
    let region_ids: Vec<_> = REGIONS.iter().map(|s| sheet.intern(s)).collect();
    let status_ids: Vec<_> = STATUSES.iter().map(|s| sheet.intern(s)).collect();
    for r in 1..=rows as u32 {
        let i = r as usize - 1;
        sheet.set(CellRef::new(r, 0), Value::Text(region_ids[i % 4]));
        sheet.set(CellRef::new(r, 1), Value::Text(status_ids[i % 3]));
        sheet.set(CellRef::new(r, 2), Value::Number((i % 1000) as f64));
    }
    println!(
        "  built in {:.2}s, arena holds {} distinct strings, {:.1} MB heap",
        t.elapsed().as_secs_f64(),
        sheet.arena.len(),
        sheet.heap_bytes() as f64 / 1e6
    );

    let mut table = Table::new("Bench", TableRange::new(0, 0, rows as u32, 2)).with_columns(vec![
        TableColumn::new("region").typed(ColumnType::Text),
        TableColumn::new("status").typed(ColumnType::Text),
        TableColumn::new("amount").typed(ColumnType::Number),
    ]);

    // Step 1 in isolation: the arena pass. This is the whole trick, and it
    // should be independent of `rows`.
    let t = std::time::Instant::now();
    let compiled =
        CompiledPredicate::compile(&Predicate::ValueList(vec!["north".into()]), &sheet.arena);
    println!(
        "\narena pass: {} of {} distinct strings matched in {:.3}ms",
        compiled.matched_strings(),
        sheet.arena.len(),
        t.elapsed().as_secs_f64() * 1000.0
    );

    let cases: [(&str, Predicate, usize); 4] = [
        (
            "text checklist  region in {north}",
            Predicate::ValueList(vec!["north".into()]),
            0,
        ),
        (
            "text contains   region ~ 'th'",
            Predicate::Text {
                needle: "th".into(),
                case_sensitive: false,
                whole_cell: false,
            },
            0,
        ),
        (
            "numeric compare amount > 900",
            Predicate::Compare {
                op: CmpOp::Gt,
                value: 900.0,
            },
            2,
        ),
        (
            "numeric between  100 <= amount <= 200",
            Predicate::Between {
                min: 100.0,
                max: 200.0,
            },
            2,
        ),
    ];

    println!("\nsingle-column filters over {rows} rows:");
    for (label, pred, col) in cases {
        for c in &mut table.columns {
            c.filter = None;
        }
        table.columns[col].filter = Some(pred);
        let t = std::time::Instant::now();
        let mask = sheet.filter_table(&table, usize::MAX);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!(
            "  {label:38}  {:>12} visible  {ms:8.1}ms  ({:.1}M rows/s)",
            mask.visible_rows(),
            rows as f64 / (ms / 1000.0) / 1e6
        );
    }

    // Two filters compose, so both columns get scanned.
    for c in &mut table.columns {
        c.filter = None;
    }
    table.columns[0].filter = Some(Predicate::ValueList(vec!["north".into()]));
    table.columns[1].filter = Some(Predicate::ValueList(vec!["open".into()]));
    let t = std::time::Instant::now();
    let mask = sheet.filter_table(&table, usize::MAX);
    println!(
        "  {:38}  {:>12} visible  {:8.1}ms",
        "two columns ANDed",
        mask.visible_rows(),
        t.elapsed().as_secs_f64() * 1000.0
    );

    // The row-index mapping the renderer uses every frame.
    let t = std::time::Instant::now();
    let probes = 100_000;
    let mut acc = 0usize;
    for i in 0..probes {
        let n = (i * 7919) % mask.visible_rows().max(1);
        acc += mask.nth_visible(n).unwrap_or(0);
    }
    let ns = t.elapsed().as_nanos() as f64 / probes as f64;
    println!(
        "\nnth_visible: {ns:.0}ns per lookup over {} visible rows (checksum {acc})",
        mask.visible_rows()
    );
    println!(
        "  a full viewport of ~50 rows costs {:.3}ms — the per-frame cost of a filtered view",
        ns * 50.0 / 1e6
    );

    // And the interactive-budget path: a bounded scan.
    for c in &mut table.columns {
        c.filter = None;
    }
    table.columns[0].filter = Some(Predicate::ValueList(vec!["north".into()]));
    let budget = 1_000_000;
    let t = std::time::Instant::now();
    let bounded = sheet.filter_table(&table, budget);
    println!(
        "\nbounded scan of {budget} rows: {:.1}ms, truncated={}, {} visible so far",
        t.elapsed().as_secs_f64() * 1000.0,
        bounded.is_truncated(),
        bounded.visible_rows()
    );
}
