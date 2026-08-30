//! Spill mechanics for dynamic arrays (#27 P2).
//!
//! P1 gave a formula an [`crate::array::EvalResult`] that can be an
//! [`crate::array::ArrayData`]. P2 is what lets such an `Array` result PAINT
//! into the neighbouring cells starting at the host, instead of collapsing to
//! its top-left cell.
//!
//! ## What lives here, and what does not
//!
//! This module is the **pure planner**: given a host cell, the array its
//! formula produced, and a way to ask "is this target cell a blocker?", it
//! computes one of two outcomes —
//!
//! * [`SpillPlan::Spilled`] — the rectangle the array covers, plus the scalar
//!   projection each covered cell should display. Each projection is a plain
//!   16-byte [`Value`]; the array bytes are owned ONCE by the host, never
//!   copied per cell. This is the scale invariant the whole feature rests on:
//!   spill memory is bounded by the RESULT extent, never by the sheet.
//! * [`SpillPlan::Blocked`] — the address of the first occupied cell in the
//!   target rectangle. A `#SPILL!` with no recoverable blocker is a dead end
//!   for the user, so the blocker address is a first-class part of the result,
//!   not a detail dropped on the floor.
//!
//! It holds NO state and reads NO sheet directly. The caller supplies a
//! `is_blocked` predicate, which is what keeps the merged-cell rule out of
//! here: to the planner a merged region is just another occupied cell, exactly
//! as the acceptance criteria require ("the merge is a blocker like any other
//! occupied cell"). The stateful region store and the workbook wiring live
//! beside this, consuming what it returns.

use ferrix_core::{CellRef, Value};

use crate::array::ArrayData;

/// The inclusive rectangle a spilled array covers, its top-left at the host.
///
/// Self-contained rather than reusing `ferrix_core::TableRange` so the planner
/// stays a pure formula-crate concept — a spill rect is defined by a host and
/// an array's extent, and never needs the table machinery. Coordinates are
/// `(row, col)` with `end` inclusive, matching how the array's own `rows`/
/// `cols` count cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpillRect {
    pub top_left: CellRef,
    pub rows: u32,
    pub cols: u32,
}

impl SpillRect {
    /// Build the rectangle an `rows x cols` array rooted at `host` covers.
    #[inline]
    pub fn new(host: CellRef, rows: u32, cols: u32) -> Self {
        debug_assert!(rows >= 1 && cols >= 1, "a spill rect is at least 1x1");
        Self {
            top_left: host,
            rows,
            cols,
        }
    }

    /// The bottom-right cell of the (inclusive) rectangle.
    #[inline]
    pub fn bottom_right(&self) -> CellRef {
        CellRef::new(
            self.top_left.row + self.rows - 1,
            self.top_left.col + self.cols - 1,
        )
    }

    /// Does the rectangle cover `cell`?
    #[inline]
    pub fn contains(&self, cell: CellRef) -> bool {
        let br = self.bottom_right();
        cell.row >= self.top_left.row
            && cell.row <= br.row
            && cell.col >= self.top_left.col
            && cell.col <= br.col
    }

    /// Is `cell` the host — the one cell that always holds the formula itself
    /// and is never a spilled projection?
    #[inline]
    pub fn is_host(&self, cell: CellRef) -> bool {
        cell == self.top_left
    }

    /// Every cell the rectangle covers, in row-major reading order.
    ///
    /// Bounded by `rows * cols` — the result extent — so iterating a spill is
    /// as cheap as the array is small, whatever the sheet's height.
    pub fn cells(&self) -> impl Iterator<Item = CellRef> + '_ {
        let (r0, c0) = (self.top_left.row, self.top_left.col);
        (0..self.rows)
            .flat_map(move |dr| (0..self.cols).map(move |dc| CellRef::new(r0 + dr, c0 + dc)))
    }
}

/// The outcome of planning a spill.
#[derive(Clone, PartialEq, Debug)]
pub enum SpillPlan {
    /// The array spilled. `rect` is what it covers; `projections` is the
    /// scalar value to write into EACH covered cell (host included, in
    /// row-major order). The host's own projection is the array's top-left
    /// cell — so a spilled host displays its first element, exactly like
    /// Excel, while still owning the formula and the array behind the scenes.
    Spilled {
        rect: SpillRect,
        projections: Vec<(CellRef, Value)>,
    },
    /// The array could not spill: `blocker` is the first occupied cell in the
    /// target rectangle, in row-major order. The host cell shows `#SPILL!` and
    /// this address is what the hover/error surfaces.
    Blocked { blocker: CellRef },
}

/// Plan the spill of `array` rooted at `host`.
///
/// `is_blocked(cell)` answers, for each cell the array would cover OTHER than
/// the host: is this cell occupied by something that must not be overwritten?
/// The caller wires it to mean "a non-empty value the user put there, or a
/// merged region" — and, crucially, to return `false` for cells this same
/// host already owns from a previous spill, so a re-spill never blocks on its
/// own old projection.
///
/// The host cell itself is NEVER passed to `is_blocked`: it holds the formula
/// and is the array's anchor, so it cannot block its own spill.
///
/// A 1x1 array always spills (there is nothing but the host to place), which
/// is what makes a scalar-shaped array result behave like an ordinary formula.
///
/// Scan order is row-major, so the reported blocker is the top-most then
/// left-most occupied cell — a stable, explainable choice rather than whatever
/// a hash map happened to yield first.
pub fn plan_spill<F>(host: CellRef, array: &ArrayData, mut is_blocked: F) -> SpillPlan
where
    F: FnMut(CellRef) -> bool,
{
    let rect = SpillRect::new(host, array.rows(), array.cols());

    // First pass: refuse before writing anything if ANY covered cell (other
    // than the host) is occupied. All-or-nothing, like a protected bulk edit:
    // a spill that painted around a blocker would be a plausibly-shaped, wrong
    // result, and Excel's rule is the whole array or none of it.
    for cell in rect.cells() {
        if rect.is_host(cell) {
            continue;
        }
        if is_blocked(cell) {
            return SpillPlan::Blocked { blocker: cell };
        }
    }

    // Second pass: the array spilled. Project each covered cell to its scalar.
    let mut projections = Vec::with_capacity(rect.rows as usize * rect.cols as usize);
    for dr in 0..rect.rows {
        for dc in 0..rect.cols {
            let cell = CellRef::new(host.row + dr, host.col + dc);
            projections.push((cell, array.get(dr, dc)));
        }
    }
    SpillPlan::Spilled { rect, projections }
}

/// The live state of one host's spill: either a successful region or a block.
#[derive(Clone, PartialEq, Debug)]
enum RegionState {
    /// A live spill: the array is owned HERE, once, and covers `rect`. The
    /// covered cells hold scalar projections in the store; only this struct
    /// holds the array bytes.
    Spilled { rect: SpillRect, array: ArrayData },
    /// A blocked host: it shows `#SPILL!` and this is the blocker's address,
    /// kept so the hover/error can name it. No rectangle is claimed — a
    /// blocked spill owns no cells.
    Blocked { blocker: CellRef },
}

/// Every spilling formula's region on ONE sheet, keyed by host cell.
///
/// ## Why keyed by host, and scanned rather than reverse-indexed
///
/// The design mandate (P1) is that a spilled cell carries a lightweight
/// "owned by host@A1" marker PER REGION, never per-cell array bytes. So this
/// store keeps one entry per HOST — a workbook quantity, like merges or
/// defined names — and answers "who owns this cell?" by testing the covered
/// cell against each region's rectangle.
///
/// That scan is O(spilling formulas), never O(cells): a 200M-row sheet with
/// three spilling formulas tests three rectangles. It runs on edit — a rare
/// event — so it is never on a hot path, and it avoids a per-covered-cell
/// reverse map that would make a tall spill cost memory per row. The array's
/// own memory is bounded by its result extent and lives once, in the region.
#[derive(Clone, Debug, Default)]
pub struct SpillRegions {
    by_host: std::collections::HashMap<CellRef, RegionState>,
}

impl SpillRegions {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.by_host.is_empty()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.by_host.len()
    }

    /// Record that `host` spilled `array` across `rect`. Replaces any previous
    /// state for that host (a re-spill supersedes the old region cleanly).
    pub fn set_spilled(&mut self, host: CellRef, rect: SpillRect, array: ArrayData) {
        self.by_host
            .insert(host, RegionState::Spilled { rect, array });
    }

    /// Record that `host` is blocked by the cell `blocker`. Replaces any
    /// previous state for that host.
    pub fn set_blocked(&mut self, host: CellRef, blocker: CellRef) {
        self.by_host.insert(host, RegionState::Blocked { blocker });
    }

    /// Forget `host`'s region entirely — used when its formula is deleted or
    /// replaced by a scalar. Returns the covered rectangle if it had one, so
    /// the caller can clear the projections it painted.
    pub fn clear(&mut self, host: CellRef) -> Option<SpillRect> {
        match self.by_host.remove(&host) {
            Some(RegionState::Spilled { rect, .. }) => Some(rect),
            _ => None,
        }
    }

    /// The live rectangle `host` spilled across, if it is spilling.
    pub fn rect_of(&self, host: CellRef) -> Option<SpillRect> {
        match self.by_host.get(&host) {
            Some(RegionState::Spilled { rect, .. }) => Some(*rect),
            _ => None,
        }
    }

    /// The array `host` owns, if it is spilling.
    pub fn array_of(&self, host: CellRef) -> Option<&ArrayData> {
        match self.by_host.get(&host) {
            Some(RegionState::Spilled { array, .. }) => Some(array),
            _ => None,
        }
    }

    /// The blocker address for a blocked host — the answer to "why #SPILL!?".
    /// `None` when the host is not blocked (it may be spilling, or absent).
    pub fn blocker_of(&self, host: CellRef) -> Option<CellRef> {
        match self.by_host.get(&host) {
            Some(RegionState::Blocked { blocker }) => Some(*blocker),
            _ => None,
        }
    }

    /// The host whose LIVE spill covers `cell`, if any.
    ///
    /// Scans the regions (a workbook quantity). A blocked host owns no cells,
    /// so it never answers here. The host cell of a live spill DOES answer —
    /// it is part of its own region — which callers distinguish with
    /// [`SpillRect::is_host`] when they need "spilled but not the host".
    pub fn owner_of(&self, cell: CellRef) -> Option<CellRef> {
        self.by_host.iter().find_map(|(host, state)| match state {
            RegionState::Spilled { rect, .. } if rect.contains(cell) => Some(*host),
            _ => None,
        })
    }

    /// Is `cell` a spilled projection that must REFUSE a direct edit?
    ///
    /// True for a covered cell that is NOT its host: the host holds the
    /// formula and stays editable (that is how a user deletes a spill), while
    /// the cells it painted into are read-only, exactly as in Excel.
    pub fn is_locked_spill_cell(&self, cell: CellRef) -> bool {
        match self.owner_of(cell) {
            Some(host) => host != cell,
            None => false,
        }
    }

    /// Every host that currently has a region (spilled or blocked), sorted for
    /// determinism. Used when a structural change forces a full re-plan.
    pub fn hosts(&self) -> Vec<CellRef> {
        let mut v: Vec<CellRef> = self.by_host.keys().copied().collect();
        v.sort_by_key(|c| (c.row, c.col));
        v
    }

    /// Approximate heap cost, so a status bar can show that spilling stays
    /// bounded by result extent, not by sheet size.
    pub fn heap_bytes(&self) -> usize {
        self.by_host
            .values()
            .map(|s| {
                std::mem::size_of::<CellRef>()
                    + match s {
                        RegionState::Spilled { array, .. } => {
                            array.rows() as usize
                                * array.cols() as usize
                                * std::mem::size_of::<Value>()
                        }
                        RegionState::Blocked { .. } => 0,
                    }
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::ErrorKind;

    fn v(n: f64) -> Value {
        Value::Number(n)
    }

    /// A column array `[1;2;3]` (3 rows x 1 col).
    fn col3() -> ArrayData {
        ArrayData::from_cells(3, 1, vec![v(1.0), v(2.0), v(3.0)])
    }

    #[test]
    fn spill_rect_geometry() {
        let r = SpillRect::new(CellRef::new(0, 0), 3, 1);
        assert_eq!(r.bottom_right(), CellRef::new(2, 0));
        assert!(r.contains(CellRef::new(1, 0)));
        assert!(!r.contains(CellRef::new(3, 0)));
        assert!(r.is_host(CellRef::new(0, 0)));
        assert!(!r.is_host(CellRef::new(1, 0)));
        let cells: Vec<_> = r.cells().collect();
        assert_eq!(
            cells,
            vec![CellRef::new(0, 0), CellRef::new(1, 0), CellRef::new(2, 0)]
        );
    }

    #[test]
    fn rect_cells_are_row_major() {
        // 2x3 rooted at B2 (row 1, col 1): row-major reading order.
        let r = SpillRect::new(CellRef::new(1, 1), 2, 3);
        let cells: Vec<_> = r.cells().collect();
        assert_eq!(
            cells,
            vec![
                CellRef::new(1, 1),
                CellRef::new(1, 2),
                CellRef::new(1, 3),
                CellRef::new(2, 1),
                CellRef::new(2, 2),
                CellRef::new(2, 3),
            ]
        );
    }

    #[test]
    fn clear_spill_projects_every_cell_from_the_host() {
        // =SEQUENCE(10)-shaped: A1 holds a 10x1 array. It fills A1:A10, and
        // every covered cell gets its projection of the array.
        let data = ArrayData::from_cells(10, 1, (1..=10).map(|n| v(n as f64)).collect());
        let plan = plan_spill(CellRef::new(0, 0), &data, |_| false);
        match plan {
            SpillPlan::Spilled { rect, projections } => {
                assert_eq!(rect.top_left, CellRef::new(0, 0));
                assert_eq!(rect.bottom_right(), CellRef::new(9, 0));
                assert_eq!(projections.len(), 10);
                // The host shows the array's top-left, like Excel.
                assert_eq!(projections[0], (CellRef::new(0, 0), v(1.0)));
                assert_eq!(projections[9], (CellRef::new(9, 0), v(10.0)));
            }
            other => panic!("expected spill, got {other:?}"),
        }
    }

    #[test]
    fn a_value_in_the_rect_blocks_and_names_the_blocker() {
        // A2 is occupied. The 3x1 array rooted at A1 cannot spill; the blocker
        // A2 must be recoverable from the plan.
        let blocker = CellRef::new(1, 0);
        let plan = plan_spill(CellRef::new(0, 0), &col3(), |c| c == blocker);
        assert_eq!(plan, SpillPlan::Blocked { blocker });
    }

    #[test]
    fn the_host_is_never_its_own_blocker() {
        // Even if the predicate would claim the host is occupied (it holds the
        // formula, after all), the host cannot block its own spill.
        let host = CellRef::new(0, 0);
        let plan = plan_spill(host, &col3(), |c| c == host);
        assert!(matches!(plan, SpillPlan::Spilled { .. }));
    }

    #[test]
    fn first_blocker_in_row_major_order_wins() {
        // A 2x2 array rooted at A1 covers A1,B1,A2,B2. Both B1 and A2 are
        // occupied; row-major order names B1 (row 0) before A2 (row 1).
        let data = ArrayData::from_cells(2, 2, vec![v(1.0), v(2.0), v(3.0), v(4.0)]);
        let b1 = CellRef::new(0, 1);
        let a2 = CellRef::new(1, 0);
        let plan = plan_spill(CellRef::new(0, 0), &data, |c| c == b1 || c == a2);
        assert_eq!(plan, SpillPlan::Blocked { blocker: b1 });
    }

    #[test]
    fn a_merged_region_blocks_exactly_like_an_occupied_cell() {
        // The planner does not know what a merge IS; the caller reports the
        // merged cell as blocked. So a spill overlapping a merge is a #SPILL!
        // and the merge is untouched — proven here at the planner boundary,
        // and end-to-end in the workbook integration tests.
        let merged_cell = CellRef::new(2, 0);
        let plan = plan_spill(CellRef::new(0, 0), &col3(), |c| c == merged_cell);
        assert_eq!(
            plan,
            SpillPlan::Blocked {
                blocker: merged_cell
            }
        );
    }

    #[test]
    fn a_1x1_array_always_spills() {
        // Nothing to place but the host, so even a fully-occupied neighbourhood
        // cannot block it.
        let data = ArrayData::scalar(v(42.0));
        let plan = plan_spill(CellRef::new(5, 5), &data, |_| true);
        match plan {
            SpillPlan::Spilled { rect, projections } => {
                assert_eq!(rect.rows, 1);
                assert_eq!(rect.cols, 1);
                assert_eq!(projections, vec![(CellRef::new(5, 5), v(42.0))]);
            }
            other => panic!("expected spill, got {other:?}"),
        }
    }

    #[test]
    fn projections_carry_error_values_through_unchanged() {
        // An array element that is an error spills as that error, not swallowed.
        let data = ArrayData::from_cells(2, 1, vec![v(1.0), Value::Error(ErrorKind::DivZero)]);
        let plan = plan_spill(CellRef::new(0, 0), &data, |_| false);
        match plan {
            SpillPlan::Spilled { projections, .. } => {
                assert_eq!(projections[1].1, Value::Error(ErrorKind::DivZero));
            }
            other => panic!("expected spill, got {other:?}"),
        }
    }

    // --- SpillRegions store ------------------------------------------------

    #[test]
    fn store_records_a_spill_and_reports_ownership() {
        let mut regions = SpillRegions::new();
        let host = CellRef::new(0, 0);
        let rect = SpillRect::new(host, 3, 1);
        regions.set_spilled(host, rect, col3());

        // The host owns every covered cell, including itself.
        assert_eq!(regions.owner_of(CellRef::new(0, 0)), Some(host));
        assert_eq!(regions.owner_of(CellRef::new(1, 0)), Some(host));
        assert_eq!(regions.owner_of(CellRef::new(2, 0)), Some(host));
        // A cell outside the rect is owned by nobody.
        assert_eq!(regions.owner_of(CellRef::new(3, 0)), None);
        assert_eq!(regions.rect_of(host), Some(rect));
        assert!(regions.array_of(host).is_some());
    }

    #[test]
    fn only_non_host_covered_cells_refuse_edits() {
        let mut regions = SpillRegions::new();
        let host = CellRef::new(0, 0);
        regions.set_spilled(host, SpillRect::new(host, 3, 1), col3());

        // The host holds the formula and stays editable; the cells it painted
        // into are locked.
        assert!(!regions.is_locked_spill_cell(host));
        assert!(regions.is_locked_spill_cell(CellRef::new(1, 0)));
        assert!(regions.is_locked_spill_cell(CellRef::new(2, 0)));
        // A cell outside the spill is not locked.
        assert!(!regions.is_locked_spill_cell(CellRef::new(9, 9)));
    }

    #[test]
    fn a_blocked_host_owns_no_cells_but_names_its_blocker() {
        let mut regions = SpillRegions::new();
        let host = CellRef::new(0, 0);
        let blocker = CellRef::new(1, 0);
        regions.set_blocked(host, blocker);

        // A blocked host claims no rectangle, so it locks nothing.
        assert_eq!(regions.owner_of(CellRef::new(1, 0)), None);
        assert!(!regions.is_locked_spill_cell(CellRef::new(1, 0)));
        assert_eq!(regions.rect_of(host), None);
        // But the blocker address is recoverable — no dead-end #SPILL!.
        assert_eq!(regions.blocker_of(host), Some(blocker));
    }

    #[test]
    fn clearing_a_region_returns_its_rect_and_frees_ownership() {
        let mut regions = SpillRegions::new();
        let host = CellRef::new(0, 0);
        let rect = SpillRect::new(host, 3, 1);
        regions.set_spilled(host, rect, col3());

        assert_eq!(regions.clear(host), Some(rect));
        assert!(regions.is_empty());
        assert_eq!(regions.owner_of(CellRef::new(1, 0)), None);
        // Clearing a blocked host returns no rect (it owned no cells).
        regions.set_blocked(host, CellRef::new(1, 0));
        assert_eq!(regions.clear(host), None);
        assert!(regions.is_empty());
    }

    #[test]
    fn a_respill_replaces_the_old_region_cleanly() {
        let mut regions = SpillRegions::new();
        let host = CellRef::new(0, 0);
        // First a 3x1 spill, then it re-plans to 2x1 (a shorter array).
        regions.set_spilled(host, SpillRect::new(host, 3, 1), col3());
        let shorter = ArrayData::from_cells(2, 1, vec![v(1.0), v(2.0)]);
        regions.set_spilled(host, SpillRect::new(host, 2, 1), shorter);

        assert_eq!(regions.len(), 1);
        // The cell the old spill covered but the new one does not is released.
        assert_eq!(regions.owner_of(CellRef::new(2, 0)), None);
        assert_eq!(regions.owner_of(CellRef::new(1, 0)), Some(host));
    }

    #[test]
    fn heap_cost_is_bounded_by_result_extent_not_sheet_height() {
        // A spill rooted a million rows down still costs only its result's
        // worth of values — the scale invariant, made measurable.
        let mut regions = SpillRegions::new();
        let host = CellRef::new(1_000_000, 0);
        let data = ArrayData::from_cells(5, 1, (0..5).map(|n| v(n as f64)).collect());
        regions.set_spilled(host, SpillRect::new(host, 5, 1), data);
        // 5 values + one key, nowhere near a per-row cost.
        assert!(
            regions.heap_bytes() < 1_000,
            "spill of 5 cells cost {} bytes",
            regions.heap_bytes()
        );
    }
}
