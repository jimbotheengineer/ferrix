//! Filter mode: rendering the grid over a subset of rows.
//!
//! ## The problem
//!
//! Search gives you a sorted list of matching *cells*. Filter mode wants to
//! show only the *rows* those cells live in, with everything else hidden — but
//! the grid must keep working exactly as it does unfiltered:
//!
//! * it scrolls by f64 row index, so 200M rows stay individually addressable;
//! * it paints a fixed ~280 cells per frame regardless of sheet size;
//! * row headers show the ORIGINAL row number, because a user filtering a
//!   dataset still needs to know that a hit is on row 4,912,733.
//!
//! ## The mapping
//!
//! [`RowFilter`] is a strictly ascending, deduplicated `Vec<u32>` of the rows
//! that contain at least one match. Position *i* in that vector is visible row
//! *i*; its value is the underlying row. That makes the two directions:
//!
//! * visible -> underlying: a slice index (`rows[i]`), O(1);
//! * underlying -> visible: `binary_search`, O(log n).
//!
//! The vector is built ONCE per search — never per frame. The grid's paint
//! loop takes a `&[u32]` subslice of the visible window and indexes it; it
//! allocates nothing. See [`RowFilter::window`].
//!
//! ## Precision
//!
//! Row identity stays integral (`u32` here, `f64` row indices in the grid's
//! scroll state). Nothing in the mapping layer converts a row through f32 or
//! multiplies a row index by a pixel height, so the f64 mantissa still gives
//! exact addressing well past 10^15 rows.
//!
//! ## Truncation
//!
//! `SearchResults` is capped (100,000 matches in the UI). A filter derived
//! from a capped result set shows a PREFIX of the matching rows, not all of
//! them. That is a correctness-of-understanding problem, not a cosmetic one: a
//! user who scrolls to the bottom of a filtered view and sees no more rows
//! will conclude there are no more matches. So the flag rides along with the
//! mapping ([`RowFilter::truncated`]) and the UI is expected to say so.

use crate::{CellRef, SearchResults};

/// A visible-row -> underlying-row mapping derived from a sorted match list.
#[derive(Clone, Debug, Default)]
pub struct RowFilter {
    /// Underlying row indices, strictly ascending and deduplicated.
    rows: Vec<u32>,
    /// True when the source result set hit its cap, so `rows` is a prefix of
    /// the rows that actually match. MUST be surfaced in the UI.
    truncated: bool,
    /// Total matching cells reported by the search, including any beyond the
    /// cap. Only an upper bound on the hidden rows, but honest about scale.
    total_matches: usize,
}

impl RowFilter {
    /// Build the mapping from a row-major-sorted match list.
    ///
    /// Runs once per search, not once per frame. Because `matches` is already
    /// sorted by (row, col), deduplication is a single linear pass with no
    /// hashing and no sort.
    pub fn from_matches(matches: &[CellRef], truncated: bool, total_matches: usize) -> Self {
        let mut rows: Vec<u32> = Vec::new();
        let mut last: Option<u32> = None;
        for m in matches {
            if last != Some(m.row) {
                rows.push(m.row);
                last = Some(m.row);
            }
        }
        Self {
            rows,
            truncated,
            total_matches,
        }
    }

    /// Build directly from a search result, carrying its cap flag across.
    pub fn from_results(results: &SearchResults) -> Self {
        Self::from_matches(&results.matches, results.truncated, results.total)
    }

    /// Number of visible rows — this is what the grid uses as its row count.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// True when the underlying result set was capped, so this mapping omits
    /// matching rows that exist in the sheet.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn total_matches(&self) -> usize {
        self.total_matches
    }

    /// Visible row -> underlying row. `None` past the end.
    #[inline]
    pub fn underlying(&self, visible: usize) -> Option<u32> {
        self.rows.get(visible).copied()
    }

    /// Underlying row -> visible row, or `None` when that row is filtered out.
    #[inline]
    pub fn visible_of(&self, underlying: u32) -> Option<usize> {
        self.rows.binary_search(&underlying).ok()
    }

    /// Visible position of the first kept row at or after `underlying`.
    ///
    /// Used to keep the viewport and the selection anchored to something
    /// sensible when the filter is switched on while parked on a row that does
    /// not survive it. Returns `len()` when nothing at or after it is kept.
    pub fn visible_at_or_after(&self, underlying: u32) -> usize {
        self.rows.partition_point(|&r| r < underlying)
    }

    /// The underlying rows for a half-open range of visible rows.
    ///
    /// This is the per-frame entry point: a borrowed subslice, so painting a
    /// filtered viewport allocates nothing. Out-of-range ends are clamped.
    #[inline]
    pub fn window(&self, first_visible: usize, last_visible: usize) -> &[u32] {
        let lo = first_visible.min(self.rows.len());
        let hi = last_visible.clamp(lo, self.rows.len());
        &self.rows[lo..hi]
    }

    /// All underlying rows, ascending.
    pub fn rows(&self) -> &[u32] {
        &self.rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(pairs: &[(u32, u32)]) -> Vec<CellRef> {
        pairs.iter().map(|&(r, c)| CellRef::new(r, c)).collect()
    }

    #[test]
    fn maps_visible_positions_to_original_rows() {
        // Matches on rows 3, 3, 9, 40 -> three visible rows.
        let f = RowFilter::from_matches(&cells(&[(3, 0), (3, 2), (9, 1), (40, 0)]), false, 4);
        assert_eq!(f.len(), 3, "duplicate row 3 collapses to one visible row");
        assert_eq!(f.underlying(0), Some(3));
        assert_eq!(f.underlying(1), Some(9));
        assert_eq!(f.underlying(2), Some(40));
        assert_eq!(f.underlying(3), None);
    }

    #[test]
    fn row_headers_show_original_numbers() {
        // Acceptance criterion: the header for visible row 0 must read "4"
        // (row 3, one-based), not "1".
        let f = RowFilter::from_matches(&cells(&[(3, 0), (100, 0)]), false, 2);
        let header_for = |v: usize| f.underlying(v).map(|r| r + 1);
        assert_eq!(header_for(0), Some(4));
        assert_eq!(header_for(1), Some(101));
    }

    #[test]
    fn round_trips_both_directions() {
        let f = RowFilter::from_matches(&cells(&[(0, 0), (7, 0), (7, 1), (12, 3)]), false, 4);
        for v in 0..f.len() {
            let u = f.underlying(v).unwrap();
            assert_eq!(f.visible_of(u), Some(v), "visible {v} did not round-trip");
        }
        assert_eq!(f.visible_of(1), None, "row 1 has no match and is hidden");
        assert_eq!(f.visible_of(99), None);
    }

    #[test]
    fn edit_write_through_targets_the_underlying_row() {
        // The write-through contract: a click on visible row 2 must produce a
        // CellRef addressing the ORIGINAL row, so an edit lands there.
        let f = RowFilter::from_matches(&cells(&[(5, 0), (11, 0), (900, 0)]), false, 3);
        let clicked_visible = 2usize;
        let target = CellRef::new(f.underlying(clicked_visible).unwrap(), 4);
        assert_eq!(target.row, 900, "edit would have hit the wrong row");
        assert_eq!(target.col, 4);
    }

    #[test]
    fn window_is_a_borrowed_slice_not_a_copy() {
        let f =
            RowFilter::from_matches(&cells(&[(1, 0), (2, 0), (3, 0), (4, 0), (5, 0)]), false, 5);
        let w = f.window(1, 4);
        assert_eq!(w, &[2u32, 3, 4]);
        // Same backing allocation as the filter's own storage.
        assert!(std::ptr::eq(w.as_ptr(), f.rows()[1..].as_ptr()));
    }

    #[test]
    fn window_clamps_out_of_range_ends() {
        let f = RowFilter::from_matches(&cells(&[(1, 0), (2, 0)]), false, 2);
        assert_eq!(f.window(0, 999), &[1u32, 2]);
        assert_eq!(f.window(5, 999), &[] as &[u32]);
        assert_eq!(f.window(2, 1), &[] as &[u32], "reversed range is empty");
    }

    #[test]
    fn visible_at_or_after_finds_the_next_kept_row() {
        let f = RowFilter::from_matches(&cells(&[(10, 0), (20, 0), (30, 0)]), false, 3);
        assert_eq!(f.visible_at_or_after(0), 0);
        assert_eq!(f.visible_at_or_after(10), 0);
        assert_eq!(f.visible_at_or_after(11), 1);
        assert_eq!(f.visible_at_or_after(30), 2);
        assert_eq!(f.visible_at_or_after(31), 3, "past the end");
    }

    #[test]
    fn empty_matches_produce_an_empty_filter() {
        let f = RowFilter::from_matches(&[], false, 0);
        assert!(f.is_empty());
        assert_eq!(f.underlying(0), None);
        assert_eq!(f.window(0, 10), &[] as &[u32]);
        assert_eq!(f.visible_at_or_after(5), 0);
    }

    #[test]
    fn truncation_flag_survives_into_the_mapping() {
        // The pitfall from issue #6: a capped result set must not silently
        // present itself as the complete picture.
        let results = SearchResults {
            matches: cells(&[(1, 0), (2, 0)]),
            total: 5_000_000,
            truncated: true,
            ..Default::default()
        };
        let f = RowFilter::from_results(&results);
        assert!(f.truncated(), "UI must be able to warn about this");
        assert_eq!(f.total_matches(), 5_000_000);
        assert_eq!(f.len(), 2, "only the capped prefix is mappable");
    }

    #[test]
    fn untruncated_results_do_not_warn() {
        let results = SearchResults {
            matches: cells(&[(1, 0)]),
            total: 1,
            truncated: false,
            ..Default::default()
        };
        assert!(!RowFilter::from_results(&results).truncated());
    }

    #[test]
    fn addressing_stays_exact_at_200m_rows() {
        // The grid scrolls by f64 row index. Mapping through this layer must
        // not lose a row at 200M scale — check the mapping AND the f64 round
        // trip the grid will perform on the value it returns.
        let deep = cells(&[(0, 0), (199_999_998, 0), (199_999_999, 3)]);
        let f = RowFilter::from_matches(&deep, false, 3);
        assert_eq!(f.len(), 3);
        for v in 0..f.len() {
            let u = f.underlying(v).unwrap();
            let as_scroll = u as f64;
            assert_eq!(
                as_scroll.floor() as u32,
                u,
                "row {u} lost precision through the f64 scroll index"
            );
            assert_eq!(f.visible_of(u), Some(v));
        }
        // Adjacent deep rows stay distinct — an f32 index would collapse them.
        assert_ne!(f.underlying(1), f.underlying(2));
        assert!(
            199_999_998u32 as f32 as u32 == 199_999_999u32 as f32 as u32,
            "f32 really does collapse these, which is why we do not use it"
        );
    }

    #[test]
    fn mapping_cost_is_independent_of_sheet_size() {
        // 100k matches (the UI cap) spread across a 200M-row sheet: building
        // the mapping touches the match list only, never the sheet.
        let matches: Vec<CellRef> = (0..100_000u32)
            .map(|i| CellRef::new(i.saturating_mul(2000), 0))
            .collect();
        let f = RowFilter::from_matches(&matches, true, 100_000);
        assert_eq!(f.len(), 100_000);
        // A viewport-sized window is what a frame actually touches.
        let w = f.window(50_000, 50_060);
        assert_eq!(w.len(), 60, "a frame reads ~60 rows, not 100k");
    }
}
