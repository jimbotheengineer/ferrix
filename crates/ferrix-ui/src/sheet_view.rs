//! `SheetView`: the single read surface over base data plus edits.
//!
//! Everything that reads cells — the renderer, the formula evaluator, the
//! status bar — goes through this. It composes an immutable base with a sparse
//! [`EditOverlay`], so no consumer needs to know whether the base is an
//! in-RAM `Sheet` or a memory-mapped 12GB file, or whether a given value came
//! from disk or from an edit.

use ferrix_core::{CellRef, EditOverlay, Sheet, SheetOrder, StrId, Value};
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
///
/// ## Display space vs data space
///
/// Every `CellRef` crossing this boundary is a DISPLAY reference — what the
/// user is pointing at. Only the base read is translated into data space,
/// through [`SheetOrder`]. That is the entire cost of row/column reordering on
/// the read path: one `O(log runs)` lookup per axis, skipped outright when
/// nothing has been reordered.
pub struct SheetView<'a> {
    pub base: &'a BaseData,
    pub overlay: &'a EditOverlay,
    /// Display permutation. `None` — and the identity case — take a fast path
    /// that costs nothing, so an unreordered sheet is exactly as fast as
    /// before this feature existed.
    pub order: Option<&'a SheetOrder>,
}

impl<'a> SheetView<'a> {
    pub fn new(base: &'a BaseData, overlay: &'a EditOverlay) -> Self {
        Self {
            base,
            overlay,
            order: None,
        }
    }

    /// A view whose base reads are permuted by `order`.
    /// Build a view that reads through a display-order permutation.
    ///
    /// NOT YET WIRED TO THE UI. The reordering model (`ferrix_core::order`,
    /// 26 tests) and the formula-reference remapping (`ferrix_formula::remap`)
    /// are complete and tested, but app.rs and grid.rs do not yet construct a
    /// SheetOrder or expose header drag-to-reorder. Issue #17 stays open for
    /// that wiring; this is the seam it will attach to.
    #[allow(dead_code)]
    pub fn with_order(base: &'a BaseData, overlay: &'a EditOverlay, order: &'a SheetOrder) -> Self {
        Self {
            base,
            overlay,
            // An untouched sheet has NEITHER axis materialised, and carrying
            // an absent order would pay a mapping cost for nothing — so that
            // case drops it and every read below stays on the fast path.
            //
            // A materialised-but-identity-shaped axis is NOT dropped. After a
            // row removal at the END of the sheet the surviving runs are
            // `[Run { data: 0, len: n }]`, which `is_identity` correctly
            // reports as an identity MAPPING while the axis's LENGTH is the
            // only remaining record that the sheet got shorter. Dropping it
            // here would resurrect the removed rows in `row_count`.
            order: (order.rows.is_some() || order.cols.is_some()).then_some(order),
        }
    }

    /// Translate a display cell to the base cell it shows.
    ///
    /// `None` means the position addresses no base data at all — an inserted
    /// row or column — which reads as empty rather than as some other cell's
    /// value.
    #[inline]
    fn base_cell(&self, cell: CellRef) -> Option<CellRef> {
        match self.order {
            None => Some(cell),
            Some(o) => o
                .to_data(cell.row, cell.col)
                .map(|(r, c)| CellRef::new(r, c)),
        }
    }

    /// Read the base through the display permutation.
    #[inline]
    fn base_get(&self, cell: CellRef) -> Value {
        match self.base_cell(cell) {
            Some(c) => self.base.get(c),
            None => Value::Empty,
        }
    }

    /// Decompose a DISPLAY rectangle into the base rectangles it covers.
    ///
    /// A block that is contiguous on screen need not be contiguous in the data
    /// once an axis is reordered: display columns A:C might be data columns
    /// 0, 2, 3. Rather than give up and walk cell by cell — which would turn a
    /// 200M-row SUM into a 200M-iteration loop — the rectangle is cut into the
    /// runs it spans and each piece keeps the base's columnar fast path.
    ///
    /// The piece count is bounded by the number of runs the user's edits
    /// created, not by the size of the range, so a reordered 200M-row SUM is
    /// still a handful of streaming scans.
    fn base_rects(&self, start: CellRef, end: CellRef) -> Vec<(CellRef, CellRef)> {
        let (r0, r1) = (start.row.min(end.row), start.row.max(end.row));
        let (c0, c1) = (start.col.min(end.col), start.col.max(end.col));
        let Some(order) = self.order else {
            return vec![(CellRef::new(r0, c0), CellRef::new(r1, c1))];
        };
        let row_spans = match &order.rows {
            None => vec![(r0, r1 - r0 + 1)],
            Some(o) => o.data_spans(u64::from(r0), u64::from(r1 - r0 + 1)),
        };
        let col_spans = match &order.cols {
            None => vec![(c0, c1 - c0 + 1)],
            Some(o) => o.data_spans(u64::from(c0), u64::from(c1 - c0 + 1)),
        };
        let mut out = Vec::with_capacity(row_spans.len() * col_spans.len());
        for &(rs, rn) in &row_spans {
            for &(cs, cn) in &col_spans {
                out.push((CellRef::new(rs, cs), CellRef::new(rs + rn - 1, cs + cn - 1)));
            }
        }
        out
    }

    /// Base sum over a display rectangle, keeping the columnar fast path.
    fn base_sum_rect(&self, start: CellRef, end: CellRef) -> f64 {
        if self.order.is_none() {
            return self.base.sum_rect(start, end);
        }
        // Compensated accumulation across the pieces, so splitting a range
        // into runs cannot change the answer it would have had unsplit.
        let mut total = 0.0f64;
        let mut c = 0.0f64;
        for (a, b) in self.base_rects(start, end) {
            let v = self.base.sum_rect(a, b);
            let y = v - c;
            let t = total + y;
            c = (t - total) - y;
            total = t;
        }
        total
    }

    /// Base numeric count over a display rectangle.
    fn base_count_rect(&self, start: CellRef, end: CellRef) -> usize {
        if self.order.is_none() {
            return self.base.count_rect(start, end);
        }
        self.base_rects(start, end)
            .into_iter()
            .map(|(a, b)| self.base.count_rect(a, b))
            .sum()
    }

    /// Rows, accounting for edits that extend past the base.
    #[inline]
    pub fn row_count(&self) -> usize {
        // A ROW permutation defines how many display positions exist, which
        // is not the same as how many data rows the base holds: removing
        // rows (issue #34) drops display positions without erasing a byte of
        // base data. Reading the base's count here instead would leave the
        // removed rows addressable at the bottom of the sheet, painting as
        // blanks under row numbers that no longer mean anything.
        let displayed = match self.order.and_then(|o| o.rows.as_ref()) {
            Some(a) => a.len() as usize,
            None => self.base.row_count(),
        };
        displayed.max(self.overlay.extent().0)
    }

    #[inline]
    pub fn col_count(&self) -> usize {
        // A COLUMN permutation defines how many display positions exist, just
        // like the row case above. Inserting a column grows the order by one
        // display position; reading only the base's count here would leave
        // that new position unpainted and push the sheet's last real column
        // off the right edge (it silently vanished after an insert). Removing
        // a column likewise drops a display position without erasing base data.
        let displayed = match self.order.and_then(|o| o.cols.as_ref()) {
            Some(a) => a.len() as usize,
            None => self.base.col_count(),
        };
        displayed.max(self.overlay.extent().1)
    }

    /// Read a cell: overlay wins, base is the fallback.
    #[inline]
    pub fn get(&self, cell: CellRef) -> Value {
        match self.overlay.value(cell) {
            Some(v) => v,
            None => self.base_get(cell),
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

    /// Header text for a DISPLAY column — so a reordered column carries its
    /// own name with it rather than inheriting the name of whatever position
    /// it landed in.
    pub fn header_or_letter(&self, col: usize) -> String {
        match self.order {
            None => self.base.header_or_letter(col),
            Some(o) => match o.cols.as_ref() {
                None => self.base.header_or_letter(col),
                // An inserted column has no base header; it is named by its
                // position, like a fresh spreadsheet column.
                Some(c) => match c.data_of(col as u64) {
                    // A FRESH inserted column carries a synthetic data index
                    // allocated from beyond the base's extent (see
                    // `AxisOrder::insert_fresh`). That index is not a base
                    // column, so it must not be looked up in the base headers —
                    // doing so named the new empty column after whatever letter
                    // its synthetic index happened to land on (e.g. an insert in
                    // a 5-column sheet showed "F"). Name it by its display
                    // position instead, exactly like the no-base-header case.
                    Some(d) if (d as usize) < self.base.col_count() => {
                        self.base.header_or_letter(d as usize)
                    }
                    _ => ferrix_core::column_name(col as u32),
                },
            },
        }
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
            None => match self.base_cell(cell) {
                Some(c) => self.base.display(c),
                None => String::new(),
            },
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
        let base_total = self.base_sum_rect(start, end);
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
                if let Value::Number(n) = self.base_get(*cell) {
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
        let mut total = self.base_count_rect(start, end) as i64;
        if self.overlay.is_empty() {
            return total.max(0) as usize;
        }
        let (r0, r1) = (start.row.min(end.row), start.row.max(end.row));
        let (c0, c1) = (start.col.min(end.col), start.col.max(end.col));
        for (cell, input) in self.overlay.edited_cells() {
            if cell.row >= r0 && cell.row <= r1 && cell.col >= c0 && cell.col <= c1 {
                if matches!(self.base_get(*cell), Value::Number(_)) {
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
        // The base reports DATA coordinates. Everything downstream — the
        // filter mapping, the match cursor, the selection it moves — works in
        // DISPLAY space, so a reordered sheet has to translate here.
        //
        // Missing this is the two-mapping bug in its most convincing disguise:
        // the search reports the right NUMBER of hits, `RowFilter` builds a
        // mapping of the right SIZE, and the grid then shows that many rows —
        // just the wrong ones. Nothing looks broken until you read a record.
        //
        // Cost is O(matches), which is already bounded by `limit`, and the
        // whole block is skipped for an unreordered sheet.
        if let Some(order) = self.order {
            if !order.is_identity() {
                results.matches.retain_mut(|cell| {
                    let row = match &order.rows {
                        None => Some(cell.row),
                        // A data row with no display position was deleted; it
                        // is not on screen, so it is not a hit.
                        Some(a) => a.display_of(cell.row).and_then(|d| u32::try_from(d).ok()),
                    };
                    let col = match &order.cols {
                        None => Some(cell.col),
                        Some(a) => a.display_of(cell.col).and_then(|d| u32::try_from(d).ok()),
                    };
                    match (row, col) {
                        (Some(r), Some(c)) => {
                            *cell = CellRef::new(r, c);
                            true
                        }
                        _ => false,
                    }
                });
                // Row-major order is a contract of `SearchResults` (the filter
                // mapping and `index_at_or_after` both binary search it), and
                // a permutation does not preserve it.
                results.matches.sort_by_key(|c| (c.row, c.col));
            }
        }
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

    /// Search one half-open row window, base only, without overlay
    /// reconciliation.
    ///
    /// Replace All reconciles the overlay itself as it walks (an edited cell's
    /// current text is read through `replace_text`), so a windowed scan does
    /// not repeat that work.
    fn search_rows(
        &self,
        query: &ferrix_core::Query,
        r0: usize,
        r1: usize,
        limit: usize,
    ) -> ferrix_core::SearchResults {
        match self.base {
            BaseData::Memory(s) => s.search_rows(query, r0, r1, limit),
            BaseData::Mapped(m) => m.search_rows(query, r0, r1, limit),
        }
    }

    /// The text a replace reads for `cell`, under `look_in`.
    ///
    /// `None` means "this cell is not a candidate at all", which is how
    /// `LookIn::Values` skips formula cells: a formula's displayed value is a
    /// computed result, and overwriting a result with text would silently
    /// destroy the formula that produced it.
    pub fn replace_text(&self, cell: CellRef, look_in: ferrix_core::LookIn) -> Option<String> {
        match look_in {
            ferrix_core::LookIn::Values => {
                if self.overlay.has_formula(cell) {
                    return None;
                }
                Some(self.display(cell))
            }
            // `edit_text` is exactly the right reader here: a formula answers
            // with its SOURCE (`=A1*2`), everything else with its text. That
            // is what makes "look in: formulas" rewrite the formula rather
            // than the number it currently shows.
            ferrix_core::LookIn::Formulas => Some(self.edit_text(cell)),
        }
    }

    /// Candidate cells in one half-open row window, in row-major order.
    ///
    /// The window is the unit that bounds memory. Callers that must hold a
    /// `&mut` on the workbook to apply edits (which is every real caller)
    /// cannot keep a borrowing iterator alive across the write, so they loop
    /// over windows instead: scan a window, drop the borrow, apply, repeat.
    /// Peak memory is therefore O(window hits), never O(total matches).
    pub fn replace_window(
        &self,
        query: &ferrix_core::Query,
        look_in: ferrix_core::LookIn,
        r0: usize,
        r1: usize,
    ) -> Vec<(CellRef, String)> {
        let mut hits = self.search_rows(query, r0, r1, usize::MAX).matches;

        // Overlay cells in this window the base scan could not know about: an
        // edit may have created a match, and under LookIn::Formulas a
        // formula's SOURCE may match while its displayed result does not.
        if !self.overlay.is_empty() {
            let mut extra: Vec<CellRef> = Vec::new();
            for (cell, input) in self.overlay.edited_cells() {
                let row = cell.row as usize;
                if row < r0 || row >= r1 {
                    continue;
                }
                let matched = match look_in {
                    ferrix_core::LookIn::Values => self.value_matches(&input.value(), query),
                    ferrix_core::LookIn::Formulas => match input.formula_src() {
                        Some(src) => query.matches_str(src),
                        None => self.value_matches(&input.value(), query),
                    },
                };
                if matched
                    && hits
                        .binary_search_by(|m| (m.row, m.col).cmp(&(cell.row, cell.col)))
                        .is_err()
                {
                    extra.push(*cell);
                }
            }
            if !extra.is_empty() {
                hits.extend(extra);
                hits.sort_by_key(|c| (c.row, c.col));
                hits.dedup();
            }
        }

        let mut out = Vec::with_capacity(hits.len());
        for cell in hits {
            // An edited cell is a candidate only if its CURRENT text matches;
            // the base scan may be reporting a match the user has edited away.
            if look_in == ferrix_core::LookIn::Values {
                if let Some(v) = self.overlay.value(cell) {
                    if !self.value_matches(&v, query) {
                        continue;
                    }
                }
            }
            let Some(text) = self.replace_text(cell, look_in) else {
                continue;
            };
            // Under LookIn::Formulas the base scan matched the displayed
            // value, but the text about to be rewritten is the SOURCE — re-test
            // it so a cell whose source does not contain the needle is never
            // touched.
            if look_in == ferrix_core::LookIn::Formulas && !query.matches_str(&text) {
                continue;
            }
            out.push((cell, text));
        }
        out
    }
}

/// Sorting reads its keys through the same composite view everything else
/// reads through, which is what makes a sort see edits and mmap data alike.
///
/// Note what this does NOT do: it never builds a key column. Each call hands
/// back a borrow — a `f64` or a `&str` pointing into the arena or the mapped
/// file — so a sort of a 200M-row column pages cells in through the OS and
/// copies none of them. The sorter's only allocation is its index vector.
impl ferrix_core::CellKeys for SheetView<'_> {
    #[inline]
    fn key(&self, row: u32, col: u32) -> ferrix_core::SortCell<'_> {
        use ferrix_core::SortCell;
        match self.get(CellRef::new(row, col)) {
            Value::Empty => SortCell::Empty,
            Value::Number(n) => SortCell::Number(n),
            Value::Bool(b) => SortCell::Bool(b),
            Value::Text(id) => SortCell::Text(self.resolve(id)),
            Value::Error(e) => SortCell::Error(e.as_str()),
        }
    }
}

/// Deduplication reads the SAME composite view the grid paints, so a row an
/// edit made unique is unique to the dedupe too (issue #34). Streaming: one
/// cell per key column per row, nothing retained.
impl ferrix_core::DupeKeys for SheetView<'_> {
    #[inline]
    fn key_value(&self, row: u32, col: u32) -> Value {
        self.get(CellRef::new(row, col))
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

/// An owned, `Send` snapshot of a sheet, for reading on a worker thread.
///
/// ## Why this exists
///
/// [`SheetView`] borrows the workbook, so it cannot cross a thread boundary —
/// which is why exporting used to run on the UI thread and why sheets above
/// five million rows were refused outright rather than freezing the window for
/// minutes. This is the same composite view with both halves owned, so an
/// export can be handed to `std::thread::spawn` and the user can keep working.
///
/// ## Memory implication
///
/// The two halves are treated very differently, on purpose:
///
/// * **Base** — shared via `Arc`, never copied. The base is immutable (every
///   edit lands in the overlay), so sharing it needs no lock and a 12 GB
///   memory-mapped sheet costs a refcount bump. This is what keeps a 200M-row
///   export's peak memory flat.
/// * **Overlay** — deep-copied, because the user must stay free to edit while
///   the export runs and the export must write a consistent picture rather
///   than a smear of two. Edits are sparse (a few thousand cells on a typical
///   sheet), so the copy is normally kilobytes — but "normally" is not
///   "always", so the caller admits `snapshot_cost_bytes` against the memory
///   budget before taking one and refuses with a message if it will not fit.
///
/// The snapshot is a point-in-time picture: edits made after it is taken are
/// not in the exported file. That is the correct semantics for "export what I
/// am looking at now", and the status line says the export is running so the
/// user knows which moment they got.
pub struct OwnedSheet {
    base: std::sync::Arc<BaseData>,
    overlay: EditOverlay,
    /// Sheet name for the print header/footer `&A` field and HTML title. The
    /// CSV/Parquet paths do not need it, so it defaults to empty and is set
    /// only on the print path via [`OwnedSheet::with_name`].
    name: String,
    /// Conditional formatting + decoration, cloned for the print snapshot so a
    /// styled export renders fills, text colour, typography and alignment the
    /// way the grid shows them. Stored per column/range, so the clone is a
    /// function of how many formats the user applied, never of row count.
    /// `None` on the CSV/Parquet paths, which do not carry style.
    format: Option<ferrix_core::SheetFormat>,
    /// Merged regions, cloned for the same reason. `None` = no merges carried.
    merges: Option<ferrix_core::merge::MergeMap>,
}

impl OwnedSheet {
    /// Snapshot the base (shared) and the overlay (copied).
    pub fn new(base: std::sync::Arc<BaseData>, overlay: &EditOverlay) -> Self {
        Self {
            base,
            overlay: overlay.clone(),
            name: String::new(),
            format: None,
            merges: None,
        }
    }

    /// Give the snapshot a sheet name, for the print `&A` field / HTML title.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Carry the sheet's formatting and merges into the snapshot, so a styled
    /// print (PDF/HTML) renders them. Both maps are per column/range, so this
    /// is cheap regardless of sheet size; account for it via
    /// [`OwnedSheet::style_cost_bytes`] before constructing the snapshot.
    pub fn with_style(
        mut self,
        format: &ferrix_core::SheetFormat,
        merges: &ferrix_core::merge::MergeMap,
    ) -> Self {
        self.format = Some(format.clone());
        self.merges = Some(merges.clone());
        self
    }

    /// Heap the style clone will allocate, for the budget check.
    pub fn style_cost_bytes(
        format: &ferrix_core::SheetFormat,
        merges: &ferrix_core::merge::MergeMap,
    ) -> u64 {
        (format.heap_bytes() + merges.heap_bytes()) as u64
    }

    /// What taking this snapshot will actually allocate.
    ///
    /// Only the overlay: the base is shared, not copied. Check this against
    /// the memory budget BEFORE constructing the snapshot, so a pathological
    /// overlay is refused rather than doubled.
    pub fn snapshot_cost_bytes(overlay: &EditOverlay) -> u64 {
        overlay.heap_bytes() as u64
    }

    /// Borrow as a normal composite view.
    pub fn view(&self) -> SheetView<'_> {
        SheetView::new(&self.base, &self.overlay)
    }

    pub fn row_count(&self) -> usize {
        self.view().row_count()
    }
}

// SAFETY-free: both fields are `Send`. `Arc<BaseData>` is `Send + Sync`
// because `BaseData` holds either an owned `Sheet` or a `memmap2::Mmap`, both
// of which are `Send + Sync`, and the `Arc` is only ever read through. This is
// a plain auto-derived bound, stated here only to record the reasoning.

/// Exporting must write what the user *sees* — base plus edits — so the
/// snapshot exports through the same composite view the grid renders from.
impl ferrix_io::export::ExportSource for OwnedSheet {
    fn row_count(&self) -> usize {
        self.view().row_count()
    }
    fn col_count(&self) -> usize {
        self.view().col_count()
    }
    fn display(&self, cell: CellRef) -> String {
        self.view().display(cell)
    }
    fn header(&self, col: usize) -> String {
        self.view().header_or_letter(col)
    }
}

/// Print/PDF export renders the composite view too. This first cut prints the
/// values as unstyled text — the same strings the CSV export writes. Carrying
/// conditional-format fills, per-cell typography and merged-cell spanning into
/// the print snapshot is a follow-up: it needs the format map and merge map
/// copied alongside the overlay, which `snapshot_cost_bytes` would have to
/// account for. `sheet_name` is overridden so the header/footer `&A` field and
/// the HTML title are correct.
impl ferrix_io::render::RenderSource for OwnedSheet {
    fn sheet_name(&self) -> String {
        if self.name.is_empty() {
            "Sheet1".to_string()
        } else {
            self.name.clone()
        }
    }

    /// Resolve a cell's fill, text colour, bold/italic and alignment the same
    /// way the grid paints it, so a styled export matches the screen.
    ///
    /// Conditional rules that need a *window* of the column's values (data
    /// bars, top-N) are NOT applied here: resolving them per cell would make
    /// the export walk the column, breaking the one-page-at-a-time streaming
    /// bound. Threshold/value rules, manual overrides and alignment — the
    /// overwhelming majority — do resolve. Documented on the export.
    fn paint(&self, cell: CellRef) -> ferrix_io::render::CellPaint {
        let Some(format) = &self.format else {
            return ferrix_io::render::CellPaint::default();
        };
        let view = self.view();
        let mut plan = Vec::new();
        format.plan(cell.col, &mut plan);
        let value = view.get(cell);
        let text = if ferrix_core::SheetFormat::plan_needs_text(&plan) {
            view.display(cell)
        } else {
            String::new()
        };
        // Empty evals: window-dependent rules resolve to their unstyled state.
        let style = format.resolve(cell, &value, &text, &plan, &[]);
        let decor = format.decor_at(cell);
        let align = match decor.h_align {
            Some(ferrix_core::HAlign::Left) => ferrix_core::HAlign::Left,
            Some(ferrix_core::HAlign::Center) => ferrix_core::HAlign::Center,
            Some(ferrix_core::HAlign::Right) => ferrix_core::HAlign::Right,
            Some(ferrix_core::HAlign::Justify) => ferrix_core::HAlign::Justify,
            _ => ferrix_core::HAlign::General,
        };
        ferrix_io::render::CellPaint {
            fill: style.fill,
            text_color: style.text,
            bold: style.typography.bold.unwrap_or(false),
            italic: style.typography.italic.unwrap_or(false),
            align,
        }
    }

    fn merge_at(&self, cell: CellRef) -> Option<ferrix_core::TableRange> {
        self.merges
            .as_ref()
            .and_then(|m| m.region_at(cell).copied())
    }
}

/// The same snapshot, exported with TYPES rather than display strings.
///
/// Parquet needs the typed value: rendering a number to text first would throw
/// away exactly the type information the format exists to carry, and the file
/// would come back into pandas as a column of strings.
impl ferrix_io::ArrowSource for OwnedSheet {
    fn row_count(&self) -> usize {
        self.view().row_count()
    }
    fn col_count(&self) -> usize {
        self.view().col_count()
    }
    fn header(&self, col: usize) -> String {
        self.view().header_or_letter(col)
    }
    fn value(&self, cell: CellRef) -> Value {
        self.view().get(cell)
    }
    fn text(&self, id: StrId) -> String {
        self.view().resolve(id).to_string()
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
