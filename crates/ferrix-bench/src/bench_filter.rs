//! Frame-budget check for search filter mode (issue #6).
//!
//! Acceptance criterion: "Scrolling a filtered 200M-row sheet stays within the
//! 16.67 ms budget." This measures the part of a frame that filter mode adds —
//! the visible-row -> underlying-row mapping and the match-list narrowing the
//! grid performs — over a match set built from a 200M-row sheet.
//!
//! It deliberately does NOT open a GPU window. What it isolates is the only
//! thing filter mode changes about the frame's cost: the mapping layer. If the
//! mapping ever starts allocating per row, or degrades to a linear scan over
//! the match list, this reports it immediately.

use ferrix_core::{CellRef, RowFilter};
use std::time::Instant;

/// Rows per frame at 1080p — the grid paints one viewport regardless of size.
const VIEWPORT_ROWS: usize = 50;
/// The UI's search cap.
const MATCH_CAP: usize = 100_000;
const SHEET_ROWS: u64 = 200_000_000;
const BUDGET_MS: f64 = 16.67;

fn main() {
    // A 200M-row sheet whose matches are spread across its whole height, then
    // capped exactly as the UI caps them.
    let stride = SHEET_ROWS / MATCH_CAP as u64;
    let t = Instant::now();
    let matches: Vec<CellRef> = (0..MATCH_CAP as u64)
        .map(|i| CellRef::new((i * stride) as u32, (i % 8) as u32))
        .collect();
    let gen_ms = t.elapsed().as_secs_f64() * 1000.0;

    // Built ONCE per search, not per frame.
    let t = Instant::now();
    let filter = RowFilter::from_matches(&matches, true, MATCH_CAP);
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;

    println!("Ferrix filter-mode frame budget");
    println!("  sheet rows      : {SHEET_ROWS}");
    println!("  matches (capped): {}", matches.len());
    println!("  visible rows    : {}", filter.len());
    println!("  truncated       : {}", filter.truncated());
    println!("  match gen       : {gen_ms:.2} ms (setup, not per frame)");
    println!("  mapping build   : {build_ms:.2} ms (once per search)");

    // --- simulated scroll ---
    //
    // 600 frames of scrolling through the filtered view, doing exactly the
    // per-frame mapping work the grid does: take the visible window, resolve
    // every row in it, and derive the match-narrowing bounds.
    const FRAMES: usize = 600;
    let step = filter.len().saturating_sub(VIEWPORT_ROWS) / FRAMES.max(1);
    let mut worst_ms = 0.0f64;
    let mut total_ms = 0.0f64;
    let mut checksum: u64 = 0;

    for frame in 0..FRAMES {
        let first = frame * step;
        let last = (first + VIEWPORT_ROWS).min(filter.len());
        let t = Instant::now();

        // 1. The visible window: a borrowed slice, no allocation.
        let window = filter.window(first, last);
        // 2. Resolve each visible row to its underlying row (what the paint
        //    loop and the row headers both do).
        for (i, &row) in window.iter().enumerate() {
            checksum = checksum.wrapping_add(row as u64 + i as u64);
        }
        // 3. Narrow the match list to the visible underlying row span, which
        //    is how the grid decides what to highlight.
        if let (Some(&lo), Some(&hi)) = (window.first(), window.last()) {
            let a = matches.partition_point(|m| m.row < lo);
            let b = matches.partition_point(|m| m.row <= hi);
            checksum = checksum.wrapping_add((b - a) as u64);
        }

        let ms = t.elapsed().as_secs_f64() * 1000.0;
        total_ms += ms;
        worst_ms = worst_ms.max(ms);
    }

    let avg_us = (total_ms / FRAMES as f64) * 1000.0;
    println!("  frames measured : {FRAMES}");
    println!("  avg per frame   : {avg_us:.3} µs");
    println!("  worst frame     : {:.3} µs", worst_ms * 1000.0);
    println!("  budget          : {BUDGET_MS} ms/frame (60 fps)");
    println!("  checksum        : {checksum}");

    // Headroom, stated as a multiple rather than a pass/fail hidden in a log.
    let headroom = BUDGET_MS / worst_ms.max(f64::MIN_POSITIVE);
    println!("  headroom        : {headroom:.0}x on the worst frame");

    if worst_ms > BUDGET_MS {
        eprintln!("FAIL: worst frame {worst_ms:.3} ms exceeds the {BUDGET_MS} ms budget");
        std::process::exit(1);
    }
    println!("OK: filter-mode mapping fits the 60 fps budget with room to spare");
}
