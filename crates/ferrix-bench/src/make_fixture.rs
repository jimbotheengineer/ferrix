//! Write a small multi-sheet .xlsx for manual/GUI verification.
//!
//! Not part of the test suite — this exists so a human (or an agent driving
//! the GUI) can open a real multi-sheet workbook with a cross-sheet formula
//! in it and see what Ferrix does with it.
//!
//! Usage: `cargo run --release --bin make-fixture -- out.xlsx`

use ferrix_core::{CellInput, CellRef, EditOverlay, Sheet, Value};
use ferrix_io::SheetExport;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "fixture.xlsx".to_string());

    let mut alpha = Sheet::new("Alpha");
    alpha.set_text(CellRef::new(0, 0), "value");
    for r in 1..=5u32 {
        alpha.set(CellRef::new(r, 0), Value::Number((r * 10) as f64));
    }

    let mut beta = Sheet::new("Beta");
    beta.set_text(CellRef::new(0, 0), "note");
    beta.set_text(CellRef::new(1, 0), "second sheet");
    beta.set(CellRef::new(1, 1), Value::Number(7.0));

    // Summary reaches across to Alpha. The cached values are deliberately
    // wrong so it is obvious whether Ferrix recomputed on load.
    let mut summary = Sheet::new("Summary");
    summary.set_text(CellRef::new(0, 0), "total");
    summary.set(CellRef::new(0, 1), Value::Number(-1.0));
    summary.set_text(CellRef::new(1, 0), "beta cell");
    summary.set(CellRef::new(1, 1), Value::Number(-1.0));

    let mut fx = EditOverlay::new();
    fx.set(
        CellRef::new(0, 1),
        CellInput::Formula {
            src: "=SUM(Alpha!A2:A6)".into(),
            cached: Value::Number(-1.0),
        },
    );
    fx.set(
        CellRef::new(1, 1),
        CellInput::Formula {
            src: "=Beta!B2*2".into(),
            cached: Value::Number(-1.0),
        },
    );

    ferrix_io::export_workbook(
        &path,
        &[
            SheetExport::new("Alpha", &alpha),
            SheetExport::new("Beta", &beta),
            SheetExport::new("Summary", &summary).with_formulas(&fx),
        ],
    )
    .expect("write fixture");

    println!("wrote {path}");
    println!("  Alpha!A2:A6  = 10,20,30,40,50");
    println!("  Beta!B2      = 7");
    println!("  Summary!B1   = SUM(Alpha!A2:A6)  -> expect 150");
    println!("  Summary!B2   = Beta!B2*2         -> expect 14");
}
