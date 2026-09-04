//! Copy-on-write edit overlay.
//!
//! The base data may be an immutable memory-mapped file — for a 10GB dataset
//! we cannot mutate it in place, and would not want to even if we could. So
//! edits live in a sparse in-memory layer that is consulted *before* the base:
//!
//! ```text
//!   get(cell) -> overlay.get(cell).unwrap_or_else(|| base.get(cell))
//! ```
//!
//! This gives three properties that matter at scale:
//!
//! 1. **Editing is O(edits), not O(rows).** A million-row file with three
//!    edited cells costs three HashMap entries.
//! 2. **The base can be read-only.** Works identically over an in-RAM `Sheet`
//!    or an mmap'd file the OS may evict at any time.
//! 3. **Undo is trivial.** Reverting an edit means restoring the previous
//!    overlay entry (or removing it), never touching the base.

use std::collections::HashMap;

use crate::{CellRef, StrId, StringArena, Value};

/// What the user typed into a cell, before evaluation.
#[derive(Clone, Debug, PartialEq)]
pub enum CellInput {
    /// A literal value: number, text, bool.
    Literal(Value),
    /// A formula, stored as its source text (e.g. "=SUM(A1:A10)"). The
    /// computed result is cached alongside it.
    Formula { src: String, cached: Value },
}

impl CellInput {
    /// The value this cell currently displays.
    pub fn value(&self) -> Value {
        match self {
            CellInput::Literal(v) => *v,
            CellInput::Formula { cached, .. } => *cached,
        }
    }

    pub fn is_formula(&self) -> bool {
        matches!(self, CellInput::Formula { .. })
    }

    /// Source text for the formula bar: formulas show their source, literals
    /// show nothing (the caller renders the value instead).
    pub fn formula_src(&self) -> Option<&str> {
        match self {
            CellInput::Formula { src, .. } => Some(src),
            _ => None,
        }
    }
}

/// A single cell's before/after state, recorded by a batch apply.
///
/// This is the unit an undo entry is built from. Making it a core type rather
/// than a UI-private one is what lets a bulk operation be assembled by the
/// overlay itself — the layer that actually knows what was there before.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlayChange {
    pub cell: CellRef,
    pub before: Option<CellInput>,
    pub after: Option<CellInput>,
}

/// Sparse edit layer over an immutable base.
///
/// `Clone` is what lets a long export run off the UI thread: the exporter
/// takes a snapshot of the overlay rather than borrowing the live workbook,
/// so the user can keep editing while 200M rows stream to disk. The clone
/// costs `heap_bytes()`, which the caller admits against the memory budget
/// before taking it.
#[derive(Clone, Debug, Default)]
pub struct EditOverlay {
    cells: HashMap<CellRef, CellInput>,
    /// Strings interned by edits. Kept separate from the base arena so the
    /// base can stay memory-mapped and read-only.
    arena: StringArena,
    /// Rows/cols the user extended the sheet to by editing past the end.
    extra_rows: usize,
    extra_cols: usize,
    /// Bumped on every mutation that changes what would be serialized.
    ///
    /// Autosave uses this to answer "has anything changed since the last
    /// tick?" in O(1). Comparing overlays cell by cell would make an idle
    /// timer cost as much as a save, which is exactly what the timer exists
    /// to avoid; a counter makes the common case — nothing typed in the last
    /// 30 seconds — free.
    revision: u64,
}

impl EditOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Monotonic edit counter. Two reads returning the same value mean the
    /// serialized form of this overlay is unchanged between them.
    #[inline]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Look up an edited cell. `None` means "defer to the base".
    #[inline]
    pub fn get(&self, cell: CellRef) -> Option<&CellInput> {
        self.cells.get(&cell)
    }

    #[inline]
    pub fn value(&self, cell: CellRef) -> Option<Value> {
        self.cells.get(&cell).map(|c| c.value())
    }

    pub fn has_formula(&self, cell: CellRef) -> bool {
        self.cells.get(&cell).is_some_and(|c| c.is_formula())
    }

    /// Write an edit, returning whatever was there before (for undo).
    pub fn set(&mut self, cell: CellRef, input: CellInput) -> Option<CellInput> {
        self.extra_rows = self.extra_rows.max(cell.row as usize + 1);
        self.extra_cols = self.extra_cols.max(cell.col as usize + 1);
        self.revision = self.revision.wrapping_add(1);
        self.cells.insert(cell, input)
    }

    /// Remove an edit, reverting the cell to its base value.
    pub fn clear(&mut self, cell: CellRef) -> Option<CellInput> {
        let prev = self.cells.remove(&cell);
        if prev.is_some() {
            self.revision = self.revision.wrapping_add(1);
        }
        prev
    }

    /// Restore a previous state exactly — the undo primitive.
    pub fn restore(&mut self, cell: CellRef, prev: Option<CellInput>) {
        match prev {
            Some(input) => {
                self.set(cell, input);
            }
            None => {
                self.clear(cell);
            }
        }
    }

    /// One cell's before/after, as recorded by [`EditOverlay::apply_batch`].
    ///
    /// `None` on either side means "no overlay entry" — the cell deferred to
    /// the base. Undo needs that distinction: restoring `Some(Empty)` leaves a
    /// cell explicitly blanked, while restoring `None` gives the base value
    /// back.
    ///
    /// Batch application is what keeps a bulk edit one undo step: the caller
    /// collects these into a single entry rather than pushing one per cell.
    pub fn apply_batch<I>(&mut self, edits: I, changes: &mut Vec<OverlayChange>) -> usize
    where
        I: IntoIterator<Item = (CellRef, Option<CellInput>)>,
    {
        let start = changes.len();
        for (cell, after) in edits {
            let before = self.get(cell).cloned();
            match &after {
                Some(input) => {
                    self.set(cell, input.clone());
                }
                None => {
                    self.clear(cell);
                }
            }
            changes.push(OverlayChange {
                cell,
                before,
                after,
            });
        }
        changes.len() - start
    }

    /// Undo a batch produced by [`EditOverlay::apply_batch`].
    ///
    /// Reverse order, so overlapping writes to the same cell unwind exactly as
    /// they were made.
    pub fn revert_batch(&mut self, changes: &[OverlayChange]) {
        for ch in changes.iter().rev() {
            self.restore(ch.cell, ch.before.clone());
        }
    }

    /// Intern text typed into the overlay. The returned id carries
    /// [`crate::arena::OVERLAY_TEXT_TAG`], so a composite view can tell an
    /// overlay string from a base string with the same raw index — the
    /// collision that used to make formulas misread base text columns the
    /// moment any text was typed anywhere.
    pub fn intern(&mut self, s: &str) -> StrId {
        let raw = self.arena.intern(s);
        debug_assert!(
            raw.0 & (crate::arena::OVERLAY_TEXT_TAG | crate::arena::FORMULA_TEXT_TAG) == 0,
            "overlay arena produced an id colliding with a provenance tag"
        );
        StrId(raw.0 | crate::arena::OVERLAY_TEXT_TAG)
    }

    /// Resolve an id THIS overlay produced (tagged), or a formula-text id.
    /// A plain untagged id belongs to the base and returns `None`, which is
    /// what lets `SheetView::resolve` route overlay-first without ever
    /// serving a base id from the wrong arena.
    pub fn resolve(&self, id: StrId) -> Option<&str> {
        if id.0 & crate::arena::FORMULA_TEXT_TAG != 0 {
            // Formula-produced text: the arena routes it to the process-wide
            // interner regardless of which arena is asked.
            return self.arena.resolve(id);
        }
        if id.0 & crate::arena::OVERLAY_TEXT_TAG != 0 {
            return self
                .arena
                .resolve(StrId(id.0 & !crate::arena::OVERLAY_TEXT_TAG));
        }
        None
    }

    /// Update just the cached result of a formula, leaving its source intact.
    /// Used by recalc, which must not disturb what the user typed.
    pub fn update_cached(&mut self, cell: CellRef, value: Value) {
        if let Some(CellInput::Formula { cached, .. }) = self.cells.get_mut(&cell) {
            // Only a real change counts: recalc runs far more often than the
            // values actually move, and treating every recalc as an edit would
            // make an idle autosave timer rewrite the file forever.
            if *cached != value {
                *cached = value;
                self.revision = self.revision.wrapping_add(1);
            }
        }
    }

    /// Every formula cell, for dependency-graph construction.
    pub fn formula_cells(&self) -> impl Iterator<Item = (CellRef, &str)> + '_ {
        self.cells
            .iter()
            .filter_map(|(cell, input)| input.formula_src().map(|src| (*cell, src)))
    }

    pub fn edited_cells(&self) -> impl Iterator<Item = (&CellRef, &CellInput)> {
        self.cells.iter()
    }

    /// How far the sheet extends because of edits past the base's extent.
    pub fn extent(&self) -> (usize, usize) {
        (self.extra_rows, self.extra_cols)
    }

    /// The overlay's own string arena, for serialization.
    pub fn arena(&self) -> &StringArena {
        &self.arena
    }

    /// Rebuild an overlay from saved parts. Used by the loader; the extent is
    /// recomputed from the cells rather than trusted from the file.
    pub fn from_parts(cells: HashMap<CellRef, CellInput>, arena: StringArena) -> Self {
        let mut extra_rows = 0usize;
        let mut extra_cols = 0usize;
        for cell in cells.keys() {
            extra_rows = extra_rows.max(cell.row as usize + 1);
            extra_cols = extra_cols.max(cell.col as usize + 1);
        }
        Self {
            cells,
            arena,
            extra_rows,
            extra_cols,
            revision: 0,
        }
    }

    /// Approximate heap cost — surfaced in the status bar so the user can see
    /// that editing a huge file stays cheap.
    pub fn heap_bytes(&self) -> usize {
        self.cells.capacity()
            * (std::mem::size_of::<CellRef>() + std::mem::size_of::<CellInput>() + 16)
            + self.arena.heap_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_overlay_defers_everything() {
        let o = EditOverlay::new();
        assert!(o.is_empty());
        assert_eq!(o.get(CellRef::new(0, 0)), None);
        assert_eq!(o.value(CellRef::new(5, 5)), None);
    }

    #[test]
    fn set_and_get_roundtrip() {
        let mut o = EditOverlay::new();
        let c = CellRef::new(3, 4);
        o.set(c, CellInput::Literal(Value::Number(42.0)));
        assert_eq!(o.value(c), Some(Value::Number(42.0)));
        assert_eq!(o.len(), 1);
    }

    #[test]
    fn set_returns_previous_for_undo() {
        let mut o = EditOverlay::new();
        let c = CellRef::new(0, 0);
        assert_eq!(o.set(c, CellInput::Literal(Value::Number(1.0))), None);
        let prev = o.set(c, CellInput::Literal(Value::Number(2.0)));
        assert_eq!(prev, Some(CellInput::Literal(Value::Number(1.0))));
        assert_eq!(o.value(c), Some(Value::Number(2.0)));
    }

    #[test]
    fn restore_undoes_an_edit() {
        let mut o = EditOverlay::new();
        let c = CellRef::new(1, 1);
        let prev = o.set(c, CellInput::Literal(Value::Number(9.0)));
        assert_eq!(o.value(c), Some(Value::Number(9.0)));
        // Undo back to "no edit" must fully defer to the base again.
        o.restore(c, prev);
        assert_eq!(o.get(c), None);
        assert!(o.is_empty());
    }

    #[test]
    fn formula_keeps_source_and_cached_value() {
        let mut o = EditOverlay::new();
        let c = CellRef::new(0, 0);
        o.set(
            c,
            CellInput::Formula {
                src: "=SUM(A1:A3)".into(),
                cached: Value::Number(6.0),
            },
        );
        assert!(o.has_formula(c));
        assert_eq!(o.get(c).unwrap().formula_src(), Some("=SUM(A1:A3)"));
        assert_eq!(o.value(c), Some(Value::Number(6.0)));

        // Recalc updates the cache without touching the source.
        o.update_cached(c, Value::Number(10.0));
        assert_eq!(o.value(c), Some(Value::Number(10.0)));
        assert_eq!(o.get(c).unwrap().formula_src(), Some("=SUM(A1:A3)"));
    }

    #[test]
    fn update_cached_ignores_literals() {
        let mut o = EditOverlay::new();
        let c = CellRef::new(0, 0);
        o.set(c, CellInput::Literal(Value::Number(1.0)));
        o.update_cached(c, Value::Number(99.0));
        // A literal is not a formula; its value must not be silently rewritten.
        assert_eq!(o.value(c), Some(Value::Number(1.0)));
    }

    #[test]
    fn tracks_extent_past_base() {
        let mut o = EditOverlay::new();
        o.set(CellRef::new(999, 5), CellInput::Literal(Value::Number(1.0)));
        assert_eq!(o.extent(), (1000, 6));
    }

    #[test]
    fn apply_batch_records_before_and_after_for_every_cell() {
        let mut o = EditOverlay::new();
        // One cell already edited, one untouched — undo must tell them apart.
        o.set(CellRef::new(0, 0), CellInput::Literal(Value::Number(1.0)));

        let mut changes = Vec::new();
        let n = o.apply_batch(
            vec![
                (
                    CellRef::new(0, 0),
                    Some(CellInput::Literal(Value::Number(2.0))),
                ),
                (
                    CellRef::new(1, 0),
                    Some(CellInput::Literal(Value::Number(3.0))),
                ),
            ],
            &mut changes,
        );

        assert_eq!(n, 2);
        assert_eq!(
            changes[0].before,
            Some(CellInput::Literal(Value::Number(1.0)))
        );
        assert_eq!(
            changes[1].before, None,
            "an unedited cell defers to the base"
        );
        assert_eq!(o.value(CellRef::new(0, 0)), Some(Value::Number(2.0)));
        assert_eq!(o.value(CellRef::new(1, 0)), Some(Value::Number(3.0)));
    }

    #[test]
    fn revert_batch_restores_every_cell_exactly() {
        let mut o = EditOverlay::new();
        o.set(CellRef::new(0, 0), CellInput::Literal(Value::Number(1.0)));

        let mut changes = Vec::new();
        o.apply_batch(
            (0..50u32).map(|r| {
                (
                    CellRef::new(r, 0),
                    Some(CellInput::Literal(Value::Number(99.0))),
                )
            }),
            &mut changes,
        );
        assert_eq!(o.len(), 50);

        o.revert_batch(&changes);
        // The pre-existing edit comes back as an EDIT; the other 49 go back to
        // having no overlay entry at all, deferring to the base again.
        assert_eq!(o.value(CellRef::new(0, 0)), Some(Value::Number(1.0)));
        assert_eq!(o.len(), 1, "reverting must not leave 49 phantom entries");
        for r in 1..50u32 {
            assert_eq!(o.get(CellRef::new(r, 0)), None);
        }
    }

    #[test]
    fn revert_batch_unwinds_overlapping_writes_in_order() {
        // Two writes to the same cell in one batch: undo must land on the
        // state before the batch, not on the intermediate value.
        let mut o = EditOverlay::new();
        let c = CellRef::new(4, 4);
        o.set(c, CellInput::Literal(Value::Number(0.0)));
        let mut changes = Vec::new();
        o.apply_batch(
            vec![
                (c, Some(CellInput::Literal(Value::Number(1.0)))),
                (c, Some(CellInput::Literal(Value::Number(2.0)))),
            ],
            &mut changes,
        );
        assert_eq!(o.value(c), Some(Value::Number(2.0)));
        o.revert_batch(&changes);
        assert_eq!(o.value(c), Some(Value::Number(0.0)));
    }
    #[test]
    fn formula_cells_enumerates_only_formulas() {
        let mut o = EditOverlay::new();
        o.set(CellRef::new(0, 0), CellInput::Literal(Value::Number(1.0)));
        o.set(
            CellRef::new(1, 0),
            CellInput::Formula {
                src: "=A1*2".into(),
                cached: Value::Number(2.0),
            },
        );
        let found: Vec<_> = o.formula_cells().collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, CellRef::new(1, 0));
    }

    #[test]
    fn editing_huge_sheet_stays_cheap() {
        // The scale claim: cost is O(edits), not O(rows). Edits scattered
        // across a 200M-row address space must cost only what was edited.
        let mut o = EditOverlay::new();
        for i in 0..1000u32 {
            o.set(
                CellRef::new(i * 200_000, 0),
                CellInput::Literal(Value::Number(i as f64)),
            );
        }
        assert_eq!(o.len(), 1000);
        assert!(
            o.heap_bytes() < 500_000,
            "1000 edits over a 200M-row sheet cost {} bytes",
            o.heap_bytes()
        );
        // Deep row indices must survive the u32 round-trip.
        assert_eq!(
            o.value(CellRef::new(999 * 200_000, 0)),
            Some(Value::Number(999.0))
        );
    }
}
