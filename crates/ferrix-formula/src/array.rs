//! The array-result evaluation fork (#27 P1): a formula-only 2-D value shape.
//!
//! ## Why this lives here and not in `ferrix-core`
//!
//! `ferrix_core::Value` is the PER-CELL columnar type. It is `Copy`, and a
//! guard test (`value_stays_16_bytes`) pins it to 16 bytes — because at 10M
//! rows every extra byte is 10 MB of RAM and a worse cache-miss rate while
//! scrolling (see `.github/AGENT_GUIDE.md`, invariant "Value <= 16 bytes").
//! Adding an owned `Array(Vec<..>)` variant to it would either blow that
//! budget or break `Copy` and force clones through the entire columnar store.
//!
//! A dynamic array is NOT a property of a cell; it is a property of one
//! formula's RESULT. So it lives on the evaluation side, in this crate,
//! entirely out of the store's hot path. The store keeps storing 16-byte
//! `Value` scalars; when spill lands (#27 P2) each covered cell will hold its
//! own scalar projection, and only the host formula will own the [`ArrayData`].
//!
//! ## The two shapes
//!
//! [`EvalResult`] is what an array-aware evaluation returns: either a
//! `Scalar(Value)` (everything today) or an `Array(ArrayData)`. The legacy
//! scalar entrypoint collapses an `Array` to its top-left cell — Excel's
//! "implicit intersection" — so every existing caller (formula bar, dependency
//! graph, xlsx writeback) keeps seeing a bare `Value` and nothing about their
//! contract changes. Spill (#27 P2) is what will let an `Array` paint into
//! neighbouring cells instead of collapsing.
//!
//! ## The scale invariant still holds
//!
//! An [`ArrayData`]'s memory is bounded by its own extent — the RESULT size —
//! never by the scan input. Materialising `A1:A5` allocates five values;
//! materialising the first five rows of a 200M-row column also allocates five.
//! The bounds live in `rows`/`cols`, and construction from a range reads
//! exactly `rows * cols` cells through the existing columnar `spec_get` path.

use ferrix_core::Value;

use crate::eval::{eval_view_array, range_spec, spec_get, CellSource};
use crate::parser::Expr;

/// A rectangular, row-major grid of cell values produced by one formula.
///
/// Always at least 1x1. Memory is bounded by `rows * cols` — the result
/// extent — and never by the sheet the values were read from.
#[derive(Clone, Debug, PartialEq)]
pub struct ArrayData {
    rows: u32,
    cols: u32,
    /// Row-major: `cells[r * cols + c]`. Length is exactly `rows * cols`.
    cells: Vec<Value>,
}

impl ArrayData {
    /// Build from row-major cells. Panics if `cells.len() != rows * cols` or
    /// if either dimension is zero — an array is never empty, matching Excel
    /// where the smallest dynamic result is a single cell.
    pub fn from_cells(rows: u32, cols: u32, cells: Vec<Value>) -> Self {
        assert!(rows >= 1 && cols >= 1, "an array is at least 1x1");
        assert_eq!(
            cells.len(),
            rows as usize * cols as usize,
            "cell count must equal rows * cols"
        );
        Self { rows, cols, cells }
    }

    /// A 1x1 array wrapping a single value. Rarely what you want — a scalar
    /// result should stay [`EvalResult::Scalar`] so callers pay no allocation
    /// — but useful when a genuinely array-native op has a degenerate case.
    pub fn scalar(v: Value) -> Self {
        Self {
            rows: 1,
            cols: 1,
            cells: vec![v],
        }
    }

    #[inline]
    pub fn rows(&self) -> u32 {
        self.rows
    }

    #[inline]
    pub fn cols(&self) -> u32 {
        self.cols
    }

    /// The cell at `(r, c)`. Out-of-range reads are [`Value::Empty`], matching
    /// how Excel pads a jagged read, and are never a sheet read or a panic.
    #[inline]
    pub fn get(&self, r: u32, c: u32) -> Value {
        if r >= self.rows || c >= self.cols {
            return Value::Empty;
        }
        self.cells[r as usize * self.cols as usize + c as usize]
    }

    /// The top-left cell — the value an array collapses to under implicit
    /// intersection when a scalar caller consumes it.
    #[inline]
    pub fn top_left(&self) -> Value {
        self.cells.first().copied().unwrap_or(Value::Empty)
    }

    /// Row-major iterator over every cell, in reading order.
    pub fn iter(&self) -> impl Iterator<Item = Value> + '_ {
        self.cells.iter().copied()
    }
}

/// What an array-aware evaluation yields: a scalar (everything today) or a
/// materialised 2-D array (a dynamic-array formula's result).
///
/// The whole fork rests on keeping these two shapes distinct all the way to
/// the boundary. Collapsing to a scalar too early loses the spill; wrapping
/// every scalar in a 1x1 array costs an allocation per cell across the sheet.
#[derive(Clone, Debug, PartialEq)]
pub enum EvalResult {
    Scalar(Value),
    Array(ArrayData),
}

impl EvalResult {
    /// Collapse to a scalar via implicit intersection: a scalar is itself, an
    /// array is its top-left cell. This is the seam the legacy `eval_view`
    /// crosses so every existing caller keeps receiving a bare `Value`.
    #[inline]
    pub fn into_scalar(self) -> Value {
        match self {
            EvalResult::Scalar(v) => v,
            EvalResult::Array(a) => a.top_left(),
        }
    }

    /// Borrow as an array if it is one.
    #[inline]
    pub fn as_array(&self) -> Option<&ArrayData> {
        match self {
            EvalResult::Array(a) => Some(a),
            EvalResult::Scalar(_) => None,
        }
    }
}

impl From<Value> for EvalResult {
    #[inline]
    fn from(v: Value) -> Self {
        EvalResult::Scalar(v)
    }
}

impl From<ArrayData> for EvalResult {
    #[inline]
    fn from(a: ArrayData) -> Self {
        EvalResult::Array(a)
    }
}

/// Every function name the array family owns.
///
/// This is the single source of truth for [`is_array_fn`] and for the
/// dispatch-seam mutual-exclusion test. #27 P3 will grow this list with the
/// 16 dynamic-array functions (UNIQUE, SORT, FILTER, SEQUENCE, ...); P1 seeds
/// it with the one foundational array-consuming builtin needed to prove the
/// seam carries arrays through the shared `eval_call` match uncollapsed.
pub const ARRAY_FN_NAMES: &[&str] = &["ARRAYTOTEXT"];

/// Does the array family own `name`? Guard predicate for `eval_call`.
///
/// Exact-match only, like every sibling family predicate — a prefix or
/// contains test would over-claim and silently swallow a near-named function
/// from another family (guard arms match in order; the first claimant wins).
#[inline]
pub fn is_array_fn(name: &str) -> bool {
    ARRAY_FN_NAMES.contains(&name)
}

/// Dispatch an array-family call. Returns an [`EvalResult`] so the shape the
/// function chooses (scalar or array) is carried across the seam intact rather
/// than being collapsed at the dispatch point.
pub fn call<S: CellSource + ?Sized>(name: &str, args: &[Expr], src: &S) -> EvalResult {
    match name {
        // ARRAYTOTEXT(array) -> a single text cell listing every element.
        // It CONSUMES an array (proving arrays reach a function through the
        // seam) and PRODUCES a scalar (proving the seam carries the function's
        // chosen shape, not an imposed one). The concise form joins with
        // ", "; the verbose form (second arg = 1/TRUE) brace-wraps, matching
        // Excel closely enough for a round trip.
        "ARRAYTOTEXT" => array_to_text(args, src),
        // Unreachable: `is_array_fn` gates this dispatch, so any name arriving
        // here is one `ARRAY_FN_NAMES` lists. A missing arm is a bug, not a
        // user #NAME? — that is caught by the seam test.
        _ => EvalResult::Scalar(Value::Error(ferrix_core::ErrorKind::Value)),
    }
}

/// Materialise a range/reference argument as an [`ArrayData`], reading exactly
/// `rows * cols` cells through the existing columnar `spec_get` path.
///
/// Bounded by the RESULT extent: `A1:A5` reads five cells whether the column
/// holds five rows or 200 million. A non-range argument is evaluated in array
/// context and returned as-is (a scalar stays a 1x1 collapse target; a nested
/// array flows straight through).
pub fn materialize<S: CellSource + ?Sized>(arg: &Expr, src: &S) -> EvalResult {
    if let Some(spec) = range_spec(arg, src) {
        let rows = spec.rows.max(1);
        let cols = spec.cols.max(1);
        let mut cells = Vec::with_capacity(rows as usize * cols as usize);
        for r in 0..rows {
            for c in 0..cols {
                cells.push(spec_get(&spec, src, r, c));
            }
        }
        return EvalResult::Array(ArrayData::from_cells(rows, cols, cells));
    }
    // Not a range: evaluate in array context so a nested array-native call
    // keeps its shape.
    eval_view_array(arg, src)
}

fn array_to_text<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    if args.is_empty() || args.len() > 2 {
        return EvalResult::Scalar(Value::Error(ferrix_core::ErrorKind::Value));
    }
    // Second arg controls format: 0/FALSE (default) concise, 1/TRUE verbose.
    let verbose = match args.get(1) {
        None => false,
        Some(a) => match eval_view_array(a, src).into_scalar().as_bool() {
            Some(b) => b,
            None => return EvalResult::Scalar(Value::Error(ferrix_core::ErrorKind::Value)),
        },
    };

    let data = match materialize(&args[0], src) {
        EvalResult::Array(a) => a,
        EvalResult::Scalar(v) => ArrayData::scalar(v),
    };

    let mut out = String::new();
    for r in 0..data.rows() {
        if r > 0 {
            out.push(';');
        }
        for c in 0..data.cols() {
            if c > 0 {
                out.push_str(", ");
            }
            out.push_str(&render_cell(data.get(r, c), src));
        }
    }
    if verbose {
        out = format!("{{{out}}}");
    }
    // A computed string becomes an arena `StrId` through the same process-wide
    // interner the text family uses, so an array-produced string round-trips
    // exactly like `=UPPER(..)`'s. A full interner degrades to `#VALUE!` —
    // bounded and visible, never an unbounded leak.
    match ferrix_core::arena::intern_formula_text(&out) {
        Some(id) => EvalResult::Scalar(Value::Text(id)),
        None => EvalResult::Scalar(Value::Error(ferrix_core::ErrorKind::Value)),
    }
}

fn render_cell<S: CellSource + ?Sized>(v: Value, src: &S) -> String {
    match v {
        Value::Empty => String::new(),
        Value::Number(n) => ferrix_core::format_number(n),
        Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Text(id) => src.resolve(id).to_string(),
        Value::Error(e) => e.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::{CellRef, Sheet};

    fn v(n: f64) -> Value {
        Value::Number(n)
    }

    #[test]
    fn from_cells_builds_row_major_and_reads_back() {
        // 2 rows x 3 cols: [1 2 3 / 4 5 6]
        let a = ArrayData::from_cells(2, 3, vec![v(1.0), v(2.0), v(3.0), v(4.0), v(5.0), v(6.0)]);
        assert_eq!((a.rows(), a.cols()), (2, 3));
        assert_eq!(a.get(0, 0), v(1.0));
        assert_eq!(a.get(0, 2), v(3.0));
        assert_eq!(a.get(1, 0), v(4.0));
        assert_eq!(a.get(1, 2), v(6.0));
    }

    #[test]
    fn out_of_range_reads_are_empty_never_a_panic() {
        let a = ArrayData::from_cells(1, 1, vec![v(9.0)]);
        assert_eq!(a.get(0, 0), v(9.0));
        assert_eq!(a.get(1, 0), Value::Empty);
        assert_eq!(a.get(0, 1), Value::Empty);
        assert_eq!(a.get(100, 100), Value::Empty);
    }

    #[test]
    #[should_panic(expected = "at least 1x1")]
    fn a_zero_dimension_array_is_rejected() {
        ArrayData::from_cells(0, 3, vec![]);
    }

    #[test]
    #[should_panic(expected = "rows * cols")]
    fn a_wrong_cell_count_is_rejected() {
        ArrayData::from_cells(2, 2, vec![v(1.0), v(2.0), v(3.0)]);
    }

    #[test]
    fn into_scalar_collapses_an_array_to_its_top_left() {
        let a = ArrayData::from_cells(2, 2, vec![v(7.0), v(8.0), v(9.0), v(10.0)]);
        assert_eq!(EvalResult::Array(a).into_scalar(), v(7.0));
        assert_eq!(EvalResult::Scalar(v(42.0)).into_scalar(), v(42.0));
    }

    #[test]
    fn scalar_stays_scalar_and_array_borrows_as_array() {
        assert!(EvalResult::Scalar(v(1.0)).as_array().is_none());
        let a = ArrayData::scalar(v(1.0));
        assert!(EvalResult::Array(a).as_array().is_some());
    }

    #[test]
    fn iter_yields_cells_in_reading_order() {
        let a = ArrayData::from_cells(2, 2, vec![v(1.0), v(2.0), v(3.0), v(4.0)]);
        let got: Vec<f64> = a.iter().filter_map(|c| c.as_number()).collect();
        assert_eq!(got, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn from_conversions_pick_the_right_shape() {
        assert!(matches!(
            EvalResult::from(v(3.0)),
            EvalResult::Scalar(Value::Number(_))
        ));
        assert!(matches!(
            EvalResult::from(ArrayData::scalar(v(3.0))),
            EvalResult::Array(_)
        ));
    }

    #[test]
    fn is_array_fn_is_exact_match_only() {
        assert!(is_array_fn("ARRAYTOTEXT"));
        // No prefix/contains over-claim that would swallow a near-named fn.
        assert!(!is_array_fn("ARRAYTOTEXTX"));
        assert!(!is_array_fn("ARRAY"));
        assert!(!is_array_fn("SUM"));
    }

    #[test]
    fn array_to_text_concise_and_verbose() {
        use crate::{eval, parse};
        let mut s = Sheet::new("t");
        s.set(CellRef::new(0, 0), v(1.0));
        s.set(CellRef::new(1, 0), v(2.0));
        s.set(CellRef::new(0, 1), v(3.0));
        s.set(CellRef::new(1, 1), v(4.0));

        // Concise (default): rows joined by ';', cells within a row by ', '.
        let concise = match eval(&parse("=ARRAYTOTEXT(A1:B2)").unwrap(), &s) {
            Value::Text(id) => s.resolve(id).to_string(),
            other => panic!("concise = {other:?}"),
        };
        assert_eq!(concise, "1, 3;2, 4");

        // Verbose (second arg TRUE): the whole thing is brace-wrapped.
        let verbose = match eval(&parse("=ARRAYTOTEXT(A1:B2,TRUE)").unwrap(), &s) {
            Value::Text(id) => s.resolve(id).to_string(),
            other => panic!("verbose = {other:?}"),
        };
        assert_eq!(verbose, "{1, 3;2, 4}");

        // A lone scalar argument is a 1x1 array: no separators.
        let scalar = match eval(&parse("=ARRAYTOTEXT(A1)").unwrap(), &s) {
            Value::Text(id) => s.resolve(id).to_string(),
            other => panic!("scalar = {other:?}"),
        };
        assert_eq!(scalar, "1");
    }
}
