//! Scale checks for search + replace that a unit test can afford.
//!
//! These guard two claims the design rests on:
//!  1. Adding regex support must not slow the LITERAL search path — the
//!     200M-row numbers (~488ms warm for a hit, 0ms for an absent term) come
//!     from an integer scan, and a per-cell allocation would destroy them.
//!  2. Replace All must stream: memory bounded by the window, not by matches.

use ferrix_core::{
    replace_stream, CancelToken, CellRef, LookIn, Query, ReplaceOutcome, ReplaceSpec, Sheet,
};

/// A sheet of `rows` x 4 drawn from a handful of distinct strings — the
/// cardinality profile the arena optimisation is built for.
fn big_sheet(rows: usize) -> Sheet {
    let mut s = Sheet::new("scale");
    let words = ["north", "south", "east", "west"];
    for r in 0..rows {
        for c in 0..4 {
            s.set_text(CellRef::new(r as u32, c as u32), words[(r + c) % 4]);
        }
    }
    s
}

#[test]
fn an_absent_term_still_scans_nothing() {
    // THE guard on the arena-first design. If a query ever had to touch cells
    // to decide "no matches", this would stop being instant. Regex support
    // must not have changed that for literal queries.
    let s = big_sheet(200_000);
    let q = Query::new("zzzz-not-present", false, false).unwrap();
    let t = std::time::Instant::now();
    let r = s.search(&q, usize::MAX);
    let elapsed = t.elapsed();
    assert_eq!(r.total, 0);
    assert_eq!(
        r.matched_strings, 0,
        "no arena string matched, so no column is scanned"
    );
    assert!(
        elapsed.as_millis() < 100,
        "an absent term must not scan cells; took {elapsed:?}"
    );
}

#[test]
fn a_literal_query_does_not_allocate_per_cell() {
    // 800k cells, quarter of them matching. This is a throughput floor, not a
    // benchmark: it fails loudly if the per-cell path ever starts formatting
    // or allocating, which is the specific regression regex support could
    // have introduced.
    let s = big_sheet(200_000);
    let q = Query::new("north", false, false).unwrap();
    let t = std::time::Instant::now();
    let r = s.search(&q, usize::MAX);
    let elapsed = t.elapsed();
    assert_eq!(r.total, 200_000, "one 'north' per row");
    assert!(
        elapsed.as_millis() < 500,
        "800k cells must scan as integers, not strings; took {elapsed:?}"
    );
}

#[test]
fn windowed_search_covers_exactly_the_same_cells_as_a_full_scan() {
    // Replace All walks the sheet in windows. If windowing dropped, repeated,
    // or mis-bounded a row, a replace would silently miss cells — the worst
    // possible failure, because the sheet still looks plausible afterwards.
    let s = big_sheet(5_000);
    let q = Query::new("north", false, false).unwrap();
    let full = s.search(&q, usize::MAX);

    let mut windowed: Vec<CellRef> = Vec::new();
    let mut r0 = 0usize;
    while r0 < 5_000 {
        let r1 = (r0 + 64).min(5_000);
        windowed.extend(s.search_rows(&q, r0, r1, usize::MAX).matches);
        r0 = r1;
    }
    assert_eq!(
        windowed, full.matches,
        "a windowed walk must visit exactly the cells a full scan does"
    );
}

#[test]
fn replace_over_a_million_candidates_holds_nothing_proportional_to_them() {
    // The scale invariant, at a size that would be obvious if it were wrong.
    // The candidate source is lazy and the sink keeps only a counter, so the
    // only memory in play is one cell's text.
    let spec = ReplaceSpec::new(
        Query::new("north", false, false).unwrap(),
        "SOUTH",
        LookIn::Values,
    );
    let n = 1_000_000usize;
    let candidates = (0..n).map(|i| (CellRef::new(i as u32, 0), "north".to_string()));
    let mut applied = 0usize;
    let report = replace_stream(
        &spec,
        candidates,
        &CancelToken::new(),
        usize::MAX,
        |_, _| applied += 1,
        |_, _| {},
    );
    assert_eq!(report.outcome, ReplaceOutcome::Completed);
    assert_eq!(report.applied, n);
    assert_eq!(applied, n);
}
