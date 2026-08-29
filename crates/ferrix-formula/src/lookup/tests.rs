//! Tests for the lookup library.
//!
//! Kept in `src/lookup/` so this file cannot conflict with sibling work in
//! `eval.rs`'s own test module.
//!
//! The scale tests at the bottom are the point of the file. Every one of them
//! runs against a `CellSource` that *reports* 10,000,000 rows, answers any
//! cell in O(1), and COUNTS the reads — because a 10M-row `Sheet` cannot be
//! built in a unit test, and because an assertion on the returned value alone
//! would pass against a fully-materialising implementation and therefore
//! certify nothing.
//!
//! Note on syntax: this engine's parser has no empty-argument form (`f(a,,b)`
//! is a parse error), so tests that want XLOOKUP's default `if_not_found`
//! while still setting a later mode pass an explicit `NA()`, which evaluates
//! to exactly the `#N/A` the default produces.

use super::*;
use crate::eval::eval;
use crate::parser::parse;
use ferrix_core::{Sheet, StrId, StringArena};
use std::cell::Cell;

// --- fixtures -------------------------------------------------------------

/// A four-column sheet:
///   A: ascending numeric keys 10,20,30,40,50
///   B: names (alpha, bravo, charlie, delta, echo)
///   C: payload numbers 1..5
///   D: DESCENDING numeric keys 50,40,30,20,10
fn fixture() -> Sheet {
    let mut s = Sheet::new("lk");
    let names = ["alpha", "bravo", "charlie", "delta", "echo"];
    for r in 0..5u32 {
        s.set(CellRef::new(r, 0), Value::Number((r as f64 + 1.0) * 10.0));
        s.set_text(CellRef::new(r, 1), names[r as usize]);
        s.set(CellRef::new(r, 2), Value::Number(r as f64 + 1.0));
        s.set(CellRef::new(r, 3), Value::Number(50.0 - r as f64 * 10.0));
    }
    s
}

#[track_caller]
fn ev(f: &str, sheet: &Sheet) -> Value {
    let expr = parse(f).unwrap_or_else(|e| panic!("parse {f}: {e}"));
    eval(&expr, sheet)
}

#[track_caller]
fn num(f: &str, sheet: &Sheet) -> f64 {
    match ev(f, sheet) {
        Value::Number(n) => n,
        other => panic!("{f} produced {other:?}, expected a number"),
    }
}

#[track_caller]
fn text(f: &str, sheet: &Sheet) -> String {
    match ev(f, sheet) {
        Value::Text(id) => sheet.resolve(id).to_string(),
        other => panic!("{f} produced {other:?}, expected text"),
    }
}

#[track_caller]
fn err(f: &str, sheet: &Sheet) -> ErrorKind {
    match ev(f, sheet) {
        Value::Error(e) => e,
        other => panic!("{f} produced {other:?}, expected an error"),
    }
}

// --- family ownership -----------------------------------------------------

#[test]
fn the_family_claims_exactly_its_own_names() {
    for n in [
        "VLOOKUP", "HLOOKUP", "INDEX", "MATCH", "XLOOKUP", "CHOOSE", "INDIRECT",
    ] {
        assert!(is_lookup_fn(n), "{n} must be owned by the lookup family");
    }
    // The negative side is the one that matters: an over-broad predicate here
    // swallows another family's calls in `eval_call`'s ordered guard match,
    // and that family's own tests would never notice.
    for n in [
        "LEFT",
        "LEN",
        "SEARCH",
        "TEXT",
        "VALUE",
        "REPT",
        "YEAR",
        "DATE",
        "EOMONTH",
        "DAYS",
        "MEDIAN",
        "LARGE",
        "SMALL",
        "RANK",
        "SUM",
        "COUNTIF",
        "IF",
        "LOOKUP",
        "VLOOKUP2",
        "INDEXOF",
        "MATCHES",
        "CHOOSECOLS",
        "INDIRECTION",
    ] {
        assert!(!is_lookup_fn(n), "{n} must NOT be claimed by lookup");
    }
}

// --- VLOOKUP --------------------------------------------------------------

#[test]
fn vlookup_exact_mode_finds_a_key_and_returns_the_named_column() {
    let s = fixture();
    assert_eq!(text("=VLOOKUP(30,A1:C5,2,FALSE)", &s), "charlie");
    assert_eq!(num("=VLOOKUP(30,A1:C5,3,FALSE)", &s), 3.0);
    // The key column itself is column 1.
    assert_eq!(num("=VLOOKUP(50,A1:C5,1,FALSE)", &s), 50.0);
    // 0 coerces to FALSE, which is how most real workbooks spell exact mode.
    assert_eq!(text("=VLOOKUP(20,A1:C5,2,0)", &s), "bravo");
    // Text keys, case-insensitively, through the one collation in criteria.rs.
    assert_eq!(num("=VLOOKUP(\"DELTA\",B1:C5,2,FALSE)", &s), 4.0);
}

#[test]
fn vlookup_exact_mode_missing_key_is_not_available() {
    let s = fixture();
    assert_eq!(
        err("=VLOOKUP(35,A1:C5,2,FALSE)", &s),
        ErrorKind::NotAvailable,
        "a missing key is #N/A, never a nearby row"
    );
    assert_eq!(
        err("=VLOOKUP(\"nope\",B1:C5,2,FALSE)", &s),
        ErrorKind::NotAvailable
    );
}

#[test]
fn vlookup_approximate_mode_takes_the_largest_key_not_exceeding_the_probe() {
    let s = fixture();
    // 35 falls between 30 and 40 -> the 30 row.
    assert_eq!(text("=VLOOKUP(35,A1:C5,2,TRUE)", &s), "charlie");
    // Exactly on a boundary still takes that row.
    assert_eq!(text("=VLOOKUP(40,A1:C5,2,TRUE)", &s), "delta");
    // Past the top takes the last row.
    assert_eq!(text("=VLOOKUP(999,A1:C5,2,TRUE)", &s), "echo");
    // Below the bottom has nothing to take: #N/A.
    assert_eq!(err("=VLOOKUP(1,A1:C5,2,TRUE)", &s), ErrorKind::NotAvailable);
    // Omitting range_lookup means TRUE — the default people forget.
    assert_eq!(text("=VLOOKUP(35,A1:C5,2)", &s), "charlie");
}

#[test]
fn approximate_vlookup_on_unsorted_data_answers_rather_than_erroring() {
    // Excel does NOT validate sortedness; it binary-searches regardless and
    // returns whatever the probes land on. Validating would cost the full O(n)
    // scan the binary search exists to avoid, on every call.
    let mut s = Sheet::new("unsorted");
    for (r, k) in [50.0, 10.0, 40.0, 20.0, 30.0].iter().enumerate() {
        s.set(CellRef::new(r as u32, 0), Value::Number(*k));
        s.set(CellRef::new(r as u32, 1), Value::Number(*k * 100.0));
    }

    // Probe trace over [50,10,40,20,30] for 35, accepting cell <= 35:
    //   mid=2 -> 40, rejected, hi=2
    //   mid=1 -> 10, accepted, ans=1, lo=2
    // -> offset 1, whose payload is 1000. Not "the right answer" in any
    // semantic sense — it is Excel's answer, which is the contract.
    assert_eq!(
        ev("=VLOOKUP(35,A1:B5,2,TRUE)", &s),
        Value::Number(1000.0),
        "approximate VLOOKUP over unsorted data must return whatever the \
         binary probes land on, not an error and not a full-scan answer"
    );
}

#[test]
fn vlookup_rejects_a_bad_column_index_by_kind() {
    let s = fixture();
    // Below 1 is a broken formula.
    assert_eq!(err("=VLOOKUP(30,A1:C5,0,FALSE)", &s), ErrorKind::Value);
    assert_eq!(err("=VLOOKUP(30,A1:C5,-1,FALSE)", &s), ErrorKind::Value);
    // Past the table's edge is a broken reference.
    assert_eq!(err("=VLOOKUP(30,A1:C5,4,FALSE)", &s), ErrorKind::Ref);
    // A non-numeric index is a type error.
    assert_eq!(
        err("=VLOOKUP(30,A1:C5,\"two\",FALSE)", &s),
        ErrorKind::Value
    );
    // A non-range table is a type error too.
    assert_eq!(err("=VLOOKUP(30,7,2,FALSE)", &s), ErrorKind::Value);
    // Wrong arity.
    assert_eq!(err("=VLOOKUP(30,A1:C5)", &s), ErrorKind::Value);
}

#[test]
fn vlookup_exact_mode_honours_wildcards_in_a_text_probe() {
    let s = fixture();
    assert_eq!(num("=VLOOKUP(\"cha*\",B1:C5,2,FALSE)", &s), 3.0);
    assert_eq!(num("=VLOOKUP(\"?elta\",B1:C5,2,FALSE)", &s), 4.0);
    // `~` escapes, inherited from crate::criteria rather than reimplemented:
    // "cha~*" is a literal `cha*`, which no cell holds.
    assert_eq!(
        err("=VLOOKUP(\"cha~*\",B1:C5,2,FALSE)", &s),
        ErrorKind::NotAvailable
    );
}

// --- HLOOKUP --------------------------------------------------------------

#[test]
fn hlookup_searches_the_first_row_and_returns_the_named_row() {
    let mut s = Sheet::new("h");
    for c in 0..5u32 {
        s.set(CellRef::new(0, c), Value::Number((c as f64 + 1.0) * 10.0));
        s.set(CellRef::new(1, c), Value::Number(c as f64 + 1.0));
    }
    assert_eq!(num("=HLOOKUP(30,A1:E2,2,FALSE)", &s), 3.0);
    assert_eq!(num("=HLOOKUP(35,A1:E2,2,TRUE)", &s), 3.0);
    assert_eq!(
        err("=HLOOKUP(35,A1:E2,2,FALSE)", &s),
        ErrorKind::NotAvailable
    );
    assert_eq!(err("=HLOOKUP(30,A1:E2,3,FALSE)", &s), ErrorKind::Ref);
}

// --- MATCH ----------------------------------------------------------------

#[test]
fn match_type_zero_is_exact_and_one_based() {
    let s = fixture();
    assert_eq!(num("=MATCH(30,A1:A5,0)", &s), 3.0);
    assert_eq!(num("=MATCH(10,A1:A5,0)", &s), 1.0);
    assert_eq!(num("=MATCH(50,A1:A5,0)", &s), 5.0);
    assert_eq!(num("=MATCH(\"delta\",B1:B5,0)", &s), 4.0);
    // Case-insensitive, through the ONE collation in crate::criteria.
    assert_eq!(num("=MATCH(\"DELTA\",B1:B5,0)", &s), 4.0);
    // Wildcards, same matcher as SUMIF/SEARCH.
    assert_eq!(num("=MATCH(\"ech?\",B1:B5,0)", &s), 5.0);
    assert_eq!(err("=MATCH(35,A1:A5,0)", &s), ErrorKind::NotAvailable);
}

#[test]
fn match_type_one_is_largest_not_exceeding_and_is_the_default() {
    let s = fixture();
    assert_eq!(num("=MATCH(35,A1:A5,1)", &s), 3.0);
    assert_eq!(num("=MATCH(30,A1:A5,1)", &s), 3.0);
    assert_eq!(num("=MATCH(999,A1:A5,1)", &s), 5.0);
    assert_eq!(err("=MATCH(1,A1:A5,1)", &s), ErrorKind::NotAvailable);
    // Omitted match_type is 1.
    assert_eq!(num("=MATCH(35,A1:A5)", &s), 3.0);
}

#[test]
fn match_type_minus_one_is_smallest_not_below_over_descending_data() {
    let s = fixture();
    // Column D is 50,40,30,20,10.
    assert_eq!(num("=MATCH(35,D1:D5,-1)", &s), 2.0, "40 sits at position 2");
    assert_eq!(num("=MATCH(40,D1:D5,-1)", &s), 2.0);
    assert_eq!(num("=MATCH(1,D1:D5,-1)", &s), 5.0);
    assert_eq!(err("=MATCH(999,D1:D5,-1)", &s), ErrorKind::NotAvailable);
}

#[test]
fn match_over_a_horizontal_vector_works_too() {
    let mut s = Sheet::new("row");
    for c in 0..5u32 {
        s.set(CellRef::new(0, c), Value::Number((c as f64 + 1.0) * 10.0));
    }
    assert_eq!(num("=MATCH(40,A1:E1,0)", &s), 4.0);
    assert_eq!(num("=MATCH(45,A1:E1,1)", &s), 4.0);
}

#[test]
fn match_over_a_two_dimensional_array_is_not_available() {
    let s = fixture();
    assert_eq!(err("=MATCH(30,A1:C5,0)", &s), ErrorKind::NotAvailable);
}

// --- INDEX ----------------------------------------------------------------

#[test]
fn index_with_a_row_and_column_pair() {
    let s = fixture();
    assert_eq!(num("=INDEX(A1:C5,3,1)", &s), 30.0);
    assert_eq!(text("=INDEX(A1:C5,3,2)", &s), "charlie");
    assert_eq!(num("=INDEX(A1:C5,5,3)", &s), 5.0);
}

#[test]
fn index_with_zero_means_the_whole_row_or_column() {
    let s = fixture();
    // 0 as the column over a single-column array means "the whole column",
    // which here collapses to exactly one cell, so it is answerable.
    assert_eq!(num("=INDEX(A1:A5,3,0)", &s), 30.0);
    // 0 as the row over a single-row array, likewise.
    let mut row = Sheet::new("r");
    for c in 0..5u32 {
        row.set(CellRef::new(0, c), Value::Number(c as f64));
    }
    assert_eq!(num("=INDEX(A1:E1,0,4)", &row), 3.0);
    // Where 0 really would select more than one cell there is no array value
    // to return (spilling is out of scope for #23), so it is #VALUE! rather
    // than a plausible-looking wrong scalar.
    assert_eq!(err("=INDEX(A1:C5,3,0)", &s), ErrorKind::Value);
    assert_eq!(err("=INDEX(A1:C5,0,2)", &s), ErrorKind::Value);
}

#[test]
fn index_two_argument_form_indexes_the_only_axis() {
    let s = fixture();
    assert_eq!(num("=INDEX(A1:A5,4)", &s), 40.0);
    let mut row = Sheet::new("r");
    for c in 0..5u32 {
        row.set(CellRef::new(0, c), Value::Number(c as f64 * 3.0));
    }
    assert_eq!(num("=INDEX(A1:E1,4)", &row), 9.0);
}

#[test]
fn index_out_of_bounds_is_a_reference_error_and_negative_is_a_value_error() {
    let s = fixture();
    assert_eq!(err("=INDEX(A1:C5,6,1)", &s), ErrorKind::Ref);
    assert_eq!(err("=INDEX(A1:C5,1,4)", &s), ErrorKind::Ref);
    assert_eq!(err("=INDEX(A1:C5,-1,1)", &s), ErrorKind::Value);
    assert_eq!(err("=INDEX(A1:C5,\"x\",1)", &s), ErrorKind::Value);
    assert_eq!(err("=INDEX(7,1,1)", &s), ErrorKind::Value);
}

#[test]
fn index_and_match_compose_the_way_real_workbooks_use_them() {
    let s = fixture();
    assert_eq!(text("=INDEX(B1:B5,MATCH(40,A1:A5,0))", &s), "delta");
    assert_eq!(num("=INDEX(A1:C5,MATCH(\"echo\",B1:B5,0),3)", &s), 5.0);
}

// --- XLOOKUP --------------------------------------------------------------

#[test]
fn xlookup_exact_match_is_the_default() {
    let s = fixture();
    assert_eq!(text("=XLOOKUP(30,A1:A5,B1:B5)", &s), "charlie");
    assert_eq!(num("=XLOOKUP(\"delta\",B1:B5,C1:C5)", &s), 4.0);
}

#[test]
fn xlookup_if_not_found_replaces_the_na_and_is_only_evaluated_on_a_miss() {
    let s = fixture();
    assert_eq!(num("=XLOOKUP(99,A1:A5,C1:C5,-1)", &s), -1.0);
    assert_eq!(text("=XLOOKUP(99,A1:A5,B1:B5,\"none\")", &s), "none");
    // Without it, a miss is #N/A.
    assert_eq!(err("=XLOOKUP(99,A1:A5,C1:C5)", &s), ErrorKind::NotAvailable);
    // On a HIT the fallback is not evaluated at all: a fallback that would
    // itself error still leaves the hit intact.
    assert_eq!(num("=XLOOKUP(30,A1:A5,C1:C5,1/0)", &s), 3.0);
}

#[test]
fn xlookup_reverse_search_finds_the_last_match_not_the_first() {
    let mut s = Sheet::new("dup");
    // Key 7 appears twice with different payloads, so first vs last is
    // visible in the RESULT and not merely in an index.
    for (r, (k, p)) in [(7.0, 100.0), (8.0, 200.0), (7.0, 300.0)]
        .iter()
        .enumerate()
    {
        s.set(CellRef::new(r as u32, 0), Value::Number(*k));
        s.set(CellRef::new(r as u32, 1), Value::Number(*p));
    }
    assert_eq!(
        num("=XLOOKUP(7,A1:A3,B1:B3,NA(),0,1)", &s),
        100.0,
        "forward search must take the FIRST match"
    );
    assert_eq!(
        num("=XLOOKUP(7,A1:A3,B1:B3,NA(),0,-1)", &s),
        300.0,
        "reverse search must take the LAST match"
    );
}

#[test]
fn xlookup_match_modes_cover_exact_smaller_larger_and_wildcard() {
    let s = fixture();
    // -1: exact or next smaller.
    assert_eq!(num("=XLOOKUP(35,A1:A5,C1:C5,NA(),-1)", &s), 3.0);
    assert_eq!(num("=XLOOKUP(30,A1:A5,C1:C5,NA(),-1)", &s), 3.0);
    assert_eq!(
        err("=XLOOKUP(5,A1:A5,C1:C5,NA(),-1)", &s),
        ErrorKind::NotAvailable,
        "nothing is smaller than 5, so there is no next-smaller"
    );
    // 1: exact or next larger.
    assert_eq!(num("=XLOOKUP(35,A1:A5,C1:C5,NA(),1)", &s), 4.0);
    assert_eq!(num("=XLOOKUP(30,A1:A5,C1:C5,NA(),1)", &s), 3.0);
    assert_eq!(
        err("=XLOOKUP(99,A1:A5,C1:C5,NA(),1)", &s),
        ErrorKind::NotAvailable
    );
    // 2: wildcard.
    assert_eq!(num("=XLOOKUP(\"cha*\",B1:B5,C1:C5,NA(),2)", &s), 3.0);
    // ...and mode 0 must NOT treat a wildcard as one.
    assert_eq!(
        err("=XLOOKUP(\"cha*\",B1:B5,C1:C5,NA(),0)", &s),
        ErrorKind::NotAvailable
    );
}

#[test]
fn xlookup_nearest_match_modes_work_on_unsorted_data_too() {
    // The linear nearest-match path exists precisely so match_mode ±1 is
    // correct without a sortedness promise. Over unsorted data a binary
    // search would land somewhere arbitrary; this must not.
    let mut s = Sheet::new("jumbled");
    for (r, (k, p)) in [(50.0, 5.0), (10.0, 1.0), (40.0, 4.0), (20.0, 2.0)]
        .iter()
        .enumerate()
    {
        s.set(CellRef::new(r as u32, 0), Value::Number(*k));
        s.set(CellRef::new(r as u32, 1), Value::Number(*p));
    }
    assert_eq!(
        num("=XLOOKUP(35,A1:A4,B1:B4,NA(),-1)", &s),
        2.0,
        "the greatest key below 35 is 20, wherever it sits"
    );
    assert_eq!(
        num("=XLOOKUP(35,A1:A4,B1:B4,NA(),1)", &s),
        4.0,
        "the smallest key above 35 is 40, wherever it sits"
    );
}

#[test]
fn xlookup_binary_search_modes_agree_with_the_linear_ones_on_sorted_data() {
    let s = fixture();
    // search_mode 2: binary ascending.
    assert_eq!(num("=XLOOKUP(30,A1:A5,C1:C5,NA(),0,2)", &s), 3.0);
    assert_eq!(num("=XLOOKUP(35,A1:A5,C1:C5,NA(),-1,2)", &s), 3.0);
    assert_eq!(num("=XLOOKUP(35,A1:A5,C1:C5,NA(),1,2)", &s), 4.0);
    assert_eq!(
        err("=XLOOKUP(35,A1:A5,C1:C5,NA(),0,2)", &s),
        ErrorKind::NotAvailable,
        "binary EXACT must reject a near miss, not return its neighbour"
    );
    // search_mode -2: binary descending, over column D (50..10).
    assert_eq!(num("=XLOOKUP(30,D1:D5,C1:C5,NA(),0,-2)", &s), 3.0);
    assert_eq!(
        err("=XLOOKUP(35,D1:D5,C1:C5,NA(),0,-2)", &s),
        ErrorKind::NotAvailable
    );
}

#[test]
fn xlookup_rejects_mismatched_arrays_and_bad_modes() {
    let s = fixture();
    assert_eq!(err("=XLOOKUP(30,A1:A5,C1:C3)", &s), ErrorKind::Value);
    assert_eq!(err("=XLOOKUP(30,A1:A5,C1:C5,NA(),9)", &s), ErrorKind::Value);
    assert_eq!(
        err("=XLOOKUP(30,A1:A5,C1:C5,NA(),0,7)", &s),
        ErrorKind::Value
    );
    // A 2-D lookup array has no single lane to search.
    assert_eq!(err("=XLOOKUP(30,A1:C5,C1:C5)", &s), ErrorKind::Value);
    assert_eq!(err("=XLOOKUP(30,A1:A5)", &s), ErrorKind::Value);
}

// --- CHOOSE ---------------------------------------------------------------

#[test]
fn choose_selects_the_nth_argument_one_based() {
    let s = fixture();
    assert_eq!(num("=CHOOSE(2,10,20,30)", &s), 20.0);
    assert_eq!(text("=CHOOSE(3,\"a\",\"b\",\"c\")", &s), "c");
    // The index truncates, as Excel does.
    assert_eq!(num("=CHOOSE(2.9,10,20,30)", &s), 20.0);
    // Cell references work as choices.
    assert_eq!(num("=CHOOSE(1,A3,A4)", &s), 30.0);
}

#[test]
fn choose_evaluates_only_the_selected_argument() {
    let s = fixture();
    // The unselected branch is a division by zero. If CHOOSE evaluated every
    // argument eagerly this would be #DIV/0! — and, the reason it matters at
    // Ferrix's scale, an unselected `SUM(A:A)` over 200M rows would be paid
    // for on every recalculation.
    assert_eq!(num("=CHOOSE(1,42,1/0)", &s), 42.0);
    assert_eq!(err("=CHOOSE(2,42,1/0)", &s), ErrorKind::DivZero);
}

#[test]
fn choose_out_of_range_is_a_value_error() {
    let s = fixture();
    assert_eq!(err("=CHOOSE(0,10,20)", &s), ErrorKind::Value);
    assert_eq!(err("=CHOOSE(3,10,20)", &s), ErrorKind::Value);
    assert_eq!(err("=CHOOSE(-1,10,20)", &s), ErrorKind::Value);
    assert_eq!(err("=CHOOSE(\"x\",10,20)", &s), ErrorKind::Value);
    assert_eq!(err("=CHOOSE(1)", &s), ErrorKind::Value);
}

// --- INDIRECT -------------------------------------------------------------

#[test]
fn indirect_resolves_a_literal_and_a_computed_reference() {
    let s = fixture();
    assert_eq!(num("=INDIRECT(\"A3\")", &s), 30.0);
    assert_eq!(num("=INDIRECT(\"$A$3\")", &s), 30.0);
    assert_eq!(text("=INDIRECT(\"B4\")", &s), "delta");
    // Computed at runtime — the whole reason its edges cannot be static.
    assert_eq!(num("=INDIRECT(CONCAT(\"A\",TEXT(C5,\"0\")))", &s), 50.0);
    // R1C1 form, absolute only.
    assert_eq!(num("=INDIRECT(\"R3C1\",FALSE)", &s), 30.0);
}

#[test]
fn indirect_re_resolves_its_target_on_every_evaluation() {
    // The behavioural half of the "dependencies cannot be cached" argument in
    // the module docs: the SAME parsed expression must follow the driving cell
    // when that cell changes. An implementation that resolved once and cached
    // would return 30 both times.
    let mut s = fixture();
    s.set_text(CellRef::new(0, 4), "A3");
    let expr = parse("=INDIRECT(E1)").unwrap();
    assert_eq!(eval(&expr, &s), Value::Number(30.0));

    s.set_text(CellRef::new(0, 4), "A5");
    assert_eq!(
        eval(&expr, &s),
        Value::Number(50.0),
        "INDIRECT must re-resolve ref_text on every evaluate; a cached target \
         would still point at A3 and would be silently stale"
    );
}

#[test]
fn indirect_rejects_a_malformed_or_unsupported_reference() {
    let s = fixture();
    assert_eq!(err("=INDIRECT(\"not a ref\")", &s), ErrorKind::Ref);
    assert_eq!(err("=INDIRECT(\"A0\")", &s), ErrorKind::Ref);
    assert_eq!(err("=INDIRECT(7)", &s), ErrorKind::Ref);
    // A range is not a scalar in this engine; returning one is out of scope.
    assert_eq!(err("=INDIRECT(\"A1:A5\")", &s), ErrorKind::Value);
    // Relative R1C1 has no anchor here, so it is refused rather than guessed.
    assert_eq!(err("=INDIRECT(\"R[1]C[1]\",FALSE)", &s), ErrorKind::Ref);
    // An unknown sheet is #REF!.
    assert_eq!(err("=INDIRECT(\"Nope!A1\")", &s), ErrorKind::Ref);
    // Wrong arity.
    assert_eq!(err("=INDIRECT()", &s), ErrorKind::Value);
}

/// A source that answers EVERY cell with the text `"A1"`, so a nested
/// `INDIRECT` recurses forever unless the budget stops it.
struct SelfRef {
    arena: StringArena,
    id: StrId,
}

impl crate::CellSource for SelfRef {
    fn get(&self, _c: CellRef) -> Value {
        Value::Text(self.id)
    }
    fn resolve(&self, id: StrId) -> &str {
        self.arena.resolve(id).unwrap_or("")
    }
    fn sum_rect(&self, _s: CellRef, _e: CellRef) -> f64 {
        0.0
    }
    fn count_rect(&self, _s: CellRef, _e: CellRef) -> usize {
        0
    }
    fn row_count(&self) -> usize {
        10
    }
}

#[test]
fn indirect_recursion_budget_yields_ref_instead_of_hanging() {
    // The budget is the ONLY defence here: the static dependency graph cannot
    // see an INDIRECT cycle at all (see the module docs), so without it this
    // is a stack overflow rather than a formula error.
    let mut arena = StringArena::new();
    let id = arena.intern("A1");
    let src = SelfRef { arena, id };

    // Nest INDIRECT well past the budget. Every level's ref_text resolves to
    // another "A1", so nothing here can terminate on its own.
    let depth = (MAX_INDIRECT_DEPTH as usize) + 8;
    let mut f = String::from("=");
    f.push_str(&"INDIRECT(".repeat(depth));
    f.push_str("\"A1\"");
    f.push_str(&")".repeat(depth));

    let got = crate::eval_view(&parse(&f).unwrap(), &src);
    assert_eq!(
        got,
        Value::Error(ErrorKind::Ref),
        "an INDIRECT nest deeper than MAX_INDIRECT_DEPTH must be #REF!, not a \
         stack overflow and not a hang"
    );

    // And the budget must be RELEASED on the error path: a subsequent shallow
    // call still works. A counter leaked by an early return would poison
    // every later formula on this thread.
    assert_eq!(
        crate::eval_view(&parse("=INDIRECT(\"A1\")").unwrap(), &src),
        Value::Text(id),
        "the depth counter must unwind on the error path too"
    );
}

// --- INDIRECT and the dependency graph ------------------------------------

/// The documented gap, asserted rather than merely described.
///
/// The module docs claim `INDIRECT`'s edges cannot be collected statically.
/// This test PINS that claim, so it cannot quietly become false (or quietly
/// stay false after someone "fixes" it in a way that produces a stale edge).
/// If a future change did make `collect_precedents` see through `INDIRECT`,
/// this test fails and forces the module docs — and the staleness argument in
/// them — to be revisited deliberately.
#[test]
fn indirect_contributes_no_static_precedent_edge_for_its_target() {
    use crate::depgraph::{collect_precedents, Precedent};

    // A direct reference to A3 produces an edge to A3.
    let mut direct = Vec::new();
    collect_precedents(&parse("=A3").unwrap(), &mut direct);
    assert_eq!(direct, vec![Precedent::Cell(CellRef::new(2, 0))]);

    // The SAME target reached through INDIRECT produces NO edge at all: the
    // parse tree holds a string literal, not a reference.
    let mut via_indirect = Vec::new();
    collect_precedents(&parse("=INDIRECT(\"A3\")").unwrap(), &mut via_indirect);
    assert!(
        via_indirect.is_empty(),
        "INDIRECT(\"A3\") contributed {via_indirect:?}; the dependency graph \
         walks the parse tree, where the target is a string. If this ever \
         becomes non-empty, the staleness argument in the module docs needs \
         rewriting, not silently ignoring"
    );

    // And the ARGUMENT's own references ARE collected — INDIRECT does not
    // hide the cells that compute its target, only the cell it lands on. This
    // is what makes `=INDIRECT(E1)` recalculate when E1 changes even though
    // it does not recalculate when the cell E1 NAMES changes.
    let mut computed = Vec::new();
    collect_precedents(&parse("=INDIRECT(E1)").unwrap(), &mut computed);
    assert_eq!(
        computed,
        vec![Precedent::Cell(CellRef::new(0, 4))],
        "the driving cell E1 must still be an edge; only the resolved target \
         is invisible"
    );
}

/// The consequence, stated as a test: an `INDIRECT` cycle is invisible to the
/// graph's cycle detector, which is precisely why the runtime budget exists.
///
/// Written as a positive assertion about what the detector DOES see, so it
/// documents the limitation instead of pretending it is not there.
#[test]
fn the_dep_graph_cannot_see_an_indirect_cycle() {
    use crate::depgraph::DepGraph;
    use ferrix_core::CellRef as CR;

    let mut g = DepGraph::new();
    // A1 = INDIRECT("A1") is a genuine self-cycle at runtime.
    g.set_formula(CR::new(0, 0), &parse("=INDIRECT(\"A1\")").unwrap());
    assert!(
        !g.is_circular(CR::new(0, 0)),
        "the static graph reported an INDIRECT cycle; if it has learned to \
         see through INDIRECT then MAX_INDIRECT_DEPTH is no longer the only \
         defence and the module docs must say so"
    );

    // A direct self-reference, by contrast, IS caught — so the detector is
    // working and the line above is a real limitation, not a dead assertion.
    g.set_formula(CR::new(1, 0), &parse("=A2+1").unwrap());
    assert!(
        g.is_circular(CR::new(1, 0)),
        "the cycle detector failed on a direct self-reference, so the \
         INDIRECT assertion above proves nothing"
    );
}

#[test]
fn indirect_nesting_just_inside_the_budget_still_resolves() {
    // The complement of the test above: the budget must not be so tight that
    // legitimate nesting fails. Without this, an off-by-one that refused at
    // depth 1 would still pass the "deep nest is #REF!" test.
    let mut arena = StringArena::new();
    let id = arena.intern("A1");
    let src = SelfRef { arena, id };

    let depth = MAX_INDIRECT_DEPTH as usize;
    let mut f = String::from("=");
    f.push_str(&"INDIRECT(".repeat(depth));
    f.push_str("\"A1\"");
    f.push_str(&")".repeat(depth));

    assert_eq!(
        crate::eval_view(&parse(&f).unwrap(), &src),
        Value::Text(id),
        "nesting exactly at MAX_INDIRECT_DEPTH must still resolve"
    );
}

// --- scale ----------------------------------------------------------------

/// A `CellSource` that CLAIMS ten million rows, answers any cell in O(1), and
/// counts every read.
///
/// Column 0 holds ascending keys `row * 10`; every other column holds `row`.
/// Nothing is stored, so the source itself is a few words — which is the only
/// way to exercise 10M-row behaviour in a unit test at all.
struct TallSorted {
    arena: StringArena,
    reads: Cell<u64>,
    rows: usize,
}

const TALL_ROWS: usize = 10_000_000;

impl TallSorted {
    fn new(rows: usize) -> Self {
        Self {
            arena: StringArena::new(),
            reads: Cell::new(0),
            rows,
        }
    }
    fn reset(&self) {
        self.reads.set(0);
    }
}

impl crate::CellSource for TallSorted {
    fn get(&self, cell: CellRef) -> Value {
        self.reads.set(self.reads.get() + 1);
        match cell.col {
            0 => Value::Number(cell.row as f64 * 10.0),
            _ => Value::Number(cell.row as f64),
        }
    }
    fn resolve(&self, id: StrId) -> &str {
        self.arena.resolve(id).unwrap_or("")
    }
    fn sum_rect(&self, _s: CellRef, _e: CellRef) -> f64 {
        0.0
    }
    fn count_rect(&self, _s: CellRef, _e: CellRef) -> usize {
        0
    }
    fn row_count(&self) -> usize {
        self.rows
    }
}

#[test]
fn exact_lookup_over_ten_million_rows_stops_at_the_first_match() {
    // THE acceptance criterion: a lookup over a 10M-row column must not
    // materialise the column. Asserting only the returned value would pass
    // against an implementation that collected all 10M cells first, so this
    // asserts the number of cells VISITED.
    let src = TallSorted::new(TALL_ROWS);
    let f = format!("=VLOOKUP(70,A1:B{TALL_ROWS},2,FALSE)");
    let expr = parse(&f).unwrap();

    src.reset();
    let got = crate::eval_view(&expr, &src);
    let reads = src.reads.get();

    assert_eq!(got, Value::Number(7.0), "row 8 (key 70) carries payload 7");
    assert!(
        reads <= 16,
        "exact VLOOKUP hitting row 8 of a {TALL_ROWS}-row column read {reads} \
         cells; it must stop at the first match (~9 reads), not scan or \
         collect the column"
    );
}

#[test]
fn exact_match_over_ten_million_rows_stops_at_the_first_match() {
    let src = TallSorted::new(TALL_ROWS);
    let f = format!("=MATCH(30,A1:A{TALL_ROWS},0)");
    let expr = parse(&f).unwrap();
    src.reset();
    let got = crate::eval_view(&expr, &src);
    let reads = src.reads.get();
    assert_eq!(got, Value::Number(4.0));
    assert!(
        reads <= 8,
        "exact MATCH read {reads} cells to find position 4 of {TALL_ROWS}"
    );
}

#[test]
fn approximate_lookup_over_ten_million_rows_is_logarithmic() {
    // The deep-row case: a key near the BOTTOM of a 10M-row column. A linear
    // scan would read ~10M cells and a collecting implementation exactly 10M.
    // Binary search reads ~24.
    let src = TallSorted::new(TALL_ROWS);
    let deep_key = (TALL_ROWS as f64 - 3.0) * 10.0;
    let f = format!("=VLOOKUP({deep_key},A1:B{TALL_ROWS},2,TRUE)");
    let expr = parse(&f).unwrap();

    src.reset();
    let got = crate::eval_view(&expr, &src);
    let reads = src.reads.get();

    assert_eq!(
        got,
        Value::Number(TALL_ROWS as f64 - 3.0),
        "approximate lookup must still land on the right row"
    );
    // log2(10M) ~= 23.3, so 32 is generous — and still ~300,000x below the
    // row count. Nothing that materialises the column can pass this.
    assert!(
        reads <= 32,
        "approximate VLOOKUP over {TALL_ROWS} rows read {reads} cells; it must \
         binary-search (~24 reads), not scan or collect the column"
    );
}

#[test]
fn approximate_match_over_ten_million_rows_is_logarithmic() {
    let src = TallSorted::new(TALL_ROWS);
    let probe = (TALL_ROWS as f64 - 2.0) * 10.0;
    let f = format!("=MATCH({probe},A1:A{TALL_ROWS},1)");
    let expr = parse(&f).unwrap();
    src.reset();
    let got = crate::eval_view(&expr, &src);
    let reads = src.reads.get();
    assert_eq!(got, Value::Number(TALL_ROWS as f64 - 1.0));
    assert!(reads <= 32, "MATCH type 1 read {reads} cells");

    // XLOOKUP's binary search modes must hold the same line.
    let k = (TALL_ROWS as f64 - 5.0) * 10.0;
    let f = format!("=XLOOKUP({k},A1:A{TALL_ROWS},B1:B{TALL_ROWS},NA(),0,2)");
    let expr = parse(&f).unwrap();
    src.reset();
    let got = crate::eval_view(&expr, &src);
    let reads = src.reads.get();
    assert_eq!(got, Value::Number(TALL_ROWS as f64 - 5.0));
    assert!(
        reads <= 40,
        "XLOOKUP search_mode 2 over {TALL_ROWS} rows read {reads} cells"
    );
}

#[test]
fn index_over_a_ten_million_row_range_reads_exactly_one_cell() {
    let src = TallSorted::new(TALL_ROWS);
    let f = format!("=INDEX(A1:B{TALL_ROWS},9999999,2)");
    let expr = parse(&f).unwrap();
    src.reset();
    let got = crate::eval_view(&expr, &src);
    let reads = src.reads.get();
    assert_eq!(got, Value::Number(9_999_998.0));
    assert_eq!(
        reads, 1,
        "INDEX must address its cell directly; it read {reads} cells instead \
         of 1, which means it walked or built the range"
    );
}

#[test]
fn xlookup_reverse_search_over_ten_million_rows_stops_at_the_last_match() {
    // Reverse search from the bottom must be as cheap as forward search from
    // the top: it walks from the far end, rather than materialising the lane
    // and reversing it.
    let src = TallSorted::new(TALL_ROWS);
    let last_key = (TALL_ROWS as f64 - 1.0) * 10.0;
    let f = format!("=XLOOKUP({last_key},A1:A{TALL_ROWS},B1:B{TALL_ROWS},NA(),0,-1)");
    let expr = parse(&f).unwrap();
    src.reset();
    let got = crate::eval_view(&expr, &src);
    let reads = src.reads.get();
    assert_eq!(got, Value::Number(TALL_ROWS as f64 - 1.0));
    assert!(
        reads <= 8,
        "reverse XLOOKUP hitting the LAST row read {reads} cells; it must walk \
         backwards from the end, not build the column to reverse it"
    );
}

#[test]
fn an_unselected_choose_branch_never_touches_its_range() {
    // Belt and braces on the laziness claim: a CHOOSE that does not select the
    // tall lookup proves the 10M-row range was never touched at all.
    let src = TallSorted::new(TALL_ROWS);
    let f = format!("=CHOOSE(1,5,VLOOKUP(70,A1:B{TALL_ROWS},2,FALSE))");
    let expr = parse(&f).unwrap();
    src.reset();
    let got = crate::eval_view(&expr, &src);
    assert_eq!(got, Value::Number(5.0));
    assert_eq!(
        src.reads.get(),
        0,
        "the unselected CHOOSE branch must not be evaluated at all"
    );
}
