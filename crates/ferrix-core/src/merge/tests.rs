//! Tests for merged cell regions.

use super::*;

fn tr(r0: u32, c0: u32, r1: u32, c1: u32) -> TableRange {
    TableRange::new(r0, c0, r1, c1)
}

#[test]
fn the_anchor_holds_the_value_and_the_rest_read_as_covered() {
    let mut m = MergeMap::new();
    m.merge(tr(1, 1, 2, 3)).expect("merge");

    // Top-left is the anchor.
    assert!(m.is_anchor(CellRef::new(1, 1)));
    assert!(!m.is_covered(CellRef::new(1, 1)));

    // Everything else in the rectangle is covered.
    for (r, c) in [(1, 2), (1, 3), (2, 1), (2, 2), (2, 3)] {
        assert!(
            m.is_covered(CellRef::new(r, c)),
            "({r},{c}) must be covered by the merge"
        );
        assert!(!m.is_anchor(CellRef::new(r, c)));
    }

    // Cells outside are untouched.
    assert!(!m.is_covered(CellRef::new(0, 1)));
    assert!(!m.is_covered(CellRef::new(3, 1)));
    assert!(!m.is_covered(CellRef::new(1, 4)));
}

#[test]
fn every_cell_resolves_to_the_anchor_that_holds_its_value() {
    let mut m = MergeMap::new();
    m.merge(tr(5, 2, 7, 4)).expect("merge");
    let anchor = CellRef::new(5, 2);
    for r in 5..=7 {
        for c in 2..=4 {
            assert_eq!(m.resolve(CellRef::new(r, c)), anchor);
        }
    }
    // An unmerged cell resolves to itself.
    let free = CellRef::new(9, 9);
    assert_eq!(m.resolve(free), free);
}

#[test]
fn a_single_cell_merge_is_refused() {
    let mut m = MergeMap::new();
    assert_eq!(m.merge(tr(1, 1, 1, 1)), Err(MergeError::Degenerate));
    assert!(m.is_empty());
}

#[test]
fn overlapping_merges_are_refused_rather_than_silently_absorbed() {
    // Excel absorbs the older merge here. Refusing is the safer behaviour:
    // absorbing changes what a click selects with nothing on screen to say so.
    let mut m = MergeMap::new();
    m.merge(tr(1, 1, 3, 3)).expect("first");

    for overlap in [
        tr(2, 2, 4, 4), // corner overlap
        tr(0, 0, 1, 1), // touches the top-left cell
        tr(1, 1, 3, 3), // exact duplicate
        tr(2, 2, 2, 3), // wholly inside
        tr(0, 0, 9, 9), // wholly containing
    ] {
        assert_eq!(
            m.merge(overlap),
            Err(MergeError::Overlaps),
            "{overlap:?} overlaps the existing region and must be refused"
        );
    }
    assert_eq!(m.len(), 1, "no failed merge may be recorded");
}

#[test]
fn adjacent_but_non_overlapping_merges_are_allowed() {
    // Off-by-one in the overlap test would reject these, which would make
    // side-by-side merged headers impossible — the single most common layout.
    let mut m = MergeMap::new();
    m.merge(tr(0, 0, 0, 2)).expect("first");
    m.merge(tr(0, 3, 0, 5)).expect("immediately to the right");
    m.merge(tr(1, 0, 1, 2)).expect("immediately below");
    assert_eq!(m.len(), 3);
}

#[test]
fn unmerging_from_any_covered_cell_removes_the_whole_region() {
    let mut m = MergeMap::new();
    m.merge(tr(2, 2, 4, 4)).expect("merge");

    // Unmerge addressed from a NON-anchor cell, which is what a user clicking
    // the middle of a merged block actually does.
    let removed = m.unmerge_at(CellRef::new(3, 3)).expect("region found");
    assert_eq!(removed, tr(2, 2, 4, 4));
    assert!(m.is_empty());
    assert!(!m.is_covered(CellRef::new(3, 3)));
}

#[test]
fn unmerging_a_selection_drops_every_region_it_touches() {
    let mut m = MergeMap::new();
    m.merge(tr(0, 0, 0, 1)).expect("a");
    m.merge(tr(2, 0, 2, 1)).expect("b");
    m.merge(tr(9, 9, 10, 10)).expect("far away");

    let n = m.unmerge_range(tr(0, 0, 5, 5));
    assert_eq!(n, 2, "both regions inside the selection must go");
    assert_eq!(m.len(), 1, "the distant region must survive");
    assert!(m.is_covered(CellRef::new(10, 10)));
}

#[test]
fn lookup_is_bounded_by_the_tallest_region_not_the_total() {
    // The performance claim in the module docs. With many short merges spread
    // over many rows, a lookup must not scan them all. This asserts the
    // observable consequence: correctness with a large map, at speed.
    let mut m = MergeMap::new();
    for r in (0..20_000).step_by(2) {
        m.merge(tr(r, 0, r, 1)).expect("merge");
    }
    assert_eq!(m.len(), 10_000);

    let t = std::time::Instant::now();
    for r in (0..20_000).step_by(2) {
        assert!(m.is_anchor(CellRef::new(r, 0)));
        assert!(m.is_covered(CellRef::new(r, 1)));
        assert!(!m.is_covered(CellRef::new(r + 1, 0)));
    }
    let elapsed = t.elapsed();
    assert!(
        elapsed.as_millis() < 500,
        "30k lookups over 10k regions took {elapsed:?}; lookup is probably \
         scanning every region instead of range-querying by row"
    );
}

#[test]
fn a_tall_region_is_still_found_from_its_bottom_row() {
    // The lookup window is `row - tallest ..= row`. An off-by-one there would
    // lose the last row of every merge — invisible until someone edits it.
    let mut m = MergeMap::new();
    m.merge(tr(100, 0, 200, 0)).expect("tall merge");
    assert!(
        m.is_covered(CellRef::new(200, 0)),
        "bottom row must be found"
    );
    assert!(m.is_covered(CellRef::new(150, 0)));
    assert!(m.is_anchor(CellRef::new(100, 0)));
    assert!(!m.is_covered(CellRef::new(201, 0)));
    assert!(!m.is_covered(CellRef::new(99, 0)));
}

#[test]
fn a_merge_at_row_zero_does_not_underflow() {
    // `row - tallest` on a u32 near zero is the classic panic here.
    let mut m = MergeMap::new();
    m.merge(tr(0, 0, 5, 5)).expect("merge at origin");
    assert!(m.is_anchor(CellRef::new(0, 0)));
    assert!(m.is_covered(CellRef::new(0, 1)));
    assert!(!m.is_covered(CellRef::new(6, 6)));
    // And a lookup above the region must not underflow either.
    assert!(!m.is_covered(CellRef::new(0, 6)));
}

#[test]
fn regions_survive_a_round_trip_through_the_iterator() {
    let mut m = MergeMap::new();
    let want = [tr(0, 0, 1, 1), tr(5, 5, 6, 7), tr(10, 0, 10, 3)];
    for r in want {
        m.merge(r).expect("merge");
    }
    let mut got: Vec<TableRange> = m.regions().copied().collect();
    got.sort_by_key(|r| (r.first_row, r.first_col));
    assert_eq!(got, want);
}
