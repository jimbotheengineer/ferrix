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

/// Sparse edit layer over an immutable base.
#[derive(Debug, Default)]
pub struct EditOverlay {
    cells: HashMap<CellRef, CellInput>,
    /// Strings interned by edits. Kept separate from the base arena so the
    /// base can stay memory-mapped and read-only.
    arena: StringArena,
    /// Rows/cols the user extended the sheet to by editing past the end.
    extra_rows: usize,
    extra_cols: usize,
}

impl EditOverlay {
    pub fn new() -> Self {
        Self::default()
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
        self.cells.insert(cell, input)
    }

    /// Remove an edit, reverting the cell to its base value.
    pub fn clear(&mut self, cell: CellRef) -> Option<CellInput> {
        self.cells.remove(&cell)
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

    pub fn intern(&mut self, s: &str) -> StrId {
        self.arena.intern(s)
    }

    pub fn resolve(&self, id: StrId) -> Option<&str> {
        self.arena.resolve(id)
    }

    /// Update just the cached result of a formula, leaving its source intact.
    /// Used by recalc, which must not disturb what the user typed.
    pub fn update_cached(&mut self, cell: CellRef, value: Value) {
        if let Some(CellInput::Formula { cached, .. }) = self.cells.get_mut(&cell) {
            *cached = value;
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
