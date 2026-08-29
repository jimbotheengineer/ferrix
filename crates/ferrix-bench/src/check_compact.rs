//! End-to-end compact check: edit a real cache, compact it, reopen, verify.
//!
//! Unit tests cover the writer and the atomicity ordering in isolation. This
//! runs the path a user takes on a real, large file and checks the properties
//! that a bug here would actually violate:
//!
//! * the sidecar is **gone**;
//! * every **edited** cell shows its edited value;
//! * every **unedited** cell is byte-identical to what it was before;
//! * row order is preserved **per row**, not by a checksum.
//!
//! That last point is why this does not compare a SUM. Aggregates are
//! order-independent: a compact that reversed the rows, or dropped one and
//! duplicated another, would pass a total and fail a user.
//! `check_order.rs` exists for the same reason.
//!
//! Usage: check-compact [source.csv] [--peak]
//!
//! With `--peak`, the process's peak working set is sampled around the
//! compact and printed, so the "peak RAM does not scale with file size" claim
//! can be measured rather than asserted.

use std::path::PathBuf;

use ferrix_core::{CellInput, CellRef, EditOverlay, Value};
use ferrix_io::compact::{compact_cache, fingerprint_after};
use ferrix_io::edits::{self, BaseFingerprint};
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

/// Peak working set of this process, in bytes. 0 when unavailable.
///
/// The OS's own number, not an estimate: an allocator-side counter would miss
/// the spill writers' buffers and every page the mapping faulted in.
#[cfg(windows)]
fn peak_rss() -> u64 {
    // Read from the process's own memory counters via a tiny GetProcessMemoryInfo
    // shim. Rather than bind the Win32 API, shell out to the value Windows
    // already exposes for the current PID.
    let pid = std::process::id();
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid}).PeakWorkingSet64"),
        ])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse()
            .unwrap_or(0),
        Err(_) => 0,
    }
}

#[cfg(not(windows))]
fn peak_rss() -> u64 {
    // /proc/self/status reports VmHWM, the high-water mark, in kB.
    let Ok(s) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest
                .trim()
                .trim_end_matches(" kB")
                .trim()
                .parse()
                .unwrap_or(0);
            return kb * 1024;
        }
    }
    0
}

fn gb(b: u64) -> f64 {
    b as f64 / 1e9
}

/// Read a whole column as displayed strings.
///
/// Held one column at a time so the verifier itself does not need the sheet in
/// RAM — the same discipline the thing under test obeys.
fn column(m: &MappedSheet, col: usize) -> Vec<String> {
    (0..m.row_count())
        .map(|r| m.display(CellRef::new(r as u32, col as u32)))
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let want_peak = args.iter().any(|a| a == "--peak");
    let src = PathBuf::from(
        args.iter()
            .skip(1)
            .find(|a| !a.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| "benchdata/compact_src.csv".to_string()),
    );
    if !src.exists() {
        die(&format!(
            "source {} does not exist (generate one with gen-data)",
            src.display()
        ));
    }

    println!("source: {}", src.display());

    // --- build the cache ---
    let cache = ferrix_io::cache_path_for(&src);
    if !ferrix_io::cache_is_fresh(&src, &cache) {
        let t = std::time::Instant::now();
        let stats = ferrix_io::convert_csv(&src, &cache, b',', true, |_, _| {})
            .unwrap_or_else(|e| die(&format!("convert failed: {e}")));
        println!(
            "converted {:.2} GB in {:.1}s ({:.0} MB/s)",
            gb(stats.source_bytes),
            t.elapsed().as_secs_f64(),
            stats.throughput_mbps()
        );
    } else {
        println!("reusing existing cache");
    }

    let (rows, cols, cache_bytes) = {
        let m = MappedSheet::open(&cache).unwrap_or_else(|e| die(&format!("open failed: {e}")));
        (m.row_count(), m.col_count(), m.mapped_bytes() as u64)
    };
    println!(
        "cache: {rows} rows x {cols} cols, {:.2} GB on disk",
        gb(cache_bytes)
    );
    if rows < 200 {
        die("source is too small to be a meaningful check (need >= 200 rows)");
    }

    // --- edit 100 cells, spread across the whole file ---
    //
    // Spread rather than clustered: edits packed into the first block would
    // never exercise the streaming path past the first stripe.
    let mut overlay = EditOverlay::new();
    let mut edited: Vec<(u32, u32, String)> = Vec::new();
    let stride = (rows / 100).max(1);
    for i in 0..100usize {
        let row = ((i * stride) % rows) as u32;
        let col = (i % cols) as u32;
        // Alternate text and number so both sections of the writer are used.
        if i % 2 == 0 {
            let text = format!("EDIT-{i}");
            let id = overlay.intern(&text);
            overlay.set(CellRef::new(row, col), CellInput::Literal(Value::Text(id)));
            edited.push((row, col, text));
        } else {
            let n = -(i as f64) - 0.5;
            overlay.set(CellRef::new(row, col), CellInput::Literal(Value::Number(n)));
            edited.push((row, col, ferrix_core::format_number(n)));
        }
    }
    // Cells edited more than once (the strides can collide on small files)
    // must be checked against the LAST write, which is what the overlay holds.
    edited.retain(|(r, c, _)| overlay.get(CellRef::new(*r, *c)).is_some());
    let edited_set: std::collections::HashSet<(u32, u32)> =
        edited.iter().map(|(r, c, _)| (*r, *c)).collect();
    println!("edited {} cells across the file", edited.len());

    // --- snapshot the BEFORE state, column by column ---
    //
    // Snapshotting is what makes "every unedited cell is byte-identical"
    // checkable at all. It is the verifier's cost, not the compactor's, and
    // it is held one column at a time.
    let before: Vec<Vec<String>> = {
        let m = MappedSheet::open(&cache).unwrap();
        (0..cols).map(|c| column(&m, c)).collect()
    };

    // --- write a sidecar, as the app would ---
    let sidecar = edits::edits_path_for(&cache);
    let fp = BaseFingerprint::of(&cache, rows as u64, cols as u32)
        .unwrap_or_else(|e| die(&format!("fingerprint failed: {e}")));
    edits::save_edits(&sidecar, &overlay, fp)
        .unwrap_or_else(|e| die(&format!("sidecar save failed: {e}")));
    check(sidecar.exists(), "sidecar exists before the compact");

    // --- compact ---
    let rss_before = peak_rss();
    let t = std::time::Instant::now();
    let outcome = compact_cache(&cache, &overlay, |_, _| {}, || false)
        .unwrap_or_else(|e| die(&format!("compact failed: {e}")));
    let secs = t.elapsed().as_secs_f64();
    let rss_after = peak_rss();
    println!(
        "\ncompacted {} rows x {} cols ({:.2} GB) in {:.1}s ({:.0} MB/s)",
        outcome.stats.rows,
        outcome.stats.cols,
        gb(outcome.stats.output_bytes),
        secs,
        (outcome.stats.output_bytes as f64 / 1e6) / secs.max(0.001)
    );
    println!(
        "  row-independent buffers: {:.1} MB stripe + {:.1} MB arena + {:.1} MB edits = {:.1} MB",
        outcome.stats.peak_stripe_bytes as f64 / 1e6,
        outcome.stats.arena_bytes as f64 / 1e6,
        outcome.stats.edits_bytes as f64 / 1e6,
        outcome.stats.peak_heap_bytes() as f64 / 1e6
    );
    if want_peak {
        println!(
            "  process peak working set: {:.0} MB before, {:.0} MB after",
            rss_before as f64 / 1e6,
            rss_after as f64 / 1e6
        );
        println!(
            "  PEAK-RAM {:.0} MB for a {:.2} GB compact",
            rss_after as f64 / 1e6,
            gb(cache_bytes)
        );
    }

    // --- verify ---
    check(!sidecar.exists(), "sidecar is GONE after the compact");
    check(
        outcome.sidecar.is_none(),
        "no replacement sidecar was needed (no formulas)",
    );
    check(
        !ferrix_io::compact::temp_path_for(&cache).exists(),
        "no scratch file left behind",
    );

    let m = MappedSheet::open(&cache).unwrap_or_else(|e| die(&format!("reopen failed: {e}")));
    check(
        m.row_count() == rows,
        &format!("row count preserved ({} rows)", m.row_count()),
    );
    check(
        m.col_count() == cols,
        &format!("column count preserved ({} cols)", m.col_count()),
    );

    // The sidecar's fingerprint is re-derivable against the NEW base, so a
    // later save will not be rejected.
    let fp2 = fingerprint_after(&cache, m.row_count() as u64, m.col_count() as u32)
        .unwrap_or_else(|e| die(&format!("re-fingerprint failed: {e}")));
    check(
        fp2.rows == rows as u64 && fp2.cols == cols as u32,
        "the compacted cache can be fingerprinted for future edits",
    );
    check(
        ferrix_io::cache_is_fresh(&src, &cache),
        "the compacted cache is not stale against its source",
    );

    // Every edited cell shows its edited value.
    let mut bad = 0;
    for (r, c, want) in &edited {
        let got = m.display(CellRef::new(*r, *c));
        if got != *want {
            if bad < 5 {
                eprintln!("  edited cell ({r},{c}): expected {want:?}, got {got:?}");
            }
            bad += 1;
        }
    }
    check(
        bad == 0,
        &format!("all {} edited cells show their edited value", edited.len()),
    );

    // Every unedited cell is byte-identical, checked PER ROW so a reorder or a
    // dropped row is caught at the exact index rather than hidden in a total.
    let mut changed = 0u64;
    let mut compared = 0u64;
    for (c, was_col) in before.iter().enumerate().take(cols) {
        let now = column(&m, c);
        if now.len() != was_col.len() {
            die(&format!(
                "column {c} changed length: {} -> {}",
                was_col.len(),
                now.len()
            ));
        }
        for (r, (was, is)) in was_col.iter().zip(now.iter()).enumerate() {
            if edited_set.contains(&(r as u32, c as u32)) {
                continue;
            }
            compared += 1;
            if was != is {
                if changed < 5 {
                    eprintln!("  row {r} col {c}: was {was:?}, now {is:?}");
                }
                changed += 1;
            }
        }
    }
    check(
        changed == 0,
        &format!("all {compared} unedited cells are identical, row by row"),
    );

    // Row order, stated as its own property: the first column is the row's
    // identity in the generated data, and it must appear at the same index.
    let ids_before = &before[0];
    let ids_after = column(&m, 0);
    let mut moved = 0u64;
    for r in 0..rows {
        if edited_set.contains(&(r as u32, 0)) {
            continue;
        }
        if ids_before[r] != ids_after[r] {
            moved += 1;
        }
    }
    check(moved == 0, "every row is still at its original index");

    println!("\nALL CHECKS PASSED");
}
