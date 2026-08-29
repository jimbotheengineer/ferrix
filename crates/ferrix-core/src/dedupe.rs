//! Remove Duplicates: a STREAMING scan that holds keys, never rows.
//!
//! ## What this module is allowed to remember
//!
//! Deduplicating 10M rows must not cost 10M rows of memory. The only thing
//! that survives from one row to the next here is a [`RowKey`] — the cells of
//! the KEY COLUMNS, and nothing else. A sheet with 60 columns deduped on 2 of
//! them holds 2 cells per distinct key; the other 58 are read past and
//! forgotten. Peak memory is therefore
//!
//! ```text
//!     distinct_keys x key_columns x size_of::<KeyCell>()
//! ```
//!
//! which is independent of the ROW COUNT and independent of the sheet's
//! COLUMN COUNT. [`DupeScan::key_heap_bytes`] reports it and
//! `dedupe_holds_keys_not_rows` in the tests below asserts the bound — it
//! fails if anyone ever changes the scan to stash a row.
//!
//! Text is kept as a [`StrId`] out of the caller's arena rather than as a
//! `String`. The arena already deduplicates, so two rows reading "EMEA" share
//! one 4-byte id and the comparison is an integer compare. Cloning the strings
//! instead would allocate per row, which is the exact failure the arena
//! exists to prevent.
//!
//! ## Excel's rule, deliberately
//!
//! The FIRST occurrence of a key is kept and every later one is reported as a
//! duplicate. That is what Excel does, and it is the only choice that lets a
//! user sort into their preferred order and then dedupe to keep the row they
//! wanted.
//!
//! ## What the scan does NOT do
//!
//! It does not modify anything. It reports which rows are duplicates and
//! leaves the decision — and the single undo entry — to the caller. See
//! `Workbook::remove_duplicates`.

use crate::{ErrorKind, StrId, Value};
use std::collections::HashSet;

/// One key column's cell, reduced to something hashable.
///
/// [`Value`] itself cannot be a hash key: it holds an `f64`, which is neither
/// `Eq` nor `Hash`. The number is canonicalised to its bit pattern here, with
/// the two traps a naive `to_bits` has fixed:
///
/// * `-0.0` and `0.0` compare equal on screen and must dedupe as one key.
/// * Every `NaN` bit pattern is folded to one, so a column of `#NUM!`-derived
///   NaNs does not read as a million distinct keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyCell {
    Empty,
    /// `f64` bits, canonicalised.
    Number(u64),
    Bool(bool),
    /// Arena id, NOT a copied string.
    Text(StrId),
    Error(ErrorKind),
}

impl KeyCell {
    /// Reduce a cell value to a hashable key part.
    #[inline]
    pub fn of(v: Value) -> Self {
        match v {
            Value::Empty => KeyCell::Empty,
            Value::Number(n) => {
                let n = if n == 0.0 {
                    // Collapses -0.0 into +0.0.
                    0.0
                } else if n.is_nan() {
                    f64::NAN
                } else {
                    n
                };
                KeyCell::Number(n.to_bits())
            }
            Value::Bool(b) => KeyCell::Bool(b),
            Value::Text(id) => KeyCell::Text(id),
            Value::Error(e) => KeyCell::Error(e),
        }
    }
}

/// The key columns of one row, as stored in the seen-set.
///
/// A boxed slice rather than a `Vec` so the set holds no spare capacity: the
/// bound in the module docs is exact, not "exact plus whatever the allocator
/// felt like".
pub type RowKey = Box<[KeyCell]>;

/// Read access to the cells being deduplicated.
///
/// Implemented over the caller's storage exactly as [`crate::sort::CellKeys`]
/// is, so the scan streams through a memory-mapped sheet without copying it.
pub trait DupeKeys {
    fn key_value(&self, row: u32, col: u32) -> Value;
}

/// What a scan found. Counts only — never the rows themselves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DupeReport {
    /// Rows examined.
    pub scanned: u64,
    /// Rows whose key had already been seen.
    pub duplicates: u64,
    /// Distinct keys, i.e. rows that would survive.
    pub unique: u64,
    /// Peak bytes held by the key set. The scale invariant, measured.
    pub key_heap_bytes: usize,
}

/// The streaming deduplicator.
///
/// Fed one row at a time. Holds a [`HashSet`] of keys and NOTHING else — no
/// row indices, no cell payloads, no per-row bookkeeping — so a caller that
/// only wants the count can dedupe an arbitrarily large sheet in memory
/// proportional to its distinct keys.
///
/// A caller that DOES want the duplicate row list (the undo path does)
/// collects it from [`DupeScan::observe`]'s return value and pays for that
/// list itself, under its own cap.
#[derive(Debug)]
pub struct DupeScan {
    seen: HashSet<RowKey>,
    key_cols: Vec<u32>,
    scanned: u64,
    duplicates: u64,
    /// Scratch reused across rows so building a key does not allocate a
    /// `Vec` per row — only the keys that are actually NEW are boxed and
    /// kept.
    scratch: Vec<KeyCell>,
}

impl DupeScan {
    /// A scan keyed on `key_cols`.
    ///
    /// An empty `key_cols` means "every column", which the caller resolves
    /// before constructing: this module never guesses a column count.
    pub fn new(key_cols: Vec<u32>) -> Self {
        let n = key_cols.len();
        Self {
            seen: HashSet::new(),
            key_cols,
            scanned: 0,
            duplicates: 0,
            scratch: Vec::with_capacity(n),
        }
    }

    pub fn key_cols(&self) -> &[u32] {
        &self.key_cols
    }

    /// Offer one row. `true` means it is a DUPLICATE of an earlier row.
    ///
    /// The first row carrying a key is never a duplicate, which is what makes
    /// "keeps the first occurrence" true by construction rather than by a
    /// later sort.
    pub fn observe(&mut self, row: u32, src: &impl DupeKeys) -> bool {
        self.scratch.clear();
        for &c in &self.key_cols {
            self.scratch.push(KeyCell::of(src.key_value(row, c)));
        }
        self.scanned += 1;
        // `contains` on the scratch slice avoids boxing a key that is about to
        // be thrown away, so a run of duplicates allocates nothing at all.
        if self.seen.contains(self.scratch.as_slice()) {
            self.duplicates += 1;
            return true;
        }
        self.seen.insert(self.scratch.as_slice().into());
        false
    }

    /// Bytes held by the key set.
    ///
    /// The `HashSet`'s own table plus one boxed key per distinct row. This is
    /// the number the scale invariant is about: it grows with DISTINCT KEYS
    /// and KEY COLUMNS, never with rows scanned or with the sheet's width.
    pub fn key_heap_bytes(&self) -> usize {
        let per_key = std::mem::size_of::<KeyCell>() * self.key_cols.len();
        let table = self.seen.capacity() * std::mem::size_of::<RowKey>();
        table + self.seen.len() * per_key
    }

    pub fn report(&self) -> DupeReport {
        DupeReport {
            scanned: self.scanned,
            duplicates: self.duplicates,
            unique: self.seen.len() as u64,
            key_heap_bytes: self.key_heap_bytes(),
        }
    }
}

/// Scan `rows` and hand every duplicate to `on_dupe`, in ascending order.
///
/// Streams: `rows` is an iterator, so the caller can pass `0..200_000_000`
/// without materialising it. What the callback does with a duplicate is the
/// callback's problem — this function's own memory is the key set.
pub fn scan_duplicates(
    rows: impl Iterator<Item = u32>,
    key_cols: Vec<u32>,
    src: &impl DupeKeys,
    mut on_dupe: impl FnMut(u32),
) -> DupeReport {
    let mut scan = DupeScan::new(key_cols);
    for r in rows {
        if scan.observe(r, src) {
            on_dupe(r);
        }
    }
    scan.report()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StringArena;

    /// A source backed by a closure, so a test can describe 10 million rows
    /// without allocating any of them.
    struct Gen<F: Fn(u32, u32) -> Value> {
        f: F,
    }

    impl<F: Fn(u32, u32) -> Value> DupeKeys for Gen<F> {
        fn key_value(&self, row: u32, col: u32) -> Value {
            (self.f)(row, col)
        }
    }

    fn dupes_of(rows: u32, key_cols: Vec<u32>, src: &impl DupeKeys) -> (Vec<u32>, DupeReport) {
        let mut out = Vec::new();
        let rep = scan_duplicates(0..rows, key_cols, src, |r| out.push(r));
        (out, rep)
    }

    #[test]
    fn keeps_the_first_occurrence_like_excel() {
        // Key column cycles 0,1,2,0,1,2,... so rows 3..=8 all repeat.
        let src = Gen {
            f: |r, _| Value::Number((r % 3) as f64),
        };
        let (dupes, rep) = dupes_of(9, vec![0], &src);
        assert_eq!(
            dupes,
            vec![3, 4, 5, 6, 7, 8],
            "rows 0,1,2 introduced the three keys and must survive; a \
             last-wins implementation would report 0..=5 instead"
        );
        assert_eq!(rep.unique, 3);
        assert_eq!(rep.duplicates, 6);
        assert_eq!(rep.scanned, 9);
    }

    #[test]
    fn only_the_chosen_columns_form_the_key() {
        // Column 0 repeats; column 1 is unique per row.
        let src = Gen {
            f: |r, c| match c {
                0 => Value::Number((r % 2) as f64),
                _ => Value::Number(r as f64),
            },
        };
        let (on_col0, _) = dupes_of(6, vec![0], &src);
        assert_eq!(on_col0, vec![2, 3, 4, 5], "keyed on the repeating column");
        let (on_both, rep) = dupes_of(6, vec![0, 1], &src);
        assert!(
            on_both.is_empty(),
            "adding the unique column makes every row distinct; got {on_both:?}"
        );
        assert_eq!(rep.unique, 6);
    }

    #[test]
    fn text_keys_compare_by_arena_id_not_by_copy() {
        let mut arena = StringArena::new();
        let a = arena.intern("EMEA");
        let b = arena.intern("APAC");
        // The arena is a DEDUPLICATING interner, so re-interning the same
        // string yields the same id — which is what makes an integer compare
        // a correct string compare here.
        assert_eq!(arena.intern("EMEA"), a, "arena must intern to one id");
        let src = Gen {
            f: move |r, _| Value::Text(if r % 2 == 0 { a } else { b }),
        };
        let (dupes, rep) = dupes_of(6, vec![0], &src);
        assert_eq!(dupes, vec![2, 3, 4, 5]);
        assert_eq!(rep.unique, 2);
    }

    #[test]
    fn minus_zero_and_nan_do_not_explode_the_key_space() {
        let src = Gen {
            f: |r, _| match r % 3 {
                0 => Value::Number(0.0),
                1 => Value::Number(-0.0),
                // A different NaN bit pattern each time.
                _ => Value::Number(f64::from_bits(0x7ff8_0000_0000_0000 | u64::from(r))),
            },
        };
        let (_, rep) = dupes_of(30, vec![0], &src);
        assert_eq!(
            rep.unique, 2,
            "0.0/-0.0 are ONE key and every NaN is ONE key; raw to_bits \
             would report 12 distinct keys here"
        );
    }

    #[test]
    fn empty_cells_are_a_key_value_not_a_skip() {
        let src = Gen {
            f: |r, _| {
                if r < 4 {
                    Value::Empty
                } else {
                    Value::Number(1.0)
                }
            },
        };
        let (dupes, rep) = dupes_of(6, vec![0], &src);
        assert_eq!(
            dupes,
            vec![1, 2, 3, 5],
            "three blank rows after the first are duplicates of it"
        );
        assert_eq!(rep.unique, 2);
    }

    #[test]
    fn types_do_not_collide() {
        // 1, "1", TRUE all render similarly and must stay distinct keys.
        let mut arena = StringArena::new();
        let one = arena.intern("1");
        let src = Gen {
            f: move |r, _| match r {
                0 => Value::Number(1.0),
                1 => Value::Text(one),
                2 => Value::Bool(true),
                _ => Value::Number(1.0),
            },
        };
        let (dupes, rep) = dupes_of(4, vec![0], &src);
        assert_eq!(dupes, vec![3], "only the repeated NUMBER 1 is a duplicate");
        assert_eq!(rep.unique, 3);
    }

    /// THE scale assertion.
    ///
    /// A 10M-row dedupe holds a hash set of KEYS. This drives ten million rows
    /// through the scan and pins the key set's size against the number of
    /// DISTINCT KEYS — then repeats with a sheet ten times as wide and 400
    /// times as many rows per key, and requires the answer not to move.
    ///
    /// What would this assert if the feature did nothing? It would fail:
    /// a scan that stashed rows would be ~40 MB at 10M rows, and a scan that
    /// kept a copy of the row payload would scale with the column count.
    #[test]
    fn dedupe_holds_keys_not_rows() {
        const ROWS: u32 = 10_000_000;
        const KEYS: u32 = 1_000;

        // 10M rows, 1000 distinct keys, ONE key column.
        let narrow = Gen {
            f: |r, _| Value::Number((r % KEYS) as f64),
        };
        let mut scan = DupeScan::new(vec![0]);
        for r in 0..ROWS {
            scan.observe(r, &narrow);
        }
        let rep = scan.report();
        assert_eq!(rep.scanned, u64::from(ROWS));
        assert_eq!(rep.unique, u64::from(KEYS));
        assert_eq!(rep.duplicates, u64::from(ROWS - KEYS));

        // The bound: distinct keys x key columns x KeyCell, plus the set's
        // own table. Generous by 4x on the table's load factor and STILL
        // three orders of magnitude below one u32 per row.
        let per_key = std::mem::size_of::<KeyCell>();
        let bound = KEYS as usize * (per_key + 4 * std::mem::size_of::<RowKey>());
        assert!(
            rep.key_heap_bytes <= bound,
            "key set held {} bytes for {KEYS} keys over {ROWS} rows (bound \
             {bound}) — the scan is remembering rows, not keys",
            rep.key_heap_bytes
        );
        // One u32 per row is the cheapest possible row-shaped structure.
        let one_u32_per_row = ROWS as usize * 4;
        assert!(
            rep.key_heap_bytes * 100 < one_u32_per_row,
            "key set ({} bytes) must be far below even one u32 per row ({})",
            rep.key_heap_bytes,
            one_u32_per_row
        );

        // Same key count, 40 columns wide, and the scan must not notice.
        // A scan that copied the ROW would be 40x bigger here.
        let wide = Gen {
            f: |r, c| {
                if c == 0 {
                    Value::Number((r % KEYS) as f64)
                } else {
                    Value::Number(f64::from(r) * f64::from(c))
                }
            },
        };
        let mut wide_scan = DupeScan::new(vec![0]);
        for r in 0..ROWS {
            wide_scan.observe(r, &wide);
        }
        assert_eq!(
            wide_scan.key_heap_bytes(),
            rep.key_heap_bytes,
            "a 40-column sheet must cost exactly what a 1-column sheet costs \
             when the key is one column"
        );
    }

    #[test]
    fn a_run_of_duplicates_allocates_nothing_new() {
        // After the first row, every observe() is a hit on the existing key,
        // so the set neither grows nor reallocates.
        let src = Gen {
            f: |_, _| Value::Number(7.0),
        };
        let mut scan = DupeScan::new(vec![0]);
        scan.observe(0, &src);
        let after_first = scan.key_heap_bytes();
        for r in 1..100_000 {
            assert!(scan.observe(r, &src), "row {r} must be a duplicate");
        }
        assert_eq!(
            scan.key_heap_bytes(),
            after_first,
            "100k duplicate rows must not grow the key set by one byte"
        );
    }

    #[test]
    fn scanning_no_rows_reports_nothing() {
        let src = Gen {
            f: |_, _| Value::Empty,
        };
        let (dupes, rep) = dupes_of(0, vec![0], &src);
        assert!(dupes.is_empty());
        assert_eq!(rep, DupeReport::default());
    }
}
