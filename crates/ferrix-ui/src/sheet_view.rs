//! `SheetView`: the single read surface over base data plus edits.
//!
//! Everything that reads cells — the renderer, the formula evaluator, the
//! status bar — goes through this. It composes an immutable base (in-RAM
//! `Sheet` today, memory-mapped columnar file next) with a sparse
//! `EditOverlay`, so no consumer needs to know which layer a value came from
//! or whether the base is resident in memory.

use ferrix_core::{CellRef, EditOverlay, Sheet, StrId, Value};

/// Read-only composite view. Cheap to construct — it borrows both layers.
pub struct SheetView<'a> {
    pub base: &'a Sheet,
    pub overlay: &'a EditOverlay,
}

impl<'a> SheetView<'a> {
    pub fn new(base: &'a Sheet, overlay: &'a EditOverlay) -> Self {
        Self { base, overlay }
    }

    /// Rows, accounting for edits that extend past the base.
    #[inline]
    pub fn row_count(&self) -> usize {
        self.base.row_count().max(self.overlay.extent().0)
    }

    #[inline]
    pub fn col_count(&self) -> usize {
        self.base.col_count().max(self.overlay.extent().1)
    }

    /// Read a cell: overlay wins, base is the fallback.
    #[inline]
    pub fn get(&self, cell: CellRef) -> Value {
        match self.overlay.value(cell) {
            Some(v) => v,
            None => self.base.get(cell),
        }
    }

    /// Resolve a string id. Overlay ids and base ids come from different
    /// arenas, so we try the overlay first and fall back to the base.
    #[inline]
    pub fn resolve(&self, id: StrId) -> &str {
        // Overlay strings are interned into the overlay's own arena; a value
        // read from the overlay resolves there. Base values resolve in the
        // base arena. Trying overlay-first is correct because base ids are
        // only ever produced by base reads.
        self.overlay
            .resolve(id)
            .unwrap_or_else(|| self.base.resolve(id))
    }

    pub fn has_formula(&self, cell: CellRef) -> bool {
        self.overlay.has_formula(cell)
    }

    pub fn header_or_letter(&self, col: usize) -> String {
        self.base.header_or_letter(col)
    }

    /// Text shown in a cell.
    pub fn display(&self, cell: CellRef) -> String {
        match self.overlay.value(cell) {
            Some(v) => match v {
                Value::Empty => String::new(),
                Value::Number(n) => ferrix_core::format_number(n),
                Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.to_string(),
                Value::Text(id) => self.resolve(id).to_string(),
                Value::Error(e) => e.to_string(),
            },
            None => self.base.display(cell),
        }
    }

    /// What the formula bar should show: a formula's source, else its value.
    pub fn edit_text(&self, cell: CellRef) -> String {
        if let Some(input) = self.overlay.get(cell) {
            if let Some(src) = input.formula_src() {
                return src.to_string();
            }
        }
        self.display(cell)
    }

    /// Sum over a rectangle. Delegates to the base's columnar fast path, then
    /// applies overlay corrections — so a 10M-row SUM stays a typed slice walk
    /// even when a handful of cells in the range have been edited.
    pub fn sum_rect(&self, start: CellRef, end: CellRef) -> f64 {
        let mut total = self.base.sum_rect(start, end);
        if self.overlay.is_empty() {
            return total;
        }
        let (r0, r1) = (start.row.min(end.row), start.row.max(end.row));
        let (c0, c1) = (start.col.min(end.col), start.col.max(end.col));
        for (cell, input) in self.overlay.edited_cells() {
            if cell.row >= r0 && cell.row <= r1 && cell.col >= c0 && cell.col <= c1 {
                // Remove the base contribution, add the edited one.
                if let Some(base_n) = self.base.get(*cell).as_number() {
                    if matches!(self.base.get(*cell), Value::Number(_)) {
                        total -= base_n;
                    }
                }
                if let Value::Number(n) = input.value() {
                    total += n;
                }
            }
        }
        total
    }

    /// Count of numeric cells in a rectangle, overlay-corrected.
    pub fn count_rect(&self, start: CellRef, end: CellRef) -> usize {
        let mut total = self.base.count_rect(start, end) as i64;
        if self.overlay.is_empty() {
            return total.max(0) as usize;
        }
        let (r0, r1) = (start.row.min(end.row), start.row.max(end.row));
        let (c0, c1) = (start.col.min(end.col), start.col.max(end.col));
        for (cell, input) in self.overlay.edited_cells() {
            if cell.row >= r0 && cell.row <= r1 && cell.col >= c0 && cell.col <= c1 {
                if matches!(self.base.get(*cell), Value::Number(_)) {
                    total -= 1;
                }
                if matches!(input.value(), Value::Number(_)) {
                    total += 1;
                }
            }
        }
        total.max(0) as usize
    }

    pub fn heap_bytes(&self) -> usize {
        self.base.heap_bytes() + self.overlay.heap_bytes()
    }
}

/// Formulas evaluate against the composite view, so `=SUM(A1:A10)` sees edited
/// cells and base cells alike.
impl<'a> ferrix_formula::CellSource for SheetView<'a> {
    #[inline]
    fn get(&self, cell: CellRef) -> Value {
        SheetView::get(self, cell)
    }
    #[inline]
    fn resolve(&self, id: StrId) -> &str {
        SheetView::resolve(self, id)
    }
    #[inline]
    fn sum_rect(&self, start: CellRef, end: CellRef) -> f64 {
        SheetView::sum_rect(self, start, end)
    }
    #[inline]
    fn count_rect(&self, start: CellRef, end: CellRef) -> usize {
        SheetView::count_rect(self, start, end)
    }
    #[inline]
    fn row_count(&self) -> usize {
        SheetView::row_count(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::CellInput;

    fn base_sheet() -> Sheet {
        let mut s = Sheet::new("t");
        for r in 0..10u32 {
            s.set(CellRef::new(r, 0), Value::Number((r + 1) as f64));
        }
        s
    }

    #[test]
    fn empty_overlay_passes_through_to_base() {
        let base = base_sheet();
        let ov = EditOverlay::new();
        let v = SheetView::new(&base, &ov);
        assert_eq!(v.get(CellRef::new(0, 0)), Value::Number(1.0));
        assert_eq!(v.row_count(), 10);
        assert_eq!(v.sum_rect(CellRef::new(0, 0), CellRef::new(9, 0)), 55.0);
    }

    #[test]
    fn overlay_shadows_base() {
        let base = base_sheet();
        let mut ov = EditOverlay::new();
        ov.set(CellRef::new(0, 0), CellInput::Literal(Value::Number(100.0)));
        let v = SheetView::new(&base, &ov);
        assert_eq!(v.get(CellRef::new(0, 0)), Value::Number(100.0));
        // Unedited cells still come from the base.
        assert_eq!(v.get(CellRef::new(1, 0)), Value::Number(2.0));
    }

    #[test]
    fn sum_is_overlay_corrected() {
        let base = base_sheet(); // 1..10 = 55
        let mut ov = EditOverlay::new();
        // Replace 1 with 100: total becomes 55 - 1 + 100 = 154.
        ov.set(CellRef::new(0, 0), CellInput::Literal(Value::Number(100.0)));
        let v = SheetView::new(&base, &ov);
        assert_eq!(v.sum_rect(CellRef::new(0, 0), CellRef::new(9, 0)), 154.0);
    }

    #[test]
    fn count_is_overlay_corrected() {
        let base = base_sheet();
        let mut ov = EditOverlay::new();
        let v0 = SheetView::new(&base, &ov);
        assert_eq!(v0.count_rect(CellRef::new(0, 0), CellRef::new(9, 0)), 10);
        drop(v0);

        // Blanking a numeric cell must drop the count.
        ov.set(CellRef::new(0, 0), CellInput::Literal(Value::Empty));
        let v = SheetView::new(&base, &ov);
        assert_eq!(v.count_rect(CellRef::new(0, 0), CellRef::new(9, 0)), 9);
    }

    #[test]
    fn edits_can_extend_past_base_extent() {
        let base = base_sheet(); // 10 rows, 1 col
        let mut ov = EditOverlay::new();
        ov.set(CellRef::new(99, 4), CellInput::Literal(Value::Number(7.0)));
        let v = SheetView::new(&base, &ov);
        assert_eq!(v.row_count(), 100);
        assert_eq!(v.col_count(), 5);
        assert_eq!(v.get(CellRef::new(99, 4)), Value::Number(7.0));
        // Cells between base and the new extent read as empty, not garbage.
        assert_eq!(v.get(CellRef::new(50, 3)), Value::Empty);
    }

    #[test]
    fn edit_text_prefers_formula_source() {
        let base = base_sheet();
        let mut ov = EditOverlay::new();
        let c = CellRef::new(0, 1);
        ov.set(
            c,
            CellInput::Formula {
                src: "=SUM(A1:A10)".into(),
                cached: Value::Number(55.0),
            },
        );
        let v = SheetView::new(&base, &ov);
        // The grid shows the value...
        assert_eq!(v.display(c), "55");
        // ...but the formula bar shows the source.
        assert_eq!(v.edit_text(c), "=SUM(A1:A10)");
        // A plain data cell shows its value in both.
        assert_eq!(v.edit_text(CellRef::new(0, 0)), "1");
    }

    #[test]
    fn overlay_text_resolves_from_overlay_arena() {
        let base = base_sheet();
        let mut ov = EditOverlay::new();
        let id = ov.intern("hello");
        ov.set(CellRef::new(0, 0), CellInput::Literal(Value::Text(id)));
        let v = SheetView::new(&base, &ov);
        assert_eq!(v.display(CellRef::new(0, 0)), "hello");
    }
}
