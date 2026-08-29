//! Excel lookup functions: `VLOOKUP`, `HLOOKUP`, `INDEX`, `MATCH`, `XLOOKUP`,
//! `CHOOSE`, `INDIRECT`.
//!
//! Everything lives here rather than in `eval.rs` so the evaluator keeps a
//! single delegating arm (see [`is_lookup_fn`]) and this file can grow without
//! colliding with the rest of the function library — the same shape
//! [`crate::text`], [`crate::datetime`] and [`crate::stats`] already use.
//!
//! # The scale invariant, and how it is held
//!
//! Ferrix targets 200M+ rows, so a lookup over a 10M-row column must not
//! materialise the column. Nothing in this module ever builds a `Vec` of cells.
//! Every search runs against [`crate::eval::RangeSpec`] +
//! [`crate::eval::spec_get`], which read one cell at a time, and each search
//! stops as early as its semantics allow:
//!
//! | mode | cells visited |
//! |---|---|
//! | exact (`VLOOKUP`/`HLOOKUP` `FALSE`, `MATCH` 0, `XLOOKUP` search_mode ±1) | up to the first hit |
//! | approximate (`VLOOKUP`/`HLOOKUP` `TRUE`, `MATCH` ±1, `XLOOKUP` search_mode ±2) | `O(log n)` — binary search |
//! | `XLOOKUP` match_mode ±1 with a *linear* search_mode | all of them, by definition |
//!
//! The last row is the honest exception: "nearest smaller/larger" over data
//! that is not asserted to be sorted cannot be answered without looking at
//! every candidate. It is still **O(1) memory** — one running best offset, no
//! buffer — so peak memory stays bounded by the viewport even though the visit
//! count is not. Callers who want the sublinear path ask for `search_mode` 2
//! or -2, which is exactly the trade Excel exposes.
//!
//! `crates/ferrix-formula/src/lookup/tests.rs` pins this with a `CellSource`
//! that reports 10,000,000 rows and *counts every `get`*, plus
//! `tests/lookup_alloc.rs` which counts allocations through a global allocator
//! across a 10x change in row count. Both fail loudly against an
//! implementation that collects the column first; asserting only on the
//! returned value would not.
//!
//! # Approximate lookup over unsorted data
//!
//! `VLOOKUP(..., TRUE)` and `MATCH(..., 1)` **assume** their key column is
//! sorted. Excel does not check, and neither do we: a check would cost the
//! full O(n) scan the binary search exists to avoid, on every call. On
//! unsorted data the binary search lands wherever its probes lead it and that
//! value is returned — an answer, never an error. That is Excel's documented
//! behaviour and the behaviour the tests pin.
//!
//! # `INDIRECT` and the dependency graph
//!
//! [`crate::depgraph::collect_precedents`] walks the *parsed expression* and
//! emits an edge for every `Expr::Ref` / `Expr::Range` it finds. `INDIRECT`
//! has neither: its target is a **runtime string**, so the parse tree of
//! `INDIRECT("A"&B1)` contains an edge to `B1` and no edge whatsoever to the
//! cell it will actually read.
//!
//! That is a deliberate, documented gap rather than a bug to paper over:
//!
//! * **The edge cannot be resolved statically.** `"A"&B1` names a different
//!   cell every time `B1` changes. Any edge computed at parse time is a
//!   guess.
//! * **The edge cannot be cached after the first evaluate either.** Caching
//!   `INDIRECT` -> `A7` after one evaluation would be *stale the instant*
//!   `B1` becomes 8, and the staleness is silent: the formula keeps
//!   recalculating (it still depends on `B1`) but against the wrong
//!   precedent set, so a change to `A8` would never wake it. A wrong cached
//!   edge is worse than no edge, because it looks like coverage.
//! * **So the target is re-resolved on every evaluate.** [`indirect`] parses
//!   `ref_text` and reads the cell each time it runs. The cost is one A1 parse
//!   per evaluation — bounded, cheap, and never wrong.
//!
//! The consequence a caller must know: a cell whose only path to its data is
//! through `INDIRECT` is **not** in the recalculation graph for that data, and
//! `INDIRECT` cycles are invisible to
//! [`DepGraph::is_circular_at`](crate::depgraph::DepGraph::is_circular_at),
//! which walks static edges only. That is why the runtime budget below exists:
//! it is the *only* thing standing between an `INDIRECT` cycle and a blown
//! stack, because the graph cannot see the cycle to reject it.

use std::cell::Cell;
use std::cmp::Ordering;

use ferrix_core::arena::intern_formula_text;
use ferrix_core::{CellRef, ErrorKind, Value};

use crate::criteria::{cmp_ignore_case, eq_ignore_case, Pattern};
use crate::eval::{eval_view, range_spec, spec_get, CellSource, RangeSpec};
use crate::parser::Expr;

#[cfg(test)]
mod tests;

/// How deep `INDIRECT` may nest before it gives up with `#REF!`.
///
/// The static dependency graph cannot see an `INDIRECT` cycle (see the module
/// docs), so this counter is the whole defence. 16 is far past anything a
/// spreadsheet does on purpose and far below the recursion depth that would
/// threaten the stack, so exhausting it is a diagnosis, not a limitation.
pub const MAX_INDIRECT_DEPTH: u32 = 16;

thread_local! {
    /// Nesting depth of `INDIRECT` evaluation on THIS thread.
    ///
    /// Thread-local rather than global because evaluation is per-thread and a
    /// shared counter would let one thread's deep formula bankrupt another's
    /// shallow one. `const`-initialised so entering the guard cannot itself
    /// allocate.
    static INDIRECT_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Is `name` handled by this module?
///
/// Used by the one delegating arm in `eval::eval_call`. Guard arms in a Rust
/// `match` are tried **in order**, so this predicate must claim nothing owned
/// by [`crate::text`], [`crate::datetime`] or [`crate::stats`] — an over-broad
/// predicate here would silently swallow their calls, and their own tests
/// could never see it because they do not route through the merged match.
/// `crate::compose_tests` pins the mutual exclusion.
pub fn is_lookup_fn(name: &str) -> bool {
    matches!(
        name,
        "VLOOKUP" | "HLOOKUP" | "INDEX" | "MATCH" | "XLOOKUP" | "CHOOSE" | "INDIRECT"
    )
}

/// Evaluate a lookup function. Assumes [`is_lookup_fn`] said yes.
pub fn call<S: CellSource + ?Sized>(name: &str, args: &[Expr], src: &S) -> Value {
    match dispatch(name, args, src) {
        Ok(v) => v,
        Err(e) => Value::Error(e),
    }
}

fn dispatch<S: CellSource + ?Sized>(
    name: &str,
    args: &[Expr],
    src: &S,
) -> Result<Value, ErrorKind> {
    match name {
        "VLOOKUP" | "HLOOKUP" => vhlookup(name == "VLOOKUP", args, src),
        "MATCH" => match_fn(args, src),
        "INDEX" => index_fn(args, src),
        "XLOOKUP" => xlookup(args, src),
        "CHOOSE" => choose(args, src),
        "INDIRECT" => indirect(args, src),
        _ => Err(ErrorKind::Name),
    }
}

// --- keys -----------------------------------------------------------------

/// A comparable cell, borrowed. Text points straight into the source's arena,
/// so building one of these allocates nothing — which is what lets a scan of
/// 10M cells allocate nothing at all.
#[derive(Clone, Copy, Debug)]
enum Key<'a> {
    Number(f64),
    Text(&'a str),
    Bool(bool),
    Blank,
}

impl Key<'_> {
    /// Excel's cross-type collation: numbers sort before text, text before
    /// booleans. Blanks sort last so an over-long range's trailing empties
    /// cannot displace a real answer in an ascending search.
    #[inline]
    fn rank(&self) -> u8 {
        match self {
            Key::Number(_) => 0,
            Key::Text(_) => 1,
            Key::Bool(_) => 2,
            Key::Blank => 3,
        }
    }
}

/// Total order over keys. Allocation-free: text goes through
/// [`cmp_ignore_case`], the single collation definition in [`crate::criteria`].
fn cmp_keys(a: &Key<'_>, b: &Key<'_>) -> Ordering {
    match (a, b) {
        (Key::Number(x), Key::Number(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Key::Text(x), Key::Text(y)) => cmp_ignore_case(x, y),
        (Key::Bool(x), Key::Bool(y)) => x.cmp(y),
        (Key::Blank, Key::Blank) => Ordering::Equal,
        _ => a.rank().cmp(&b.rank()),
    }
}

/// Equality as a lookup sees it, with optional wildcard matching.
///
/// `pat` is `Some` only when the probe is text AND the caller's mode allows
/// wildcards AND the probe actually contains one — so a plain text probe never
/// pays for the glob engine.
#[inline]
fn key_eq(probe: &Key<'_>, pat: Option<&Pattern>, cell: &Key<'_>) -> bool {
    if let (Key::Text(p), Key::Text(c)) = (probe, cell) {
        return match pat {
            Some(pt) => pt.matches(c),
            None => eq_ignore_case(p, c),
        };
    }
    cmp_keys(probe, cell) == Ordering::Equal
}

fn key_of_value<S: CellSource + ?Sized>(v: Value, src: &S) -> Result<Key<'_>, ErrorKind> {
    match v {
        Value::Empty => Ok(Key::Blank),
        Value::Number(n) => Ok(Key::Number(n)),
        Value::Bool(b) => Ok(Key::Bool(b)),
        Value::Text(id) => Ok(Key::Text(src.resolve(id))),
        // An error cell inside the searched lane IS the answer: Excel does not
        // step over #N/A looking for a better match.
        Value::Error(e) => Err(e),
    }
}

/// The probe value of a lookup. A bare string literal has no `Value` in this
/// engine, so it is borrowed straight out of the expression.
fn key_of_arg<'a, S: CellSource + ?Sized>(arg: &'a Expr, src: &'a S) -> Result<Key<'a>, ErrorKind> {
    if let Expr::Text(s) = arg {
        return Ok(Key::Text(s.as_str()));
    }
    key_of_value(eval_view(arg, src), src)
}

// --- lanes ----------------------------------------------------------------

/// A one-dimensional slice of a range: a single column or a single row.
///
/// Holds a [`RangeSpec`], not any cells. `lane_get` is O(1) and reads exactly
/// one cell, so a lane over a 10M-row column costs the same as one over three
/// rows until something actually walks it.
#[derive(Clone, Copy)]
struct Lane<'a> {
    spec: RangeSpec<'a>,
    vertical: bool,
}

impl Lane<'_> {
    #[inline]
    fn len(&self) -> u32 {
        if self.vertical {
            self.spec.rows
        } else {
            self.spec.cols
        }
    }
}

/// View a range as a lane. `None` when it is genuinely two-dimensional, which
/// `MATCH` reports as `#N/A` and `XLOOKUP` as `#VALUE!`, matching Excel.
fn lane_of<'a>(spec: RangeSpec<'a>) -> Option<Lane<'a>> {
    if spec.cols == 1 {
        Some(Lane {
            spec,
            vertical: true,
        })
    } else if spec.rows == 1 {
        Some(Lane {
            spec,
            vertical: false,
        })
    } else {
        None
    }
}

#[inline]
fn lane_get<S: CellSource + ?Sized>(lane: &Lane<'_>, src: &S, i: u32) -> Value {
    if lane.vertical {
        spec_get(&lane.spec, src, i, 0)
    } else {
        spec_get(&lane.spec, src, 0, i)
    }
}

/// Compile the probe's wildcard pattern, once per call, if the mode wants one.
///
/// Returns a pattern for ANY text probe when `enabled`, not only one that
/// contains a wildcard, because [`Pattern::compile`] is also what resolves `~`
/// escapes: `"cha~*"` must match the literal text `cha*`, and comparing the
/// raw probe would look for a tilde that is not there. The cost is one compile
/// **per call**, never per row.
fn wildcard_of(probe: &Key<'_>, enabled: bool) -> Option<Pattern> {
    match probe {
        Key::Text(t) if enabled => Some(Pattern::compile(t)),
        _ => None,
    }
}

// --- searches -------------------------------------------------------------

/// First (or last, when `reverse`) offset in `lane` equal to `probe`.
///
/// SCALE: stops at the hit. A match in row 5 of a 10M-row column visits six
/// cells, which is the property `lookup/tests.rs` measures.
fn linear_find<S: CellSource + ?Sized>(
    lane: &Lane<'_>,
    src: &S,
    probe: &Key<'_>,
    pat: Option<&Pattern>,
    reverse: bool,
) -> Result<Option<u32>, ErrorKind> {
    let len = lane.len();
    for step in 0..len {
        let i = if reverse { len - 1 - step } else { step };
        let cell = key_of_value(lane_get(lane, src, i), src)?;
        if key_eq(probe, pat, &cell) {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// Binary search for the LAST offset whose cell is still acceptable.
///
/// `descending == false` assumes ascending order and accepts `cell <= probe`
/// (Excel's `MATCH` type 1 / `VLOOKUP` TRUE). `descending == true` assumes
/// descending order and accepts `cell >= probe` (`MATCH` type -1).
///
/// SCALE: `O(log n)` probes, so approximate lookup over 10M rows touches ~24
/// cells. On UNSORTED input the probes lead wherever they lead and the result
/// is whatever this lands on — an answer, not an error, exactly as Excel.
fn binary_last<S: CellSource + ?Sized>(
    lane: &Lane<'_>,
    src: &S,
    probe: &Key<'_>,
    descending: bool,
) -> Result<Option<u32>, ErrorKind> {
    let (mut lo, mut hi) = (0u32, lane.len());
    let mut ans = None;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let cell = key_of_value(lane_get(lane, src, mid), src)?;
        let ord = cmp_keys(&cell, probe);
        let acceptable = if descending {
            ord != Ordering::Less
        } else {
            ord != Ordering::Greater
        };
        if acceptable {
            ans = Some(mid);
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(ans)
}

/// Binary search for the FIRST offset whose cell is `>= probe` in ascending
/// data — `XLOOKUP` match_mode 1 with search_mode 2.
fn binary_first_ge<S: CellSource + ?Sized>(
    lane: &Lane<'_>,
    src: &S,
    probe: &Key<'_>,
) -> Result<Option<u32>, ErrorKind> {
    let (mut lo, mut hi) = (0u32, lane.len());
    let mut ans = None;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let cell = key_of_value(lane_get(lane, src, mid), src)?;
        if cmp_keys(&cell, probe) != Ordering::Less {
            ans = Some(mid);
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(ans)
}

/// Full scan keeping the single best "nearest" candidate.
///
/// `larger == false` wants the greatest cell `<= probe`; `larger == true` the
/// smallest cell `>= probe`. This is the one search here that must visit every
/// cell — nearest-match over data with no sortedness promise is not answerable
/// otherwise. Memory is still O(1): one `u32` offset and one `Key`, never a
/// buffer. Ask for `search_mode` ±2 to get the `O(log n)` path instead.
fn linear_nearest<S: CellSource + ?Sized>(
    lane: &Lane<'_>,
    src: &S,
    probe: &Key<'_>,
    larger: bool,
    reverse: bool,
) -> Result<Option<u32>, ErrorKind> {
    let len = lane.len();
    let mut best: Option<u32> = None;
    let mut best_key: Option<Key<'_>> = None;
    for step in 0..len {
        let i = if reverse { len - 1 - step } else { step };
        let cell = key_of_value(lane_get(lane, src, i), src)?;
        // Blanks are not "the nearest smaller value"; skipping them stops an
        // over-long range's trailing empties from winning.
        if matches!(cell, Key::Blank) {
            continue;
        }
        let ord = cmp_keys(&cell, probe);
        if ord == Ordering::Equal {
            return Ok(Some(i));
        }
        let eligible = if larger {
            ord == Ordering::Greater
        } else {
            ord == Ordering::Less
        };
        if !eligible {
            continue;
        }
        let better = match &best_key {
            None => true,
            Some(bk) => {
                let c = cmp_keys(&cell, bk);
                if larger {
                    c == Ordering::Less
                } else {
                    c == Ordering::Greater
                }
            }
        };
        if better {
            best = Some(i);
            best_key = Some(cell);
        }
    }
    Ok(best)
}

// --- VLOOKUP / HLOOKUP ----------------------------------------------------

/// `VLOOKUP(lookup, table, col_index, [range_lookup])` and its transpose.
///
/// `range_lookup` defaults to TRUE (approximate), which is Excel's default and
/// the one people forget. Exact mode honours wildcards in a text probe; Excel
/// only applies them in exact mode, so approximate mode compiles no pattern.
fn vhlookup<S: CellSource + ?Sized>(
    vertical: bool,
    args: &[Expr],
    src: &S,
) -> Result<Value, ErrorKind> {
    if args.len() < 3 || args.len() > 4 {
        return Err(ErrorKind::Value);
    }
    let probe = key_of_arg(&args[0], src)?;
    let table = range_spec(&args[1], src).ok_or(ErrorKind::Value)?;
    let index = number_arg(args, 2, src)?;
    if !index.is_finite() {
        return Err(ErrorKind::Value);
    }
    let index = index.trunc();
    // Excel splits these two deliberately: an index below 1 is a broken
    // formula (#VALUE!), an index past the table's edge is a broken reference
    // (#REF!).
    if index < 1.0 {
        return Err(ErrorKind::Value);
    }
    let index = index as u32;
    let extent = if vertical { table.cols } else { table.rows };
    if index > extent {
        return Err(ErrorKind::Ref);
    }
    let approximate = match args.get(3) {
        Some(a) => bool_arg(a, src)?,
        None => true,
    };

    // The key lane is the table's first column (VLOOKUP) or first row
    // (HLOOKUP). Constructing it reads nothing, and `lane_get` pins the minor
    // axis at 0, so a wide table is still searched down one column only.
    let key_lane = Lane {
        spec: table,
        vertical,
    };
    let hit = if approximate {
        binary_last(&key_lane, src, &probe, false)?
    } else {
        let pat = wildcard_of(&probe, true);
        linear_find(&key_lane, src, &probe, pat.as_ref(), false)?
    };
    let hit = hit.ok_or(ErrorKind::NotAvailable)?;
    Ok(if vertical {
        spec_get(&table, src, hit, index - 1)
    } else {
        spec_get(&table, src, index - 1, hit)
    })
}

// --- MATCH ----------------------------------------------------------------

/// `MATCH(lookup, array, [match_type])`, 1-based result.
///
/// * `0` — exact, linear, wildcards honoured.
/// * `1` (default) — largest value `<= lookup`, assuming ascending order.
/// * `-1` — smallest value `>= lookup`, assuming descending order.
fn match_fn<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> Result<Value, ErrorKind> {
    if args.is_empty() || args.len() > 3 {
        return Err(ErrorKind::Value);
    }
    let probe = key_of_arg(&args[0], src)?;
    let spec = range_spec(&args[1], src).ok_or(ErrorKind::Value)?;
    // A two-dimensional array is #N/A, not #VALUE!: Excel treats it as "no
    // vector to search", not as a type error.
    let lane = lane_of(spec).ok_or(ErrorKind::NotAvailable)?;
    let mt = match args.get(2) {
        Some(_) => number_arg(args, 2, src)?,
        None => 1.0,
    };
    if !mt.is_finite() {
        return Err(ErrorKind::Value);
    }
    let hit = match mt.trunc() as i64 {
        0 => {
            let pat = wildcard_of(&probe, true);
            linear_find(&lane, src, &probe, pat.as_ref(), false)?
        }
        n if n > 0 => binary_last(&lane, src, &probe, false)?,
        _ => binary_last(&lane, src, &probe, true)?,
    };
    let hit = hit.ok_or(ErrorKind::NotAvailable)?;
    Ok(Value::Number(hit as f64 + 1.0))
}

// --- INDEX ----------------------------------------------------------------

/// `INDEX(array, row_num, [col_num])`.
///
/// `0` means "the whole row/column". This engine has no array values (dynamic
/// arrays and spilling are explicitly out of scope for issue #23), so a `0`
/// that still selects more than one cell is `#VALUE!` rather than a quietly
/// wrong scalar. The cases where `0` collapses to exactly one cell — the
/// overwhelmingly common `INDEX(A1:A9, 0, 1)` / `INDEX(A1:I1, 1, 0)` shapes —
/// are answered normally.
fn index_fn<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> Result<Value, ErrorKind> {
    if args.len() < 2 || args.len() > 3 {
        return Err(ErrorKind::Value);
    }
    let spec = range_spec(&args[0], src).ok_or(ErrorKind::Value)?;
    let first = index_arg(args, 1, src)?;

    // Two-argument form over a single-ROW range indexes across the row, which
    // is what `INDEX(A1:E1, 3)` has to mean.
    let (row, col) = if args.len() == 2 {
        if spec.rows == 1 && spec.cols > 1 {
            (1, first)
        } else {
            (first, 0)
        }
    } else {
        (first, index_arg(args, 2, src)?)
    };

    if row > spec.rows || col > spec.cols {
        return Err(ErrorKind::Ref);
    }
    // Resolve each 0 ("the whole axis") against the other axis's extent.
    let r = match row {
        0 if spec.rows == 1 => 0,
        0 => return Err(ErrorKind::Value),
        n => n - 1,
    };
    let c = match col {
        0 if spec.cols == 1 => 0,
        0 => return Err(ErrorKind::Value),
        n => n - 1,
    };
    Ok(spec_get(&spec, src, r, c))
}

/// A non-negative, finite INDEX subscript. Negative or non-numeric is
/// `#VALUE!`; past the end is the caller's `#REF!`.
fn index_arg<S: CellSource + ?Sized>(args: &[Expr], i: usize, src: &S) -> Result<u32, ErrorKind> {
    let n = number_arg(args, i, src)?;
    if !n.is_finite() || n < 0.0 || n >= u32::MAX as f64 {
        return Err(ErrorKind::Value);
    }
    Ok(n.trunc() as u32)
}

// --- XLOOKUP --------------------------------------------------------------

/// `XLOOKUP(lookup, lookup_array, return_array, [if_not_found], [match_mode],
/// [search_mode])`.
///
/// match_mode: `0` exact (default), `-1` exact-or-next-smaller, `1`
/// exact-or-next-larger, `2` wildcard.
/// search_mode: `1` first-to-last (default), `-1` last-to-first, `2` binary
/// ascending, `-2` binary descending.
///
/// `if_not_found` is evaluated **only** when the search misses, so an
/// expensive fallback costs nothing on the hot path. Absent, a miss is `#N/A`.
fn xlookup<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> Result<Value, ErrorKind> {
    if args.len() < 3 || args.len() > 6 {
        return Err(ErrorKind::Value);
    }
    let probe = key_of_arg(&args[0], src)?;
    let look =
        lane_of(range_spec(&args[1], src).ok_or(ErrorKind::Value)?).ok_or(ErrorKind::Value)?;
    let ret =
        lane_of(range_spec(&args[2], src).ok_or(ErrorKind::Value)?).ok_or(ErrorKind::Value)?;
    // Excel: mismatched array lengths are #VALUE!, because there is no
    // defensible answer — silently truncating would return a neighbour's row.
    if look.len() != ret.len() {
        return Err(ErrorKind::Value);
    }
    let match_mode = opt_int(args, 4, src, 0)?;
    let search_mode = opt_int(args, 5, src, 1)?;
    // Clippy prefers the range spelling for match_mode; search_mode stays an
    // OR pattern because 0 is deliberately NOT a valid search mode (Excel has
    // no search_mode 0) and a range would quietly admit it.
    if !matches!(match_mode, -1..=2) || !matches!(search_mode, -2 | -1 | 1 | 2) {
        return Err(ErrorKind::Value);
    }

    let binary = search_mode.abs() == 2;
    let reverse = search_mode == -1;
    let hit = match (match_mode, binary) {
        // Exact / wildcard.
        (0, false) | (2, false) => {
            let pat = wildcard_of(&probe, match_mode == 2);
            linear_find(&look, src, &probe, pat.as_ref(), reverse)?
        }
        (0, true) | (2, true) => {
            // Wildcards have no meaning in a binary search (a glob has no
            // position in a sort order), so mode 2 degrades to exact here —
            // which is all the ordering promise actually supports.
            let descending = search_mode == -2;
            match binary_last(&look, src, &probe, descending)? {
                // `binary_last` lands on the nearest acceptable neighbour, so
                // an EXACT search has to confirm the landing is a real hit
                // rather than returning the neighbour's row.
                Some(i) => {
                    let cell = key_of_value(lane_get(&look, src, i), src)?;
                    if cmp_keys(&cell, &probe) == Ordering::Equal {
                        Some(i)
                    } else {
                        None
                    }
                }
                None => None,
            }
        }
        // Nearest smaller.
        (-1, false) => linear_nearest(&look, src, &probe, false, reverse)?,
        (-1, true) => binary_last(&look, src, &probe, search_mode == -2)?,
        // Nearest larger.
        (1, false) => linear_nearest(&look, src, &probe, true, reverse)?,
        (1, true) => binary_first_ge(&look, src, &probe)?,
        _ => None,
    };

    match hit {
        Some(i) => Ok(lane_get(&ret, src, i)),
        None => match args.get(3) {
            Some(a) => value_arg(a, src),
            None => Err(ErrorKind::NotAvailable),
        },
    }
}

// --- CHOOSE ---------------------------------------------------------------

/// `CHOOSE(index_num, value1, [value2], ...)`.
///
/// Only the SELECTED argument is evaluated, the way `IF` behaves in this
/// engine. That keeps `CHOOSE(1, A1, SUM(B:B))` from paying for a 200M-row
/// sum it does not want — the scale invariant applied to argument evaluation.
fn choose<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> Result<Value, ErrorKind> {
    if args.len() < 2 {
        return Err(ErrorKind::Value);
    }
    let n = number_arg(args, 0, src)?;
    if !n.is_finite() {
        return Err(ErrorKind::Value);
    }
    let n = n.trunc();
    if n < 1.0 || n as usize >= args.len() {
        return Err(ErrorKind::Value);
    }
    value_arg(&args[n as usize], src)
}

// --- INDIRECT -------------------------------------------------------------

/// Decrements the depth counter however [`indirect`] returns.
struct DepthGuard;

impl Drop for DepthGuard {
    fn drop(&mut self) {
        INDIRECT_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Claim one level of `INDIRECT` nesting, or report `#REF!`.
///
/// The counter is NOT incremented on refusal, so a budget-exhausted formula
/// leaves the counter exactly where it found it.
fn enter_indirect() -> Result<DepthGuard, ErrorKind> {
    INDIRECT_DEPTH.with(|d| {
        if d.get() >= MAX_INDIRECT_DEPTH {
            Err(ErrorKind::Ref)
        } else {
            d.set(d.get() + 1);
            Ok(DepthGuard)
        }
    })
}

/// `INDIRECT(ref_text, [a1])`.
///
/// The target is resolved HERE, on every evaluation, and never cached — see
/// the module docs for why a cached edge would be silently stale.
///
/// Supported: `A1`, `$A$1`, `Sheet2!A1`, `'My Sheet'!A1`, and (with `a1` FALSE)
/// absolute `R5C3`. Not supported, each yielding a documented error rather
/// than a guess:
///
/// * A **range** (`"A1:A5"`) is `#VALUE!` — a range is not a scalar in this
///   engine, the same answer a literal `A1:A5` gives. Returning ranges is out
///   of scope for issue #23.
/// * Relative R1C1 (`R[1]C[1]`) is `#REF!`: it is defined relative to the
///   formula's own position, which evaluation here does not carry.
fn indirect<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> Result<Value, ErrorKind> {
    if args.is_empty() || args.len() > 2 {
        return Err(ErrorKind::Value);
    }
    let _guard = enter_indirect()?;

    let a1 = match args.get(1) {
        Some(a) => bool_arg(a, src)?,
        None => true,
    };

    // Borrow the reference text. A bare literal is borrowed from the
    // expression; a computed one from the source's arena. Neither allocates.
    let text: &str = match &args[0] {
        Expr::Text(s) => s.as_str(),
        other => match eval_view(other, src) {
            Value::Text(id) => src.resolve(id),
            Value::Error(e) => return Err(e),
            // A number or boolean can never name a cell.
            _ => return Err(ErrorKind::Ref),
        },
    };

    let (sheet, body) = split_sheet(text);
    if body.contains(':') {
        return Err(ErrorKind::Value);
    }
    let cell = if a1 {
        parse_a1(body).ok_or(ErrorKind::Ref)?
    } else {
        parse_r1c1(body).ok_or(ErrorKind::Ref)?
    };

    match sheet {
        None => Ok(src.get(cell)),
        Some(name) => {
            if !src.has_sheet(name) {
                return Err(ErrorKind::Ref);
            }
            Ok(src.get_in(name, cell))
        }
    }
}

/// Split `Sheet!A1` into its parts, unquoting `'My Sheet'` if present.
fn split_sheet(text: &str) -> (Option<&str>, &str) {
    match text.rfind('!') {
        None => (None, text.trim()),
        Some(i) => {
            let name = text[..i].trim();
            let name = name
                .strip_prefix('\'')
                .and_then(|n| n.strip_suffix('\''))
                .unwrap_or(name);
            (Some(name), text[i + 1..].trim())
        }
    }
}

/// `A1` with optional `$` anchors. The anchors carry no meaning for a runtime
/// reference — nothing is going to fill this formula down — so they are simply
/// accepted and dropped.
fn parse_a1(body: &str) -> Option<CellRef> {
    let mut clean = String::with_capacity(body.len());
    for c in body.chars() {
        if c != '$' {
            clean.push(c);
        }
    }
    CellRef::from_a1(&clean)
}

/// Absolute `R5C3` only. A bracketed relative form is deliberately rejected.
fn parse_r1c1(body: &str) -> Option<CellRef> {
    let b = body.trim();
    let rest = b.strip_prefix('R').or_else(|| b.strip_prefix('r'))?;
    let split = rest.find(['C', 'c'])?;
    let (rs, cs) = rest.split_at(split);
    let cs = &cs[1..];
    let row: u32 = rs.trim().parse().ok()?;
    let col: u32 = cs.trim().parse().ok()?;
    if row == 0 || col == 0 {
        return None;
    }
    Some(CellRef::new(row - 1, col - 1))
}

// --- argument helpers -----------------------------------------------------

/// A numeric argument, coercing numeric text the way Excel does.
fn number_arg<S: CellSource + ?Sized>(args: &[Expr], i: usize, src: &S) -> Result<f64, ErrorKind> {
    let arg = args.get(i).ok_or(ErrorKind::Value)?;
    if let Expr::Text(s) = arg {
        return s.trim().parse::<f64>().map_err(|_| ErrorKind::Value);
    }
    let v = eval_view(arg, src);
    if let Some(e) = v.error() {
        return Err(e);
    }
    match v {
        Value::Text(id) => src
            .resolve(id)
            .trim()
            .parse::<f64>()
            .map_err(|_| ErrorKind::Value),
        other => other.as_number().ok_or(ErrorKind::Value),
    }
}

/// An optional small integer argument (XLOOKUP's modes).
fn opt_int<S: CellSource + ?Sized>(
    args: &[Expr],
    i: usize,
    src: &S,
    default: i32,
) -> Result<i32, ErrorKind> {
    if i >= args.len() {
        return Ok(default);
    }
    let n = number_arg(args, i, src)?;
    if !n.is_finite() || n.abs() > 1e9 {
        return Err(ErrorKind::Value);
    }
    Ok(n.trunc() as i32)
}

/// A boolean argument. Numbers coerce (0 is FALSE), which is what makes
/// `VLOOKUP(x, t, 2, 0)` behave as `FALSE` in Excel.
fn bool_arg<S: CellSource + ?Sized>(arg: &Expr, src: &S) -> Result<bool, ErrorKind> {
    let v = eval_view(arg, src);
    if let Some(e) = v.error() {
        return Err(e);
    }
    match v {
        Value::Bool(b) => Ok(b),
        Value::Number(n) => Ok(n != 0.0),
        Value::Empty => Ok(false),
        _ => Err(ErrorKind::Value),
    }
}

/// Evaluate an argument to a value, interning a bare string literal so it
/// survives as text rather than collapsing to `#VALUE!`.
fn value_arg<S: CellSource + ?Sized>(arg: &Expr, src: &S) -> Result<Value, ErrorKind> {
    match arg {
        Expr::Text(s) => intern_formula_text(s)
            .map(Value::Text)
            .ok_or(ErrorKind::Value),
        other => Ok(eval_view(other, src)),
    }
}
