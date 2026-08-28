//! `SheetView`: the single read surface over base data plus edits.
//!
//! Everything that reads cells — the renderer, the formula evaluator, the
//! status bar — goes through this. It composes an immutable base with a sparse
//! [`EditOverlay`], so no consumer needs to know whether the base is an
//! in-RAM `Sheet` or a memory-mapped 12GB file, or whether a given value came
//! from disk or from an edit.

use ferrix_core::{CellRef, EditOverlay, Sheet, StrId, Value};
use ferrix_io::MappedSheet;

/// The immutable data under the overlay.
///
/// Small files live in RAM; large ones are memory-mapped so the dataset is
/// bounded by disk rather than memory. Both answer the same questions, which
/// is what lets the entire UI and formula engine be storage-agnostic.
pub enum BaseData {
    Memory(Sheet),
    Mapped(Box<MappedSheet>),
}

impl BaseData {
    #[inline]
    pub fn row_count(&self) -> usize {
        match self {
            BaseData::Memory(s) => s.row_count(),
            BaseData::Mapped(m) => m.row_count(),
        }
    }

    #[inline]
    pub fn col_count(&self) -> usize {
        match self {
            BaseData::Memory(s) => s.col_count(),
            BaseData::Mapped(m) => m.col_count(),
        }
    }

    #[inline]
    pub fn get(&self, cell: CellRef) -> Value {
        match self {
            BaseData::Memory(s) => s.get(cell),
            BaseData::Mapped(m) => m.get(cell),
        }
    }

    #[inline]
    pub fn resolve(&self, id: StrId) -> &str {
        match self {
            BaseData::Memory(s) => s.resolve(id),
            BaseData::Mapped(m) => m.resolve(id),
        }
    }

    pub fn display(&self, cell: CellRef) -> String {
        match self {
            BaseData::Memory(s) => s.display(cell),
            BaseData::Mapped(m) => m.display(cell),
        }
    }

    pub fn header_or_letter(&self, col: usize) -> String {
        match self {
            BaseData::Memory(s) => s.header_or_letter(col),
            BaseData::Mapped(m) => m.header_or_letter(col),
        }
    }

    pub fn sum_rect(&self, start: CellRef, end: CellRef) -> f64 {
        match self {
            BaseData::Memory(s) => s.sum_rect(start, end),
            BaseData::Mapped(m) => m.sum_rect(start, end),
        }
    }

    pub fn count_rect(&self, start: CellRef, end: CellRef) -> usize {
        match self {
            BaseData::Memory(s) => s.count_rect(start, end),
            BaseData::Mapped(m) => m.count_rect(start, end),
        }
    }

    /// Resident cost. For a mapping this is address space, not RAM — the OS
    /// pages in only what is touched.
    ///
    /// Kept as public API for memory reporting/diagnostics; no UI surface
    /// consumes it yet.
    #[allow(dead_code)]
    pub fn bytes(&self) -> usize {
        match self {
            BaseData::Memory(s) => s.heap_bytes(),
            BaseData::Mapped(m) => m.mapped_bytes(),
        }
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self, BaseData::Mapped(_))
    }
}

/// Read-only composite view. Cheap to construct — it borrows both layers.
pub struct SheetView<'a> {
    pub base: &'a BaseData,
    pub overlay: &'a EditOverlay,
}

impl<'a> SheetView<'a> {
    pub fn new(base: &'a BaseData, overlay: &'a EditOverlay) -> Self {
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

    /// Sum over a rectangle. Delegates to the base's columnar fast path (which
    /// uses compensated summation), then applies overlay corrections — so a
    /// 200M-row SUM stays a streaming scan even when cells in the range have
    /// been edited.
    pub fn sum_rect(&self, start: CellRef, end: CellRef) -> f64 {
        let base_total = self.base.sum_rect(start, end);
        if self.overlay.is_empty() {
            return base_total;
        }
        let (r0, r1) = (start.row.min(end.row), start.row.max(end.row));
        let (c0, c1) = (start.col.min(end.col), start.col.max(end.col));

        // Accumulate the correction separately and apply it once. Adding each
        // delta straight onto a large base total would round it away.
        let mut delta = 0.0f64;
        let mut c = 0.0f64;
        let mut add = |v: f64| {
            let y = v - c;
            let t = delta + y;
            c = (t - delta) - y;
            delta = t;
        };
        for (cell, input) in self.overlay.edited_cells() {
            if cell.row >= r0 && cell.row <= r1 && cell.col >= c0 && cell.col <= c1 {
                // Remove the base contribution, add the edited one.
                if let Value::Number(n) = self.base.get(*cell) {
                    add(-n);
                }
                if let Value::Number(n) = input.value() {
                    add(n);
                }
            }
        }
        base_total + delta
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

    /// Kept as public API for memory reporting/diagnostics; no UI surface
    /// consumes it yet.
    #[allow(dead_code)]
    pub fn heap_bytes(&self) -> usize {
        self.base.bytes() + self.overlay.heap_bytes()
    }

    /// Search base data plus edits.
    ///
    /// The base does the heavy lifting through its columnar fast path; edited
    /// cells are then reconciled: an edit can create a match the base does not
    /// have, or destroy one the base does. Because edits are sparse this stays
    /// O(base scan + edits) rather than forcing a slow path.
    pub fn search(&self, query: &ferrix_core::Query, limit: usize) -> ferrix_core::SearchResults {
        let mut results = match self.base {
            BaseData::Memory(s) => s.search(query, limit),
            BaseData::Mapped(m) => m.search(query, limit),
        };
        if self.overlay.is_empty() {
            return results;
        }

        // Reconcile: drop base matches the user has edited away, and add
        // matches the user has edited in.
        let mut removed = 0usize;
        results.matches.retain(|cell| {
            match self.overlay.value(*cell) {
                // Cell was edited; keep it only if the new value still matches.
                Some(v) => {
                    let keep = self.value_matches(&v, query);
                    if !keep {
                        removed += 1;
                    }
                    keep
                }
                None => true,
            }
        });

        let mut added: Vec<CellRef> = Vec::new();
        for (cell, input) in self.overlay.edited_cells() {
            let v = input.value();
            if self.value_matches(&v, query) {
                // Only add if the base did not already report it.
                let base_matched = results
                    .matches
                    .binary_search_by(|m| (m.row, m.col).cmp(&(cell.row, cell.col)));
                if base_matched.is_err() {
                    added.push(*cell);
                }
            }
        }

        if !added.is_empty() {
            results.matches.extend(added.iter().copied());
            results.matches.sort_by_key(|c| (c.row, c.col));
            results.matches.truncate(limit);
        }
        results.total = results.total + added.len() - removed;
        results
    }

    fn value_matches(&self, v: &Value, query: &ferrix_core::Query) -> bool {
        match v {
            Value::Empty => false,
            Value::Number(n) => query.matches_number(*n),
            Value::Bool(b) => query.matches_bool(*b),
            Value::Text(id) => query.matches_str(self.resolve(*id)),
            Value::Error(e) => query.matches_str(e.as_str()),
        }
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

    fn base_sheet() -> BaseData {
        let mut s = Sheet::new("t");
        for r in 0..10u32 {
            s.set(CellRef::new(r, 0), Value::Number((r + 1) as f64));
        }
        BaseData::Memory(s)
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
        {
            // Scoped so the immutable borrow of `ov` ends before we mutate it.
            let v0 = SheetView::new(&base, &ov);
            assert_eq!(v0.count_rect(CellRef::new(0, 0), CellRef::new(9, 0)), 10);
        }

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

    fn text_base() -> BaseData {
        let mut s = Sheet::new("t");
        for r in 0..10u32 {
            s.set_text(
                CellRef::new(r, 0),
                if r % 2 == 0 { "north" } else { "south" },
            );
        }
        BaseData::Memory(s)
    }

    fn q(needle: &str) -> ferrix_core::Query {
        ferrix_core::Query::new(needle, false, false).unwrap()
    }

    #[test]
    fn search_without_edits_matches_the_base() {
        let base = text_base();
        let ov = EditOverlay::new();
        let v = SheetView::new(&base, &ov);
        let r = v.search(&q("north"), 100);
        assert_eq!(r.total, 5);
        assert_eq!(r.matches[0], CellRef::new(0, 0));
    }

    #[test]
    fn editing_a_cell_away_removes_it_from_results() {
        let base = text_base();
        let mut ov = EditOverlay::new();
        // Row 0 was "north"; change it so it no longer matches.
        let id = ov.intern("west");
        ov.set(CellRef::new(0, 0), CellInput::Literal(Value::Text(id)));
        let v = SheetView::new(&base, &ov);
        let r = v.search(&q("north"), 100);
        assert_eq!(r.total, 4, "the edited cell must drop out");
        assert!(!r.matches.contains(&CellRef::new(0, 0)));
    }

    #[test]
    fn editing_a_cell_in_adds_it_to_results() {
        let base = text_base();
        let mut ov = EditOverlay::new();
        // Row 1 was "south"; make it match.
        let id = ov.intern("north-east");
        ov.set(CellRef::new(1, 0), CellInput::Literal(Value::Text(id)));
        let v = SheetView::new(&base, &ov);
        let r = v.search(&q("north"), 100);
        assert_eq!(r.total, 6);
        assert!(r.matches.contains(&CellRef::new(1, 0)));
        // Results must stay row-major after the insertion.
        let rows: Vec<u32> = r.matches.iter().map(|m| m.row).collect();
        assert!(rows.windows(2).all(|w| w[0] <= w[1]), "order broken");
    }

    #[test]
    fn edits_beyond_the_base_are_searchable() {
        let base = text_base();
        let mut ov = EditOverlay::new();
        let id = ov.intern("northwest");
        ov.set(CellRef::new(500, 3), CellInput::Literal(Value::Text(id)));
        let v = SheetView::new(&base, &ov);
        let r = v.search(&q("north"), 100);
        assert!(r.matches.contains(&CellRef::new(500, 3)));
    }
}
