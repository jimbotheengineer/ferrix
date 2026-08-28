//! Rectangular cell selection.
//!
//! ## Why bounds, never a list
//!
//! A user can select an entire column of a 200M-row sheet. Materializing that
//! as a `Vec<CellRef>` would allocate 1.6 GB to represent something two corners
//! describe exactly. So a selection is always stored as an **anchor** (where
//! the selection started) and a **cursor** (where it currently ends), and every
//! operation over it is either O(1) or bounded by what is actually visible.
//!
//! The anchor/cursor pair is kept unnormalized on purpose: extending a
//! selection with Shift+Arrow has to grow from the anchor and can legitimately
//! move the cursor above or left of it. Normalization happens on read, via
//! [`Selection::bounds`].

use crate::CellRef;

/// A rectangular range of cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Selection {
    /// Where the selection started — fixed while extending.
    pub anchor: CellRef,
    /// The moving end. Equal to `anchor` for a single-cell selection.
    pub cursor: CellRef,
}

impl Default for Selection {
    fn default() -> Self {
        Self::single(CellRef::new(0, 0))
    }
}

impl Selection {
    #[inline]
    pub fn single(cell: CellRef) -> Self {
        Self {
            anchor: cell,
            cursor: cell,
        }
    }

    #[inline]
    pub fn new(anchor: CellRef, cursor: CellRef) -> Self {
        Self { anchor, cursor }
    }

    /// True when the selection covers exactly one cell.
    #[inline]
    pub fn is_single(&self) -> bool {
        self.anchor == self.cursor
    }

    /// Normalized bounds as `(top_left, bottom_right)`, inclusive.
    #[inline]
    pub fn bounds(&self) -> (CellRef, CellRef) {
        (
            CellRef::new(
                self.anchor.row.min(self.cursor.row),
                self.anchor.col.min(self.cursor.col),
            ),
            CellRef::new(
                self.anchor.row.max(self.cursor.row),
                self.anchor.col.max(self.cursor.col),
            ),
        )
    }

    #[inline]
    pub fn row_range(&self) -> (u32, u32) {
        let (a, b) = self.bounds();
        (a.row, b.row)
    }

    #[inline]
    pub fn col_range(&self) -> (u32, u32) {
        let (a, b) = self.bounds();
        (a.col, b.col)
    }

    /// Rows covered. `u64` because a full-column selection over a 200M-row
    /// sheet overflows nothing here but would in a `u32` count of cells.
    #[inline]
    pub fn row_count(&self) -> u64 {
        let (a, b) = self.row_range();
        (b - a) as u64 + 1
    }

    #[inline]
    pub fn col_count(&self) -> u64 {
        let (a, b) = self.col_range();
        (b - a) as u64 + 1
    }

    /// Total cells covered, as `u64` — a full column of 200M rows across 8
    /// columns is 1.6e9, which overflows `u32`.
    #[inline]
    pub fn cell_count(&self) -> u64 {
        self.row_count() * self.col_count()
    }

    #[inline]
    pub fn contains(&self, cell: CellRef) -> bool {
        let (tl, br) = self.bounds();
        cell.row >= tl.row && cell.row <= br.row && cell.col >= tl.col && cell.col <= br.col
    }

    /// Move both ends — a plain click or arrow key, collapsing any range.
    #[inline]
    pub fn move_to(&mut self, cell: CellRef) {
        self.anchor = cell;
        self.cursor = cell;
    }

    /// Move only the cursor — Shift+click or Shift+Arrow.
    #[inline]
    pub fn extend_to(&mut self, cell: CellRef) {
        self.cursor = cell;
    }

    /// Iterate the cells, row-major.
    ///
    /// Deliberately an iterator rather than a `Vec`: callers that must walk a
    /// selection (copy, clear) stream it, and callers that only need its size
    /// use `cell_count` without touching this at all. Still O(cells) to
    /// consume, so callers guard against absurd ranges before doing so.
    pub fn iter(&self) -> impl Iterator<Item = CellRef> + '_ {
        let (tl, br) = self.bounds();
        (tl.row..=br.row).flat_map(move |r| (tl.col..=br.col).map(move |c| CellRef::new(r, c)))
    }

    /// A1-style label for the status bar: `B3` or `B3:D7`.
    pub fn label(&self) -> String {
        let (tl, br) = self.bounds();
        if self.is_single() {
            tl.to_a1()
        } else {
            format!("{}:{}", tl.to_a1(), br.to_a1())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(r: u32, col: u32) -> CellRef {
        CellRef::new(r, col)
    }

    #[test]
    fn single_cell_selection() {
        let s = Selection::single(c(3, 4));
        assert!(s.is_single());
        assert_eq!(s.cell_count(), 1);
        assert_eq!(s.bounds(), (c(3, 4), c(3, 4)));
        assert!(s.contains(c(3, 4)));
        assert!(!s.contains(c(3, 5)));
    }

    #[test]
    fn bounds_normalize_regardless_of_drag_direction() {
        // Dragging up-left must describe the same rectangle as down-right.
        let down_right = Selection::new(c(1, 1), c(5, 3));
        let up_left = Selection::new(c(5, 3), c(1, 1));
        assert_eq!(down_right.bounds(), up_left.bounds());
        assert_eq!(down_right.bounds(), (c(1, 1), c(5, 3)));
        assert_eq!(down_right.cell_count(), up_left.cell_count());
    }

    #[test]
    fn extend_keeps_the_anchor_fixed() {
        let mut s = Selection::single(c(2, 2));
        s.extend_to(c(6, 5));
        assert_eq!(s.anchor, c(2, 2), "anchor must not move");
        assert_eq!(s.cursor, c(6, 5));
        // Extending backwards past the anchor is legal and re-normalizes.
        s.extend_to(c(0, 0));
        assert_eq!(s.anchor, c(2, 2));
        assert_eq!(s.bounds(), (c(0, 0), c(2, 2)));
    }

    #[test]
    fn move_to_collapses_a_range() {
        let mut s = Selection::new(c(0, 0), c(9, 9));
        assert_eq!(s.cell_count(), 100);
        s.move_to(c(4, 4));
        assert!(s.is_single());
        assert_eq!(s.cell_count(), 1);
    }

    #[test]
    fn dimensions_are_inclusive() {
        let s = Selection::new(c(2, 1), c(4, 3));
        assert_eq!(s.row_count(), 3, "rows 2,3,4");
        assert_eq!(s.col_count(), 3, "cols 1,2,3");
        assert_eq!(s.cell_count(), 9);
    }

    #[test]
    fn full_column_of_a_huge_sheet_costs_nothing_to_describe() {
        // The scale claim: selecting 200M rows is two corners, not 200M cells.
        let s = Selection::new(c(0, 0), c(199_999_999, 7));
        assert_eq!(s.row_count(), 200_000_000);
        assert_eq!(s.col_count(), 8);
        // 1.6e9 cells — must not overflow, and must not allocate.
        assert_eq!(s.cell_count(), 1_600_000_000);
        assert_eq!(std::mem::size_of::<Selection>(), 16, "two CellRefs");
        assert!(s.contains(c(199_999_999, 7)));
        assert!(!s.contains(c(0, 8)));
    }

    #[test]
    fn iter_walks_row_major() {
        let s = Selection::new(c(0, 0), c(1, 2));
        let cells: Vec<CellRef> = s.iter().collect();
        assert_eq!(
            cells,
            vec![c(0, 0), c(0, 1), c(0, 2), c(1, 0), c(1, 1), c(1, 2)]
        );
    }

    #[test]
    fn labels_read_like_a_spreadsheet() {
        assert_eq!(Selection::single(c(0, 0)).label(), "A1");
        assert_eq!(Selection::new(c(2, 1), c(6, 3)).label(), "B3:D7");
        // Reversed drag produces the same label.
        assert_eq!(Selection::new(c(6, 3), c(2, 1)).label(), "B3:D7");
    }
}
