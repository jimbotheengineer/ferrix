//! End-to-end persistence check against real files on disk.
//!
//! Unit tests cover the sidecar codec in isolation. This exercises the whole
//! path the user actually takes: convert a CSV, edit cells, save, drop
//! everything, reopen from scratch, and confirm the edits came back.

use std::path::{Path, PathBuf};

use ferrix_core::{CellInput, CellRef, EditOverlay, Value};
use ferrix_io::edits::{edits_path_for, load_edits, save_edits, BaseFingerprint};
use ferrix_io::MappedSheet;

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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let src = PathBuf::from(
        args.get(1)
            .cloned()
            .unwrap_or_else(|| "benchdata/persist.csv".to_string()),
    );
    if !src.exists() {
        die(&format!("source {} does not exist", src.display()));
    }

    let cache = ferrix_io::cache_path_for(&src);
    println!("source: {}", src.display());

    // --- convert ---
    if !ferrix_io::cache_is_fresh(&src, &cache) {
        let t = std::time::Instant::now();
        let stats = ferrix_io::convert_csv(&src, &cache, b',', true, |_, _| {})
            .unwrap_or_else(|e| die(&format!("convert failed: {e}")));
        println!(
            "converted {} rows in {:.1}s",
            stats.rows,
            t.elapsed().as_secs_f64()
        );
    } else {
        println!("using existing cache");
    }

    let mapped = MappedSheet::open(&cache).unwrap_or_else(|e| die(&format!("open failed: {e}")));
    let rows = mapped.row_count() as u64;
    let cols = mapped.col_count() as u32;
    println!("mapped: {rows} rows x {cols} cols");

    let fp = BaseFingerprint::of(&cache, rows, cols)
        .unwrap_or_else(|e| die(&format!("fingerprint failed: {e}")));
    let sidecar = edits_path_for(&cache);
    let _ = std::fs::remove_file(&sidecar);

    // --- record what the base holds, so we can prove edits shadow it ---
    let deep = CellRef::new((rows - 1) as u32, 1);
    let base_deep = mapped.display(deep);
    println!("base value at last row, col B: {base_deep:?}");

    // --- edit ---
    let mut overlay = EditOverlay::new();
    let text_id = overlay.intern("edited by persistence test");
    overlay.set(CellRef::new(0, 0), CellInput::Literal(Value::Number(-1.5)));
    overlay.set(CellRef::new(5, 2), CellInput::Literal(Value::Text(text_id)));
    overlay.set(CellRef::new(9, 3), CellInput::Literal(Value::Bool(true)));
    // An edit at the very end of a huge sheet: proves deep row indices survive.
    overlay.set(deep, CellInput::Literal(Value::Number(424242.0)));
    overlay.set(
        CellRef::new(1, 4),
        CellInput::Formula {
            src: "=1+2".into(),
            cached: Value::Number(3.0),
        },
    );

    let t = std::time::Instant::now();
    let bytes = save_edits(&sidecar, &overlay, fp).unwrap_or_else(|e| die(&format!("save: {e}")));
    let save_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("\nsaved 5 edits -> {} bytes in {:.2} ms", bytes, save_ms);
    check(
        bytes < 4096,
        "sidecar is kilobytes, not proportional to the dataset",
    );
    check(save_ms < 100.0, "saving is fast regardless of dataset size");

    // --- drop everything and reload from scratch ---
    drop(overlay);
    drop(mapped);

    let mapped2 = MappedSheet::open(&cache).unwrap_or_else(|e| die(&format!("reopen: {e}")));
    let fp2 = BaseFingerprint::of(
        &cache,
        mapped2.row_count() as u64,
        mapped2.col_count() as u32,
    )
    .unwrap_or_else(|e| die(&format!("refingerprint: {e}")));

    let t = std::time::Instant::now();
    let restored = load_edits(&sidecar, fp2)
        .unwrap_or_else(|e| die(&format!("load: {e}")))
        .unwrap_or_else(|| die("sidecar vanished"));
    println!(
        "\nreloaded {} edits in {:.2} ms",
        restored.len(),
        t.elapsed().as_secs_f64() * 1000.0
    );

    check(restored.len() == 5, "all five edits came back");
    check(
        restored.value(CellRef::new(0, 0)) == Some(Value::Number(-1.5)),
        "number round-tripped",
    );
    check(
        restored.value(CellRef::new(9, 3)) == Some(Value::Bool(true)),
        "bool round-tripped",
    );
    check(
        restored.value(deep) == Some(Value::Number(424242.0)),
        "edit at the last row of a huge sheet round-tripped",
    );
    match restored.value(CellRef::new(5, 2)) {
        Some(Value::Text(id)) => check(
            restored.resolve(id) == Some("edited by persistence test"),
            "text resolved through the restored arena",
        ),
        other => die(&format!("expected text, got {other:?}")),
    }
    match restored.get(CellRef::new(1, 4)) {
        Some(CellInput::Formula { src, cached }) => {
            check(src == "=1+2", "formula source round-tripped");
            check(*cached == Value::Number(3.0), "formula cache round-tripped");
        }
        other => die(&format!("expected formula, got {other:?}")),
    }

    // The base must be untouched: persistence writes a sidecar, never the data.
    check(
        mapped2.display(deep) == base_deep,
        "base file was NOT modified by saving edits",
    );

    // --- staleness ---
    let wrong = BaseFingerprint {
        rows: fp2.rows + 1,
        ..fp2
    };
    match load_edits(&sidecar, wrong) {
        Err(ferrix_io::edits::EditError::StaleBase { .. }) => {
            println!("  ok: a sidecar from a different base is rejected");
        }
        other => die(&format!("stale base must be rejected, got {other:?}")),
    }

    let _ = std::fs::remove_file(&sidecar);
    println!("\nALL PERSISTENCE CHECKS PASSED");
}

#[allow(dead_code)]
fn unused(_: &Path) {}
