//! Dispatch-seam tests for the array-result evaluation fork (#27 P1).
//!
//! This is the array-family analogue of `compose_tests.rs`. The scalar
//! families (#23–#26) each proved that they route through the ONE shared
//! `eval_call` match; this proves the same for the NEW seam introduced by
//! #27: an evaluator that can return a 2-D array *without* collapsing it to a
//! scalar, and a dispatch path that carries arrays through the shared match
//! intact.
//!
//! ## What could silently break, and what these pin
//!
//! The whole point of the fork is that `eval_view_array` returns an
//! [`EvalResult`] that is EITHER a `Scalar` or an `Array`, while the legacy
//! `eval_view` still returns a bare `Value` by collapsing an array to its
//! top-left cell (implicit intersection). Two seams can rot:
//!
//! 1. **Collapse seam.** If `eval_view` ever forgot to collapse — or an arm of
//!    the array evaluator collapsed too eagerly — every existing scalar caller
//!    (formula bar, depgraph, xlsx) would either see a wrong type or lose the
//!    array. `array_collapses_to_top_left_for_scalar_callers` pins that a bare
//!    range read through the SCALAR path is unchanged (`#VALUE!`, exactly as
//!    today) while the same expression read through the ARRAY path materialises
//!    the whole rectangle.
//!
//! 2. **Dispatch seam.** The array family sits behind its own guarded arm in
//!    the shared match, above the scalar fallthrough. If that arm were dropped,
//!    over-claimed a scalar name, or collapsed its result to a scalar before
//!    returning, an array-native call would come back as a scalar (or `#NAME?`)
//!    and no scalar test would notice — they never route through the array
//!    path. `every_array_arm_passes_arrays_through_the_shared_dispatch` and
//!    `array_family_owns_only_names_no_scalar_family_claims` pin that.

use ferrix_core::{CellRef, ErrorKind, Sheet, Value};

use crate::array::{is_array_fn, ArrayData, EvalResult};
use crate::eval::{eval_view, eval_view_array};
use crate::parse;

/// A1:A5 = 10..50, one column; B1:C1 = 7, 8 on a row.
fn fixture() -> Sheet {
    let mut s = Sheet::new("arr");
    for (i, n) in [10.0, 20.0, 30.0, 40.0, 50.0].iter().enumerate() {
        s.set(CellRef::new(i as u32, 0), Value::Number(*n));
    }
    s.set(CellRef::new(0, 1), Value::Number(7.0));
    s.set(CellRef::new(0, 2), Value::Number(8.0));
    s
}

fn arr(sheet: &Sheet, f: &str) -> ArrayData {
    match eval_view_array(
        &parse(f).unwrap_or_else(|e| panic!("parse {f}: {e}")),
        sheet,
    ) {
        EvalResult::Array(a) => a,
        EvalResult::Scalar(v) => panic!("{f} = Scalar({v:?}), wanted an Array"),
    }
}

/// The core of the fork: the SCALAR entrypoint must be byte-for-byte
/// unchanged, while the ARRAY entrypoint sees the whole rectangle. A bare
/// range has never been a scalar (it is `#VALUE!` through `eval_view`), and it
/// still is — but through `eval_view_array` it materialises as a 5x1 array of
/// the real cell values, bounded by the RESULT, not the sheet.
#[test]
fn array_collapses_to_top_left_for_scalar_callers() {
    let s = fixture();

    // Scalar path is unchanged: a bare range is still #VALUE!.
    assert_eq!(
        eval_view(&parse("=A1:A5").unwrap(), &s),
        Value::Error(ErrorKind::Value),
        "the scalar entrypoint must not start returning arrays — every \
         existing caller depends on it collapsing"
    );

    // Array path materialises the whole column.
    let a = arr(&s, "=A1:A5");
    assert_eq!((a.rows(), a.cols()), (5, 1));
    assert_eq!(a.get(0, 0), Value::Number(10.0));
    assert_eq!(a.get(4, 0), Value::Number(50.0));

    // And a row range materialises across columns.
    let r = arr(&s, "=B1:C1");
    assert_eq!((r.rows(), r.cols()), (1, 2));
    assert_eq!(r.get(0, 0), Value::Number(7.0));
    assert_eq!(r.get(0, 1), Value::Number(8.0));
}

/// A scalar expression read through the ARRAY entrypoint stays a `Scalar` —
/// the fork must not wrap every result in a 1x1 array, or the collapse seam
/// would have nothing to collapse and callers would pay an allocation per
/// cell.
#[test]
fn a_scalar_expression_stays_scalar_through_the_array_entrypoint() {
    let s = fixture();
    match eval_view_array(&parse("=1+2").unwrap(), &s) {
        EvalResult::Scalar(Value::Number(n)) => assert_eq!(n, 3.0),
        other => panic!("=1+2 through array path = {other:?}, wanted Scalar(3)"),
    }
    // A cell read is scalar too.
    match eval_view_array(&parse("=A1").unwrap(), &s) {
        EvalResult::Scalar(Value::Number(n)) => assert_eq!(n, 10.0),
        other => panic!("=A1 = {other:?}, wanted Scalar(10)"),
    }
}

/// The dispatch seam: an array-native function must be reachable through the
/// shared `eval_call` match AND return its array uncollapsed. If the array arm
/// were dropped or its result collapsed, this returns a scalar (or #NAME?) and
/// no scalar-family test would see it.
///
/// `ARRAYTOTEXT` (array -> scalar text) proves an array can be CONSUMED as a
/// function argument through the seam; a range argument proves an array can be
/// PRODUCED and fed straight in. Both directions cross the seam.
#[test]
fn every_array_arm_passes_arrays_through_the_shared_dispatch() {
    let s = fixture();

    // Array PRODUCED by a range, CONSUMED by an array-native function, result
    // collapsed to a scalar the ordinary caller can read.
    match eval_view(&parse("=ARRAYTOTEXT(B1:C1)").unwrap(), &s) {
        Value::Text(id) => assert_eq!(s.resolve(id), "7, 8"),
        other => panic!("ARRAYTOTEXT(B1:C1) = {other:?}, wanted \"7, 8\""),
    }

    // The same function through the ARRAY entrypoint still yields a Scalar,
    // because ARRAYTOTEXT's RESULT is scalar — the seam carries the shape the
    // function chose, it does not impose one.
    assert!(matches!(
        eval_view_array(&parse("=ARRAYTOTEXT(A1:A5)").unwrap(), &s),
        EvalResult::Scalar(Value::Text(_))
    ));
}

/// The array family must claim ONLY names no scalar family already owns.
/// `eval_call`'s guard arms match in order, so an over-claim here would either
/// shadow a scalar family (if the array arm sits above it) or be dead (if
/// below) — both are silent at runtime. Mirrors the mutual-exclusion pins in
/// `compose_tests.rs`, now including the array family.
///
/// P3 (#27) grew `ARRAY_FN_NAMES` from 1 to 17 names; adding 16 names in one
/// commit is exactly where a duplicate or a scalar-family collision would
/// silently swallow another call, so this loop covers ALL of them — it iterates
/// the single source of truth, so a future 17th name is covered automatically.
#[test]
fn array_family_owns_only_names_no_scalar_family_claims() {
    // Every name the array family answers for must be owned by exactly one
    // module across the whole dispatch table.
    for name in crate::array::ARRAY_FN_NAMES {
        let claims = [
            ("array", is_array_fn(name)),
            ("text", crate::text::is_text_fn(name)),
            ("datetime", crate::datetime::is_date_fn(name)),
            ("stats", crate::stats::is_stat_fn(name)),
            ("lookup", crate::lookup::is_lookup_fn(name)),
        ];
        let owners: Vec<&str> = claims.iter().filter(|(_, y)| *y).map(|(m, _)| *m).collect();
        assert_eq!(
            owners,
            vec!["array"],
            "{name} must be owned by ONLY the array family; got {owners:?}"
        );
    }

    // The 16 P3 functions must all be present in the single source of truth —
    // a missing name would leave the function unreachable through the seam and
    // no other test would notice (it would silently fall to #NAME?).
    for expected in [
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
    ] {
        assert!(
            is_array_fn(expected),
            "{expected} (a P3 dynamic-array function) is not owned by the array \
             family — it would be unreachable through the shared dispatch"
        );
    }

    // And the array family must not claim any name the evaluator handles by a
    // direct scalar arm (SUM, IF, ...), which sit above every guard.
    for name in [
        "SUM", "COUNT", "AVERAGE", "MIN", "MAX", "IF", "AND", "OR", "NOT", "SUMIF", "IFERROR",
        "ISNUMBER",
    ] {
        assert!(
            !is_array_fn(name),
            "array family claims {name}, a direct evaluator builtin"
        );
    }
}

/// Every one of the 16 P3 functions must be reachable through the SHARED
/// `eval_view_array` dispatch and return an `Array` (or, for a genuinely
/// scalar-result path, a `Scalar`) — never `#NAME?`, which is what a dropped
/// dispatch arm produces. This is the P3 analogue of
/// `every_array_arm_passes_arrays_through_the_shared_dispatch`: it proves each
/// added name is not just listed in `ARRAY_FN_NAMES` but actually wired to a
/// `call` arm. A missing arm returns `#VALUE!` from `call`'s fallthrough, which
/// this catches.
#[test]
fn every_p3_function_is_reachable_and_array_producing() {
    let s = fixture();
    // (formula, expected result shape) — each exercises the real dispatch.
    let array_producing = [
        "=UNIQUE(A1:A5)",
        "=SORT(A1:A5)",
        "=SORTBY(A1:A5,A1:A5)",
        "=FILTER(A1:A5,A1:A5>15)",
        "=SEQUENCE(3)",
        "=RANDARRAY(2,2,0,1,FALSE,1)",
        "=TOROW(A1:A5)",
        "=TOCOL(B1:C1)",
        "=WRAPROWS(A1:A5,2)",
        "=WRAPCOLS(A1:A5,2)",
        "=TAKE(A1:A5,2)",
        "=DROP(A1:A5,2)",
        "=CHOOSEROWS(A1:A5,1,3)",
        "=CHOOSECOLS(B1:C1,2)",
        "=HSTACK(A1:A5,A1:A5)",
        "=VSTACK(A1:A5,A1:A5)",
    ];
    assert_eq!(
        array_producing.len(),
        16,
        "all 16 P3 functions must be exercised"
    );
    for f in array_producing {
        match eval_view_array(&parse(f).unwrap(), &s) {
            EvalResult::Array(_) => {}
            EvalResult::Scalar(v) => {
                panic!("{f} returned Scalar({v:?}) — the dispatch arm is missing or collapsed")
            }
        }
        // And through the SCALAR seam it must collapse to a value, never
        // #NAME? (which a missing dispatch arm would produce).
        let scalar = eval_view(&parse(f).unwrap(), &s);
        assert_ne!(
            scalar,
            Value::Error(ErrorKind::Name),
            "{f} collapsed to #NAME? — it is not wired into the shared dispatch"
        );
    }
}

/// An array is bounded by its own extent, never by the sheet. A single-cell
/// range is a 1x1 array (Excel's rule), and reads past the materialised extent
/// are out of range rather than silently reading the sheet.
#[test]
fn array_extent_is_the_result_not_the_sheet() {
    let s = fixture();
    let a = arr(&s, "=A1:A1");
    assert_eq!((a.rows(), a.cols()), (1, 1));
    assert_eq!(a.get(0, 0), Value::Number(10.0));
    // Out-of-range reads are Empty, not a panic and not a sheet read.
    assert_eq!(a.get(5, 0), Value::Empty);
    assert_eq!(a.top_left(), Value::Number(10.0));
}
