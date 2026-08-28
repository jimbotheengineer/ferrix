//! End-to-end export check: edit a real file, export it, reimport, compare.
//!
//! Unit tests cover the exporter's quoting and atomicity in isolation. This
//! exercises the path a user actually takes — open a CSV, edit cells, export
//! the result, and confirm the edits are present in a file other tools can
//! read.

use std::path::PathBuf;

use ferrix_core::{CellInput, CellRef, EditOverlay, Value};
use ferrix_io::export::{export_csv, ExportOptions, ExportSource};
use ferrix_io::{load_csv, CsvOptions};

fn die(msg: &str) -> ! {
    eprintln!("FAIL: {msg}");
    std::process::exit(1);
}

fn check(cond: bool, msg: &str) {
    if !cond {
        die(msg);
    }
    println!("  ok: {msg}");
}

/// Base sheet plus an edit overlay, mirroring what the UI exports through.
struct Composite {
    base: ferrix_core::Sheet,
    overlay: EditOverlay,
}

impl ExportSource for Composite {
    fn row_count(&self) -> usize {
        let (r, _) = self.overlay.extent();
        self.base.row_count().max(r)
    }
    fn col_count(&self) -> usize {
        let (_, c) = self.overlay.extent();
        self.base.col_count().max(c)
    }
    fn display(&self, cell: CellRef) -> String {
        match self.overlay.get(cell) {
            Some(input) => match input.value() {
                Value::Empty => String::new(),
                Value::Number(n) => ferrix_core::format_number(n),
                Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.to_string(),
                Value::Text(id) => self.overlay.resolve(id).unwrap_or_default().to_string(),
                Value::Error(e) => e.as_str().to_string(),
            },
            None => self.base.display(cell),
        }
    }
    fn header(&self, col: usize) -> String {
        self.base.header_or_letter(col).to_string()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let src = PathBuf::from(
        args.get(1)
            .cloned()
            .unwrap_or_else(|| "benchdata/export_src.csv".to_string()),
    );
    if !src.exists() {
        die(&format!("source {} does not exist", src.display()));
    }

    println!("source: {}", src.display());
    let (base, stats) =
        load_csv(&src, CsvOptions::default()).unwrap_or_else(|e| die(&format!("load failed: {e}")));
    println!("loaded {} rows x {} cols", stats.rows, stats.cols);

    // Edit some cells, including values that MUST be quoted on the way out.
    let mut overlay = EditOverlay::new();
    let tricky = overlay.intern("has,comma and \"quotes\"");
    let newline = overlay.intern("two\nlines");
    overlay.set(CellRef::new(0, 1), CellInput::Literal(Value::Text(tricky)));
    overlay.set(CellRef::new(1, 1), CellInput::Literal(Value::Text(newline)));
    overlay.set(CellRef::new(2, 4), CellInput::Literal(Value::Number(-99.5)));
    let last = (stats.rows - 1) as u32;
    overlay.set(
        CellRef::new(last, 0),
        CellInput::Literal(Value::Number(123456.0)),
    );

    let comp = Composite { base, overlay };
    let out = src.with_file_name("exported.csv");
    let _ = std::fs::remove_file(&out);

    let t = std::time::Instant::now();
    let es = export_csv(
        &out,
        &comp,
        ExportOptions {
            crlf: false,
            ..Default::default()
        },
        |_, _| {},
        || false,
    )
    .unwrap_or_else(|e| die(&format!("export failed: {e}")));
    println!(
        "\nexported {} rows ({:.1} MB) in {:.2}s = {:.0} MB/s",
        es.rows,
        es.bytes as f64 / 1e6,
        t.elapsed().as_secs_f64(),
        es.throughput_mbps()
    );

    // Reimport with the ordinary loader — the tool anyone else would use.
    let (back, bstats) = load_csv(&out, CsvOptions::default())
        .unwrap_or_else(|e| die(&format!("reimport failed: {e}")));

    check(
        bstats.rows == stats.rows,
        &format!("row count survived ({} rows)", bstats.rows),
    );
    check(
        bstats.cols == stats.cols,
        &format!("column count survived ({} cols)", bstats.cols),
    );
    check(
        back.display(CellRef::new(0, 1)) == "has,comma and \"quotes\"",
        "commas and quotes round-tripped exactly",
    );
    check(
        back.display(CellRef::new(1, 1)) == "two\nlines",
        "embedded newline round-tripped without splitting the row",
    );
    check(
        back.display(CellRef::new(2, 4)) == "-99.5",
        "edited number round-tripped",
    );
    check(
        back.display(CellRef::new(last, 0)) == "123456",
        "edit on the final row round-tripped",
    );
    // An untouched cell must come through unchanged too.
    check(
        back.display(CellRef::new(5, 2)) == comp.base.display(CellRef::new(5, 2)),
        "unedited cells are byte-identical",
    );

    let _ = std::fs::remove_file(&out);
    println!("\nALL EXPORT CHECKS PASSED");
}
