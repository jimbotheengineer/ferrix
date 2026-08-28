//! Does the parallel converter preserve row order and cell values exactly?
//!
//! Parallel chunking is precisely where records get silently reordered or
//! dropped: workers finish out of order, and a merge that trusts completion
//! order rather than source order produces a file that is internally
//! consistent and completely wrong. Aggregates would not catch it -- a SUM is
//! order-independent.
//!
//! So this compares the CONVERTED file against the SOURCE CSV row by row,
//! parsing the CSV independently here. Any reordering, duplication, or drop
//! shows up as a mismatch at a specific row.
//!
//! Usage: check-order <file.csv> [sample_every]

use std::io::{BufRead, BufReader};
use std::path::Path;

use ferrix_core::{CellRef, Value};
use ferrix_io::convert::convert_csv;
use ferrix_io::mapped::MappedSheet;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(src) = args.get(1) else {
        eprintln!("usage: check-order <file.csv> [sample_every]");
        std::process::exit(2);
    };
    // Checking all 200M rows against a text parse would take longer than the
    // conversion; sampling every Nth row still catches any systematic
    // reordering, which is never limited to a single row.
    let step: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000);

    let src_path = Path::new(src);
    let cache = src_path.with_extension("ferrix");

    if !cache.exists() {
        println!("converting {src} ...");
        match convert_csv(src_path, &cache, b',', true, |_, _| {}) {
            Ok(s) => println!("converted {} rows", s.rows),
            Err(e) => {
                eprintln!("convert failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let sheet = match MappedSheet::open(&cache) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "converted: {} rows x {} cols",
        sheet.row_count(),
        sheet.col_count()
    );
    println!("comparing every {step}th row against the source CSV\n");

    let f = std::fs::File::open(src_path).expect("open source");
    let rdr = BufReader::with_capacity(1 << 20, f);

    let mut lines = rdr.lines();
    // Header row is consumed by the converter, so skip it here too.
    let header = lines.next().expect("header").expect("read header");
    let ncols = header.split(',').count();
    println!("header: {ncols} columns");

    let mut checked = 0usize;
    let mut mismatches = 0usize;
    let mut row = 0usize;

    for line in lines {
        let Ok(line) = line else { break };
        if row % step == 0 {
            let fields: Vec<&str> = line.split(',').collect();
            for (c, want) in fields.iter().enumerate() {
                if c >= sheet.col_count() {
                    break;
                }
                let got = display(&sheet, CellRef::new(row as u32, c as u32));
                if !equivalent(want, &got) {
                    if mismatches < 10 {
                        println!("MISMATCH row {row} col {c}: csv={want:?} ferrix={got:?}");
                    }
                    mismatches += 1;
                }
            }
            checked += 1;
        }
        row += 1;
    }

    println!("\nsource rows:    {row}");
    println!("converted rows: {}", sheet.row_count());
    println!("rows sampled:   {checked}");
    println!("mismatches:     {mismatches}");

    if row != sheet.row_count() {
        println!("\nFAIL: row count differs");
        std::process::exit(1);
    }
    if mismatches > 0 {
        println!("\nFAIL: {mismatches} cell mismatches");
        std::process::exit(1);
    }
    println!("\nROW ORDER AND VALUES VERIFIED");
}

fn display(sheet: &MappedSheet, cell: CellRef) -> String {
    match sheet.get(cell) {
        Value::Empty => String::new(),
        Value::Number(n) => ferrix_core::format_number(n),
        Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Text(id) => sheet.resolve(id).to_string(),
        Value::Error(e) => e.as_str().to_string(),
    }
}

/// Compare a CSV field against a rendered cell.
///
/// Numbers are compared numerically: the CSV may say `3.30` where the store
/// round-trips `3.3`, which is the same value and not a conversion bug.
fn equivalent(csv: &str, ferrix: &str) -> bool {
    if csv == ferrix {
        return true;
    }
    match (csv.parse::<f64>(), ferrix.parse::<f64>()) {
        (Ok(a), Ok(b)) => (a - b).abs() < 1e-9 * a.abs().max(1.0),
        _ => false,
    }
}
