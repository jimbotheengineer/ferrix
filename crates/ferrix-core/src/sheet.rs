//! Sheet: a collection of columns plus the shared string arena.

use crate::arena::{StrId, StringArena};
use crate::bitmap::Bitmap;
use crate::column::Column;
use crate::value::Value;

/// A zero-based cell coordinate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct CellRef {
    pub row: u32,
    pub col: u32,
}

impl CellRef {
    pub const fn new(row: u32, col: u32) -> Self {
        Self { row, col }
    }

    /// Render as A1 notation, e.g. (0,0) -> "A1", (0,26) -> "AA1".
    pub fn to_a1(self) -> String {
        let mut name = column_name(self.col);
        name.push_str(&(self.row + 1).to_string());
        name
    }

    /// Parse A1 notation. Returns None on malformed input.
    pub fn from_a1(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let split = s.find(|c: char| c.is_ascii_digit())?;
        let (letters, digits) = s.split_at(split);
        if letters.is_empty() || !letters.bytes().all(|b| b.is_ascii_alphabetic()) {
            return None;
        }
        let mut col: u32 = 0;
        for b in letters.bytes() {
            let v = (b.to_ascii_uppercase() - b'A') as u32 + 1;
            col = col.checked_mul(26)?.checked_add(v)?;
        }
        let row: u32 = digits.parse().ok()?;
        if row == 0 {
            return None;
        }
        Some(CellRef::new(row - 1, col - 1))
    }
}

/// Spreadsheet column name for a zero-based index: 0->A, 25->Z, 26->AA.
pub fn column_name(mut col: u32) -> String {
    let mut buf = Vec::new();
    loop {
        buf.push(b'A' + (col % 26) as u8);
        if col < 26 {
            break;
        }
        col = col / 26 - 1;
    }
    buf.reverse();
    String::from_utf8(buf).expect("ascii only")
}

/// A worksheet.
#[derive(Debug, Default)]
pub struct Sheet {
    pub name: String,
    columns: Vec<Column>,
    /// Optional header labels taken from a CSV's first row.
    headers: Vec<String>,
    pub arena: StringArena,
    row_count: usize,
}

impl Sheet {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Total rows (the maximum extent of any column).
    #[inline]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Total columns.
    #[inline]
    pub fn col_count(&self) -> usize {
        self.columns.len()
    }

    pub fn headers(&self) -> &[String] {
        &self.headers
    }

    pub fn set_headers(&mut self, headers: Vec<String>) {
        self.headers = headers;
    }

    /// Display label for a column: its CSV header if present, else A/B/C.
    pub fn header_or_letter(&self, col: usize) -> String {
        self.headers
            .get(col)
            .filter(|h| !h.is_empty())
            .cloned()
            .unwrap_or_else(|| column_name(col as u32))
    }

    fn ensure_col(&mut self, col: usize) {
        if col >= self.columns.len() {
            self.columns.resize_with(col + 1, Column::new);
        }
    }

    /// Read a cell. Anything outside the populated area reads as Empty.
    #[inline]
    pub fn get(&self, cell: CellRef) -> Value {
        self.columns
            .get(cell.col as usize)
            .map(|c| c.get(cell.row as usize))
            .unwrap_or(Value::Empty)
    }

    /// Write a cell, extending the sheet as needed.
    pub fn set(&mut self, cell: CellRef, value: Value) {
        self.ensure_col(cell.col as usize);
        self.columns[cell.col as usize].set(cell.row as usize, value);
        self.row_count = self.row_count.max(cell.row as usize + 1);
    }

    /// Write a string cell, interning the text.
    pub fn set_text(&mut self, cell: CellRef, text: &str) {
        let id = self.arena.intern(text);
        self.set(cell, Value::Text(id));
    }

    /// Resolve a cell to display text.
    pub fn display(&self, cell: CellRef) -> String {
        match self.get(cell) {
            Value::Empty => String::new(),
            Value::Number(n) => crate::value::format_number(n),
            Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.to_string(),
            Value::Text(id) => self.arena.resolve_or_empty(id).to_string(),
            Value::Error(e) => e.to_string(),
        }
    }

    pub fn column(&self, col: usize) -> Option<&Column> {
        self.columns.get(col)
    }

    pub fn column_mut(&mut self, col: usize) -> Option<&mut Column> {
        self.columns.get_mut(col)
    }

    /// Install a fully-built column. Used by the parallel CSV loader, which
    /// constructs columns off-thread and hands them over finished.
    pub fn push_column(&mut self, column: Column) {
        self.row_count = self.row_count.max(column.len());
        self.columns.push(column);
    }

    pub fn intern(&mut self, s: &str) -> StrId {
        self.arena.intern(s)
    }

    pub fn resolve(&self, id: StrId) -> &str {
        self.arena.resolve_or_empty(id)
    }

    /// Approximate total heap footprint, shown in the status bar.
    pub fn heap_bytes(&self) -> usize {
        self.columns.iter().map(|c| c.heap_bytes()).sum::<usize>() + self.arena.heap_bytes()
    }

    /// Sum a rectangular range — the primitive behind SUM(A1:A100000).
    pub fn sum_rect(&self, start: CellRef, end: CellRef) -> f64 {
        let (r0, r1) = (
            start.row.min(end.row) as usize,
            start.row.max(end.row) as usize + 1,
        );
        let (c0, c1) = (
            start.col.min(end.col) as usize,
            start.col.max(end.col) as usize + 1,
        );
        (c0..c1.min(self.columns.len()))
            .map(|c| self.columns[c].sum_range(r0, r1))
            .sum()
    }

    /// Count numeric cells in a rectangular range.
    pub fn count_rect(&self, start: CellRef, end: CellRef) -> usize {
        let (r0, r1) = (
            start.row.min(end.row) as usize,
            start.row.max(end.row) as usize + 1,
        );
        let (c0, c1) = (
            start.col.min(end.col) as usize,
            start.col.max(end.col) as usize + 1,
        );
        (c0..c1.min(self.columns.len()))
            .map(|c| self.columns[c].count_numeric(r0, r1))
            .sum()
    }

    pub fn shrink_to_fit(&mut self) {
        for c in &mut self.columns {
            c.shrink_to_fit();
        }
        self.arena.shrink_for_readonly();
    }

    /// Find every cell matching `query`, in row-major order.
    ///
    /// Strategy: match the needle against the arena once (cheap — the arena
    /// holds each distinct string once), then scan columns comparing 4-byte
    /// ids against the resulting bitset. Results are collected per column and
    /// merged, so the output is ordered by row then column regardless of the
    /// column-major scan.
    pub fn search(
        &self,
        query: &crate::search::Query,
        limit: usize,
    ) -> crate::search::SearchResults {
        let t = std::time::Instant::now();
        let ids = crate::search::IdSet::from_arena(&self.arena, query);

        // Column-major scan, then merge into row-major order.
        let mut per_col: Vec<(usize, Vec<u32>)> = Vec::new();
        for (ci, col) in self.columns.iter().enumerate() {
            let mut rows = Vec::new();
            col.scan_matches(0, col.len(), query, &ids, &mut rows);
            if !rows.is_empty() {
                per_col.push((ci, rows));
            }
        }

        let total: usize = per_col.iter().map(|(_, r)| r.len()).sum();
        let matches = merge_row_major(&per_col, limit);

        crate::search::SearchResults {
            truncated: total > matches.len(),
            total,
            matches,
            millis: t.elapsed().as_millis(),
            matched_strings: ids.len(),
        }
    }

    // ------------------------------------------------------ structured tables

    /// Apply a table's active header filters, producing a row mask.
    ///
    /// Every predicate is compiled against the arena *once* (see
    /// [`CompiledPredicate`]), then each filtered column is scanned as
    /// integers via [`Column::scan_filter`]. Columns with no filter are not
    /// touched at all. The result is a bitmap plus a rank index — never a
    /// materialised copy of the matching rows.
    ///
    /// `row_budget` caps how many rows are examined, so an interactive caller
    /// can keep a 200M-row filter inside a frame budget and finish the rest in
    /// the background; the returned mask reports `is_truncated()` when it bit.
    /// Pass `usize::MAX` for a complete pass.
    ///
    /// # Measured cost
    ///
    /// `cargo run --release -p ferrix-bench --bin bench-filter` on 10M and 50M
    /// rows of 4-distinct-string data:
    ///
    /// | stage                  | 10M rows | 50M rows | scaling      |
    /// |------------------------|----------|----------|--------------|
    /// | arena pass (step 1)    | 0.006 ms | 0.003 ms | independent  |
    /// | text checklist scan    | 17 ms    | 92 ms    | ~540M rows/s |
    /// | numeric comparison     | 21 ms    | 101 ms   | ~490M rows/s |
    /// | two columns ANDed      | 34 ms    | 158 ms   | linear       |
    /// | `nth_visible` lookup   | 789 ns   | 796 ns   | independent  |
    ///
    /// The arena pass is flat because it is one comparison per *distinct*
    /// string, and the scan is flat per row because it compares 4-byte ids.
    /// Extrapolating the measured 540M rows/s, a single-column filter over
    /// 200M rows is ~370ms — not one frame, which is exactly why `row_budget`
    /// exists. `nth_visible` staying at ~790ns regardless of height is what
    /// keeps the *scrolling* of a filtered view free.
    ///
    /// Two honest caveats, both measured rather than assumed:
    ///
    /// * The bitmaps are sized by the sheet's total rows, not the budget, so a
    ///   bounded scan still pays an allocation proportional to the table's
    ///   height (31.9ms for a 1M-row budget over a 50M-row sheet, versus 7.2ms
    ///   over a 10M-row one). Reusing a mask across calls would fix that; it is
    ///   not done yet.
    /// * This is the in-RAM path only. A 200M-row dataset in `Sheet` form would
    ///   need ~7.8 GB (measured: 392 MB per 10M rows x 3 columns), so at that
    ///   size it lives in `BaseData::Mapped` — and `MappedSheet` has no
    ///   `filter_table` yet. The arena-first machinery is shared and
    ///   [`CompiledPredicate::compile_with`] exists precisely so the mapped
    ///   reader can adopt it, but that scan is not written.
    ///
    /// [`CompiledPredicate`]: crate::table::CompiledPredicate
    pub fn filter_table(
        &self,
        table: &crate::table::Table,
        row_budget: usize,
    ) -> crate::table::RowMask {
        let t = std::time::Instant::now();
        let rows = table.data_rows();
        let (r0, r1) = (rows.start as usize, rows.end as usize);
        let scan_end = r1.min(r0.saturating_add(row_budget));
        let truncated = scan_end < r1;

        // Rows outside the table are not the filter's business; they stay
        // visible so a table can sit inside a larger sheet.
        let total = self.row_count.max(r1);
        let mut mask = Bitmap::ones(total);
        // Anything past the budget is hidden rather than guessed at — a
        // partially-applied filter must not show rows it has not checked.
        for r in scan_end..r1 {
            mask.set(r, false);
        }

        for (i, tcol) in table.columns.iter().enumerate() {
            let Some(pred) = &tcol.filter else { continue };
            let compiled = crate::table::CompiledPredicate::compile(pred, &self.arena);
            let sheet_col = table.sheet_col(i) as usize;
            let mut accepted = Bitmap::zeros(total);
            if let Some(col) = self.columns.get(sheet_col) {
                col.scan_filter(r0, scan_end, &compiled, &mut accepted);
            }
            // AND into the running mask, restricted to the data rows.
            for r in r0..scan_end {
                if mask.get(r) && !accepted.get(r) {
                    mask.set(r, false);
                }
            }
        }

        crate::table::RowMask::from_bits(mask).with_stats(
            scan_end.saturating_sub(r0),
            truncated,
            t.elapsed().as_millis(),
        )
    }

    /// Build the uniqueness index for one table column, or `None` when the
    /// column does not need one.
    pub fn uniqueness_index(
        &self,
        table: &crate::table::Table,
        col_index: usize,
    ) -> Option<crate::table::UniquenessIndex> {
        let tcol = table.columns.get(col_index)?;
        if !tcol.needs_uniqueness() {
            return None;
        }
        let col = self.columns.get(table.sheet_col(col_index) as usize)?;
        let mut idx = crate::table::UniquenessIndex::new(self.arena.len());
        for r in table.data_rows() {
            idx.observe(&col.get(r as usize));
        }
        Some(idx)
    }

    /// Validate every cell of a table, capping the reported list at `limit`.
    ///
    /// Bounded like [`Sheet::search`]: `total` is honest even when `invalid`
    /// was cut short, so the UI can say "1,204,553 invalid cells" while only
    /// holding the first few hundred.
    pub fn validate_table(
        &self,
        table: &crate::table::Table,
        limit: usize,
    ) -> crate::table::ValidationReport {
        let t = std::time::Instant::now();
        let mut report = crate::table::ValidationReport::default();

        // Uniqueness needs one whole-column pass; do it once per column
        // instead of once per cell.
        let uniques: Vec<Option<crate::table::UniquenessIndex>> = (0..table.columns.len())
            .map(|i| self.uniqueness_index(table, i))
            .collect();

        for row in table.data_rows() {
            for (i, tcol) in table.columns.iter().enumerate() {
                if tcol.validation.is_vacuous() && tcol.ctype == crate::table::ColumnType::Any {
                    continue;
                }
                let cell = CellRef::new(row, table.sheet_col(i));
                let value = self.get(cell);
                // Resolving display text is the one string cost here, so it is
                // paid only for rules that actually need it.
                let text = match &tcol.validation.rule {
                    crate::table::ValidationRule::OneOf(_)
                    | crate::table::ValidationRule::Regex(_)
                    | crate::table::ValidationRule::TextLength { .. } => self.display(cell),
                    _ => String::new(),
                };
                if let Some(v) = table.validate_cell(i, &value, &text, uniques[i].as_ref()) {
                    report.total += 1;
                    if report.invalid.len() < limit {
                        report.invalid.push((cell, v));
                    } else {
                        report.truncated = true;
                    }
                }
            }
        }
        report.millis = t.elapsed().as_millis();
        report
    }
}

/// Merge per-column row lists into row-major order, stopping at `limit`.
///
/// Each column's list is already sorted, so this is a k-way merge taking the
/// smallest (row, col) at each step — no global sort of a huge result set.
pub fn merge_row_major(per_col: &[(usize, Vec<u32>)], limit: usize) -> Vec<CellRef> {
    let mut cursors = vec![0usize; per_col.len()];
    let mut out = Vec::new();
    loop {
        if out.len() >= limit {
            break;
        }
        let mut best: Option<(u32, usize, usize)> = None; // (row, col, which)
        for (i, (ci, rows)) in per_col.iter().enumerate() {
            if let Some(&r) = rows.get(cursors[i]) {
                let cand = (r, *ci, i);
                if best.is_none_or(|b| (cand.0, cand.1) < (b.0, b.1)) {
                    best = Some(cand);
                }
            }
        }
        match best {
            Some((row, col, which)) => {
                out.push(CellRef::new(row, col as u32));
                cursors[which] += 1;
            }
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_names_follow_excel() {
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(27), "AB");
        assert_eq!(column_name(51), "AZ");
        assert_eq!(column_name(52), "BA");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
        assert_eq!(column_name(16383), "XFD"); // Excel's last column
    }

    #[test]
    fn a1_roundtrip() {
        for &(r, c) in &[(0u32, 0u32), (0, 25), (0, 26), (99, 701), (1048575, 16383)] {
            let cell = CellRef::new(r, c);
            let a1 = cell.to_a1();
            assert_eq!(CellRef::from_a1(&a1), Some(cell), "roundtrip {a1}");
        }
        assert_eq!(CellRef::from_a1("A1"), Some(CellRef::new(0, 0)));
        assert_eq!(CellRef::from_a1("aa10"), Some(CellRef::new(9, 26)));
    }

    #[test]
    fn a1_rejects_garbage() {
        for bad in ["", "1", "A", "A0", "1A", "$$", "A-1"] {
            assert_eq!(CellRef::from_a1(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn sheet_read_write() {
        let mut s = Sheet::new("Sheet1");
        s.set(CellRef::new(0, 0), Value::Number(1.0));
        s.set_text(CellRef::new(1, 0), "hello");
        assert_eq!(s.get(CellRef::new(0, 0)), Value::Number(1.0));
        assert_eq!(s.display(CellRef::new(1, 0)), "hello");
        assert_eq!(s.display(CellRef::new(9, 9)), "");
        assert_eq!(s.row_count(), 2);
    }

    #[test]
    fn display_formats_each_variant() {
        let mut s = Sheet::new("s");
        s.set(CellRef::new(0, 0), Value::Number(42.0));
        s.set(CellRef::new(1, 0), Value::Bool(true));
        s.set(CellRef::new(2, 0), Value::Bool(false));
        s.set_text(CellRef::new(3, 0), "text");
        s.set(
            CellRef::new(4, 0),
            Value::Error(crate::value::ErrorKind::DivZero),
        );

        assert_eq!(s.display(CellRef::new(0, 0)), "42");
        assert_eq!(s.display(CellRef::new(1, 0)), "TRUE");
        assert_eq!(s.display(CellRef::new(2, 0)), "FALSE");
        assert_eq!(s.display(CellRef::new(3, 0)), "text");
        assert_eq!(s.display(CellRef::new(4, 0)), "#DIV/0!");
    }

    #[test]
    fn rect_aggregates() {
        let mut s = Sheet::new("s");
        for r in 0..10u32 {
            for c in 0..3u32 {
                s.set(CellRef::new(r, c), Value::Number((r * 3 + c) as f64));
            }
        }
        // 0..29 summed = 435
        assert_eq!(s.sum_rect(CellRef::new(0, 0), CellRef::new(9, 2)), 435.0);
        assert_eq!(s.count_rect(CellRef::new(0, 0), CellRef::new(9, 2)), 30);
        // Reversed corners must behave identically.
        assert_eq!(s.sum_rect(CellRef::new(9, 2), CellRef::new(0, 0)), 435.0);
    }

    #[test]
    fn headers_fall_back_to_letters() {
        let mut s = Sheet::new("s");
        s.set_headers(vec!["id".into(), String::new()]);
        assert_eq!(s.header_or_letter(0), "id");
        assert_eq!(s.header_or_letter(1), "B"); // empty header -> letter
        assert_eq!(s.header_or_letter(5), "F"); // missing header -> letter
    }

    fn search_sheet() -> Sheet {
        // 3 text columns of low cardinality plus a numeric column, mirroring
        // the shape of real data.
        let mut s = Sheet::new("t");
        let regions = ["north", "south", "east", "west"];
        for r in 0..100u32 {
            s.set_text(CellRef::new(r, 0), regions[r as usize % 4]);
            s.set_text(
                CellRef::new(r, 1),
                if r % 2 == 0 { "open" } else { "closed" },
            );
            s.set(CellRef::new(r, 2), Value::Number(r as f64));
        }
        s
    }

    fn q(needle: &str) -> crate::search::Query {
        crate::search::Query::new(needle, false, false).unwrap()
    }

    #[test]
    fn finds_text_matches_in_row_major_order() {
        let s = search_sheet();
        let r = s.search(&q("north"), 1000);
        assert_eq!(r.total, 25, "north appears every 4th row");
        // Row-major: rows must be ascending.
        let rows: Vec<u32> = r.matches.iter().map(|m| m.row).collect();
        assert!(rows.windows(2).all(|w| w[0] < w[1]), "not row-ordered");
        assert_eq!(r.matches[0], CellRef::new(0, 0));
        assert_eq!(r.matches[1], CellRef::new(4, 0));
    }

    #[test]
    fn substring_matches_partial_words() {
        let s = search_sheet();
        // "os" matches "closed" but not "open".
        let r = s.search(&q("os"), 1000);
        assert_eq!(r.total, 50);
        assert!(r.matches.iter().all(|m| m.col == 1));
    }

    #[test]
    fn search_is_case_insensitive_by_default() {
        let s = search_sheet();
        assert_eq!(s.search(&q("NORTH"), 1000).total, 25);
        assert_eq!(s.search(&q("NoRtH"), 1000).total, 25);
    }

    #[test]
    fn numbers_are_found_by_value() {
        let s = search_sheet();
        let r = s.search(&q("42"), 1000);
        assert_eq!(r.total, 1);
        assert_eq!(r.matches[0], CellRef::new(42, 2));
    }

    #[test]
    fn no_matches_returns_empty_not_error() {
        let s = search_sheet();
        let r = s.search(&q("zzzz-nothing"), 1000);
        assert_eq!(r.total, 0);
        assert!(r.matches.is_empty());
        assert_eq!(r.matched_strings, 0);
    }

    #[test]
    fn results_are_capped_but_total_is_honest() {
        let s = search_sheet();
        let r = s.search(&q("o"), 10);
        assert_eq!(r.matches.len(), 10, "capped at the limit");
        assert!(r.total > 10, "total reports the true count");
        assert!(r.truncated);
    }

    #[test]
    fn matches_across_columns_order_by_row_then_column() {
        let mut s = Sheet::new("t");
        // Same needle in two columns of the same row.
        s.set_text(CellRef::new(5, 0), "target");
        s.set_text(CellRef::new(5, 3), "target");
        s.set_text(CellRef::new(2, 7), "target");
        let r = s.search(&q("target"), 100);
        assert_eq!(
            r.matches,
            vec![CellRef::new(2, 7), CellRef::new(5, 0), CellRef::new(5, 3)],
            "must be row-major, then column-major within a row"
        );
    }

    #[test]
    fn search_cost_tracks_cardinality_not_rows() {
        // The core performance claim. A 200k-cell sheet drawn from 4 distinct
        // strings must require only 4 string comparisons to plan.
        let mut s = Sheet::new("big");
        let regions = ["north", "south", "east", "west"];
        for r in 0..50_000u32 {
            for c in 0..4u32 {
                s.set_text(CellRef::new(r, c), regions[(r as usize + c as usize) % 4]);
            }
        }
        let t = std::time::Instant::now();
        let res = s.search(&q("north"), 100);
        let ms = t.elapsed().as_millis();
        assert_eq!(res.total, 50_000);
        assert_eq!(
            res.matched_strings, 1,
            "only 'north' should match in the arena"
        );
        assert!(
            ms < 200,
            "200k-cell search took {ms}ms — the arena fast path may be broken"
        );
    }

    #[test]
    fn boolean_cells_are_searchable() {
        let mut s = Sheet::new("t");
        s.set(CellRef::new(0, 0), Value::Bool(true));
        s.set(CellRef::new(1, 0), Value::Bool(false));
        assert_eq!(s.search(&q("true"), 10).total, 1);
        assert_eq!(s.search(&q("false"), 10).total, 1);
    }

    #[test]
    fn error_cells_are_searchable() {
        // Regression: the scanner's whole-column guard did not account for
        // error cells, so searching "DIV" returned nothing even though a
        // #DIV/0! cell was present.
        let mut s = Sheet::new("t");
        s.set(CellRef::new(0, 0), Value::Error(crate::ErrorKind::DivZero));
        s.set(CellRef::new(1, 0), Value::Error(crate::ErrorKind::Ref));
        assert_eq!(s.search(&q("DIV"), 10).total, 1);
        assert_eq!(s.search(&q("REF"), 10).total, 1);
        assert_eq!(s.search(&q("#"), 10).total, 2, "both are error cells");
    }
}
