//! Columnar cell storage.
//!
//! Layout rationale: spreadsheets are read column-wise far more often than
//! row-wise (aggregations, sorts, filters, and the renderer's per-column
//! width pass), and a column of homogeneous data compresses into a typed
//! vector that SIMD and the branch predictor both love.
//!
//! Each column keeps parallel arrays:
//!   - `tags`:    1 byte/cell discriminant
//!   - `numbers`: 8 bytes/cell, only allocated if the column ever holds a number
//!   - `strings`: 4 bytes/cell arena ids, only allocated if ever holds text
//!   - `present`: 1 bit/cell validity
//!
//! A pure-numeric column therefore costs ~9.125 bytes/cell, so 10M rows is
//! ~91 MB per column — the number that makes the whole 10M-row target viable.

use crate::arena::StrId;
use crate::bitmap::Bitmap;
use crate::value::{ErrorKind, Value, ValueTag};

/// A single column of cells.
#[derive(Clone, Debug, Default)]
pub struct Column {
    tags: Vec<u8>,
    numbers: Vec<f64>,
    strings: Vec<u32>,
    present: Bitmap,
    /// Lazily allocated: a column that never sees text pays nothing for it.
    has_numbers: bool,
    has_strings: bool,
}

impl Column {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(rows: usize) -> Self {
        Self {
            tags: Vec::with_capacity(rows),
            numbers: Vec::new(),
            strings: Vec::new(),
            present: Bitmap::with_capacity(rows),
            has_numbers: false,
            has_strings: false,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.tags.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }

    /// Number of non-empty cells.
    #[inline]
    pub fn populated(&self) -> usize {
        self.present.count_set()
    }

    fn ensure_numbers(&mut self) {
        if !self.has_numbers {
            self.numbers.resize(self.tags.len(), 0.0);
            self.has_numbers = true;
        }
    }

    fn ensure_strings(&mut self) {
        if !self.has_strings {
            self.strings.resize(self.tags.len(), 0);
            self.has_strings = true;
        }
    }

    /// Grow the column so that `row` is addressable, filling gaps with Empty.
    pub fn ensure_rows(&mut self, rows: usize) {
        if rows <= self.tags.len() {
            return;
        }
        self.tags.resize(rows, ValueTag::Empty as u8);
        if self.has_numbers {
            self.numbers.resize(rows, 0.0);
        }
        if self.has_strings {
            self.strings.resize(rows, 0);
        }
        self.present.resize(rows, false);
    }

    /// Read a cell. Out-of-range reads are `Empty`, matching sheet semantics
    /// where the grid is conceptually infinite.
    #[inline]
    pub fn get(&self, row: usize) -> Value {
        if row >= self.tags.len() {
            return Value::Empty;
        }
        match self.tags[row] {
            x if x == ValueTag::Number as u8 => Value::Number(self.numbers[row]),
            x if x == ValueTag::Bool as u8 => Value::Bool(self.numbers[row] != 0.0),
            x if x == ValueTag::Text as u8 => Value::Text(StrId(self.strings[row])),
            x if x == ValueTag::Error as u8 => Value::Error(decode_error(self.numbers[row] as u8)),
            _ => Value::Empty,
        }
    }

    /// Write a cell, growing the column if needed.
    pub fn set(&mut self, row: usize, value: Value) {
        self.ensure_rows(row + 1);
        match value {
            Value::Empty => {
                self.tags[row] = ValueTag::Empty as u8;
                self.present.set(row, false);
                return;
            }
            Value::Number(n) => {
                self.ensure_numbers();
                self.numbers[row] = n;
                self.tags[row] = ValueTag::Number as u8;
            }
            Value::Bool(b) => {
                self.ensure_numbers();
                self.numbers[row] = if b { 1.0 } else { 0.0 };
                self.tags[row] = ValueTag::Bool as u8;
            }
            Value::Text(id) => {
                self.ensure_strings();
                self.strings[row] = id.0;
                self.tags[row] = ValueTag::Text as u8;
            }
            Value::Error(e) => {
                self.ensure_numbers();
                self.numbers[row] = encode_error(e) as f64;
                self.tags[row] = ValueTag::Error as u8;
            }
        }
        self.present.set(row, true);
    }

    /// Append a cell to the end.
    pub fn push(&mut self, value: Value) {
        let row = self.tags.len();
        self.tags.push(ValueTag::Empty as u8);
        if self.has_numbers {
            self.numbers.push(0.0);
        }
        if self.has_strings {
            self.strings.push(0);
        }
        self.present.push(false);
        if !value.is_empty() {
            self.set(row, value);
        }
    }

    /// Bulk-append a run of numbers. This is the CSV ingest fast path: one
    /// resize instead of N pushes, and no per-cell branch on the tag.
    pub fn extend_numbers(&mut self, values: &[f64]) {
        if values.is_empty() {
            return;
        }
        self.ensure_numbers();
        let start = self.tags.len();
        self.tags
            .resize(start + values.len(), ValueTag::Number as u8);
        self.numbers.extend_from_slice(values);
        if self.has_strings {
            self.strings.resize(start + values.len(), 0);
        }
        self.present.resize(start + values.len(), true);
    }

    /// True when no cell in `[start, end)` holds anything. The renderer uses
    /// this to skip blank bands without per-cell work.
    #[inline]
    pub fn is_range_empty(&self, start: usize, end: usize) -> bool {
        self.present.is_range_empty(start, end)
    }

    /// Sum of numeric cells in a row range, ignoring text/empty. Operates
    /// directly on the typed array so the loop stays tight and vectorizable.
    pub fn sum_range(&self, start: usize, end: usize) -> f64 {
        if !self.has_numbers {
            return 0.0;
        }
        let end = end.min(self.tags.len());
        if start >= end {
            return 0.0;
        }
        let mut acc = 0.0;
        let num_tag = ValueTag::Number as u8;
        for i in start..end {
            if self.tags[i] == num_tag {
                acc += self.numbers[i];
            }
        }
        acc
    }

    /// Count of numeric cells in a row range.
    pub fn count_numeric(&self, start: usize, end: usize) -> usize {
        let end = end.min(self.tags.len());
        if start >= end {
            return 0;
        }
        let num_tag = ValueTag::Number as u8;
        self.tags[start..end]
            .iter()
            .filter(|&&t| t == num_tag)
            .count()
    }

    /// Min and max of numeric cells in a range.
    pub fn min_max_range(&self, start: usize, end: usize) -> Option<(f64, f64)> {
        if !self.has_numbers {
            return None;
        }
        let end = end.min(self.tags.len());
        let num_tag = ValueTag::Number as u8;
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        let mut seen = false;
        for i in start..end {
            if self.tags[i] == num_tag {
                let v = self.numbers[i];
                if v < lo {
                    lo = v;
                }
                if v > hi {
                    hi = v;
                }
                seen = true;
            }
        }
        seen.then_some((lo, hi))
    }

    /// Approximate heap footprint in bytes — surfaced in the UI status bar.
    pub fn heap_bytes(&self) -> usize {
        self.tags.capacity()
            + self.numbers.capacity() * 8
            + self.strings.capacity() * 4
            + self.present.heap_bytes()
    }

    /// Release excess capacity after ingest.
    pub fn shrink_to_fit(&mut self) {
        self.tags.shrink_to_fit();
        self.numbers.shrink_to_fit();
        self.strings.shrink_to_fit();
    }
}

#[inline]
const fn encode_error(e: ErrorKind) -> u8 {
    match e {
        ErrorKind::DivZero => 0,
        ErrorKind::Value => 1,
        ErrorKind::Ref => 2,
        ErrorKind::Name => 3,
        ErrorKind::Num => 4,
        ErrorKind::NotAvailable => 5,
        ErrorKind::Null => 6,
        ErrorKind::Circular => 7,
    }
}

#[inline]
const fn decode_error(b: u8) -> ErrorKind {
    match b {
        0 => ErrorKind::DivZero,
        1 => ErrorKind::Value,
        2 => ErrorKind::Ref,
        3 => ErrorKind::Name,
        4 => ErrorKind::Num,
        5 => ErrorKind::NotAvailable,
        6 => ErrorKind::Null,
        _ => ErrorKind::Circular,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_all_variants() {
        let mut c = Column::new();
        c.set(0, Value::Number(1.5));
        c.set(1, Value::Bool(true));
        c.set(2, Value::Text(StrId(7)));
        c.set(3, Value::Error(ErrorKind::DivZero));
        c.set(4, Value::Empty);

        assert_eq!(c.get(0), Value::Number(1.5));
        assert_eq!(c.get(1), Value::Bool(true));
        assert_eq!(c.get(2), Value::Text(StrId(7)));
        assert_eq!(c.get(3), Value::Error(ErrorKind::DivZero));
        assert_eq!(c.get(4), Value::Empty);
    }

    #[test]
    fn all_error_kinds_roundtrip() {
        let kinds = [
            ErrorKind::DivZero,
            ErrorKind::Value,
            ErrorKind::Ref,
            ErrorKind::Name,
            ErrorKind::Num,
            ErrorKind::NotAvailable,
            ErrorKind::Null,
            ErrorKind::Circular,
        ];
        let mut c = Column::new();
        for (i, &k) in kinds.iter().enumerate() {
            c.set(i, Value::Error(k));
        }
        for (i, &k) in kinds.iter().enumerate() {
            assert_eq!(c.get(i), Value::Error(k), "kind {k:?}");
        }
    }

    #[test]
    fn sparse_write_fills_gaps_with_empty() {
        let mut c = Column::new();
        c.set(1000, Value::Number(42.0));
        assert_eq!(c.len(), 1001);
        assert_eq!(c.get(0), Value::Empty);
        assert_eq!(c.get(999), Value::Empty);
        assert_eq!(c.get(1000), Value::Number(42.0));
        assert_eq!(c.populated(), 1);
    }

    #[test]
    fn out_of_range_reads_are_empty() {
        let c = Column::new();
        assert_eq!(c.get(0), Value::Empty);
        assert_eq!(c.get(999_999), Value::Empty);
    }

    #[test]
    fn clearing_a_cell_updates_presence() {
        let mut c = Column::new();
        c.set(5, Value::Number(1.0));
        assert_eq!(c.populated(), 1);
        c.set(5, Value::Empty);
        assert_eq!(c.populated(), 0);
        assert!(c.is_range_empty(0, 10));
    }

    #[test]
    fn bulk_number_append() {
        let mut c = Column::new();
        let vals: Vec<f64> = (0..1000).map(|i| i as f64).collect();
        c.extend_numbers(&vals);
        assert_eq!(c.len(), 1000);
        assert_eq!(c.get(0), Value::Number(0.0));
        assert_eq!(c.get(999), Value::Number(999.0));
        assert_eq!(c.populated(), 1000);
        assert_eq!(c.sum_range(0, 1000), 499_500.0);
    }

    #[test]
    fn aggregates_skip_non_numeric() {
        let mut c = Column::new();
        c.set(0, Value::Number(10.0));
        c.set(1, Value::Text(StrId(0)));
        c.set(2, Value::Number(20.0));
        c.set(3, Value::Empty);
        c.set(4, Value::Number(-5.0));

        assert_eq!(c.sum_range(0, 5), 25.0);
        assert_eq!(c.count_numeric(0, 5), 3);
        assert_eq!(c.min_max_range(0, 5), Some((-5.0, 20.0)));
    }

    #[test]
    fn aggregates_on_empty_column() {
        let c = Column::new();
        assert_eq!(c.sum_range(0, 100), 0.0);
        assert_eq!(c.count_numeric(0, 100), 0);
        assert_eq!(c.min_max_range(0, 100), None);
    }

    #[test]
    fn text_column_allocates_no_number_array() {
        let mut c = Column::new();
        for i in 0..100 {
            c.push(Value::Text(StrId(i)));
        }
        assert!(
            !c.has_numbers,
            "text-only column should not allocate f64 storage"
        );
        assert_eq!(c.get(50), Value::Text(StrId(50)));
    }

    #[test]
    fn numeric_column_memory_is_within_budget() {
        // The 10M-row thesis: ~9.125 bytes/cell for a pure numeric column.
        let n = 100_000;
        let mut c = Column::with_capacity(n);
        c.extend_numbers(&vec![1.0; n]);
        let per_cell = c.heap_bytes() as f64 / n as f64;
        assert!(
            per_cell < 12.0,
            "numeric column costs {per_cell:.2} bytes/cell; 10M rows would need {:.0} MB",
            per_cell * 10_000_000.0 / 1_048_576.0
        );
    }
}
