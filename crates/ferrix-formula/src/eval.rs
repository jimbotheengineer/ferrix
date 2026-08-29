//! Formula evaluation.
//!
//! Range aggregations dispatch to the columnar fast paths in `ferrix-core`
//! rather than iterating cell-by-cell, so `SUM(A1:A10000000)` is a typed
//! slice walk instead of ten million enum matches.
//!
//! Evaluation is generic over [`CellSource`] so the same code runs against a
//! plain `Sheet`, a base+overlay composite, or (later) a memory-mapped file.

use ferrix_core::{CellRef, ErrorKind, Sheet, Value};

use crate::criteria::{Criterion, Scalar};
use crate::parser::{BinOp, Expr, UnOp};

/// Anything formulas can read cells from.
///
/// `sum_rect`/`count_rect` are on the trait rather than derived from `get` so
/// implementations can use a columnar fast path — the difference between a
/// typed slice walk and 10M virtual calls.
pub trait CellSource {
    fn get(&self, cell: CellRef) -> Value;
    fn resolve(&self, id: ferrix_core::StrId) -> &str;
    fn sum_rect(&self, start: CellRef, end: CellRef) -> f64;
    fn count_rect(&self, start: CellRef, end: CellRef) -> usize;
    /// Row extent, used to clamp open-ended ranges.
    fn row_count(&self) -> usize;

    // --- cross-sheet reads -----------------------------------------------
    //
    // These are on `CellSource` with defaults rather than in a second trait so
    // that a lone `Sheet` — which genuinely has no siblings — stays usable
    // everywhere it was before. A source with no workbook around it answers
    // `#REF!`, which is exactly what `Sheet2!A1` means when there is no
    // Sheet2.

    /// Read a cell in a named sibling sheet.
    fn get_in(&self, _sheet: &str, _cell: CellRef) -> Value {
        Value::Error(ErrorKind::Ref)
    }

    /// Is `sheet` a name this source can resolve? Drives `#REF!` reporting for
    /// ranges, which cannot signal an error through their return type.
    fn has_sheet(&self, _sheet: &str) -> bool {
        false
    }

    /// Columnar sum inside a named sibling sheet. `None` when unresolvable.
    fn sum_rect_in(&self, _sheet: &str, _start: CellRef, _end: CellRef) -> Option<f64> {
        None
    }

    fn count_rect_in(&self, _sheet: &str, _start: CellRef, _end: CellRef) -> Option<usize> {
        None
    }

    fn row_count_in(&self, _sheet: &str) -> Option<usize> {
        None
    }

    /// The sheet NAMES in the inclusive tab-order run `first..=last`, which is
    /// what `Sheet1:Sheet3!A1` spans.
    ///
    /// Returned as names rather than ids because the rest of `CellSource`
    /// speaks names — that is what keeps the evaluator ignorant of what a
    /// workbook is. An empty result means the run does not resolve, and a 3-D
    /// reference over it is `#REF!` rather than an empty sum.
    ///
    /// Bounded by the SHEET count, never the row count.
    fn sheet_span(&self, _first: &str, _last: &str) -> Vec<String> {
        Vec::new()
    }
}

impl CellSource for Sheet {
    #[inline]
    fn get(&self, cell: CellRef) -> Value {
        Sheet::get(self, cell)
    }
    #[inline]
    fn resolve(&self, id: ferrix_core::StrId) -> &str {
        Sheet::resolve(self, id)
    }
    #[inline]
    fn sum_rect(&self, start: CellRef, end: CellRef) -> f64 {
        Sheet::sum_rect(self, start, end)
    }
    #[inline]
    fn count_rect(&self, start: CellRef, end: CellRef) -> usize {
        Sheet::count_rect(self, start, end)
    }
    #[inline]
    fn row_count(&self) -> usize {
        Sheet::row_count(self)
    }
}

/// Evaluate against a plain sheet.
pub fn eval(expr: &Expr, sheet: &Sheet) -> Value {
    eval_view(expr, sheet)
}

/// Evaluate against any cell source.
pub fn eval_view<S: CellSource + ?Sized>(expr: &Expr, src: &S) -> Value {
    match expr {
        Expr::Number(n) => Value::Number(*n),
        Expr::Bool(b) => Value::Bool(*b),
        Expr::Text(_) => Value::Error(ErrorKind::Value),
        Expr::Ref(cell) => src.get(*cell),
        Expr::Range(_, _) => Value::Error(ErrorKind::Value),
        Expr::XRef(sheet, cell) => src.get_in(sheet, *cell),
        // A bare range is not a value in either flavour, but an unknown sheet
        // name is a #REF! and should say so rather than #VALUE!.
        Expr::XRange(sheet, _, _) => {
            if src.has_sheet(sheet) {
                Value::Error(ErrorKind::Value)
            } else {
                Value::Error(ErrorKind::Ref)
            }
        }
        // A 3-D reference is not a scalar even when it is one cell wide: it
        // stands for that cell on SEVERAL sheets, so there is no single value
        // to return. Only an aggregate can consume it — as in Excel, where
        // `=Sheet1:Sheet3!A1` alone is an error and `=SUM(Sheet1:Sheet3!A1)`
        // is the point of the feature. A broken run still says #REF! first,
        // which is the more useful of the two diagnoses.
        Expr::X3D(first, last, _, _) => {
            if src.sheet_span(first, last).is_empty() {
                Value::Error(ErrorKind::Ref)
            } else {
                Value::Error(ErrorKind::Value)
            }
        }
        Expr::Unary(op, inner) => {
            let v = eval_view(inner, src);
            if let Some(e) = v.error() {
                return Value::Error(e);
            }
            match (op, v.as_number()) {
                (UnOp::Neg, Some(n)) => Value::Number(-n),
                (UnOp::Percent, Some(n)) => Value::Number(n / 100.0),
                _ => Value::Error(ErrorKind::Value),
            }
        }
        Expr::Binary(op, lhs, rhs) => eval_binary(*op, lhs, rhs, src),
        Expr::Call(name, args) => eval_call(name, args, src),
    }
}

fn eval_binary<S: CellSource + ?Sized>(op: BinOp, lhs: &Expr, rhs: &Expr, src: &S) -> Value {
    let a = eval_view(lhs, src);
    if let Some(e) = a.error() {
        return Value::Error(e);
    }
    let b = eval_view(rhs, src);
    if let Some(e) = b.error() {
        return Value::Error(e);
    }

    match op {
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
            let ord = compare(&a, &b, src);
            let result = match (op, ord) {
                (BinOp::Eq, Some(o)) => o == std::cmp::Ordering::Equal,
                (BinOp::Ne, Some(o)) => o != std::cmp::Ordering::Equal,
                (BinOp::Lt, Some(o)) => o == std::cmp::Ordering::Less,
                (BinOp::Gt, Some(o)) => o == std::cmp::Ordering::Greater,
                (BinOp::Le, Some(o)) => o != std::cmp::Ordering::Greater,
                (BinOp::Ge, Some(o)) => o != std::cmp::Ordering::Less,
                _ => return Value::Error(ErrorKind::Value),
            };
            Value::Bool(result)
        }
        BinOp::Concat => Value::Error(ErrorKind::Value),
        _ => {
            let (x, y) = match (a.as_number(), b.as_number()) {
                (Some(x), Some(y)) => (x, y),
                _ => return Value::Error(ErrorKind::Value),
            };
            match op {
                BinOp::Add => Value::Number(x + y),
                BinOp::Sub => Value::Number(x - y),
                BinOp::Mul => Value::Number(x * y),
                BinOp::Div => {
                    if y == 0.0 {
                        Value::Error(ErrorKind::DivZero)
                    } else {
                        Value::Number(x / y)
                    }
                }
                BinOp::Pow => {
                    let r = x.powf(y);
                    if r.is_nan() && !x.is_nan() && !y.is_nan() {
                        Value::Error(ErrorKind::Num)
                    } else {
                        Value::Number(r)
                    }
                }
                _ => Value::Error(ErrorKind::Value),
            }
        }
    }
}

/// Spreadsheet comparison: numbers compare numerically, text lexicographically
/// (case-insensitive), and numbers sort before text.
fn compare<S: CellSource + ?Sized>(a: &Value, b: &Value, src: &S) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => {
            let sx = src.resolve(*x).to_ascii_lowercase();
            let sy = src.resolve(*y).to_ascii_lowercase();
            Some(sx.cmp(&sy))
        }
        (Value::Text(_), _) => Some(Ordering::Greater),
        (_, Value::Text(_)) => Some(Ordering::Less),
        _ => {
            let x = a.as_number()?;
            let y = b.as_number()?;
            x.partial_cmp(&y)
        }
    }
}

/// Collect the numeric values an argument contributes to an aggregation.
fn fold_numeric<S: CellSource + ?Sized, F>(arg: &Expr, src: &S, f: &mut F)
where
    F: FnMut(f64),
{
    match arg {
        Expr::Range(start, end) => {
            let r1 = (end.row as usize + 1).min(src.row_count().max(1));
            for c in start.col..=end.col {
                for r in start.row as usize..r1 {
                    if let Value::Number(n) = src.get(CellRef::new(r as u32, c)) {
                        f(n);
                    }
                }
            }
        }
        Expr::XRange(sheet, start, end) => {
            let Some(rows) = src.row_count_in(sheet) else {
                return;
            };
            let r1 = (end.row as usize + 1).min(rows.max(1));
            for c in start.col..=end.col {
                for r in start.row as usize..r1 {
                    if let Value::Number(n) = src.get_in(sheet, CellRef::new(r as u32, c)) {
                        f(n);
                    }
                }
            }
        }
        // The same rectangle on every sheet of the run. `arg_error` has
        // already reported an unresolvable run as #REF!, so a failure here is
        // simply nothing to add.
        Expr::X3D(first, last, start, end) => {
            let _ = for_each_3d(first, last, *start, *end, src, |spec| {
                for dc in 0..spec.cols {
                    for dr in 0..spec.rows {
                        if let Value::Number(n) = spec_get(spec, src, dr, dc) {
                            f(n);
                        }
                    }
                }
                Ok(())
            });
        }
        other => {
            if let Some(n) = eval_view(other, src).as_number() {
                f(n);
            }
        }
    }
}

fn eval_call<S: CellSource + ?Sized>(name: &str, args: &[Expr], src: &S) -> Value {
    match name {
        "SUM" => {
            // Fast path: a lone range delegates straight to the columnar sum,
            // but only after confirming the range holds no error cells —
            // sum_rect skips non-numerics silently and would mask them.
            if let [Expr::Range(s, e)] = args {
                if let Some(err) = arg_error(&args[0], src) {
                    return Value::Error(err);
                }
                return Value::Number(src.sum_rect(*s, *e));
            }
            // Same fast path for a cross-sheet range: the sibling sheet's own
            // columnar sum, not a cell-by-cell walk.
            if let [Expr::XRange(sh, s, e)] = args {
                if let Some(err) = arg_error(&args[0], src) {
                    return Value::Error(err);
                }
                return match src.sum_rect_in(sh, *s, *e) {
                    Some(v) => Value::Number(v),
                    None => Value::Error(ErrorKind::Ref),
                };
            }
            // And for a 3-D run: each sheet's own columnar sum, so summing
            // the same 200M-row column across three sheets is three slice
            // walks rather than 600M reads.
            if let [Expr::X3D(first, last, s, e)] = args {
                if let Some(err) = arg_error(&args[0], src) {
                    return Value::Error(err);
                }
                let mut acc = 0.0;
                return match sum_3d(first, last, *s, *e, src, &mut acc) {
                    Ok(()) => Value::Number(acc),
                    Err(e) => Value::Error(e),
                };
            }
            let mut acc = 0.0;
            for a in args {
                if let Some(err) = arg_error(a, src) {
                    return Value::Error(err);
                }
                fold_numeric(a, src, &mut |n| acc += n);
            }
            Value::Number(acc)
        }
        "COUNT" => {
            if let [Expr::Range(s, e)] = args {
                return Value::Number(src.count_rect(*s, *e) as f64);
            }
            if let [Expr::XRange(sh, s, e)] = args {
                return match src.count_rect_in(sh, *s, *e) {
                    Some(n) => Value::Number(n as f64),
                    None => Value::Error(ErrorKind::Ref),
                };
            }
            if let [Expr::X3D(first, last, s, e)] = args {
                let mut n = 0usize;
                return match count_3d(first, last, *s, *e, src, &mut n) {
                    Ok(()) => Value::Number(n as f64),
                    Err(e) => Value::Error(e),
                };
            }
            let mut n = 0usize;
            for a in args {
                fold_numeric(a, src, &mut |_| n += 1);
            }
            Value::Number(n as f64)
        }
        "AVERAGE" => {
            // Use the columnar paths for a lone range so a 10M-row average
            // stays two slice walks instead of ten million reads.
            if let [Expr::Range(s, e)] = args {
                if let Some(err) = arg_error(&args[0], src) {
                    return Value::Error(err);
                }
                let count = src.count_rect(*s, *e);
                if count == 0 {
                    return Value::Error(ErrorKind::DivZero);
                }
                return Value::Number(src.sum_rect(*s, *e) / count as f64);
            }
            if let [Expr::XRange(sh, s, e)] = args {
                if let Some(err) = arg_error(&args[0], src) {
                    return Value::Error(err);
                }
                let (Some(total), Some(count)) =
                    (src.sum_rect_in(sh, *s, *e), src.count_rect_in(sh, *s, *e))
                else {
                    return Value::Error(ErrorKind::Ref);
                };
                if count == 0 {
                    return Value::Error(ErrorKind::DivZero);
                }
                return Value::Number(total / count as f64);
            }
            if let [Expr::X3D(first, last, s, e)] = args {
                if let Some(err) = arg_error(&args[0], src) {
                    return Value::Error(err);
                }
                let (mut total, mut count) = (0.0, 0usize);
                if let Err(e) = sum_3d(first, last, *s, *e, src, &mut total) {
                    return Value::Error(e);
                }
                if let Err(e) = count_3d(first, last, *s, *e, src, &mut count) {
                    return Value::Error(e);
                }
                if count == 0 {
                    return Value::Error(ErrorKind::DivZero);
                }
                return Value::Number(total / count as f64);
            }
            let mut acc = 0.0;
            let mut n = 0usize;
            for a in args {
                if let Some(err) = arg_error(a, src) {
                    return Value::Error(err);
                }
                fold_numeric(a, src, &mut |v| {
                    acc += v;
                    n += 1;
                });
            }
            if n == 0 {
                Value::Error(ErrorKind::DivZero)
            } else {
                Value::Number(acc / n as f64)
            }
        }
        "MIN" | "MAX" => {
            let want_min = name == "MIN";
            let mut best: Option<f64> = None;
            for a in args {
                if let Some(err) = arg_error(a, src) {
                    return Value::Error(err);
                }
                fold_numeric(a, src, &mut |v| {
                    best = Some(match best {
                        None => v,
                        Some(b) if want_min => b.min(v),
                        Some(b) => b.max(v),
                    });
                });
            }
            Value::Number(best.unwrap_or(0.0))
        }
        "ABS" | "SQRT" | "ROUND" | "FLOOR" | "CEILING" | "INT" | "LN" | "LOG10" | "EXP" => {
            eval_math(name, args, src)
        }
        "IF" => {
            if args.len() < 2 || args.len() > 3 {
                return Value::Error(ErrorKind::Value);
            }
            let cond = eval_view(&args[0], src);
            if let Some(e) = cond.error() {
                return Value::Error(e);
            }
            match cond.as_bool() {
                Some(true) => eval_view(&args[1], src),
                Some(false) => {
                    if args.len() == 3 {
                        eval_view(&args[2], src)
                    } else {
                        Value::Bool(false)
                    }
                }
                None => Value::Error(ErrorKind::Value),
            }
        }
        "AND" | "OR" => {
            let is_and = name == "AND";
            let mut acc = is_and;
            let mut seen = false;
            for a in args {
                let v = eval_view(a, src);
                if let Some(e) = v.error() {
                    return Value::Error(e);
                }
                match v.as_bool() {
                    Some(b) => {
                        seen = true;
                        acc = if is_and { acc && b } else { acc || b };
                    }
                    None => return Value::Error(ErrorKind::Value),
                }
            }
            if !seen {
                return Value::Error(ErrorKind::Value);
            }
            Value::Bool(acc)
        }
        "NOT" => {
            if args.len() != 1 {
                return Value::Error(ErrorKind::Value);
            }
            match eval_view(&args[0], src).as_bool() {
                Some(b) => Value::Bool(!b),
                None => Value::Error(ErrorKind::Value),
            }
        }
        "SUMIF" | "AVERAGEIF" | "COUNTIF" => eval_if_single(name, args, src),
        "SUMIFS" | "AVERAGEIFS" | "COUNTIFS" => eval_ifs(name, args, src),
        name if crate::stats::is_stat_fn(name) => crate::stats::call(name, args, src),
        "IFERROR" | "IFNA" => {
            if args.len() != 2 {
                return Value::Error(ErrorKind::Value);
            }
            let v = eval_view(&args[0], src);
            let caught = match v.error() {
                Some(ErrorKind::NotAvailable) => true,
                Some(_) => name == "IFERROR",
                None => false,
            };
            if caught {
                eval_view(&args[1], src)
            } else {
                v
            }
        }
        "NA" => {
            if !args.is_empty() {
                return Value::Error(ErrorKind::Value);
            }
            Value::Error(ErrorKind::NotAvailable)
        }
        "ISBLANK" | "ISNUMBER" | "ISTEXT" | "ISERROR" | "ISERR" | "ISNA" => {
            if args.len() != 1 {
                return Value::Error(ErrorKind::Value);
            }
            // IS* functions inspect their argument rather than consuming it,
            // so an error argument is data, not something to propagate.
            let probe = probe_arg(&args[0], src);
            let r = match (name, probe) {
                ("ISBLANK", Probe::Val(v)) => v.is_empty(),
                ("ISBLANK", Probe::Text) => false,
                ("ISNUMBER", Probe::Val(v)) => matches!(v, Value::Number(_)),
                ("ISNUMBER", Probe::Text) => false,
                // A bare string literal is text even though this engine has
                // nowhere to intern it into, so `ISTEXT("x")` is TRUE.
                ("ISTEXT", Probe::Text) => true,
                ("ISTEXT", Probe::Val(v)) => matches!(v, Value::Text(_)),
                ("ISERROR", Probe::Val(v)) => v.is_error(),
                ("ISERR", Probe::Val(v)) => {
                    matches!(v.error(), Some(e) if e != ErrorKind::NotAvailable)
                }
                ("ISNA", Probe::Val(v)) => v.error() == Some(ErrorKind::NotAvailable),
                (_, Probe::Text) => false,
                _ => return Value::Error(ErrorKind::Name),
            };
            Value::Bool(r)
        }
        "ERROR.TYPE" => {
            if args.len() != 1 {
                return Value::Error(ErrorKind::Value);
            }
            match eval_view(&args[0], src).error() {
                // Excel's fixed numbering. Circular has no Excel code (Excel
                // reports it out-of-band, not as a cell error), so it gets the
                // next free number rather than pretending to be one of these.
                Some(e) => Value::Number(match e {
                    ErrorKind::Null => 1.0,
                    ErrorKind::DivZero => 2.0,
                    ErrorKind::Value => 3.0,
                    ErrorKind::Ref => 4.0,
                    ErrorKind::Name => 5.0,
                    ErrorKind::Num => 6.0,
                    ErrorKind::NotAvailable => 7.0,
                    ErrorKind::Circular => 8.0,
                }),
                None => Value::Error(ErrorKind::NotAvailable),
            }
        }
        // Date and time functions live in their own module — see
        // `crate::datetime` for the calendar rules and the injectable clock.
        name if crate::datetime::is_date_fn(name) => crate::datetime::call(name, args, src),
        // Text functions live entirely in `crate::text`. Kept to ONE arm here
        // on purpose: the whole library is one module file, so it can grow
        // without touching this match again.
        name if crate::text::is_text_fn(name) => crate::text::call(name, args, src),
        // Lookup functions live entirely in `crate::lookup`. One arm again —
        // and note that this guard, like the three above it, must claim ONLY
        // names no other family owns: guard arms match in order, so the first
        // claimant silently wins and the loser's own tests never notice
        // (they do not route through this match). `crate::compose_tests`
        // pins the mutual exclusion.
        name if crate::lookup::is_lookup_fn(name) => crate::lookup::call(name, args, src),
        _ => Value::Error(ErrorKind::Name),
    }
}

/// How an IS* function sees its argument.
///
/// A string literal has no `Value` in this engine (there is no arena to
/// intern into during evaluation), so it is carried separately rather than
/// being flattened to `#VALUE!` and mis-reported by `ISERROR`.
enum Probe {
    Text,
    Val(Value),
}

fn probe_arg<S: CellSource + ?Sized>(arg: &Expr, src: &S) -> Probe {
    match arg {
        Expr::Text(_) => Probe::Text,
        other => Probe::Val(eval_view(other, src)),
    }
}

// --- conditional aggregation ---------------------------------------------
//
// SCALE INVARIANT: these stream. A criterion is compiled once per call, then
// matched against cells read one at a time; nothing proportional to the row
// count is ever materialised, and matching a row allocates nothing (see
// `crate::criteria`). A COUNTIFS over a 200M-row column costs one pass and a
// handful of stack words.

/// A rectangular region, possibly in a sibling sheet.
#[derive(Clone, Copy)]
pub(crate) struct RangeSpec<'a> {
    sheet: Option<&'a str>,
    start: CellRef,
    pub(crate) rows: u32,
    pub(crate) cols: u32,
}

impl RangeSpec<'_> {
    #[inline]
    fn cells(&self) -> u64 {
        self.rows as u64 * self.cols as u64
    }
}

/// Interpret an argument as a range. A lone cell reference is a 1x1 range,
/// which is what Excel does.
pub(crate) fn range_spec<'a, S: CellSource + ?Sized>(
    arg: &'a Expr,
    src: &S,
) -> Option<RangeSpec<'a>> {
    let (sheet, start, end) = match arg {
        Expr::Range(s, e) => (None, *s, *e),
        Expr::XRange(sh, s, e) => (Some(sh.as_str()), *s, *e),
        Expr::Ref(c) => (None, *c, *c),
        Expr::XRef(sh, c) => (Some(sh.as_str()), *c, *c),
        // A 3-D reference is deliberately NOT a range: it is one rectangle
        // per sheet, so it has no single origin to resize a SUMIF value range
        // against. Excel refuses it in the *IF family too. Callers that CAN
        // consume several rectangles use `for_each_3d`.
        _ => return None,
    };
    spec_for(sheet, start, end, src)
}

/// One rectangle, clamped to the sheet it lives on.
///
/// Clamping the open-ended bottom edge to the sheet's real extent is what
/// makes `A:A` cost the populated rows rather than 2^20 of them.
fn spec_for<'a, S: CellSource + ?Sized>(
    sheet: Option<&'a str>,
    start: CellRef,
    end: CellRef,
    src: &S,
) -> Option<RangeSpec<'a>> {
    let extent = match sheet {
        None => src.row_count(),
        Some(sh) => src.row_count_in(sh)?,
    }
    .max(1);
    let last = (end.row as usize + 1).min(extent);
    let rows = (last as u32).saturating_sub(start.row);
    Some(RangeSpec {
        sheet,
        start,
        rows,
        cols: end.col - start.col + 1,
    })
}

/// Run `f` once per sheet in a 3-D run, handing it that sheet's rectangle.
///
/// SCALE: one `RangeSpec` is live at a time and the run is bounded by the
/// SHEET count, so `SUM(Sheet1:Sheet3!A:A)` over three 200M-row columns holds
/// three stack words, not 600M cells.
///
/// `Err(Ref)` when the run does not resolve — a 3-D reference whose endpoint
/// sheet is gone must be `#REF!`, never a quietly smaller sum.
pub(crate) fn for_each_3d<S, F>(
    first: &str,
    last: &str,
    start: CellRef,
    end: CellRef,
    src: &S,
    mut f: F,
) -> Result<(), ErrorKind>
where
    S: CellSource + ?Sized,
    F: FnMut(&RangeSpec<'_>) -> Result<(), ErrorKind>,
{
    let names = src.sheet_span(first, last);
    if names.is_empty() {
        return Err(ErrorKind::Ref);
    }
    for name in &names {
        let Some(spec) = spec_for(Some(name.as_str()), start, end, src) else {
            return Err(ErrorKind::Ref);
        };
        f(&spec)?;
    }
    Ok(())
}

/// Read the cell at `(dr, dc)` inside a spec. Offsets past the sheet read as
/// `Empty`, which is how a resized SUMIF sum_range behaves.
#[inline]
pub(crate) fn spec_get<S: CellSource + ?Sized>(
    spec: &RangeSpec<'_>,
    src: &S,
    dr: u32,
    dc: u32,
) -> Value {
    let cell = CellRef::new(spec.start.row + dr, spec.start.col + dc);
    match spec.sheet {
        None => src.get(cell),
        Some(sh) => src.get_in(sh, cell),
    }
}

/// Borrow a cell as a matcher input. Free: text points into the arena.
#[inline]
fn scalar_of<S: CellSource + ?Sized>(v: Value, src: &S) -> Scalar<'_> {
    match v {
        Value::Empty => Scalar::Blank,
        Value::Number(n) => Scalar::Number(n),
        Value::Bool(b) => Scalar::Bool(b),
        Value::Text(id) => Scalar::Text(src.resolve(id)),
        Value::Error(e) => Scalar::Error(e),
    }
}

/// Compile a criteria argument once, before any scanning starts.
fn criterion_of<S: CellSource + ?Sized>(arg: &Expr, src: &S) -> Result<Criterion, ErrorKind> {
    match arg {
        Expr::Text(s) => Ok(Criterion::parse(s)),
        Expr::Number(n) => Ok(Criterion::eq_number(*n)),
        Expr::Bool(b) => Ok(Criterion::eq_bool(*b)),
        other => match eval_view(other, src) {
            Value::Number(n) => Ok(Criterion::eq_number(n)),
            Value::Bool(b) => Ok(Criterion::eq_bool(b)),
            Value::Text(id) => Ok(Criterion::parse(src.resolve(id))),
            Value::Empty => Ok(Criterion::parse("")),
            Value::Error(e) => Err(e),
        },
    }
}

/// Running total shared by every conditional aggregate.
#[derive(Default)]
struct Tally {
    sum: f64,
    matched: u64,
}

/// Turn a tally into the value the named function returns.
fn finish(name: &str, t: Tally) -> Value {
    match name {
        "COUNTIF" | "COUNTIFS" => Value::Number(t.matched as f64),
        "SUMIF" | "SUMIFS" => Value::Number(t.sum),
        // Excel: no matching row is #DIV/0!, not zero. The distinction
        // matters — it is the difference between "average of nothing" and
        // "the average happens to be zero".
        _ if t.matched == 0 => Value::Error(ErrorKind::DivZero),
        _ => Value::Number(t.sum / t.matched as f64),
    }
}

/// `SUMIF(range, criteria, [sum_range])` and friends.
fn eval_if_single<S: CellSource + ?Sized>(name: &str, args: &[Expr], src: &S) -> Value {
    let wants_values = name != "COUNTIF";
    let max_args = if wants_values { 3 } else { 2 };
    if args.len() < 2 || args.len() > max_args {
        return Value::Error(ErrorKind::Value);
    }
    let Some(range) = range_spec(&args[0], src) else {
        return Value::Error(ErrorKind::Value);
    };
    let crit = match criterion_of(&args[1], src) {
        Ok(c) => c,
        Err(e) => return Value::Error(e),
    };
    // Excel resizes the value range to the criteria range's shape from its
    // top-left corner, so only its origin matters.
    let values = match args.get(2) {
        Some(a) => match range_spec(a, src) {
            Some(s) => Some(s),
            None => return Value::Error(ErrorKind::Value),
        },
        None => None,
    };
    let values = if wants_values {
        Some(values.unwrap_or(range))
    } else {
        None
    };

    let mut t = Tally::default();
    for dc in 0..range.cols {
        for dr in 0..range.rows {
            if !crit.matches(scalar_of(spec_get(&range, src, dr, dc), src)) {
                continue;
            }
            match values {
                None => t.matched += 1,
                Some(v) => match spec_get(&v, src, dr, dc) {
                    // An error in a contributing cell is the answer.
                    Value::Error(e) => return Value::Error(e),
                    // Excel's *IF family averages numbers only: text and
                    // blanks in the value range are skipped entirely, they do
                    // not count as zero.
                    Value::Number(n) => {
                        t.sum += n;
                        t.matched += 1;
                    }
                    _ => {}
                },
            }
        }
    }
    finish(name, t)
}

/// `SUMIFS(sum_range, crit_range, crit, ...)` / `COUNTIFS(crit_range, crit, ...)`.
fn eval_ifs<S: CellSource + ?Sized>(name: &str, args: &[Expr], src: &S) -> Value {
    let wants_values = name != "COUNTIFS";
    let pairs_from = usize::from(wants_values);
    if args.len() < pairs_from + 2 || (args.len() - pairs_from) % 2 != 0 {
        return Value::Error(ErrorKind::Value);
    }

    let values = if wants_values {
        match range_spec(&args[0], src) {
            Some(s) => Some(s),
            None => return Value::Error(ErrorKind::Value),
        }
    } else {
        None
    };

    // Compile every criterion up front. This is the only allocation in the
    // whole call and it is bounded by the number of criteria pairs, not rows.
    let n_pairs = (args.len() - pairs_from) / 2;
    let mut specs: Vec<RangeSpec<'_>> = Vec::with_capacity(n_pairs);
    let mut crits: Vec<Criterion> = Vec::with_capacity(n_pairs);
    for i in 0..n_pairs {
        let a = pairs_from + i * 2;
        let Some(spec) = range_spec(&args[a], src) else {
            return Value::Error(ErrorKind::Value);
        };
        // Excel requires every criteria range to be the same shape; a
        // mismatch is #VALUE!, never a silently truncated scan.
        if let Some(first) = specs.first() {
            if first.rows != spec.rows || first.cols != spec.cols {
                return Value::Error(ErrorKind::Value);
            }
        }
        specs.push(spec);
        match criterion_of(&args[a + 1], src) {
            Ok(c) => crits.push(c),
            Err(e) => return Value::Error(e),
        }
    }

    let shape = specs[0];
    if let Some(v) = values {
        // The value range is resized, but it must at least be able to cover
        // the criteria shape's area; Excel rejects an outright mismatch.
        if v.cells() < shape.cells() && v.rows != shape.rows {
            return Value::Error(ErrorKind::Value);
        }
    }

    let mut t = Tally::default();
    for dc in 0..shape.cols {
        'row: for dr in 0..shape.rows {
            for (spec, crit) in specs.iter().zip(crits.iter()) {
                if !crit.matches(scalar_of(spec_get(spec, src, dr, dc), src)) {
                    continue 'row;
                }
            }
            match values {
                None => t.matched += 1,
                Some(v) => match spec_get(&v, src, dr, dc) {
                    Value::Error(e) => return Value::Error(e),
                    Value::Number(n) => {
                        t.sum += n;
                        t.matched += 1;
                    }
                    _ => {}
                },
            }
        }
    }
    finish(name, t)
}

/// Columnar sum over every sheet of a 3-D run, accumulated into `acc`.
///
/// Delegates to each sheet's own `sum_rect_in`, so the fast path that makes
/// `SUM(Sheet2!A1:A200000000)` a slice walk fires once per sheet instead of
/// being lost the moment a formula becomes 3-D.
fn sum_3d<S: CellSource + ?Sized>(
    first: &str,
    last: &str,
    start: CellRef,
    end: CellRef,
    src: &S,
    acc: &mut f64,
) -> Result<(), ErrorKind> {
    let names = src.sheet_span(first, last);
    if names.is_empty() {
        return Err(ErrorKind::Ref);
    }
    for name in &names {
        match src.sum_rect_in(name, start, end) {
            Some(v) => *acc += v,
            None => return Err(ErrorKind::Ref),
        }
    }
    Ok(())
}

/// Columnar numeric count over every sheet of a 3-D run.
fn count_3d<S: CellSource + ?Sized>(
    first: &str,
    last: &str,
    start: CellRef,
    end: CellRef,
    src: &S,
    acc: &mut usize,
) -> Result<(), ErrorKind> {
    let names = src.sheet_span(first, last);
    if names.is_empty() {
        return Err(ErrorKind::Ref);
    }
    for name in &names {
        match src.count_rect_in(name, start, end) {
            Some(n) => *acc += n,
            None => return Err(ErrorKind::Ref),
        }
    }
    Ok(())
}

/// Propagate the first error found inside a range argument.
///
/// Scanning a whole range would defeat the columnar fast path on huge ranges,
/// so we bound the scan: errors only ever come from formula cells, which are
/// rare and clustered near the top of a sheet in practice.
fn arg_error<S: CellSource + ?Sized>(arg: &Expr, src: &S) -> Option<ErrorKind> {
    const MAX_SCAN: usize = 100_000;
    match arg {
        Expr::Range(start, end) => {
            let r1 = (end.row as usize + 1).min(src.row_count().max(1));
            let span = r1.saturating_sub(start.row as usize);
            if span > MAX_SCAN {
                return None;
            }
            for c in start.col..=end.col {
                for r in start.row as usize..r1 {
                    if let Value::Error(e) = src.get(CellRef::new(r as u32, c)) {
                        return Some(e);
                    }
                }
            }
            None
        }
        Expr::XRange(sheet, start, end) => {
            // An unresolvable sheet name IS the error — report it rather than
            // letting the aggregate quietly sum nothing.
            let Some(rows) = src.row_count_in(sheet) else {
                return Some(ErrorKind::Ref);
            };
            let r1 = (end.row as usize + 1).min(rows.max(1));
            let span = r1.saturating_sub(start.row as usize);
            if span > MAX_SCAN {
                return None;
            }
            for c in start.col..=end.col {
                for r in start.row as usize..r1 {
                    if let Value::Error(e) = src.get_in(sheet, CellRef::new(r as u32, c)) {
                        return Some(e);
                    }
                }
            }
            None
        }
        // An unresolvable RUN is #REF!, for the same reason an unresolvable
        // sheet name is: the alternative is an aggregate that quietly omits
        // the sheets it could not find and returns a plausible wrong number.
        Expr::X3D(first, last, start, end) => {
            let mut found = None;
            let walked = for_each_3d(first, last, *start, *end, src, |spec| {
                if spec.rows as usize > MAX_SCAN {
                    return Ok(());
                }
                for dc in 0..spec.cols {
                    for dr in 0..spec.rows {
                        if let Value::Error(e) = spec_get(spec, src, dr, dc) {
                            found = Some(e);
                            return Ok(());
                        }
                    }
                }
                Ok(())
            });
            match walked {
                Err(e) => Some(e),
                Ok(()) => found,
            }
        }
        _ => None,
    }
}

fn eval_math<S: CellSource + ?Sized>(name: &str, args: &[Expr], src: &S) -> Value {
    let arg_n =
        |i: usize| -> Option<f64> { args.get(i).and_then(|a| eval_view(a, src).as_number()) };
    if args.is_empty() {
        return Value::Error(ErrorKind::Value);
    }
    if let Some(a0) = args.first() {
        let v = eval_view(a0, src);
        if let Some(e) = v.error() {
            return Value::Error(e);
        }
    }
    let x = match arg_n(0) {
        Some(x) => x,
        None => return Value::Error(ErrorKind::Value),
    };
    let r = match name {
        "ABS" => x.abs(),
        "SQRT" => {
            if x < 0.0 {
                return Value::Error(ErrorKind::Num);
            }
            x.sqrt()
        }
        "INT" => x.floor(),
        "EXP" => x.exp(),
        "LN" => {
            if x <= 0.0 {
                return Value::Error(ErrorKind::Num);
            }
            x.ln()
        }
        "LOG10" => {
            if x <= 0.0 {
                return Value::Error(ErrorKind::Num);
            }
            x.log10()
        }
        "ROUND" => {
            let digits = arg_n(1).unwrap_or(0.0);
            let f = 10f64.powf(digits);
            (x * f).round() / f
        }
        "FLOOR" => {
            let step = arg_n(1).unwrap_or(1.0);
            if step == 0.0 {
                return Value::Error(ErrorKind::DivZero);
            }
            (x / step).floor() * step
        }
        "CEILING" => {
            let step = arg_n(1).unwrap_or(1.0);
            if step == 0.0 {
                return Value::Error(ErrorKind::DivZero);
            }
            (x / step).ceil() * step
        }
        _ => return Value::Error(ErrorKind::Name),
    };
    Value::Number(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use ferrix_core::CellRef;

    fn sheet_with_numbers() -> Sheet {
        let mut s = Sheet::new("t");
        // A1:A5 = 1..5, B1:B5 = 10,20,30,40,50
        for r in 0..5u32 {
            s.set(CellRef::new(r, 0), Value::Number((r + 1) as f64));
            s.set(CellRef::new(r, 1), Value::Number(((r + 1) * 10) as f64));
        }
        s
    }

    fn ev(formula: &str, sheet: &Sheet) -> Value {
        eval(&parse(formula).unwrap(), sheet)
    }

    fn num(formula: &str, sheet: &Sheet) -> f64 {
        match ev(formula, sheet) {
            Value::Number(n) => n,
            other => panic!("{formula} produced {other:?}, expected a number"),
        }
    }

    #[test]
    fn arithmetic() {
        let s = Sheet::new("t");
        assert_eq!(num("=1+2", &s), 3.0);
        assert_eq!(num("=10-4", &s), 6.0);
        assert_eq!(num("=6*7", &s), 42.0);
        assert_eq!(num("=10/4", &s), 2.5);
        assert_eq!(num("=2^10", &s), 1024.0);
        assert_eq!(num("=-5+3", &s), -2.0);
        assert_eq!(num("=50%", &s), 0.5);
    }

    #[test]
    fn precedence_is_respected_at_runtime() {
        let s = Sheet::new("t");
        assert_eq!(num("=1+2*3", &s), 7.0);
        assert_eq!(num("=(1+2)*3", &s), 9.0);
        assert_eq!(num("=2^3^2", &s), 512.0);
        assert_eq!(num("=10-3-2", &s), 5.0);
    }

    #[test]
    fn division_by_zero() {
        let s = Sheet::new("t");
        assert_eq!(ev("=1/0", &s), Value::Error(ErrorKind::DivZero));
    }

    #[test]
    fn cell_references() {
        let s = sheet_with_numbers();
        assert_eq!(num("=A1", &s), 1.0);
        assert_eq!(num("=A5", &s), 5.0);
        assert_eq!(num("=A1+B1", &s), 11.0);
        // Reading past the populated area is Empty -> 0.
        assert_eq!(num("=Z100+1", &s), 1.0);
    }

    #[test]
    fn sum_over_range() {
        let s = sheet_with_numbers();
        assert_eq!(num("=SUM(A1:A5)", &s), 15.0);
        assert_eq!(num("=SUM(B1:B5)", &s), 150.0);
        assert_eq!(num("=SUM(A1:B5)", &s), 165.0);
        assert_eq!(num("=SUM(A1:A5,B1:B5)", &s), 165.0);
        assert_eq!(num("=SUM(A1:A5,100)", &s), 115.0);
    }

    #[test]
    fn aggregate_functions() {
        let s = sheet_with_numbers();
        assert_eq!(num("=COUNT(A1:A5)", &s), 5.0);
        assert_eq!(num("=AVERAGE(A1:A5)", &s), 3.0);
        assert_eq!(num("=MIN(A1:A5)", &s), 1.0);
        assert_eq!(num("=MAX(A1:A5)", &s), 5.0);
        assert_eq!(num("=MAX(A1:B5)", &s), 50.0);
    }

    #[test]
    fn aggregates_ignore_empty_cells() {
        let mut s = Sheet::new("t");
        s.set(CellRef::new(0, 0), Value::Number(10.0));
        s.set(CellRef::new(4, 0), Value::Number(20.0));
        // A2:A4 are empty and must not count toward the average.
        assert_eq!(num("=SUM(A1:A5)", &s), 30.0);
        assert_eq!(num("=COUNT(A1:A5)", &s), 2.0);
        assert_eq!(num("=AVERAGE(A1:A5)", &s), 15.0);
    }

    #[test]
    fn average_of_nothing_is_div_zero() {
        let s = Sheet::new("t");
        assert_eq!(ev("=AVERAGE(A1:A5)", &s), Value::Error(ErrorKind::DivZero));
    }

    #[test]
    fn errors_propagate_through_arithmetic() {
        let mut s = Sheet::new("t");
        s.set(CellRef::new(0, 0), Value::Error(ErrorKind::DivZero));
        assert_eq!(ev("=A1+1", &s), Value::Error(ErrorKind::DivZero));
        assert_eq!(ev("=SUM(A1:A3)", &s), Value::Error(ErrorKind::DivZero));
    }

    #[test]
    // 3.14159 is a ROUND() input, not an attempt to spell PI.
    #[allow(clippy::approx_constant)]
    fn math_functions() {
        let s = Sheet::new("t");
        assert_eq!(num("=ABS(-5)", &s), 5.0);
        assert_eq!(num("=SQRT(16)", &s), 4.0);
        assert_eq!(num("=INT(3.9)", &s), 3.0);
        assert_eq!(num("=ROUND(3.14159,2)", &s), 3.14);
        assert_eq!(num("=ROUND(2.5,0)", &s), 3.0);
        assert_eq!(num("=FLOOR(7,3)", &s), 6.0);
        assert_eq!(num("=CEILING(7,3)", &s), 9.0);
        assert_eq!(num("=LOG10(1000)", &s), 3.0);
    }

    #[test]
    fn math_domain_errors() {
        let s = Sheet::new("t");
        assert_eq!(ev("=SQRT(-1)", &s), Value::Error(ErrorKind::Num));
        assert_eq!(ev("=LN(0)", &s), Value::Error(ErrorKind::Num));
        assert_eq!(ev("=LOG10(-5)", &s), Value::Error(ErrorKind::Num));
    }

    #[test]
    fn comparisons() {
        let s = sheet_with_numbers();
        assert_eq!(ev("=1<2", &s), Value::Bool(true));
        assert_eq!(ev("=2<=2", &s), Value::Bool(true));
        assert_eq!(ev("=3<>3", &s), Value::Bool(false));
        assert_eq!(ev("=A1=1", &s), Value::Bool(true));
        assert_eq!(ev("=A5>A1", &s), Value::Bool(true));
    }

    #[test]
    fn conditional_logic() {
        let s = sheet_with_numbers();
        assert_eq!(num("=IF(A1=1,100,200)", &s), 100.0);
        assert_eq!(num("=IF(A1=2,100,200)", &s), 200.0);
        assert_eq!(num("=IF(SUM(A1:A5)>10,1,0)", &s), 1.0);
        assert_eq!(ev("=AND(1=1,2=2)", &s), Value::Bool(true));
        assert_eq!(ev("=AND(1=1,2=3)", &s), Value::Bool(false));
        assert_eq!(ev("=OR(1=2,2=2)", &s), Value::Bool(true));
        assert_eq!(ev("=NOT(1=1)", &s), Value::Bool(false));
    }

    #[test]
    fn unknown_function_is_name_error() {
        let s = Sheet::new("t");
        assert_eq!(ev("=BOGUS(1)", &s), Value::Error(ErrorKind::Name));
    }

    #[test]
    fn nested_expressions() {
        let s = sheet_with_numbers();
        assert_eq!(num("=SUM(A1:A5)+MAX(B1:B5)", &s), 65.0);
        assert_eq!(num("=IF(AVERAGE(A1:A5)>2,SUM(B1:B5),0)", &s), 150.0);
        assert_eq!(num("=ABS(MIN(A1:A5)-MAX(A1:A5))", &s), 4.0);
    }

    // --- conditional aggregation -----------------------------------------

    /// Fruit / region / amount, with deliberate awkwardness:
    /// blanks, a text cell in the sum range, and mixed case.
    ///
    /// A=fruit, B=region, C=amount
    fn sales_sheet() -> Sheet {
        let rows: &[(&str, &str, Option<f64>)] = &[
            ("apple", "North", Some(100.0)),
            ("Apple", "South", Some(50.0)),
            ("apricot", "North", Some(7.0)),
            ("banana", "North", Some(300.0)),
            ("BANANA", "South", Some(25.0)),
            ("cherry", "North", None),
            ("", "South", Some(1.0)),
        ];
        let mut s = Sheet::new("sales");
        for (r, (fruit, region, amt)) in rows.iter().enumerate() {
            let r = r as u32;
            if !fruit.is_empty() {
                s.set_text(CellRef::new(r, 0), fruit);
            }
            s.set_text(CellRef::new(r, 1), region);
            if let Some(a) = amt {
                s.set(CellRef::new(r, 2), Value::Number(*a));
            }
        }
        s
    }

    #[test]
    fn countif_bare_and_comparison_criteria() {
        let s = sales_sheet();
        // Case-insensitive equality: apple + Apple.
        assert_eq!(num(r#"=COUNTIF(A1:A7,"apple")"#, &s), 2.0);
        assert_eq!(num(r#"=COUNTIF(B1:B7,"North")"#, &s), 4.0);
        assert_eq!(num(r#"=COUNTIF(C1:C7,">50")"#, &s), 2.0);
        assert_eq!(num(r#"=COUNTIF(C1:C7,"<=50")"#, &s), 4.0);
        assert_eq!(num(r#"=COUNTIF(C1:C7,"<>100")"#, &s), 6.0);
        // A numeric literal criterion agrees with its string spelling.
        assert_eq!(num("=COUNTIF(C1:C7,100)", &s), 1.0);
        assert_eq!(num(r#"=COUNTIF(C1:C7,"100")"#, &s), 1.0);
    }

    #[test]
    fn countif_wildcards() {
        let s = sales_sheet();
        assert_eq!(num(r#"=COUNTIF(A1:A7,"a*")"#, &s), 3.0);
        assert_eq!(num(r#"=COUNTIF(A1:A7,"*an*")"#, &s), 2.0);
        assert_eq!(num(r#"=COUNTIF(A1:A7,"?pple")"#, &s), 2.0);
        assert_eq!(num(r#"=COUNTIF(A1:A7,"*")"#, &s), 6.0);
        // blank A7 is genuinely "not a*", so it counts — matching Excel.
        assert_eq!(num(r#"=COUNTIF(A1:A7,"<>a*")"#, &s), 4.0);
    }

    #[test]
    fn countif_blank_semantics() {
        let s = sales_sheet();
        // A6 is text "cherry" but C6 is blank; A7 is blank.
        assert_eq!(num(r#"=COUNTIF(A1:A7,"")"#, &s), 1.0);
        assert_eq!(num(r#"=COUNTIF(A1:A7,"<>")"#, &s), 6.0);
        assert_eq!(num(r#"=COUNTIF(C1:C7,"")"#, &s), 1.0);
    }

    #[test]
    fn sumif_with_and_without_sum_range() {
        let s = sales_sheet();
        assert_eq!(num(r#"=SUMIF(A1:A7,"apple",C1:C7)"#, &s), 150.0);
        assert_eq!(num(r#"=SUMIF(B1:B7,"North",C1:C7)"#, &s), 407.0);
        // No sum_range: sum the criteria range itself.
        assert_eq!(num(r#"=SUMIF(C1:C7,">50")"#, &s), 400.0);
        // Wildcard through to a sum.
        assert_eq!(num(r#"=SUMIF(A1:A7,"b*",C1:C7)"#, &s), 325.0);
    }

    #[test]
    fn sumif_skips_blanks_in_sum_range() {
        let s = sales_sheet();
        // cherry matches but C6 is blank; contributes nothing, not zero-bias.
        assert_eq!(num(r#"=SUMIF(A1:A7,"cherry",C1:C7)"#, &s), 0.0);
        assert_eq!(ev(r#"=AVERAGEIF(A1:A7,"cherry",C1:C7)"#, &s), div0());
    }

    #[test]
    fn averageif_ignores_non_numeric_and_reports_no_match() {
        let s = sales_sheet();
        assert_eq!(num(r#"=AVERAGEIF(A1:A7,"apple",C1:C7)"#, &s), 75.0);
        assert_eq!(num(r#"=AVERAGEIF(B1:B7,"South",C1:C7)"#, &s), 76.0 / 3.0);
        // No row matches at all -> #DIV/0!, matching Excel.
        assert_eq!(ev(r#"=AVERAGEIF(A1:A7,"durian",C1:C7)"#, &s), div0());
    }

    #[test]
    fn multi_criteria_family() {
        let s = sales_sheet();
        assert_eq!(num(r#"=COUNTIFS(B1:B7,"North",C1:C7,">50")"#, &s), 2.0);
        assert_eq!(num(r#"=SUMIFS(C1:C7,B1:B7,"North",A1:A7,"b*")"#, &s), 300.0);
        assert_eq!(
            num(r#"=AVERAGEIFS(C1:C7,B1:B7,"North",C1:C7,">50")"#, &s),
            200.0
        );
        // A single criteria pair is legal and must agree with the *IF form.
        assert_eq!(
            num(r#"=COUNTIFS(A1:A7,"apple")"#, &s),
            num(r#"=COUNTIF(A1:A7,"apple")"#, &s)
        );
        assert_eq!(
            num(r#"=SUMIFS(C1:C7,B1:B7,"North")"#, &s),
            num(r#"=SUMIF(B1:B7,"North",C1:C7)"#, &s)
        );
    }

    #[test]
    fn multi_criteria_intersect_not_union() {
        let s = sales_sheet();
        // Criteria AND together: no row is both South and >200.
        assert_eq!(num(r#"=COUNTIFS(B1:B7,"South",C1:C7,">200")"#, &s), 0.0);
        assert_eq!(num(r#"=SUMIFS(C1:C7,B1:B7,"South",C1:C7,">200")"#, &s), 0.0);
        assert_eq!(
            ev(r#"=AVERAGEIFS(C1:C7,B1:B7,"South",C1:C7,">200")"#, &s),
            div0()
        );
    }

    #[test]
    fn ifs_arity_and_shape_errors() {
        let s = sales_sheet();
        // Odd number of criteria args.
        assert_eq!(ev(r#"=COUNTIFS(B1:B7,"North",C1:C7)"#, &s), val_err());
        assert_eq!(ev(r#"=SUMIFS(C1:C7,B1:B7)"#, &s), val_err());
        // Mismatched criteria-range shapes are #VALUE!, not a short scan.
        assert_eq!(ev(r#"=COUNTIFS(B1:B7,"North",C1:C3,">0")"#, &s), val_err());
        // A non-range first argument cannot be scanned.
        assert_eq!(ev(r#"=COUNTIF(5,"North")"#, &s), val_err());
    }

    #[test]
    fn errors_in_criteria_range_do_not_match_but_do_not_abort() {
        let mut s = sales_sheet();
        s.set(CellRef::new(2, 0), Value::Error(ErrorKind::Num));
        // The #NUM! cell simply fails every criterion.
        assert_eq!(num(r#"=COUNTIF(A1:A7,"a*")"#, &s), 2.0);
        // 5 surviving texts + the blank; the #NUM! is excluded.
        assert_eq!(num(r#"=COUNTIF(A1:A7,"<>zzz")"#, &s), 6.0);
    }

    #[test]
    fn error_in_summed_cell_propagates() {
        let mut s = sales_sheet();
        s.set(CellRef::new(0, 2), Value::Error(ErrorKind::DivZero));
        assert_eq!(ev(r#"=SUMIF(A1:A7,"apple",C1:C7)"#, &s), div0());
        assert_eq!(ev(r#"=SUMIFS(C1:C7,A1:A7,"apple")"#, &s), div0());
        // But a row that never matches cannot poison the result.
        // banana + BANANA: equality is case-insensitive.
        assert_eq!(num(r#"=SUMIF(A1:A7,"banana",C1:C7)"#, &s), 325.0);
    }

    #[test]
    fn error_in_criteria_argument_propagates() {
        let s = sales_sheet();
        assert_eq!(ev(r#"=COUNTIF(A1:A7,1/0)"#, &s), div0());
    }

    // --- error handling ---------------------------------------------------

    fn div0() -> Value {
        Value::Error(ErrorKind::DivZero)
    }
    fn val_err() -> Value {
        Value::Error(ErrorKind::Value)
    }
    fn na() -> Value {
        Value::Error(ErrorKind::NotAvailable)
    }

    #[test]
    fn iferror_catches_everything_and_passes_values_through() {
        let s = sheet_with_numbers();
        assert_eq!(num("=IFERROR(1/0,-1)", &s), -1.0);
        assert_eq!(num("=IFERROR(SQRT(-1),-1)", &s), -1.0);
        assert_eq!(num("=IFERROR(NA(),-1)", &s), -1.0);
        assert_eq!(num("=IFERROR(BOGUS(1),-1)", &s), -1.0);
        assert_eq!(num("=IFERROR(A1,-1)", &s), 1.0);
        assert_eq!(ev("=IFERROR(1=1,-1)", &s), Value::Bool(true));
        // The fallback is only evaluated on the error path, and its own
        // errors are not caught a second time.
        assert_eq!(ev("=IFERROR(1/0,1/0)", &s), div0());
        assert_eq!(ev("=IFERROR(1)", &s), val_err());
    }

    #[test]
    fn ifna_catches_only_na() {
        let s = sheet_with_numbers();
        assert_eq!(num("=IFNA(NA(),42)", &s), 42.0);
        // #DIV/0! is NOT #N/A and must survive IFNA untouched. This is the
        // whole reason IFNA exists alongside IFERROR.
        assert_eq!(ev("=IFNA(1/0,42)", &s), div0());
        assert_eq!(ev("=IFNA(SQRT(-1),42)", &s), Value::Error(ErrorKind::Num));
        assert_eq!(num("=IFNA(A1,42)", &s), 1.0);
    }

    #[test]
    fn na_function() {
        let s = Sheet::new("t");
        assert_eq!(ev("=NA()", &s), na());
        assert_eq!(ev("=NA(1)", &s), val_err());
        // NA() propagates like any other error.
        assert_eq!(ev("=NA()+1", &s), na());
    }

    #[test]
    fn is_predicates_over_cells() {
        let mut s = Sheet::new("t");
        s.set(CellRef::new(0, 0), Value::Number(1.0));
        s.set_text(CellRef::new(1, 0), "hello");
        s.set(CellRef::new(2, 0), Value::Bool(true));
        s.set(CellRef::new(3, 0), Value::Error(ErrorKind::DivZero));
        s.set(CellRef::new(4, 0), Value::Error(ErrorKind::NotAvailable));
        // A5 stays empty.

        let t = Value::Bool(true);
        let f = Value::Bool(false);

        assert_eq!(ev("=ISNUMBER(A1)", &s), t);
        assert_eq!(ev("=ISNUMBER(A2)", &s), f);
        assert_eq!(ev("=ISNUMBER(A3)", &s), f, "a bool is not a number");
        assert_eq!(ev("=ISNUMBER(A6)", &s), f, "a blank is not a number");

        assert_eq!(ev("=ISTEXT(A2)", &s), t);
        assert_eq!(ev("=ISTEXT(A1)", &s), f);
        assert_eq!(ev("=ISTEXT(A6)", &s), f);

        assert_eq!(ev("=ISBLANK(A6)", &s), t);
        assert_eq!(ev("=ISBLANK(A1)", &s), f);
        assert_eq!(ev("=ISBLANK(A4)", &s), f, "an error cell is not blank");
    }

    #[test]
    fn iserror_iserr_isna_split_hairs() {
        let mut s = Sheet::new("t");
        s.set(CellRef::new(0, 0), Value::Error(ErrorKind::DivZero));
        s.set(CellRef::new(1, 0), Value::Error(ErrorKind::NotAvailable));
        s.set(CellRef::new(2, 0), Value::Number(5.0));
        let t = Value::Bool(true);
        let f = Value::Bool(false);

        // ISERROR: any error, including #N/A.
        assert_eq!(ev("=ISERROR(A1)", &s), t);
        assert_eq!(ev("=ISERROR(A2)", &s), t);
        assert_eq!(ev("=ISERROR(A3)", &s), f);
        // ISERR: any error EXCEPT #N/A. This is the distinction Excel draws.
        assert_eq!(ev("=ISERR(A1)", &s), t);
        assert_eq!(ev("=ISERR(A2)", &s), f);
        // ISNA: #N/A only.
        assert_eq!(ev("=ISNA(A2)", &s), t);
        assert_eq!(ev("=ISNA(A1)", &s), f);
        assert_eq!(ev("=ISNA(A3)", &s), f);
        // IS* inspect rather than propagate: the error is data, not a fault.
        assert_eq!(ev("=ISERROR(1/0)", &s), t);
        assert_eq!(ev("=ISNA(NA())", &s), t);
        assert_eq!(ev("=ISERROR(BOGUS())", &s), t);
    }

    #[test]
    fn is_predicates_reject_wrong_arity() {
        let s = Sheet::new("t");
        assert_eq!(ev("=ISNUMBER()", &s), val_err());
        assert_eq!(ev("=ISNUMBER(1,2)", &s), val_err());
    }

    #[test]
    fn error_type_numbering_matches_excel() {
        let s = Sheet::new("t");
        assert_eq!(num("=ERROR.TYPE(1/0)", &s), 2.0);
        assert_eq!(num("=ERROR.TYPE(SQRT(-1))", &s), 6.0);
        assert_eq!(num("=ERROR.TYPE(NA())", &s), 7.0);
        assert_eq!(num("=ERROR.TYPE(BOGUS())", &s), 5.0);
        // A non-error argument is itself #N/A, which is Excel's answer.
        assert_eq!(ev("=ERROR.TYPE(1)", &s), na());
    }

    #[test]
    fn error_handling_composes_with_aggregates() {
        let mut s = sales_sheet();
        s.set(CellRef::new(0, 2), Value::Error(ErrorKind::DivZero));
        // The canonical spreadsheet idiom: wrap a fragile aggregate.
        assert_eq!(num(r#"=IFERROR(SUMIF(A1:A7,"apple",C1:C7),0)"#, &s), 0.0);
        assert_eq!(
            ev(r#"=ISERROR(SUMIF(A1:A7,"apple",C1:C7))"#, &s),
            Value::Bool(true)
        );
        assert_eq!(
            num(r#"=IFERROR(AVERAGEIF(A1:A7,"durian",C1:C7),-1)"#, &s),
            -1.0
        );
    }

    #[test]
    fn large_range_uses_columnar_path() {
        // 100k cells: must be fast and exact.
        let mut s = Sheet::new("big");
        let n = 100_000u32;
        for r in 0..n {
            s.set(CellRef::new(r, 0), Value::Number(1.0));
        }
        let t = std::time::Instant::now();
        let got = num("=SUM(A1:A100000)", &s);
        let ms = t.elapsed().as_millis();
        assert_eq!(got, 100_000.0);
        assert!(
            ms < 100,
            "SUM over 100k cells took {ms}ms; columnar path may be broken"
        );
    }
}
