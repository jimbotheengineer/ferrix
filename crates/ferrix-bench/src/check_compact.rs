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

/// Two OS memory numbers for this process, in bytes: (working set, private).
///
/// Both, because on a memory-mapped workload they answer different questions
/// and only one of them is the claim being made.
///
/// * **Working set** counts every resident page, INCLUDING pages faulted in
///   through the read-only mapping of the source cache. Streaming a 3.6 GB
///   file touches every one of those pages, so this number climbs with the
///   file — but they are clean, file-backed page cache the OS drops the
///   instant anything else wants the RAM. It is not memory the compactor is
///   holding onto.
/// * **Private bytes** counts only memory backed by the pagefile — the heap,
///   the writer buffers, the arena. Nothing file-backed. THIS is the number
///   that must not scale with the file, and it is the one to quote.
///
/// The OS's own accounting rather than an allocator counter, which would miss
/// the buffered writers entirely.
#[cfg(windows)]
fn mem_sample() -> (u64, u64) {
    let pid = std::process::id();
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "$p=Get-Process -Id {pid}; \
                 \"$($p.PeakWorkingSet64) $($p.PrivateMemorySize64)\""
            ),
        ])
        .output();
    let Ok(o) = out else { return (0, 0) };
    let s = String::from_utf8_lossy(&o.stdout);
    let mut it = s.split_whitespace().filter_map(|v| v.parse::<u64>().ok());
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

#[cfg(not(windows))]
fn mem_sample() -> (u64, u64) {
    // VmHWM is the resident high-water mark; VmData is the private data
    // segment, the closest equivalent to Windows' private bytes. Both in kB.
    let Ok(s) = std::fs::read_to_string("/proc/self/status") else {
        return (0, 0);
    };
    let field = |name: &str| -> u64 {
        s.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|r| r.trim().trim_end_matches(" kB").trim().parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    (field("VmHWM:"), field("VmData:"))
}

fn gb(b: u64) -> f64 {
    b as f64 / 1e9
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let want_peak = args.iter().any(|a| a == "--peak");
    // At multi-GB scale the full cell-by-cell comparison is minutes of extra
    // work AND it maps a second copy of the file, which would confound the
    // very working-set number `--peak` exists to measure. So `--peak` measures
    // and stops; correctness is checked on a fixture small enough to compare
    // in full.
    let skip_full = want_peak;
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

    // --- preserve the original cache for comparison ---
    //
    // NOT snapshotted into memory. A 4 GB cache is ~600M cells and holding
    // them as Strings would need more RAM than the thing under test. Instead
    // the original file is copied aside and the two are compared by streaming
    // both mappings a cell at a time — O(1) heap, so the verifier can check
    // every cell at any file size rather than sampling and hoping.
    let preserved = cache.with_extension("ferrix-orig");
    let _ = std::fs::remove_file(&preserved);
    std::fs::copy(&cache, &preserved)
        .unwrap_or_else(|e| die(&format!("could not preserve the original cache: {e}")));

    // --- write a sidecar, as the app would ---
    let sidecar = edits::edits_path_for(&cache);
    let fp = BaseFingerprint::of(&cache, rows as u64, cols as u32)
        .unwrap_or_else(|e| die(&format!("fingerprint failed: {e}")));
    edits::save_edits(&sidecar, &overlay, fp)
        .unwrap_or_else(|e| die(&format!("sidecar save failed: {e}")));
    check(sidecar.exists(), "sidecar exists before the compact");

    // --- compact ---
    let (ws_before, priv_before) = mem_sample();
    let t = std::time::Instant::now();
    let outcome = compact_cache(&cache, &overlay, |_, _| {}, || false)
        .unwrap_or_else(|e| die(&format!("compact failed: {e}")));
    let secs = t.elapsed().as_secs_f64();
    let (ws_after, priv_after) = mem_sample();
    println!(
        "\ncompacted {} rows x {} cols ({:.2} GB) in {:.1}s ({:.0} MB/s)",
        outcome.stats.rows,
        outcome.stats.cols,
        gb(outcome.stats.output_bytes),
        secs,
        (outcome.stats.output_bytes as f64 / 1e6) / secs.max(0.001)
    );
    // Size delta, explained. This check writes TEXT into columns that held
    // only numbers, and a column with any text needs a 4-byte-per-row string
    // section it did not have before — so one such edit legitimately adds
    // 4 bytes x rows to the file. That is the format working as designed, not
    // the compactor being wasteful; an all-literal-in-place compact reproduces
    // the input byte for byte (see `compacting_twice_is_a_no_op_the_second_time`).
    println!(
        "  size {:.2} GB -> {:.2} GB ({:+.2} GB; text edits into numeric columns add a \
         4-byte/row string section to those columns)",
        gb(cache_bytes),
        gb(outcome.stats.output_bytes),
        (outcome.stats.output_bytes as f64 - cache_bytes as f64) / 1e9
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
            "  peak working set: {:.0} MB -> {:.0} MB   (includes mapped file pages)",
            ws_before as f64 / 1e6,
            ws_after as f64 / 1e6
        );
        println!(
            "  private bytes:    {:.0} MB -> {:.0} MB   (heap only; mapped pages excluded)",
            priv_before as f64 / 1e6,
            priv_after as f64 / 1e6
        );
        // The headline. Private bytes is the honest measure of what compact
        // holds: the working set number above is dominated by clean,
        // file-backed pages the OS evicts on demand.
        println!(
            "  PEAK-RAM {:.0} MB private for a {:.2} GB compact \
             (working set {:.0} MB, of which mapped pages are reclaimable)",
            priv_after as f64 / 1e6,
            gb(cache_bytes),
            ws_after as f64 / 1e6
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

    if skip_full {
        println!("\n(--peak: full cell comparison skipped)");
        let _ = std::fs::remove_file(&preserved);
        println!("\nALL CHECKS PASSED");
        return;
    }

    // Every unedited cell is byte-identical, and every row is still at its
    // original index. Checked by walking the ORIGINAL and the NEW mapping in
    // lockstep, cell by cell — not by comparing a total. Aggregates are
    // order-independent: a compact that reversed the rows, dropped one, or
    // duplicated another would pass a SUM and fail a user. This is the
    // property such a bug would actually violate.
    let orig = MappedSheet::open(&preserved)
        .unwrap_or_else(|e| die(&format!("could not reopen the preserved cache: {e}")));
    check(
        orig.row_count() == m.row_count() && orig.col_count() == m.col_count(),
        "the preserved original has the same shape",
    );

    let mut changed = 0u64;
    let mut compared = 0u64;
    let mut moved = 0u64;
    for r in 0..rows {
        for c in 0..cols {
            let cell = CellRef::new(r as u32, c as u32);
            if edited_set.contains(&(r as u32, c as u32)) {
                continue;
            }
            let was = orig.display(cell);
            let is = m.display(cell);
            compared += 1;
            if was != is {
                if changed < 5 {
                    eprintln!("  row {r} col {c}: was {was:?}, now {is:?}");
                }
                changed += 1;
                // Column 0 is the row's identity in the generated data, so a
                // mismatch there is specifically a row that MOVED.
                if c == 0 {
                    moved += 1;
                }
            }
        }
        if r % 5_000_000 == 0 && r > 0 {
            println!("  compared {r} rows…");
        }
    }
    check(
        changed == 0,
        &format!("all {compared} unedited cells are identical, cell by cell"),
    );
    check(moved == 0, "every row is still at its original index");

    let _ = std::fs::remove_file(&preserved);
    println!("\nALL CHECKS PASSED");
}
