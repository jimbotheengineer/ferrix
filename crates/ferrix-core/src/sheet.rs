//! Sheet: a collection of columns plus the shared string arena.

use crate::arena::{StrId, StringArena};
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
}
