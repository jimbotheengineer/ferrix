//! Statistical formula functions.
//!
//! `MEDIAN`, `MODE`, `STDEV.P`, `STDEV.S`, `VAR.P`, `VAR.S`, `PERCENTILE.INC`,
//! `QUARTILE.INC`, `RANK`, `LARGE`, `SMALL`.
//!
//! Everything lives here rather than in `eval.rs` so the evaluator keeps a
//! single delegating arm (see [`is_stat_fn`]) and this file can grow without
//! colliding with the rest of the function library.
//!
//! # Memory: the honest version
//!
//! Ferrix's scale invariant is that peak memory stays bounded by the viewport
//! plus the edit overlay, never by row count. Two of these functions can hold
//! that line and the rest cannot:
//!
//! - **Streaming, O(1) memory:** `STDEV.*`, `VAR.*` (Welford, one pass) and
//!   `RANK` (one pass counting how many values beat the probe). These are
//!   exact and allocate nothing proportional to the input.
//! - **Buffered, O(numeric cells):** `MEDIAN`, `MODE`, `PERCENTILE.INC`,
//!   `QUARTILE.INC`, `LARGE`, `SMALL`. Order statistics are not computable in
//!   sublinear space in one pass, so these genuinely need the values in
//!   memory.
//!
//! For the buffered family we do the two things that are actually available:
//!
//! 1. Hold **one `f64` per numeric cell** (8 bytes) and select with
//!    [`slice::select_nth_unstable_by`] — quickselect, O(n) time, in place. We
//!    never sort, and we never make a second (sorted) copy. A 10M-row column
//!    therefore costs one 80 MB buffer and a partial partition, not a sort of
//!    a full copy.
//! 2. Cap that buffer at [`MAX_BUFFERED_VALUES`] and return `#NUM!` above it
//!    rather than growing without bound. Refusing loudly beats an OOM that
//!    takes the user's unsaved edits with it.
//!
//! So: criterion "must not sort a full copy" is met. The strict scale
//! invariant is *not* met by the buffered family — it is bounded by a
//! documented constant instead of by row count, which is the closest honest
//! approximation available for order statistics.

use ferrix_core::{ErrorKind, Value};

use crate::eval::{eval_view, range_spec, spec_get, CellSource};
use crate::parser::Expr;
use std::collections::HashMap;

/// Hard ceiling on the value buffer used by the order-statistic functions.
///
/// 2^24 values = 128 MiB of `f64`, chosen to sit near the ~108 MB streaming
/// peak the rest of the codebase targets while still covering the 10M-row
/// column the issue calls out (10M values = 80 MB). Past this we return
/// `#NUM!`; we do not silently truncate, because a median over a silently
/// truncated input is a wrong answer that looks right.
pub const MAX_BUFFERED_VALUES: usize = 16_777_216;

/// Does this name belong to the statistics library?
///
/// Names arrive already upper-cased by the tokenizer.
pub fn is_stat_fn(name: &str) -> bool {
    matches!(
        name,
        "MEDIAN"
            | "MODE"
            | "MODE.SNGL"
            | "STDEV.P"
            | "STDEV.S"
            | "VAR.P"
            | "VAR.S"
            | "PERCENTILE.INC"
            | "QUARTILE.INC"
            | "RANK"
            | "RANK.EQ"
            | "LARGE"
            | "SMALL"
    )
}

/// Entry point from `eval_call`.
pub fn call<S: CellSource + ?Sized>(name: &str, args: &[Expr], src: &S) -> Value {
    match name {
        "MEDIAN" => buffered(args, src, median_of),
        "MODE" | "MODE.SNGL" => buffered(args, src, mode_of),
        "STDEV.P" | "STDEV.S" | "VAR.P" | "VAR.S" => spread(name, args, src),
        "PERCENTILE.INC" => two_arg(args, src, |vals, k| percentile_of(vals, k)),
        "QUARTILE.INC" => two_arg(args, src, |vals, q| {
            // Excel truncates the quart argument; 0..=4 only.
            let q = q.trunc();
            if !(0.0..=4.0).contains(&q) {
                return Err(ErrorKind::Num);
            }
            percentile_of(vals, q / 4.0)
        }),
        "LARGE" => two_arg(args, src, |vals, k| nth_extreme(vals, k, true)),
        "SMALL" => two_arg(args, src, |vals, k| nth_extreme(vals, k, false)),
        "RANK" | "RANK.EQ" => rank(args, src),
        _ => Value::Error(ErrorKind::Name),
    }
}

// --- range streaming ------------------------------------------------------
//
// Ranges are walked with the same `range_spec`/`spec_get` pair the SUMIF /
// COUNTIFS family uses, so open-ended ranges are clamped to the sheet extent
// and cross-sheet ranges work here for free. There is deliberately no second
// range walker in this file.

/// Feed every numeric contribution of `arg` to `f`.
///
/// Text, blanks and booleans **inside a range** are skipped rather than
/// coerced, which is what Excel does: `MEDIAN` of `{1, "x", , 3}` is 2, not
/// the median of `{1, 0, 0, 3}`. A bare argument (`MEDIAN(1, TRUE)`) does
/// coerce, again matching Excel, which treats direct arguments differently
/// from range contents.
///
/// An error cell anywhere in a contributing range is the answer, so it is
/// returned immediately.
fn fold_numeric<S, F>(arg: &Expr, src: &S, f: &mut F) -> Result<(), ErrorKind>
where
    S: CellSource + ?Sized,
    F: FnMut(f64) -> Result<(), ErrorKind>,
{
    match arg {
        // `range_spec` also accepts a lone cell ref as a 1x1 range, which is
        // why the range arm is tried first.
        Expr::Range(..) | Expr::XRange(..) | Expr::Ref(..) | Expr::XRef(..) => {
            let Some(spec) = range_spec(arg, src) else {
                // Only reachable for an unresolvable sheet name.
                return Err(ErrorKind::Ref);
            };
            for dc in 0..spec.cols {
                for dr in 0..spec.rows {
                    match spec_get(&spec, src, dr, dc) {
                        Value::Number(n) => f(n)?,
                        Value::Error(e) => return Err(e),
                        // Text / blank / bool: skipped, not coerced.
                        _ => {}
                    }
                }
            }
            Ok(())
        }
        other => match eval_view(other, src) {
            Value::Error(e) => Err(e),
            // A literal string argument has no `Value` in this engine and
            // arrives as `#VALUE!` from `eval_view`, so it is handled above.
            v => {
                if let Some(n) = v.as_number() {
                    f(n)?;
                }
                Ok(())
            }
        },
    }
}

/// Buffer every numeric value the arguments contribute, up to the cap.
fn collect<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> Result<Vec<f64>, ErrorKind> {
    collect_capped(args, src, MAX_BUFFERED_VALUES)
}

/// The body of [`collect`], with the cap injected so the refusal path is
/// testable without materialising 16M cells.
fn collect_capped<S: CellSource + ?Sized>(
    args: &[Expr],
    src: &S,
    cap: usize,
) -> Result<Vec<f64>, ErrorKind> {
    let mut out: Vec<f64> = Vec::new();
    for a in args {
        fold_numeric(a, src, &mut |n| {
            if out.len() == cap {
                return Err(ErrorKind::Num);
            }
            out.push(n);
            Ok(())
        })?;
    }
    Ok(out)
}

/// Shape shared by the buffered, single-data-argument functions.
fn buffered<S, F>(args: &[Expr], src: &S, f: F) -> Value
where
    S: CellSource + ?Sized,
    F: FnOnce(&mut [f64]) -> Result<f64, ErrorKind>,
{
    if args.is_empty() {
        return Value::Error(ErrorKind::Value);
    }
    let mut vals = match collect(args, src) {
        Ok(v) => v,
        Err(e) => return Value::Error(e),
    };
    match f(&mut vals) {
        Ok(n) => Value::Number(n),
        Err(e) => Value::Error(e),
    }
}

/// Shape shared by `PERCENTILE.INC` / `QUARTILE.INC` / `LARGE` / `SMALL`:
/// one data argument followed by one scalar.
fn two_arg<S, F>(args: &[Expr], src: &S, f: F) -> Value
where
    S: CellSource + ?Sized,
    F: FnOnce(&mut [f64], f64) -> Result<f64, ErrorKind>,
{
    if args.len() != 2 {
        return Value::Error(ErrorKind::Value);
    }
    let k = match eval_view(&args[1], src) {
        Value::Error(e) => return Value::Error(e),
        v => match v.as_number() {
            Some(n) => n,
            None => return Value::Error(ErrorKind::Value),
        },
    };
    let mut vals = match collect(&args[..1], src) {
        Ok(v) => v,
        Err(e) => return Value::Error(e),
    };
    match f(&mut vals, k) {
        Ok(n) => Value::Number(n),
        Err(e) => Value::Error(e),
    }
}

// --- selection ------------------------------------------------------------

/// The value that would sit at sorted index `k`, found by quickselect.
///
/// `select_nth_unstable_by` partitions in place in O(n) and never sorts, so
/// this is the whole reason the buffered family does not need a sorted copy.
/// `f64::total_cmp` rather than `partial_cmp().unwrap()` so a stray NaN
/// orders instead of panicking.
fn kth(vals: &mut [f64], k: usize) -> f64 {
    let (_, at, _) = vals.select_nth_unstable_by(k, f64::total_cmp);
    *at
}

/// Sorted values at indices `k` and `k + 1`, in one partition.
///
/// After partitioning at `k`, everything above `k` is on the right, so the
/// next sorted value is the minimum of that side — no second selection pass.
fn kth_pair(vals: &mut [f64], k: usize) -> (f64, f64) {
    let (_, at, right) = vals.select_nth_unstable_by(k, f64::total_cmp);
    let lo = *at;
    let hi = right
        .iter()
        .copied()
        .reduce(|a, b| if b.total_cmp(&a).is_lt() { b } else { a })
        .unwrap_or(lo);
    (lo, hi)
}

/// `MEDIAN`: middle value, or the mean of the two middle values.
fn median_of(vals: &mut [f64]) -> Result<f64, ErrorKind> {
    let n = vals.len();
    if n == 0 {
        return Err(ErrorKind::Num);
    }
    if n % 2 == 1 {
        return Ok(kth(vals, n / 2));
    }
    // Partition at the upper middle; the lower middle is then the maximum of
    // the left side.
    let (left, at, _) = vals.select_nth_unstable_by(n / 2, f64::total_cmp);
    let hi = *at;
    let lo = left
        .iter()
        .copied()
        .reduce(|a, b| if b.total_cmp(&a).is_gt() { b } else { a })
        .unwrap_or(hi);
    // Halving each side first rather than (lo + hi) / 2.0 keeps the answer
    // finite when both are near f64::MAX.
    Ok(lo / 2.0 + hi / 2.0)
}

/// `PERCENTILE.INC(array, k)` with Excel's interpolation.
///
/// Excel places the k-th percentile at fractional sorted rank `k * (n - 1)`
/// and linearly interpolates between the two bracketing ranks. `k = 0` is the
/// minimum, `k = 1` the maximum, and anything outside `[0, 1]` is `#NUM!`.
fn percentile_of(vals: &mut [f64], k: f64) -> Result<f64, ErrorKind> {
    let n = vals.len();
    if n == 0 {
        return Err(ErrorKind::Num);
    }
    if !(0.0..=1.0).contains(&k) {
        return Err(ErrorKind::Num);
    }
    if n == 1 {
        return Ok(vals[0]);
    }
    let pos = k * (n - 1) as f64;
    let idx = pos.floor();
    let frac = pos - idx;
    let idx = idx as usize;
    if idx >= n - 1 {
        // k == 1 exactly.
        return Ok(kth(vals, n - 1));
    }
    let (lo, hi) = kth_pair(vals, idx);
    if frac == 0.0 {
        return Ok(lo);
    }
    Ok(lo + frac * (hi - lo))
}

/// `LARGE` / `SMALL`. `k` is 1-based and truncated, as Excel does.
fn nth_extreme(vals: &mut [f64], k: f64, largest: bool) -> Result<f64, ErrorKind> {
    let n = vals.len();
    if n == 0 {
        return Err(ErrorKind::Num);
    }
    let k = k.trunc();
    if !(1.0..=n as f64).contains(&k) {
        return Err(ErrorKind::Num);
    }
    let k = k as usize;
    let idx = if largest { n - k } else { k - 1 };
    Ok(kth(vals, idx))
}

/// `MODE`: the most frequent value; `#N/A` when nothing repeats.
///
/// Ties go to the value whose first occurrence is earliest, matching Excel.
/// Sorting the buffer would lose that ordering, so occurrences are counted
/// with a map keyed on the bit pattern (`f64` is not `Hash`). The map is
/// bounded by the number of *distinct* values, which is already covered by
/// [`MAX_BUFFERED_VALUES`].
fn mode_of(vals: &mut [f64]) -> Result<f64, ErrorKind> {
    if vals.is_empty() {
        return Err(ErrorKind::Num);
    }
    let mut seen: HashMap<u64, (usize, usize)> = HashMap::new();
    for (i, v) in vals.iter().enumerate() {
        // Normalise -0.0 to 0.0 so MODE(0, -0) sees one value, as Excel does.
        let key = (if *v == 0.0 { 0.0 } else { *v }).to_bits();
        let e = seen.entry(key).or_insert((0, i));
        e.0 += 1;
    }
    let best = seen
        .values()
        .filter(|(count, _)| *count > 1)
        .min_by_key(|(count, first)| (std::cmp::Reverse(*count), *first));
    match best {
        Some(&(_, first)) => Ok(vals[first]),
        None => Err(ErrorKind::NotAvailable),
    }
}

// --- spread ---------------------------------------------------------------

/// Welford's online algorithm: one pass, O(1) memory, numerically stable.
///
/// The naive `E[x^2] - E[x]^2` form catastrophically cancels when the values
/// are large relative to their spread — for values near 1e9 differing by 1 it
/// returns exactly 0. Welford accumulates deviations from a running mean, so
/// nothing ever squares the magnitude of the data. See
/// `variance_is_numerically_stable` in the tests, which computes the naive
/// form alongside and asserts it really does fail.
#[derive(Default)]
struct Welford {
    n: u64,
    mean: f64,
    m2: f64,
}

impl Welford {
    fn push(&mut self, x: f64) {
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        self.m2 += delta * (x - self.mean);
    }
}

fn spread<S: CellSource + ?Sized>(name: &str, args: &[Expr], src: &S) -> Value {
    if args.is_empty() {
        return Value::Error(ErrorKind::Value);
    }
    // SCALE INVARIANT: nothing is buffered here. A STDEV.P over a 200M-row
    // column costs one pass and three stack words.
    let mut w = Welford::default();
    for a in args {
        if let Err(e) = fold_numeric(a, src, &mut |n| {
            w.push(n);
            Ok(())
        }) {
            return Value::Error(e);
        }
    }
    if w.n == 0 {
        // Excel says #DIV/0! here; issue #26 specifies #NUM! for empty input
        // across this family, and a uniform answer is worth more than the
        // exact Excel code for a case that only arises from an empty range.
        return Value::Error(ErrorKind::Num);
    }
    let sample = name.ends_with(".S");
    if sample && w.n < 2 {
        // One observation has no sample variance — Excel agrees.
        return Value::Error(ErrorKind::DivZero);
    }
    let denom = if sample { w.n - 1 } else { w.n } as f64;
    let var = w.m2 / denom;
    Value::Number(if name.starts_with("STDEV") {
        var.sqrt()
    } else {
        var
    })
}

// --- rank -----------------------------------------------------------------

/// `RANK(number, ref, [order])`.
///
/// Streams: rank is just "how many values beat this one", so no buffer is
/// needed at any input size. `order` 0 or omitted ranks descending (largest
/// is rank 1), anything else ascending. A number absent from `ref` is `#N/A`,
/// matching Excel.
fn rank<S: CellSource + ?Sized>(args: &[Expr], src: &S) -> Value {
    if args.len() < 2 || args.len() > 3 {
        return Value::Error(ErrorKind::Value);
    }
    let probe = match eval_view(&args[0], src) {
        Value::Error(e) => return Value::Error(e),
        v => match v.as_number() {
            Some(n) => n,
            None => return Value::Error(ErrorKind::Value),
        },
    };
    let descending = match args.get(2) {
        None => true,
        Some(a) => match eval_view(a, src) {
            Value::Error(e) => return Value::Error(e),
            v => match v.as_number() {
                Some(n) => n == 0.0,
                None => return Value::Error(ErrorKind::Value),
            },
        },
    };

    let mut beating = 0u64;
    let mut total = 0u64;
    let mut found = false;
    if let Err(e) = fold_numeric(&args[1], src, &mut |n| {
        total += 1;
        if n == probe {
            found = true;
        } else if (descending && n > probe) || (!descending && n < probe) {
            beating += 1;
        }
        Ok(())
    }) {
        return Value::Error(e);
    }
    if total == 0 {
        return Value::Error(ErrorKind::Num);
    }
    if !found {
        return Value::Error(ErrorKind::NotAvailable);
    }
    Value::Number(beating as f64 + 1.0)
}

#[cfg(test)]
mod tests;
