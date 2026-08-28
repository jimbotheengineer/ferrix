//! Formula evaluation.
//!
//! Range aggregations dispatch to the columnar fast paths in `ferrix-core`
//! rather than iterating cell-by-cell, so `SUM(A1:A10000000)` is a typed
//! slice walk instead of ten million enum matches.
//!
//! Evaluation is generic over [`CellSource`] so the same code runs against a
//! plain `Sheet`, a base+overlay composite, or (later) a memory-mapped file.

use ferrix_core::{CellRef, ErrorKind, Sheet, Value};

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
        _ => Value::Error(ErrorKind::Name),
    }
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
