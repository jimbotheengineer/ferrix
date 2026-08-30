//! `LAMBDA` and `LET` — the last big language feature for Excel parity (#27
//! follow-up).
//!
//! These pin the whole feature end to end: the parser recognises LET/LAMBDA as
//! special forms (their leading arguments are NAMES, not values), the evaluator
//! resolves lexical variables through a scope stack, LAMBDA captures its
//! defining environment (closures), and — the composition acceptance criterion
//! — an array-producing LAMBDA/LET body carries its shape across the seam so it
//! spills exactly like any other dynamic array.
//!
//! ## What each group would catch if it broke
//!
//! - *Parsing*: if LET/LAMBDA went through the ordinary call path, a bound name
//!   like `x` would be resolved as a workbook name and fail with `#NAME?`; these
//!   assert the special-form path builds the right tree.
//! - *Scope*: shadowing, later-binding-sees-earlier, and closure capture are
//!   the three ways a naive substitution or a single flat environment gets the
//!   wrong answer.
//! - *Spill*: a LAMBDA body that returns `SEQUENCE(3)` must stay an `Array`
//!   through the scope frames, not collapse to its top-left on the way out.
//! - *Errors*: arity mismatch, calling a non-lambda, and an unbound name are
//!   the three failure modes a real sheet hits, and each has a specific error.

use ferrix_core::{CellRef, ErrorKind, Sheet, Value};
use ferrix_formula::{eval, parse, ParseError};
use ferrix_formula::{ArrayData, EvalResult};

// --- helpers ---------------------------------------------------------------

/// A1:A5 = 10,20,30,40,50; B1 = "widget".
fn fixture() -> Sheet {
    let mut s = Sheet::new("lambda");
    for (i, n) in [10.0, 20.0, 30.0, 40.0, 50.0].iter().enumerate() {
        s.set(CellRef::new(i as u32, 0), Value::Number(*n));
    }
    s.set_text(CellRef::new(0, 1), "widget");
    s
}

fn val(sheet: &Sheet, f: &str) -> Value {
    eval(
        &parse(f).unwrap_or_else(|e| panic!("parse {f}: {e}")),
        sheet,
    )
}

fn num(sheet: &Sheet, f: &str) -> f64 {
    match val(sheet, f) {
        Value::Number(n) => n,
        other => panic!("{f} = {other:?}, wanted a number"),
    }
}

fn err(sheet: &Sheet, f: &str) -> ErrorKind {
    match val(sheet, f) {
        Value::Error(e) => e,
        other => panic!("{f} = {other:?}, wanted an error"),
    }
}

/// Evaluate in array context and require an array result (for spill tests).
fn array(sheet: &Sheet, f: &str) -> ArrayData {
    let expr = parse(f).unwrap_or_else(|e| panic!("parse {f}: {e}"));
    match ferrix_formula::eval::eval_view_array(&expr, sheet) {
        EvalResult::Array(a) => a,
        EvalResult::Scalar(v) => panic!("{f} = Scalar({v:?}), wanted an array"),
    }
}

// --- LET: bindings and body ------------------------------------------------

#[test]
fn let_binds_a_single_name() {
    let s = fixture();
    assert_eq!(num(&s, "=LET(x, 5, x + 1)"), 6.0);
}

#[test]
fn let_binding_is_used_more_than_once() {
    let s = fixture();
    // x is referenced twice; a substitution bug that dropped a use would give 5.
    assert_eq!(num(&s, "=LET(x, 5, x + x)"), 10.0);
}

#[test]
fn let_later_binding_sees_earlier_one() {
    let s = fixture();
    // y is defined in terms of x — top-to-bottom LET semantics.
    assert_eq!(num(&s, "=LET(x, 2, y, x * 3, y + 1)"), 7.0);
}

#[test]
fn let_body_reads_a_cell() {
    let s = fixture();
    // A1 = 10; the binding adds to a real sheet read, proving the scope wrapper
    // delegates cell reads to the underlying source untouched.
    assert_eq!(num(&s, "=LET(bump, 5, A1 + bump)"), 15.0);
}

#[test]
fn let_binds_a_range_aggregate() {
    let s = fixture();
    // SUM(A1:A5) = 150, bound and reused.
    assert_eq!(num(&s, "=LET(total, SUM(A1:A5), total / 5)"), 30.0);
}

#[test]
fn nested_let_shadows_outer_binding() {
    let s = fixture();
    // Inner x = 100 shadows outer x = 1 for the inner body; the outer x is
    // restored for the trailing `+ x`.
    assert_eq!(num(&s, "=LET(x, 1, LET(x, 100, x) + x)"), 101.0);
}

#[test]
fn let_case_insensitive_name() {
    let s = fixture();
    // Names are case-insensitive, like Excel and the tokenizer's upper-casing.
    assert_eq!(num(&s, "=LET(Rate, 3, RATE * 2)"), 6.0);
}

// --- LAMBDA: definition and in-place invocation ----------------------------

#[test]
fn lambda_invoked_in_place() {
    let s = fixture();
    assert_eq!(num(&s, "=LAMBDA(x, x + 1)(5)"), 6.0);
}

#[test]
fn lambda_two_params() {
    let s = fixture();
    assert_eq!(num(&s, "=LAMBDA(a, b, a + b)(3, 4)"), 7.0);
}

#[test]
fn lambda_zero_params_is_a_thunk() {
    let s = fixture();
    // A no-parameter lambda is a thunk: LAMBDA(41+1)() = 42.
    assert_eq!(num(&s, "=LAMBDA(41 + 1)()"), 42.0);
}

#[test]
fn lambda_body_reads_a_cell() {
    let s = fixture();
    // A1 = 10; the lambda closes over nothing but reads a real cell.
    assert_eq!(num(&s, "=LAMBDA(k, A1 * k)(3)"), 30.0);
}

// --- LAMBDA + LET together: naming and passing closures ---------------------

#[test]
fn let_names_a_lambda_and_calls_it() {
    let s = fixture();
    assert_eq!(num(&s, "=LET(f, LAMBDA(x, x * x), f(6))"), 36.0);
}

#[test]
fn lambda_captures_an_enclosing_let_binding() {
    let s = fixture();
    // The closure captures `base` from the LET at definition; calling it later
    // adds the argument. A flat environment or a capture-by-name-at-call bug
    // would lose `base`.
    assert_eq!(
        num(&s, "=LET(base, 100, f, LAMBDA(x, base + x), f(5))"),
        105.0
    );
}

#[test]
fn lambda_argument_shadows_captured_name() {
    let s = fixture();
    // The parameter x shadows the captured x for the body.
    assert_eq!(num(&s, "=LET(x, 1, f, LAMBDA(x, x + 10), f(5))"), 15.0);
}

#[test]
fn lambda_passed_as_argument_to_another_lambda() {
    let s = fixture();
    // A higher-order call: `apply(g, v) = g(v)`, applied to a doubling lambda.
    // This is the shape MAP/REDUCE will use once they land.
    let f = "=LET(apply, LAMBDA(g, v, g(v)), dbl, LAMBDA(n, n * 2), apply(dbl, 21))";
    assert_eq!(num(&s, f), 42.0);
}

// --- Spill: an array-producing body composes with dynamic arrays -----------

#[test]
fn let_body_array_spills() {
    let s = fixture();
    // Binding a name to SEQUENCE and returning it keeps the array shape, so the
    // whole thing spills exactly like a bare =SEQUENCE(3).
    let a = array(&s, "=LET(seq, SEQUENCE(3), seq)");
    assert_eq!((a.rows(), a.cols()), (3, 1));
    assert_eq!(a.get(0, 0), Value::Number(1.0));
    assert_eq!(a.get(2, 0), Value::Number(3.0));
}

#[test]
fn lambda_body_array_spills() {
    let s = fixture();
    // A LAMBDA whose body produces an array spills when invoked.
    let a = array(&s, "=LAMBDA(n, SEQUENCE(n))(4)");
    assert_eq!((a.rows(), a.cols()), (4, 1));
    assert_eq!(a.get(3, 0), Value::Number(4.0));
}

#[test]
fn let_over_a_range_reference_spills() {
    let s = fixture();
    // A1:A5 materialises as a 5x1 array in array context; naming it in a LET
    // and returning it must keep that shape.
    let a = array(&s, "=LET(col, A1:A5, col)");
    assert_eq!((a.rows(), a.cols()), (5, 1));
    assert_eq!(a.get(0, 0), Value::Number(10.0));
    assert_eq!(a.get(4, 0), Value::Number(50.0));
}

#[test]
fn let_scalar_body_stays_scalar() {
    let s = fixture();
    // The seam must NOT wrap every scalar in a 1x1 array — a scalar LET body is
    // a Scalar, paying no allocation.
    let expr = parse("=LET(x, 7, x)").unwrap();
    assert_eq!(
        ferrix_formula::eval::eval_view_array(&expr, &s),
        EvalResult::Scalar(Value::Number(7.0))
    );
}

// --- Errors ----------------------------------------------------------------

#[test]
fn lambda_arity_mismatch_is_value_error() {
    let s = fixture();
    assert_eq!(err(&s, "=LAMBDA(x, y, x + y)(1)"), ErrorKind::Value);
    assert_eq!(err(&s, "=LAMBDA(x, x)(1, 2)"), ErrorKind::Value);
}

#[test]
fn calling_a_non_lambda_binding_is_value_error() {
    let s = fixture();
    // `n` is bound to a number, not a lambda — invoking it is #VALUE!.
    assert_eq!(err(&s, "=LET(n, 5, n(1))"), ErrorKind::Value);
}

#[test]
fn a_bare_lambda_in_a_cell_is_value_error() {
    let s = fixture();
    // A function is not a data value; a cell can't hold one.
    assert_eq!(err(&s, "=LAMBDA(x, x + 1)"), ErrorKind::Value);
}

#[test]
fn unbound_name_inside_let_body_is_name_error() {
    // `z` is never bound and is not a workbook name → #NAME?. Parsing surfaces
    // the unknown name; the workbook renders that as #NAME?. A bare unknown name
    // fails to PARSE, so assert the parse error directly.
    assert!(matches!(
        parse("=LET(x, 1, z)"),
        Err(ParseError::UnknownName(_))
    ));
}

#[test]
fn let_out_of_scope_name_does_not_leak() {
    // A name bound in one LET is not visible in a sibling expression: the scope
    // is popped when the form closes. `x` here is unbound in the second operand.
    assert!(matches!(
        parse("=LET(x, 1, x) + x"),
        Err(ParseError::UnknownName(_))
    ));
}

#[test]
fn lambda_param_does_not_leak_past_body() {
    // A parameter is only in scope inside the lambda body.
    assert!(matches!(
        parse("=LAMBDA(x, x)(1) + x"),
        Err(ParseError::UnknownName(_))
    ));
}

#[test]
fn let_arity_must_be_odd_and_at_least_three() {
    // LET(x, 1) has no body; LET(x, 1, y, 2) has an even count (no body).
    assert!(parse("=LET(x, 1)").is_err());
    assert!(parse("=LET(x, 1, y, 2)").is_err());
}

// --- Parser shape ----------------------------------------------------------

#[test]
fn let_and_lambda_are_not_ordinary_calls() {
    use ferrix_formula::Expr;
    // The special forms build Let/Lambda nodes, not Call("LET", ...).
    assert!(matches!(parse("=LET(x, 1, x)").unwrap(), Expr::Let(..)));
    assert!(matches!(
        parse("=LAMBDA(x, x)(1)").unwrap(),
        Expr::Apply(..)
    ));
    assert!(matches!(
        parse("=LAMBDA(x, x + 1)").unwrap(),
        Expr::Lambda(..)
    ));
}

#[test]
fn ordinary_calls_are_unaffected() {
    let s = fixture();
    // A regression guard: adding LET/LAMBDA special-casing must not disturb the
    // ordinary builtin call path.
    assert_eq!(num(&s, "=SUM(A1:A5)"), 150.0);
    assert_eq!(num(&s, "=MAX(A1:A5)"), 50.0);
}
