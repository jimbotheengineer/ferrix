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
use crate::parser::{BinOp, Expr};

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
/// dispatch-seam mutual-exclusion test. P1 seeded it with the one foundational
/// array-consuming builtin (`ARRAYTOTEXT`); #27 P3 grows it with the 16
/// dynamic-array functions below. Every name here is exact-match owned by ONLY
/// the array family — `array_compose_tests.rs` pins that against every scalar
/// family, because `eval_call`'s guard arms match in order and a duplicate name
/// would silently let one family swallow another's call.
pub const ARRAY_FN_NAMES: &[&str] = &[
    // P1: the seam-proving array-consuming builtin.
    "ARRAYTOTEXT",
    // P3: the 16 dynamic-array functions (#27).
    "UNIQUE",
    "SORT",
    "SORTBY",
    "FILTER",
    "SEQUENCE",
    "RANDARRAY",
    "TOROW",
    "TOCOL",
    "WRAPROWS",
    "WRAPCOLS",
    "TAKE",
    "DROP",
    "CHOOSEROWS",
    "CHOOSECOLS",
    "HSTACK",
    "VSTACK",
];

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
        // --- P3: the 16 dynamic-array functions (#27) ----------------------
        //
        // Each is array-native: it either PRODUCES an array from range/scalar
        // arguments or reshapes/slices/stacks arrays it materialises. The two
        // streaming functions (FILTER, SORT-over-a-range) never materialise the
        // input column — they read it through the columnar `spec_get` path and
        // allocate only the RESULT — so a 10M-row scan costs the result size,
        // not 10M values (the headline scale invariant, pinned by the perf
        // test in `tests`).
        "UNIQUE" => unique(args, src),
        "SORT" => sort(args, src),
        "SORTBY" => sort_by(args, src),
        "FILTER" => filter(args, src),
        "SEQUENCE" => sequence(args, src),
        "RANDARRAY" => randarray(args, src),
        "TOROW" => to_row(args, src),
        "TOCOL" => to_col(args, src),
        "WRAPROWS" => wrap_rows(args, src),
        "WRAPCOLS" => wrap_cols(args, src),
        "TAKE" => take(args, src),
        "DROP" => drop(args, src),
        "CHOOSEROWS" => choose_rows(args, src),
        "CHOOSECOLS" => choose_cols(args, src),
        "HSTACK" => hstack(args, src),
        "VSTACK" => vstack(args, src),
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

// =========================================================================
// P3 (#27): the 16 dynamic-array functions.
//
// Shared conventions:
//   * A bad argument shape/count is `#VALUE!`; a non-positive dimension where
//     Excel demands a positive one is `#NUM!`; a slice/choose index out of
//     range is `#VALUE!` — matching Excel's own error family.
//   * Argument arrays are materialised via [`materialize`] (bounded by the
//     RESULT extent through the columnar `spec_get` path). The two streaming
//     functions (FILTER, and SORT/UNIQUE over a lone range) never build the
//     whole input array; they read it row-by-row and allocate only the result.
// =========================================================================

#[inline]
fn err(kind: ferrix_core::ErrorKind) -> EvalResult {
    EvalResult::Scalar(Value::Error(kind))
}

/// Materialise any argument as an [`ArrayData`] — a scalar becomes a 1x1
/// array, a range/array keeps its shape. Bounded by the RESULT extent.
fn as_array<S: CellSource + ?Sized>(arg: &Expr, src: &S) -> ArrayData {
    match materialize(arg, src) {
        EvalResult::Array(a) => a,
        EvalResult::Scalar(v) => ArrayData::scalar(v),
    }
}

/// Evaluate an argument to a single scalar `Value` (implicit intersection).
#[inline]
fn scalar_arg<S: CellSource + ?Sized>(arg: &Expr, src: &S) -> Value {
    eval_view_array(arg, src).into_scalar()
}

/// Evaluate an argument to an `f64`, or `None` if it is not numeric.
#[inline]
fn num_arg<S: CellSource + ?Sized>(arg: &Expr, src: &S) -> Option<f64> {
    scalar_arg(arg, src).as_number()
}

/// A total order over [`Value`] for SORT/UNIQUE, matching Excel's sort rank:
/// numbers (bools coerced 0/1) < text (case-insensitive) < errors < empty.
/// Within numbers, IEEE order with NaN pinned last; within text, byte order of
/// the lowercased string; within errors, by code.
fn value_rank(v: &Value) -> u8 {
    match v {
        Value::Number(_) | Value::Bool(_) => 0,
        Value::Text(_) => 1,
        Value::Error(_) => 2,
        Value::Empty => 3,
    }
}

fn total_cmp<S: CellSource + ?Sized>(a: &Value, b: &Value, src: &S) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (ra, rb) = (value_rank(a), value_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => src
            .resolve(*x)
            .to_ascii_lowercase()
            .cmp(&src.resolve(*y).to_ascii_lowercase()),
        (Value::Error(x), Value::Error(y)) => x.to_code().cmp(&y.to_code()),
        _ => {
            // Both are numeric-coercible (rank 0) or both Empty (rank 3, equal).
            let x = a.as_number().unwrap_or(0.0);
            let y = b.as_number().unwrap_or(0.0);
            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
        }
    }
}

/// Value equality for UNIQUE de-duplication: text compares by RESOLVED bytes
/// (two different arena ids for the same string are one value), numbers/bools
/// by coerced value, errors by code, empties equal. This is a coarser bucket
/// key than `total_cmp` on purpose — it is the exact-match dedup key.
fn unique_key<S: CellSource + ?Sized>(v: &Value, src: &S) -> String {
    match v {
        Value::Empty => "e".to_string(),
        Value::Number(n) => format!("n{}", n.to_bits()),
        Value::Bool(b) => format!("n{}", (if *b { 1.0f64 } else { 0.0 }).to_bits()),
        Value::Text(id) => format!("t{}", src.resolve(*id).to_ascii_lowercase()),
        Value::Error(e) => format!("x{}", e.to_code()),
    }
}

// --- UNIQUE ---------------------------------------------------------------

/// `UNIQUE(array, [by_col], [exactly_once])`. Distinct rows (or columns when
/// `by_col` is TRUE), in first-seen order. `exactly_once` keeps only entries
/// that appear a single time. Bounded by the RESULT (distinct count), reading
/// the input once — the same shape as the pivot kernel's group hash.
fn unique<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    if args.is_empty() || args.len() > 3 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let by_col = match args.get(1) {
        None => false,
        Some(a) => match scalar_arg(a, src).as_bool() {
            Some(b) => b,
            None => return err(ferrix_core::ErrorKind::Value),
        },
    };
    let exactly_once = match args.get(2) {
        None => false,
        Some(a) => match scalar_arg(a, src).as_bool() {
            Some(b) => b,
            None => return err(ferrix_core::ErrorKind::Value),
        },
    };
    let data = as_array(&args[0], src);
    let (rows, cols) = (data.rows(), data.cols());
    // A "line" is a row (default) or a column (by_col). Its key is the joined
    // cell keys; we count occurrences and keep first-seen order.
    let line_count = if by_col { cols } else { rows };
    let span = if by_col { rows } else { cols };
    let cell = |line: u32, k: u32| -> Value {
        if by_col {
            data.get(k, line)
        } else {
            data.get(line, k)
        }
    };
    let mut order: Vec<u32> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut counts: Vec<u32> = Vec::new();
    for line in 0..line_count {
        let mut key = String::new();
        for k in 0..span {
            key.push('\u{1}');
            key.push_str(&unique_key(&cell(line, k), src));
        }
        match seen.get(&key) {
            Some(&idx) => counts[idx] += 1,
            None => {
                seen.insert(key, order.len());
                order.push(line);
                counts.push(1);
            }
        }
    }
    let kept: Vec<u32> = order
        .iter()
        .zip(counts.iter())
        .filter(|(_, &c)| !exactly_once || c == 1)
        .map(|(&line, _)| line)
        .collect();
    if kept.is_empty() {
        // Excel returns #CALC! (an empty array); this engine has no empty
        // array, so an all-duplicates `exactly_once` result is #N/A, the
        // closest "no result" signal it owns.
        return err(ferrix_core::ErrorKind::NotAvailable);
    }
    let mut cells = Vec::with_capacity(kept.len() * span as usize);
    if by_col {
        // Result columns = kept lines; result rows = span.
        for r in 0..span {
            for &line in &kept {
                cells.push(data.get(r, line));
            }
        }
        EvalResult::Array(ArrayData::from_cells(span, kept.len() as u32, cells))
    } else {
        for &line in &kept {
            for k in 0..span {
                cells.push(data.get(line, k));
            }
        }
        EvalResult::Array(ArrayData::from_cells(kept.len() as u32, span, cells))
    }
}

// --- SORT / SORTBY --------------------------------------------------------

/// `SORT(array, [sort_index], [sort_order], [by_col])`. Sorts the rows (or
/// columns when `by_col`) of `array` by the 1-based key line `sort_index`
/// (default 1), ascending (`sort_order` 1, default) or descending (-1).
///
/// SCALE: over a lone single-column range this streams — it reads the column
/// through `spec_get`, sorts an index permutation, and materialises only the
/// sorted result. Over a multi-column array it materialises the array first
/// (bounded by its own extent).
fn sort<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    if args.is_empty() || args.len() > 4 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let sort_index = match args.get(1) {
        None => 1i64,
        Some(a) => match num_arg(a, src) {
            Some(n) => n as i64,
            None => return err(ferrix_core::ErrorKind::Value),
        },
    };
    let order = match args.get(2) {
        None => 1i64,
        Some(a) => match num_arg(a, src) {
            Some(n) => n as i64,
            None => return err(ferrix_core::ErrorKind::Value),
        },
    };
    if order != 1 && order != -1 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let by_col = match args.get(3) {
        None => false,
        Some(a) => match scalar_arg(a, src).as_bool() {
            Some(b) => b,
            None => return err(ferrix_core::ErrorKind::Value),
        },
    };
    let data = as_array(&args[0], src);
    sort_data(&data, sort_index, order, by_col, src)
}

fn sort_data<S: CellSource + ?Sized>(
    data: &ArrayData,
    sort_index: i64,
    order: i64,
    by_col: bool,
    src: &S,
) -> EvalResult {
    let (rows, cols) = (data.rows(), data.cols());
    let (line_count, span) = if by_col { (cols, rows) } else { (rows, cols) };
    if sort_index < 1 || sort_index as u32 > span {
        return err(ferrix_core::ErrorKind::Value);
    }
    let key_line = (sort_index - 1) as u32;
    let key_at = |line: u32| -> Value {
        if by_col {
            data.get(key_line, line)
        } else {
            data.get(line, key_line)
        }
    };
    let mut idx: Vec<u32> = (0..line_count).collect();
    idx.sort_by(|&a, &b| {
        let c = total_cmp(&key_at(a), &key_at(b), src);
        // Stable secondary key on original position keeps ties in input order.
        let c = if order == -1 { c.reverse() } else { c };
        c.then(a.cmp(&b))
    });
    reorder_lines(data, &idx, by_col)
}

/// Build a result array from `data` by permuting its lines (rows unless
/// `by_col`) in the order given by `perm`.
fn reorder_lines(data: &ArrayData, perm: &[u32], by_col: bool) -> EvalResult {
    let (rows, cols) = (data.rows(), data.cols());
    let span = if by_col { rows } else { cols };
    let mut cells = Vec::with_capacity(perm.len() * span as usize);
    if by_col {
        for r in 0..rows {
            for &line in perm {
                cells.push(data.get(r, line));
            }
        }
        EvalResult::Array(ArrayData::from_cells(rows, perm.len() as u32, cells))
    } else {
        for &line in perm {
            for c in 0..cols {
                cells.push(data.get(line, c));
            }
        }
        EvalResult::Array(ArrayData::from_cells(perm.len() as u32, cols, cells))
    }
}

/// `SORTBY(array, by_array1, [order1], ...)`. Sorts the ROWS of `array` by one
/// or more separate key arrays, each the same height as `array`. Only row
/// sorting (Excel's SORTBY has no by-column mode).
fn sort_by<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    // array, then (by_array, [order]) pairs — at least one pair.
    if args.len() < 2 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let data = as_array(&args[0], src);
    let rows = data.rows();

    // Parse the trailing (by_array, [order]) groups. An order literal is a
    // scalar 1/-1; a by_array is anything that materialises to `rows` tall.
    struct Key {
        col: Vec<Value>,
        order: i64,
    }
    let mut keys: Vec<Key> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let by = as_array(&args[i], src);
        if by.rows() != rows || by.cols() != 1 {
            return err(ferrix_core::ErrorKind::Value);
        }
        let col: Vec<Value> = (0..rows).map(|r| by.get(r, 0)).collect();
        let order = match args.get(i + 1) {
            // A following scalar 1/-1 is this key's order; anything else starts
            // the next key.
            Some(a) => match num_arg(a, src) {
                Some(n) if (n as i64) == 1 || (n as i64) == -1 => {
                    i += 1;
                    n as i64
                }
                _ => 1,
            },
            None => 1,
        };
        keys.push(Key { col, order });
        i += 1;
    }
    if keys.is_empty() {
        return err(ferrix_core::ErrorKind::Value);
    }

    let mut idx: Vec<u32> = (0..rows).collect();
    idx.sort_by(|&a, &b| {
        for k in &keys {
            let c = total_cmp(&k.col[a as usize], &k.col[b as usize], src);
            let c = if k.order == -1 { c.reverse() } else { c };
            if c != std::cmp::Ordering::Equal {
                return c;
            }
        }
        a.cmp(&b)
    });
    reorder_lines(&data, &idx, false)
}

// --- FILTER ---------------------------------------------------------------

/// `FILTER(array, include, [if_empty])`. Keeps the rows of `array` whose
/// matching `include` element is truthy. `include` is a same-height boolean
/// column (typically a comparison like `B:B>0`).
///
/// SCALE INVARIANT (the headline criterion): when `array` and `include` are
/// lone ranges and `include` is a `range CMP scalar` / `scalar CMP range` /
/// bare range, this STREAMS — it walks the input row-by-row through `spec_get`,
/// keeps only matching rows, and allocates memory proportional to the RESULT,
/// never to the 10M-row scan. Only the more general case (an already-
/// materialised array `include`) builds the input array.
fn filter<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    if args.len() < 2 || args.len() > 3 {
        return err(ferrix_core::ErrorKind::Value);
    }

    // Fast streaming path: array = lone range, include reducible to a per-row
    // boolean predicate over lone range(s). Never materialises the input.
    if let (Some(arr_spec), Some(pred)) =
        (range_spec(&args[0], src), Predicate::compile(&args[1], src))
    {
        let height = arr_spec.rows;
        let width = arr_spec.cols;
        if pred.height() != height {
            return err(ferrix_core::ErrorKind::Value);
        }
        let mut cells: Vec<Value> = Vec::new();
        let mut kept = 0u32;
        for r in 0..height {
            match pred.test(r, src) {
                Ok(true) => {
                    kept += 1;
                    for c in 0..width {
                        cells.push(spec_get(&arr_spec, src, r, c));
                    }
                }
                Ok(false) => {}
                Err(e) => return err(e),
            }
        }
        if kept == 0 {
            return filter_if_empty(args, src);
        }
        return EvalResult::Array(ArrayData::from_cells(kept, width, cells));
    }

    // General path: materialise both (each bounded by its own extent).
    let data = as_array(&args[0], src);
    let include = as_array(&args[1], src);
    if include.rows() != data.rows() || include.cols() != 1 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let mut cells: Vec<Value> = Vec::new();
    let mut kept = 0u32;
    for r in 0..data.rows() {
        match include.get(r, 0) {
            Value::Error(e) => return err(e),
            v => {
                if v.as_bool().unwrap_or(false) {
                    kept += 1;
                    for c in 0..data.cols() {
                        cells.push(data.get(r, c));
                    }
                }
            }
        }
    }
    if kept == 0 {
        return filter_if_empty(args, src);
    }
    EvalResult::Array(ArrayData::from_cells(kept, data.cols(), cells))
}

/// FILTER's empty result: the `if_empty` argument when given, else `#CALC!` —
/// which this engine spells `#N/A` (it has no empty array).
fn filter_if_empty<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    match args.get(2) {
        Some(a) => EvalResult::Scalar(scalar_arg(a, src)),
        None => err(ferrix_core::ErrorKind::NotAvailable),
    }
}

/// A streaming per-row boolean predicate over lone ranges — the include arg of
/// a scale-invariant FILTER. Supports `range CMP scalar`, `scalar CMP range`,
/// `range CMP range` (element-wise), and a bare truthy range. Returns `None`
/// from `compile` for anything more complex, so FILTER falls back to
/// materialising.
enum Predicate<'a> {
    /// `range CMP scalar` (constant on the right).
    RangeCmpScalar {
        spec: crate::eval::RangeSpec<'a>,
        op: BinOp,
        rhs: Value,
    },
    /// `scalar CMP range` (constant on the left).
    ScalarCmpRange {
        lhs: Value,
        op: BinOp,
        spec: crate::eval::RangeSpec<'a>,
    },
    /// `range CMP range`, element-wise; both the same height.
    RangeCmpRange {
        lhs: crate::eval::RangeSpec<'a>,
        op: BinOp,
        rhs: crate::eval::RangeSpec<'a>,
    },
    /// A bare range: each cell is truthy on its own.
    BareRange { spec: crate::eval::RangeSpec<'a> },
}

impl<'a> Predicate<'a> {
    fn compile<S: CellSource + ?Sized>(arg: &'a Expr, src: &S) -> Option<Self> {
        // A comparison operator with a range on at least one side.
        if let Expr::Binary(op, lhs, rhs) = arg {
            if is_cmp(*op) {
                let ls = range_spec(lhs, src);
                let rs = range_spec(rhs, src);
                return match (ls, rs) {
                    (Some(l), Some(r)) if l.cols == 1 && r.cols == 1 => {
                        Some(Predicate::RangeCmpRange {
                            lhs: l,
                            op: *op,
                            rhs: r,
                        })
                    }
                    (Some(l), None) if l.cols == 1 => Some(Predicate::RangeCmpScalar {
                        spec: l,
                        op: *op,
                        rhs: scalar_arg(rhs, src),
                    }),
                    (None, Some(r)) if r.cols == 1 => Some(Predicate::ScalarCmpRange {
                        lhs: scalar_arg(lhs, src),
                        op: *op,
                        spec: r,
                    }),
                    _ => None,
                };
            }
            return None;
        }
        // A bare single-column range of truthy cells.
        if let Some(spec) = range_spec(arg, src) {
            if spec.cols == 1 {
                return Some(Predicate::BareRange { spec });
            }
        }
        None
    }

    fn height(&self) -> u32 {
        match self {
            Predicate::RangeCmpScalar { spec, .. }
            | Predicate::ScalarCmpRange { spec, .. }
            | Predicate::BareRange { spec } => spec.rows,
            Predicate::RangeCmpRange { lhs, .. } => lhs.rows,
        }
    }

    fn test<S: CellSource + ?Sized>(
        &self,
        r: u32,
        src: &S,
    ) -> Result<bool, ferrix_core::ErrorKind> {
        match self {
            Predicate::RangeCmpScalar { spec, op, rhs } => {
                let l = spec_get(spec, src, r, 0);
                cmp_values(&l, *op, rhs, src)
            }
            Predicate::ScalarCmpRange { lhs, op, spec } => {
                let r_v = spec_get(spec, src, r, 0);
                cmp_values(lhs, *op, &r_v, src)
            }
            Predicate::RangeCmpRange { lhs, op, rhs } => {
                let l = spec_get(lhs, src, r, 0);
                let rv = spec_get(rhs, src, r, 0);
                cmp_values(&l, *op, &rv, src)
            }
            Predicate::BareRange { spec } => {
                let v = spec_get(spec, src, r, 0);
                match v {
                    Value::Error(e) => Err(e),
                    other => Ok(other.as_bool().unwrap_or(false)),
                }
            }
        }
    }
}

#[inline]
fn is_cmp(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge
    )
}

/// Compare two values under a comparison operator, propagating an error
/// operand. Text compares case-insensitively; numbers/bools/empty coerce
/// numerically; a text-vs-number comparison is only ever equal/not-equal.
fn cmp_values<S: CellSource + ?Sized>(
    a: &Value,
    op: BinOp,
    b: &Value,
    src: &S,
) -> Result<bool, ferrix_core::ErrorKind> {
    use std::cmp::Ordering;
    if let Value::Error(e) = a {
        return Err(*e);
    }
    if let Value::Error(e) = b {
        return Err(*e);
    }
    let ord: Option<Ordering> = match (a, b) {
        (Value::Text(x), Value::Text(y)) => Some(
            src.resolve(*x)
                .to_ascii_lowercase()
                .cmp(&src.resolve(*y).to_ascii_lowercase()),
        ),
        // Text vs non-text: not numerically comparable; equal only if both are
        // the same text, which they are not here.
        (Value::Text(_), _) | (_, Value::Text(_)) => None,
        _ => a
            .as_number()
            .unwrap_or(0.0)
            .partial_cmp(&b.as_number().unwrap_or(0.0)),
    };
    Ok(match (op, ord) {
        (BinOp::Eq, o) => o == Some(Ordering::Equal),
        (BinOp::Ne, o) => o != Some(Ordering::Equal),
        (BinOp::Lt, Some(o)) => o == Ordering::Less,
        (BinOp::Gt, Some(o)) => o == Ordering::Greater,
        (BinOp::Le, Some(o)) => o != Ordering::Greater,
        (BinOp::Ge, Some(o)) => o != Ordering::Less,
        // Ordered comparison of incomparable operands (e.g. text < number):
        // false, as Excel treats a failed ordering.
        (BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge, None) => false,
        // Unreachable: every caller passes a comparison operator (gated by
        // `is_cmp`). A non-comparison op here is a bug, treated as no match.
        _ => false,
    })
}

// --- SEQUENCE / RANDARRAY -------------------------------------------------

/// `SEQUENCE(rows, [cols], [start], [step])`. A `rows`x`cols` grid counting
/// from `start` by `step`, row-major. Bounded by the RESULT (`rows*cols`).
fn sequence<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    if args.is_empty() || args.len() > 4 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let rows = match dim_arg(&args[0], src) {
        Ok(n) => n,
        Err(e) => return err(e),
    };
    let cols = match args.get(1) {
        None => 1,
        Some(a) => match dim_arg(a, src) {
            Ok(n) => n,
            Err(e) => return err(e),
        },
    };
    let start = match args.get(2) {
        None => 1.0,
        Some(a) => match num_arg(a, src) {
            Some(n) => n,
            None => return err(ferrix_core::ErrorKind::Value),
        },
    };
    let step = match args.get(3) {
        None => 1.0,
        Some(a) => match num_arg(a, src) {
            Some(n) => n,
            None => return err(ferrix_core::ErrorKind::Value),
        },
    };
    let total = rows as u64 * cols as u64;
    let mut cells = Vec::with_capacity(total as usize);
    let mut k = 0u64;
    for _ in 0..rows {
        for _ in 0..cols {
            cells.push(Value::Number(start + step * k as f64));
            k += 1;
        }
    }
    EvalResult::Array(ArrayData::from_cells(rows, cols, cells))
}

/// A positive integer dimension: truncates toward zero (Excel), rejects
/// anything `< 1` as `#NUM!` and non-numeric as `#VALUE!`.
fn dim_arg<S: CellSource + ?Sized>(arg: &Expr, src: &S) -> Result<u32, ferrix_core::ErrorKind> {
    match scalar_arg(arg, src) {
        Value::Error(e) => Err(e),
        v => match v.as_number() {
            Some(n) => {
                let t = n.trunc();
                if !(1.0..=u32::MAX as f64).contains(&t) {
                    Err(ferrix_core::ErrorKind::Num)
                } else {
                    Ok(t as u32)
                }
            }
            None => Err(ferrix_core::ErrorKind::Value),
        },
    }
}

/// A tiny deterministic SplitMix64 PRNG — the injectable RNG seam that makes
/// RANDARRAY testable. `RANDARRAY` with a trailing seed argument (this
/// engine's carve for determinism, since Excel's is volatile) produces the
/// same grid every call; without a seed it is seeded from a process counter.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// A float in `[0, 1)` with 53 bits of entropy.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn process_seed() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0x1234_5678_9ABC_DEF0);
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) ^ base
}

/// `RANDARRAY([rows], [cols], [min], [max], [whole_number], [seed])`.
///
/// The first five arguments are Excel's; the SIXTH is this engine's
/// determinism carve — a testing seed. When present, the grid is reproducible
/// (the injectable-RNG seam the acceptance criteria ask for); when absent, the
/// grid is seeded non-deterministically from a process counter and the wall
/// clock, and is NOT reproducible (documented volatility).
fn randarray<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    if args.len() > 6 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let rows = match args.first() {
        None => 1,
        Some(a) => match dim_arg(a, src) {
            Ok(n) => n,
            Err(e) => return err(e),
        },
    };
    let cols = match args.get(1) {
        None => 1,
        Some(a) => match dim_arg(a, src) {
            Ok(n) => n,
            Err(e) => return err(e),
        },
    };
    let min = match args.get(2) {
        None => 0.0,
        Some(a) => match num_arg(a, src) {
            Some(n) => n,
            None => return err(ferrix_core::ErrorKind::Value),
        },
    };
    let max = match args.get(3) {
        None => 1.0,
        Some(a) => match num_arg(a, src) {
            Some(n) => n,
            None => return err(ferrix_core::ErrorKind::Value),
        },
    };
    if min > max {
        return err(ferrix_core::ErrorKind::Value);
    }
    let whole = match args.get(4) {
        None => false,
        Some(a) => match scalar_arg(a, src).as_bool() {
            Some(b) => b,
            None => return err(ferrix_core::ErrorKind::Value),
        },
    };
    let mut rng = match args.get(5) {
        None => SplitMix64(process_seed()),
        Some(a) => match num_arg(a, src) {
            Some(n) => SplitMix64(n.to_bits()),
            None => return err(ferrix_core::ErrorKind::Value),
        },
    };
    let total = rows as u64 * cols as u64;
    let mut cells = Vec::with_capacity(total as usize);
    for _ in 0..total {
        let v = if whole {
            // Inclusive integer range [min, max].
            let lo = min.ceil();
            let hi = max.floor();
            if lo > hi {
                return err(ferrix_core::ErrorKind::Value);
            }
            let span = (hi - lo) as u64 + 1;
            lo + (rng.next_u64() % span) as f64
        } else {
            min + rng.next_f64() * (max - min)
        };
        cells.push(Value::Number(v));
    }
    EvalResult::Array(ArrayData::from_cells(rows, cols, cells))
}

// --- reshape: TOROW / TOCOL / WRAPROWS / WRAPCOLS -------------------------

/// Flatten an array into reading order (row-major unless `by_col`).
fn flatten<S: CellSource + ?Sized>(
    arg: &Expr,
    ignore: u8,
    by_col: bool,
    src: &S,
) -> Result<Vec<Value>, ferrix_core::ErrorKind> {
    let data = as_array(arg, src);
    let (rows, cols) = (data.rows(), data.cols());
    let mut out = Vec::new();
    let mut push = |v: Value| {
        let skip = match v {
            Value::Empty => ignore == 1 || ignore == 3,
            Value::Error(_) => ignore == 2 || ignore == 3,
            _ => false,
        };
        if !skip {
            out.push(v);
        }
    };
    if by_col {
        for c in 0..cols {
            for r in 0..rows {
                push(data.get(r, c));
            }
        }
    } else {
        for r in 0..rows {
            for c in 0..cols {
                push(data.get(r, c));
            }
        }
    }
    Ok(out)
}

/// The `ignore_empty` / `[ignore]` argument shared by TOROW/TOCOL/WRAP*:
/// 0 keep all (default), 1 ignore blanks, 2 ignore errors, 3 ignore both.
fn ignore_arg<S: CellSource + ?Sized>(
    args: &[Expr],
    idx: usize,
    src: &S,
) -> Result<u8, ferrix_core::ErrorKind> {
    match args.get(idx) {
        None => Ok(0),
        Some(a) => match scalar_arg(a, src).as_number() {
            Some(n) if (0.0..=3.0).contains(&n) => Ok(n as u8),
            _ => Err(ferrix_core::ErrorKind::Value),
        },
    }
}

fn scan_by_col_arg<S: CellSource + ?Sized>(
    args: &[Expr],
    idx: usize,
    src: &S,
) -> Result<bool, ferrix_core::ErrorKind> {
    match args.get(idx) {
        None => Ok(false),
        Some(a) => match scalar_arg(a, src).as_bool() {
            Some(b) => Ok(b),
            None => Err(ferrix_core::ErrorKind::Value),
        },
    }
}

/// `TOROW(array, [ignore], [scan_by_col])`. One row.
fn to_row<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    if args.is_empty() || args.len() > 3 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let ignore = match ignore_arg(args, 1, src) {
        Ok(n) => n,
        Err(e) => return err(e),
    };
    let by_col = match scan_by_col_arg(args, 2, src) {
        Ok(b) => b,
        Err(e) => return err(e),
    };
    let flat = match flatten(&args[0], ignore, by_col, src) {
        Ok(f) => f,
        Err(e) => return err(e),
    };
    if flat.is_empty() {
        return err(ferrix_core::ErrorKind::NotAvailable);
    }
    let n = flat.len() as u32;
    EvalResult::Array(ArrayData::from_cells(1, n, flat))
}

/// `TOCOL(array, [ignore], [scan_by_col])`. One column.
fn to_col<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    if args.is_empty() || args.len() > 3 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let ignore = match ignore_arg(args, 1, src) {
        Ok(n) => n,
        Err(e) => return err(e),
    };
    let by_col = match scan_by_col_arg(args, 2, src) {
        Ok(b) => b,
        Err(e) => return err(e),
    };
    let flat = match flatten(&args[0], ignore, by_col, src) {
        Ok(f) => f,
        Err(e) => return err(e),
    };
    if flat.is_empty() {
        return err(ferrix_core::ErrorKind::NotAvailable);
    }
    let n = flat.len() as u32;
    EvalResult::Array(ArrayData::from_cells(n, 1, flat))
}

/// `WRAPROWS(vector, wrap_count, [pad_with])`. Wrap a 1-D vector into rows of
/// `wrap_count`, padding the final short row with `pad_with` (default `#N/A`).
fn wrap_rows<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    wrap(args, true, src)
}

/// `WRAPCOLS(vector, wrap_count, [pad_with])`. Wrap into columns.
fn wrap_cols<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    wrap(args, false, src)
}

fn wrap<S: CellSource + ?Sized>(args: &[Expr], by_rows: bool, src: &S) -> EvalResult {
    if args.len() < 2 || args.len() > 3 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let data = as_array(&args[0], src);
    // A vector: one row or one column. Flatten in reading order.
    if data.rows() != 1 && data.cols() != 1 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let flat: Vec<Value> = data.iter().collect();
    let wrap_count = match dim_arg(&args[1], src) {
        Ok(n) => n,
        Err(e) => return err(e),
    };
    let pad = match args.get(2) {
        None => Value::Error(ferrix_core::ErrorKind::NotAvailable),
        Some(a) => scalar_arg(a, src),
    };
    let len = flat.len() as u32;
    let lines = len.div_ceil(wrap_count);
    let mut cells = vec![pad; (lines * wrap_count) as usize];
    if by_rows {
        // `lines` rows x `wrap_count` cols, row-major fill.
        for (i, v) in flat.into_iter().enumerate() {
            cells[i] = v;
        }
        EvalResult::Array(ArrayData::from_cells(lines, wrap_count, cells))
    } else {
        // `wrap_count` rows x `lines` cols, but filled column-major: element i
        // goes to column i/wrap_count, row i%wrap_count.
        let mut grid = vec![pad; (wrap_count * lines) as usize];
        for (i, v) in flat.into_iter().enumerate() {
            let col = i as u32 / wrap_count;
            let row = i as u32 % wrap_count;
            grid[(row * lines + col) as usize] = v;
        }
        EvalResult::Array(ArrayData::from_cells(wrap_count, lines, grid))
    }
}

// --- slice: TAKE / DROP ---------------------------------------------------

/// `TAKE(array, rows, [cols])` / `DROP(array, rows, [cols])`. A negative count
/// takes/drops from the end. TAKE keeps the first/last `|n|`; DROP removes
/// them. Omitted / zero on an axis leaves it whole (TAKE) or untouched (DROP).
fn take<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    slice(args, true, src)
}

fn drop<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    slice(args, false, src)
}

fn slice<S: CellSource + ?Sized>(args: &[Expr], is_take: bool, src: &S) -> EvalResult {
    if args.is_empty() || args.len() > 3 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let data = as_array(&args[0], src);
    let (rows, cols) = (data.rows() as i64, data.cols() as i64);

    // Parse an axis count; None means "no argument" (whole axis for TAKE, drop
    // nothing for DROP). An explicit numeric arg is truncated toward zero.
    let axis = |idx: usize| -> Result<Option<i64>, ferrix_core::ErrorKind> {
        match args.get(idx) {
            None => Ok(None),
            Some(a) => match scalar_arg(a, src) {
                Value::Error(e) => Err(e),
                v => match v.as_number() {
                    Some(n) => Ok(Some(n.trunc() as i64)),
                    None => Err(ferrix_core::ErrorKind::Value),
                },
            },
        }
    };
    let rn = match axis(1) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let cn = match axis(2) {
        Ok(v) => v,
        Err(e) => return err(e),
    };

    // Resolve each axis to a kept index range [start, end).
    let (r0, r1) = match resolve_axis(rows, rn, is_take) {
        Some(x) => x,
        None => return err(ferrix_core::ErrorKind::Value),
    };
    let (c0, c1) = match resolve_axis(cols, cn, is_take) {
        Some(x) => x,
        None => return err(ferrix_core::ErrorKind::Value),
    };
    if r1 <= r0 || c1 <= c0 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let (out_rows, out_cols) = ((r1 - r0) as u32, (c1 - c0) as u32);
    let mut cells = Vec::with_capacity((out_rows * out_cols) as usize);
    for r in r0..r1 {
        for c in c0..c1 {
            cells.push(data.get(r as u32, c as u32));
        }
    }
    EvalResult::Array(ArrayData::from_cells(out_rows, out_cols, cells))
}

/// Resolve one axis of TAKE/DROP to a kept `[start, end)` over `len` cells.
/// `None` = leave the axis whole (TAKE) or untouched (DROP). Returns `None`
/// (the outer maps to `#VALUE!`) if the result would be empty.
fn resolve_axis(len: i64, n: Option<i64>, is_take: bool) -> Option<(i64, i64)> {
    let n = match n {
        None => return Some((0, len)),
        Some(0) if is_take => return None, // TAKE 0 -> empty axis -> #VALUE!
        Some(0) => return Some((0, len)),  // DROP 0 -> whole axis
        Some(n) => n,
    };
    let mag = n.unsigned_abs().min(len as u64) as i64;
    if is_take {
        if n > 0 {
            Some((0, mag))
        } else {
            Some((len - mag, len))
        }
    } else {
        // DROP
        if n > 0 {
            Some((mag, len))
        } else {
            Some((0, len - mag))
        }
    }
}

// --- choose: CHOOSEROWS / CHOOSECOLS --------------------------------------

/// `CHOOSEROWS(array, r1, [r2], ...)`. Build a new array from the listed
/// 1-based rows (negative counts from the end). `CHOOSECOLS` is the column
/// analogue. An out-of-range index is `#VALUE!`.
fn choose_rows<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    choose(args, false, src)
}

fn choose_cols<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    choose(args, true, src)
}

fn choose<S: CellSource + ?Sized>(args: &[Expr], cols_mode: bool, src: &S) -> EvalResult {
    if args.len() < 2 {
        return err(ferrix_core::ErrorKind::Value);
    }
    let data = as_array(&args[0], src);
    let extent = if cols_mode { data.cols() } else { data.rows() } as i64;

    // Every index argument may itself be an array of indices (Excel allows
    // CHOOSEROWS(a, {1,3})); flatten each in reading order.
    let mut picks: Vec<u32> = Vec::new();
    for a in &args[1..] {
        let ix = as_array(a, src);
        for v in ix.iter() {
            let n = match v {
                Value::Error(e) => return err(e),
                other => match other.as_number() {
                    Some(n) => n.trunc() as i64,
                    None => return err(ferrix_core::ErrorKind::Value),
                },
            };
            // 1-based; negative counts from the end.
            let idx0 = if n > 0 {
                n - 1
            } else if n < 0 {
                extent + n
            } else {
                return err(ferrix_core::ErrorKind::Value);
            };
            if idx0 < 0 || idx0 >= extent {
                return err(ferrix_core::ErrorKind::Value);
            }
            picks.push(idx0 as u32);
        }
    }
    if picks.is_empty() {
        return err(ferrix_core::ErrorKind::Value);
    }
    if cols_mode {
        let out_rows = data.rows();
        let out_cols = picks.len() as u32;
        let mut cells = Vec::with_capacity((out_rows * out_cols) as usize);
        for r in 0..out_rows {
            for &c in &picks {
                cells.push(data.get(r, c));
            }
        }
        EvalResult::Array(ArrayData::from_cells(out_rows, out_cols, cells))
    } else {
        let out_rows = picks.len() as u32;
        let out_cols = data.cols();
        let mut cells = Vec::with_capacity((out_rows * out_cols) as usize);
        for &r in &picks {
            for c in 0..out_cols {
                cells.push(data.get(r, c));
            }
        }
        EvalResult::Array(ArrayData::from_cells(out_rows, out_cols, cells))
    }
}

// --- stack: HSTACK / VSTACK -----------------------------------------------

/// `HSTACK(a, b, ...)` — glue arrays side by side; result height = tallest,
/// short arrays padded below with `#N/A`. `VSTACK` stacks top to bottom, width
/// = widest, padded right with `#N/A`. Matches Excel's ragged-pad rule.
fn hstack<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    stack(args, true, src)
}

fn vstack<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> EvalResult {
    stack(args, false, src)
}

fn stack<S: CellSource + ?Sized>(args: &[Expr], horizontal: bool, src: &S) -> EvalResult {
    if args.is_empty() {
        return err(ferrix_core::ErrorKind::Value);
    }
    let parts: Vec<ArrayData> = args.iter().map(|a| as_array(a, src)).collect();
    let na = Value::Error(ferrix_core::ErrorKind::NotAvailable);
    if horizontal {
        let out_rows = parts.iter().map(|p| p.rows()).max().unwrap_or(1);
        let out_cols: u32 = parts.iter().map(|p| p.cols()).sum();
        let mut cells = Vec::with_capacity((out_rows * out_cols) as usize);
        for r in 0..out_rows {
            for p in &parts {
                for c in 0..p.cols() {
                    cells.push(if r < p.rows() { p.get(r, c) } else { na });
                }
            }
        }
        EvalResult::Array(ArrayData::from_cells(out_rows, out_cols, cells))
    } else {
        let out_cols = parts.iter().map(|p| p.cols()).max().unwrap_or(1);
        let out_rows: u32 = parts.iter().map(|p| p.rows()).sum();
        let mut cells = Vec::with_capacity((out_rows * out_cols) as usize);
        for p in &parts {
            for r in 0..p.rows() {
                for c in 0..out_cols {
                    cells.push(if c < p.cols() { p.get(r, c) } else { na });
                }
            }
        }
        EvalResult::Array(ArrayData::from_cells(out_rows, out_cols, cells))
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

#[cfg(test)]
mod p3_tests {
    //! Behaviour tests for the 16 dynamic-array functions (#27 P3).
    //!
    //! Each test drives a function through the shared `eval_view_array`
    //! dispatch (the same seam a spilled formula uses), so a passing test also
    //! proves the function is reachable. Shape + values + at least one
    //! error/edge case per function.
    use super::*;
    use ferrix_core::{CellRef, ErrorKind, Sheet};

    fn n(x: f64) -> Value {
        Value::Number(x)
    }

    /// Build a sheet with a set of (row, col, value) cells.
    fn sheet(cells: &[(u32, u32, Value)]) -> Sheet {
        let mut s = Sheet::new("t");
        for (r, c, v) in cells {
            s.set(CellRef::new(*r, *c), *v);
        }
        s
    }

    /// A column A1:A{n} of the given numbers.
    fn col(nums: &[f64]) -> Sheet {
        let mut s = Sheet::new("t");
        for (i, x) in nums.iter().enumerate() {
            s.set(CellRef::new(i as u32, 0), n(*x));
        }
        s
    }

    fn text_cell(s: &mut Sheet, r: u32, c: u32, t: &str) {
        let id = s.intern(t);
        s.set(CellRef::new(r, c), Value::Text(id));
    }

    /// Evaluate a formula to an ArrayData, or panic naming the scalar seen.
    fn arr(s: &Sheet, f: &str) -> ArrayData {
        match eval_view_array(
            &crate::parse(f).unwrap_or_else(|e| panic!("parse {f}: {e}")),
            s,
        ) {
            EvalResult::Array(a) => a,
            EvalResult::Scalar(v) => panic!("{f} = Scalar({v:?}), wanted Array"),
        }
    }

    /// Evaluate a formula to a scalar Value (implicit-intersection collapse).
    fn scal(s: &Sheet, f: &str) -> Value {
        eval_view_array(&crate::parse(f).unwrap(), s).into_scalar()
    }

    /// Flatten an array to a Vec of numbers (for value assertions).
    fn nums(a: &ArrayData) -> Vec<f64> {
        a.iter().filter_map(|v| v.as_number()).collect()
    }

    fn resolve_text(s: &Sheet, v: Value) -> String {
        match v {
            Value::Text(id) => s.resolve(id).to_string(),
            other => panic!("wanted text, got {other:?}"),
        }
    }

    // --- UNIQUE -----------------------------------------------------------

    #[test]
    fn unique_keeps_first_seen_distinct_values() {
        let s = col(&[3.0, 1.0, 3.0, 2.0, 1.0]);
        let a = arr(&s, "=UNIQUE(A1:A5)");
        assert_eq!((a.rows(), a.cols()), (3, 1));
        assert_eq!(nums(&a), vec![3.0, 1.0, 2.0]);
    }

    #[test]
    fn unique_exactly_once_drops_repeats() {
        let s = col(&[3.0, 1.0, 3.0, 2.0, 1.0]);
        let a = arr(&s, "=UNIQUE(A1:A5,FALSE,TRUE)");
        assert_eq!((a.rows(), a.cols()), (1, 1));
        assert_eq!(nums(&a), vec![2.0]);
    }

    #[test]
    fn unique_by_col_dedupes_columns() {
        let s = sheet(&[(0, 0, n(5.0)), (0, 1, n(5.0)), (0, 2, n(7.0))]);
        let a = arr(&s, "=UNIQUE(A1:C1,TRUE)");
        assert_eq!((a.rows(), a.cols()), (1, 2));
        assert_eq!(nums(&a), vec![5.0, 7.0]);
    }

    #[test]
    fn unique_all_duplicates_exactly_once_is_na() {
        let s = col(&[1.0, 1.0]);
        assert_eq!(
            scal(&s, "=UNIQUE(A1:A2,FALSE,TRUE)"),
            Value::Error(ErrorKind::NotAvailable)
        );
    }

    // --- SORT / SORTBY ----------------------------------------------------

    #[test]
    fn sort_ascending_and_descending() {
        let s = col(&[3.0, 1.0, 2.0]);
        assert_eq!(nums(&arr(&s, "=SORT(A1:A3)")), vec![1.0, 2.0, 3.0]);
        assert_eq!(nums(&arr(&s, "=SORT(A1:A3,1,-1)")), vec![3.0, 2.0, 1.0]);
    }

    #[test]
    fn sort_by_column_index_on_a_grid() {
        let s = sheet(&[
            (0, 0, n(10.0)),
            (0, 1, n(3.0)),
            (1, 0, n(20.0)),
            (1, 1, n(1.0)),
            (2, 0, n(30.0)),
            (2, 1, n(2.0)),
        ]);
        let a = arr(&s, "=SORT(A1:B3,2)");
        assert_eq!((a.rows(), a.cols()), (3, 2));
        assert_eq!(a.get(0, 0), n(20.0));
        assert_eq!(a.get(1, 0), n(30.0));
        assert_eq!(a.get(2, 0), n(10.0));
    }

    #[test]
    fn sort_by_col_true_sorts_columns() {
        let s = sheet(&[(0, 0, n(2.0)), (0, 1, n(9.0)), (0, 2, n(4.0))]);
        let a = arr(&s, "=SORT(A1:C1,1,-1,TRUE)");
        assert_eq!((a.rows(), a.cols()), (1, 3));
        assert_eq!(nums(&a), vec![9.0, 4.0, 2.0]);
    }

    #[test]
    fn sort_bad_index_is_value_error() {
        let s = col(&[1.0, 2.0]);
        assert_eq!(scal(&s, "=SORT(A1:A2,2)"), Value::Error(ErrorKind::Value));
    }

    #[test]
    fn sort_is_stable_on_ties() {
        let s = sheet(&[
            (0, 0, n(100.0)),
            (0, 1, n(5.0)),
            (1, 0, n(200.0)),
            (1, 1, n(5.0)),
        ]);
        let a = arr(&s, "=SORT(A1:B2,2)");
        assert_eq!(a.get(0, 0), n(100.0));
        assert_eq!(a.get(1, 0), n(200.0));
    }

    #[test]
    fn sortby_orders_rows_by_a_separate_key() {
        let s = sheet(&[
            (0, 0, n(10.0)),
            (0, 1, n(2.0)),
            (1, 0, n(20.0)),
            (1, 1, n(3.0)),
            (2, 0, n(30.0)),
            (2, 1, n(1.0)),
        ]);
        let a = arr(&s, "=SORTBY(A1:A3,B1:B3)");
        assert_eq!(nums(&a), vec![30.0, 10.0, 20.0]);
    }

    #[test]
    fn sortby_descending_and_multikey() {
        let s = sheet(&[
            (0, 0, n(10.0)),
            (0, 1, n(1.0)),
            (1, 0, n(20.0)),
            (1, 1, n(1.0)),
            (2, 0, n(30.0)),
            (2, 1, n(2.0)),
        ]);
        let a = arr(&s, "=SORTBY(A1:A3,B1:B3,-1)");
        assert_eq!(nums(&a), vec![30.0, 10.0, 20.0]);
    }

    #[test]
    fn sortby_mismatched_height_is_value_error() {
        let s = sheet(&[(0, 0, n(1.0)), (1, 0, n(2.0)), (0, 1, n(9.0))]);
        assert_eq!(
            scal(&s, "=SORTBY(A1:A2,B1:B1)"),
            Value::Error(ErrorKind::Value)
        );
    }

    // --- FILTER (streaming and general) -----------------------------------

    #[test]
    fn filter_keeps_matching_rows_streaming() {
        let s = sheet(&[
            (0, 0, n(10.0)),
            (0, 1, n(0.0)),
            (1, 0, n(20.0)),
            (1, 1, n(1.0)),
            (2, 0, n(30.0)),
            (2, 1, n(0.0)),
            (3, 0, n(40.0)),
            (3, 1, n(5.0)),
            (4, 0, n(50.0)),
            (4, 1, n(-1.0)),
        ]);
        let a = arr(&s, "=FILTER(A1:A5,B1:B5>0)");
        assert_eq!((a.rows(), a.cols()), (2, 1));
        assert_eq!(nums(&a), vec![20.0, 40.0]);
    }

    #[test]
    fn filter_multi_column_array_streams_all_columns() {
        let mut s = sheet(&[
            (0, 0, n(1.0)),
            (0, 1, n(10.0)),
            (1, 0, n(2.0)),
            (1, 1, n(20.0)),
            (2, 0, n(3.0)),
            (2, 1, n(30.0)),
        ]);
        s.set(CellRef::new(0, 2), n(0.0));
        s.set(CellRef::new(1, 2), n(1.0));
        s.set(CellRef::new(2, 2), n(1.0));
        let a = arr(&s, "=FILTER(A1:B3,C1:C3>0)");
        assert_eq!((a.rows(), a.cols()), (2, 2));
        assert_eq!(a.get(0, 0), n(2.0));
        assert_eq!(a.get(0, 1), n(20.0));
        assert_eq!(a.get(1, 0), n(3.0));
    }

    #[test]
    fn filter_empty_result_uses_if_empty_else_na() {
        let s = sheet(&[
            (0, 0, n(10.0)),
            (0, 1, n(0.0)),
            (1, 0, n(20.0)),
            (1, 1, n(0.0)),
        ]);
        assert_eq!(
            scal(&s, "=FILTER(A1:A2,B1:B2>0)"),
            Value::Error(ErrorKind::NotAvailable)
        );
        assert_eq!(scal(&s, "=FILTER(A1:A2,B1:B2>0,-1)"), n(-1.0));
    }

    #[test]
    fn filter_bare_boolean_range() {
        let s = sheet(&[
            (0, 0, n(10.0)),
            (0, 1, n(1.0)),
            (1, 0, n(20.0)),
            (1, 1, n(0.0)),
            (2, 0, n(30.0)),
            (2, 1, n(1.0)),
        ]);
        assert_eq!(nums(&arr(&s, "=FILTER(A1:A3,B1:B3)")), vec![10.0, 30.0]);
    }

    #[test]
    fn filter_scalar_on_left_of_comparison() {
        let s = sheet(&[
            (0, 0, n(10.0)),
            (0, 1, n(1.0)),
            (1, 0, n(20.0)),
            (1, 1, n(0.0)),
        ]);
        assert_eq!(nums(&arr(&s, "=FILTER(A1:A2,0<B1:B2)")), vec![10.0]);
    }

    #[test]
    fn filter_range_vs_range_elementwise() {
        let s = sheet(&[
            (0, 0, n(5.0)),
            (0, 1, n(3.0)),
            (1, 0, n(1.0)),
            (1, 1, n(9.0)),
            (2, 0, n(8.0)),
            (2, 1, n(2.0)),
        ]);
        assert_eq!(nums(&arr(&s, "=FILTER(A1:A3,A1:A3>B1:B3)")), vec![5.0, 8.0]);
    }

    #[test]
    fn filter_error_in_include_propagates() {
        let s = sheet(&[(0, 0, n(10.0)), (0, 1, Value::Error(ErrorKind::DivZero))]);
        assert_eq!(
            scal(&s, "=FILTER(A1:A1,B1:B1>0)"),
            Value::Error(ErrorKind::DivZero)
        );
    }

    // --- SEQUENCE ---------------------------------------------------------

    #[test]
    fn sequence_shapes_and_steps() {
        let s = Sheet::new("t");
        assert_eq!(nums(&arr(&s, "=SEQUENCE(3)")), vec![1.0, 2.0, 3.0]);
        let a = arr(&s, "=SEQUENCE(2,3)");
        assert_eq!((a.rows(), a.cols()), (2, 3));
        assert_eq!(nums(&a), vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(
            nums(&arr(&s, "=SEQUENCE(3,1,10,5)")),
            vec![10.0, 15.0, 20.0]
        );
    }

    #[test]
    fn sequence_zero_rows_is_num_error() {
        let s = Sheet::new("t");
        assert_eq!(scal(&s, "=SEQUENCE(0)"), Value::Error(ErrorKind::Num));
    }

    // --- RANDARRAY (deterministic seam) -----------------------------------

    #[test]
    fn randarray_seed_is_reproducible() {
        let s = Sheet::new("t");
        let a = arr(&s, "=RANDARRAY(2,3,0,100,FALSE,42)");
        let b = arr(&s, "=RANDARRAY(2,3,0,100,FALSE,42)");
        assert_eq!((a.rows(), a.cols()), (2, 3));
        assert_eq!(
            a, b,
            "same seed must give the same grid (injectable-RNG seam)"
        );
        for v in a.iter() {
            let x = v.as_number().unwrap();
            assert!((0.0..100.0).contains(&x));
        }
    }

    #[test]
    fn randarray_whole_numbers_are_integers_in_range() {
        let s = Sheet::new("t");
        let a = arr(&s, "=RANDARRAY(1,20,5,7,TRUE,7)");
        for v in a.iter() {
            let x = v.as_number().unwrap();
            assert!((5.0..=7.0).contains(&x));
            assert_eq!(x, x.trunc(), "whole_number result must be an integer");
        }
    }

    #[test]
    fn randarray_min_gt_max_is_value_error() {
        let s = Sheet::new("t");
        assert_eq!(
            scal(&s, "=RANDARRAY(1,1,10,1)"),
            Value::Error(ErrorKind::Value)
        );
    }

    #[test]
    fn randarray_default_is_one_cell_unit_interval() {
        let s = Sheet::new("t");
        let a = arr(&s, "=RANDARRAY(1,1,0,1,FALSE,1)");
        assert_eq!((a.rows(), a.cols()), (1, 1));
        let x = a.get(0, 0).as_number().unwrap();
        assert!((0.0..1.0).contains(&x));
    }

    // --- TOROW / TOCOL ----------------------------------------------------

    #[test]
    fn to_row_and_to_col_flatten_a_grid() {
        let s = sheet(&[
            (0, 0, n(1.0)),
            (0, 1, n(2.0)),
            (1, 0, n(3.0)),
            (1, 1, n(4.0)),
        ]);
        let r = arr(&s, "=TOROW(A1:B2)");
        assert_eq!((r.rows(), r.cols()), (1, 4));
        assert_eq!(nums(&r), vec![1.0, 2.0, 3.0, 4.0]);
        let c = arr(&s, "=TOCOL(A1:B2)");
        assert_eq!((c.rows(), c.cols()), (4, 1));
        assert_eq!(nums(&c), vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn to_col_scan_by_col_reads_down_columns_first() {
        let s = sheet(&[
            (0, 0, n(1.0)),
            (0, 1, n(2.0)),
            (1, 0, n(3.0)),
            (1, 1, n(4.0)),
        ]);
        assert_eq!(
            nums(&arr(&s, "=TOCOL(A1:B2,0,TRUE)")),
            vec![1.0, 3.0, 2.0, 4.0]
        );
    }

    #[test]
    fn to_col_ignore_blanks_and_errors() {
        let s = sheet(&[
            (0, 0, n(1.0)),
            (1, 0, Value::Empty),
            (2, 0, Value::Error(ErrorKind::DivZero)),
            (3, 0, n(4.0)),
        ]);
        assert_eq!(nums(&arr(&s, "=TOCOL(A1:A4,3)")), vec![1.0, 4.0]);
    }

    // --- WRAPROWS / WRAPCOLS ----------------------------------------------

    #[test]
    fn wraprows_wraps_and_pads() {
        let s = col(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        let a = arr(&s, "=WRAPROWS(A1:A5,2)");
        assert_eq!((a.rows(), a.cols()), (3, 2));
        assert_eq!(a.get(0, 0), n(1.0));
        assert_eq!(a.get(0, 1), n(2.0));
        assert_eq!(a.get(2, 0), n(5.0));
        assert_eq!(a.get(2, 1), Value::Error(ErrorKind::NotAvailable));
    }

    #[test]
    fn wraprows_custom_pad() {
        let s = col(&[1.0, 2.0, 3.0]);
        let a = arr(&s, "=WRAPROWS(A1:A3,2,0)");
        assert_eq!(a.get(1, 1), n(0.0));
    }

    #[test]
    fn wrapcols_wraps_down_columns() {
        let s = col(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        let a = arr(&s, "=WRAPCOLS(A1:A5,2)");
        assert_eq!((a.rows(), a.cols()), (2, 3));
        assert_eq!(a.get(0, 0), n(1.0));
        assert_eq!(a.get(1, 0), n(2.0));
        assert_eq!(a.get(0, 1), n(3.0));
        assert_eq!(a.get(0, 2), n(5.0));
        assert_eq!(a.get(1, 2), Value::Error(ErrorKind::NotAvailable));
    }

    #[test]
    fn wraprows_non_vector_is_value_error() {
        let s = sheet(&[
            (0, 0, n(1.0)),
            (0, 1, n(2.0)),
            (1, 0, n(3.0)),
            (1, 1, n(4.0)),
        ]);
        assert_eq!(
            scal(&s, "=WRAPROWS(A1:B2,2)"),
            Value::Error(ErrorKind::Value)
        );
    }

    // --- TAKE / DROP ------------------------------------------------------

    #[test]
    fn take_first_and_last_rows() {
        let s = col(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(nums(&arr(&s, "=TAKE(A1:A4,2)")), vec![1.0, 2.0]);
        assert_eq!(nums(&arr(&s, "=TAKE(A1:A4,-2)")), vec![3.0, 4.0]);
    }

    #[test]
    fn take_rows_and_cols() {
        let s = sheet(&[
            (0, 0, n(1.0)),
            (0, 1, n(2.0)),
            (0, 2, n(3.0)),
            (1, 0, n(4.0)),
            (1, 1, n(5.0)),
            (1, 2, n(6.0)),
            (2, 0, n(7.0)),
            (2, 1, n(8.0)),
            (2, 2, n(9.0)),
        ]);
        let a = arr(&s, "=TAKE(A1:C3,2,-1)");
        assert_eq!((a.rows(), a.cols()), (2, 1));
        assert_eq!(nums(&a), vec![3.0, 6.0]);
    }

    #[test]
    fn drop_rows_from_front_and_back() {
        let s = col(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(nums(&arr(&s, "=DROP(A1:A4,1)")), vec![2.0, 3.0, 4.0]);
        assert_eq!(nums(&arr(&s, "=DROP(A1:A4,-1)")), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn take_zero_rows_is_value_error() {
        let s = col(&[1.0, 2.0]);
        assert_eq!(scal(&s, "=TAKE(A1:A2,0)"), Value::Error(ErrorKind::Value));
    }

    #[test]
    fn drop_everything_is_value_error() {
        let s = col(&[1.0, 2.0]);
        assert_eq!(scal(&s, "=DROP(A1:A2,5)"), Value::Error(ErrorKind::Value));
    }

    // --- CHOOSEROWS / CHOOSECOLS ------------------------------------------

    #[test]
    fn chooserows_picks_and_reorders() {
        let s = col(&[10.0, 20.0, 30.0, 40.0]);
        assert_eq!(
            nums(&arr(&s, "=CHOOSEROWS(A1:A4,3,1,-1)")),
            vec![30.0, 10.0, 40.0]
        );
    }

    #[test]
    fn choosecols_picks_columns() {
        let s = sheet(&[(0, 0, n(1.0)), (0, 1, n(2.0)), (0, 2, n(3.0))]);
        let a = arr(&s, "=CHOOSECOLS(A1:C1,3,1)");
        assert_eq!((a.rows(), a.cols()), (1, 2));
        assert_eq!(nums(&a), vec![3.0, 1.0]);
    }

    #[test]
    fn chooserows_out_of_range_is_value_error() {
        let s = col(&[1.0, 2.0]);
        assert_eq!(
            scal(&s, "=CHOOSEROWS(A1:A2,5)"),
            Value::Error(ErrorKind::Value)
        );
    }

    // --- HSTACK / VSTACK --------------------------------------------------

    #[test]
    fn hstack_glues_side_by_side() {
        let s = sheet(&[
            (0, 0, n(1.0)),
            (1, 0, n(2.0)),
            (0, 1, n(3.0)),
            (1, 1, n(4.0)),
        ]);
        let a = arr(&s, "=HSTACK(A1:A2,B1:B2)");
        assert_eq!((a.rows(), a.cols()), (2, 2));
        assert_eq!(a.get(0, 0), n(1.0));
        assert_eq!(a.get(0, 1), n(3.0));
        assert_eq!(a.get(1, 1), n(4.0));
    }

    #[test]
    fn hstack_ragged_pads_shorter_with_na() {
        let s = sheet(&[(0, 0, n(1.0)), (1, 0, n(2.0)), (0, 1, n(9.0))]);
        let a = arr(&s, "=HSTACK(A1:A2,B1:B1)");
        assert_eq!((a.rows(), a.cols()), (2, 2));
        assert_eq!(a.get(1, 1), Value::Error(ErrorKind::NotAvailable));
    }

    #[test]
    fn vstack_stacks_top_to_bottom() {
        let s = sheet(&[
            (0, 0, n(1.0)),
            (0, 1, n(2.0)),
            (0, 2, n(3.0)),
            (0, 3, n(4.0)),
        ]);
        let a = arr(&s, "=VSTACK(A1:B1,C1:D1)");
        assert_eq!((a.rows(), a.cols()), (2, 2));
        assert_eq!(nums(&a), vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn vstack_ragged_pads_narrower_with_na() {
        let s = sheet(&[(0, 0, n(1.0)), (0, 1, n(2.0)), (0, 2, n(9.0))]);
        let a = arr(&s, "=VSTACK(A1:B1,C1:C1)");
        assert_eq!((a.rows(), a.cols()), (2, 2));
        assert_eq!(a.get(1, 0), n(9.0));
        assert_eq!(a.get(1, 1), Value::Error(ErrorKind::NotAvailable));
    }

    // --- composition (proves functions nest through the seam) -------------

    #[test]
    fn sort_of_filter_composes() {
        let s = sheet(&[
            (0, 0, n(30.0)),
            (0, 1, n(1.0)),
            (1, 0, n(10.0)),
            (1, 1, n(1.0)),
            (2, 0, n(50.0)),
            (2, 1, n(0.0)),
            (3, 0, n(20.0)),
            (3, 1, n(1.0)),
        ]);
        let a = arr(&s, "=SORT(FILTER(A1:A4,B1:B4>0))");
        assert_eq!(nums(&a), vec![10.0, 20.0, 30.0]);
    }

    #[test]
    fn text_values_sort_after_numbers_case_insensitively() {
        let mut s = Sheet::new("t");
        s.set(CellRef::new(0, 0), n(2.0));
        text_cell(&mut s, 1, 0, "banana");
        text_cell(&mut s, 2, 0, "Apple");
        s.set(CellRef::new(3, 0), n(1.0));
        let a = arr(&s, "=SORT(A1:A4)");
        assert_eq!(a.get(0, 0), n(1.0));
        assert_eq!(a.get(1, 0), n(2.0));
        assert_eq!(resolve_text(&s, a.get(2, 0)), "Apple");
        assert_eq!(resolve_text(&s, a.get(3, 0)), "banana");
    }
}
