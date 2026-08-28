//! Formula evaluation against a `Sheet`.
//!
//! Range aggregations dispatch to the columnar fast paths in `ferrix-core`
//! rather than iterating cell-by-cell, so `SUM(A1:A10000000)` is a typed
//! slice walk instead of ten million enum matches.

use ferrix_core::{ErrorKind, Sheet, Value};

use crate::parser::{BinOp, Expr, UnOp};

/// Evaluate a parsed expression against a sheet.
pub fn eval(expr: &Expr, sheet: &Sheet) -> Value {
    match expr {
        Expr::Number(n) => Value::Number(*n),
        Expr::Bool(b) => Value::Bool(*b),
        Expr::Text(_) => {
            // Literal text needs interning, which requires &mut Sheet. The
            // caller handles literal-only formulas; here we surface the value
            // through the error-free path by treating it as a name lookup.
            // See `eval_with_arena` for the interning variant.
            Value::Error(ErrorKind::Value)
        }
        Expr::Ref(cell) => sheet.get(*cell),
        Expr::Range(_, _) => Value::Error(ErrorKind::Value), // ranges only valid inside functions
        Expr::Unary(op, inner) => {
            let v = eval(inner, sheet);
            if let Some(e) = v.error() {
                return Value::Error(e);
            }
            match (op, v.as_number()) {
                (UnOp::Neg, Some(n)) => Value::Number(-n),
                (UnOp::Percent, Some(n)) => Value::Number(n / 100.0),
                _ => Value::Error(ErrorKind::Value),
            }
        }
        Expr::Binary(op, lhs, rhs) => eval_binary(*op, lhs, rhs, sheet),
        Expr::Call(name, args) => eval_call(name, args, sheet),
    }
}

fn eval_binary(op: BinOp, lhs: &Expr, rhs: &Expr, sheet: &Sheet) -> Value {
    let a = eval(lhs, sheet);
    if let Some(e) = a.error() {
        return Value::Error(e);
    }
    let b = eval(rhs, sheet);
    if let Some(e) = b.error() {
        return Value::Error(e);
    }

    // Comparisons work on mixed types; arithmetic requires numbers.
    match op {
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
            let ord = compare(&a, &b, sheet);
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
        BinOp::Concat => {
            // Concatenation needs to intern; without &mut we can only compare
            // against existing strings. Report VALUE so the caller uses the
            // interning path.
            Value::Error(ErrorKind::Value)
        }
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
fn compare(a: &Value, b: &Value, sheet: &Sheet) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => {
            let sx = sheet.resolve(*x).to_ascii_lowercase();
            let sy = sheet.resolve(*y).to_ascii_lowercase();
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
/// Ranges use the columnar fast path and never materialize a Vec.
fn fold_numeric<F>(arg: &Expr, sheet: &Sheet, f: &mut F)
where
    F: FnMut(f64),
{
    match arg {
        Expr::Range(start, end) => {
            let (r0, r1) = (start.row as usize, end.row as usize + 1);
            for c in start.col..=end.col {
                if let Some(col) = sheet.column(c as usize) {
                    let hi = r1.min(col.len());
                    for r in r0..hi {
                        if let Value::Number(n) = col.get(r) {
                            f(n);
                        }
                    }
                }
            }
        }
        other => {
            if let Some(n) = eval(other, sheet).as_number() {
                f(n);
            }
        }
    }
}

fn eval_call(name: &str, args: &[Expr], sheet: &Sheet) -> Value {
    match name {
        "SUM" => {
            // Fast path: a lone range delegates straight to the column sum,
            // but only after confirming the range holds no error cells —
            // sum_rect skips non-numerics silently and would mask them.
            if let [Expr::Range(s, e)] = args {
                if let Some(err) = arg_error(&args[0], sheet) {
                    return Value::Error(err);
                }
                return Value::Number(sheet.sum_rect(*s, *e));
            }
            let mut acc = 0.0;
            for a in args {
                if let Some(err) = arg_error(a, sheet) {
                    return Value::Error(err);
                }
                fold_numeric(a, sheet, &mut |n| acc += n);
            }
            Value::Number(acc)
        }
        "COUNT" => {
            if let [Expr::Range(s, e)] = args {
                return Value::Number(sheet.count_rect(*s, *e) as f64);
            }
            let mut n = 0usize;
            for a in args {
                fold_numeric(a, sheet, &mut |_| n += 1);
            }
            Value::Number(n as f64)
        }
        "AVERAGE" => {
            let mut acc = 0.0;
            let mut n = 0usize;
            for a in args {
                if let Some(err) = arg_error(a, sheet) {
                    return Value::Error(err);
                }
                fold_numeric(a, sheet, &mut |v| {
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
                if let Some(err) = arg_error(a, sheet) {
                    return Value::Error(err);
                }
                fold_numeric(a, sheet, &mut |v| {
                    best = Some(match best {
                        None => v,
                        Some(b) if want_min => b.min(v),
                        Some(b) => b.max(v),
                    });
                });
            }
            // Excel returns 0 for MIN/MAX over an empty set.
            Value::Number(best.unwrap_or(0.0))
        }
        "ABS" | "SQRT" | "ROUND" | "FLOOR" | "CEILING" | "INT" | "LN" | "LOG10" | "EXP" => {
            eval_math(name, args, sheet)
        }
        "IF" => {
            if args.len() < 2 || args.len() > 3 {
                return Value::Error(ErrorKind::Value);
            }
            let cond = eval(&args[0], sheet);
            if let Some(e) = cond.error() {
                return Value::Error(e);
            }
            match cond.as_bool() {
                Some(true) => eval(&args[1], sheet),
                Some(false) => {
                    if args.len() == 3 {
                        eval(&args[2], sheet)
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
                let v = eval(a, sheet);
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
            match eval(&args[0], sheet).as_bool() {
                Some(b) => Value::Bool(!b),
                None => Value::Error(ErrorKind::Value),
            }
        }
        _ => Value::Error(ErrorKind::Name),
    }
}

/// Propagate the first error found inside a range argument.
fn arg_error(arg: &Expr, sheet: &Sheet) -> Option<ErrorKind> {
    match arg {
        Expr::Range(start, end) => {
            for c in start.col..=end.col {
                let col = sheet.column(c as usize)?;
                let hi = (end.row as usize + 1).min(col.len());
                for r in start.row as usize..hi {
                    if let Value::Error(e) = col.get(r) {
                        return Some(e);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn eval_math(name: &str, args: &[Expr], sheet: &Sheet) -> Value {
    let arg_n = |i: usize| -> Option<f64> { args.get(i).and_then(|a| eval(a, sheet).as_number()) };
    if args.is_empty() {
        return Value::Error(ErrorKind::Value);
    }
    if let Some(a0) = args.first() {
        let v = eval(a0, sheet);
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
