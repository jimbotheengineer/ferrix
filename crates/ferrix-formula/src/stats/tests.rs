//! Tests for the statistics library.
//!
//! Kept in `src/stats/` so this file cannot conflict with sibling work in
//! `eval.rs`'s own test module.

use super::*;
use crate::eval::eval;
use crate::parser::parse;
use ferrix_core::{CellRef, Sheet};

/// A sheet with the given column-A values, `None` meaning "leave blank".
fn col_a(vals: &[Option<f64>]) -> Sheet {
    let mut s = Sheet::new("t");
    for (r, v) in vals.iter().enumerate() {
        if let Some(n) = v {
            s.set(CellRef::new(r as u32, 0), Value::Number(*n));
        }
    }
    s
}

fn nums(vals: &[f64]) -> Sheet {
    let mut s = Sheet::new("t");
    for (r, v) in vals.iter().enumerate() {
        s.set(CellRef::new(r as u32, 0), Value::Number(*v));
    }
    s
}

fn ev(formula: &str, sheet: &Sheet) -> Value {
    eval(&parse(formula).unwrap(), sheet)
}

#[track_caller]
fn num(formula: &str, sheet: &Sheet) -> f64 {
    match ev(formula, sheet) {
        Value::Number(n) => n,
        other => panic!("{formula} produced {other:?}, expected a number"),
    }
}

#[track_caller]
fn err(formula: &str, sheet: &Sheet) -> ErrorKind {
    match ev(formula, sheet) {
        Value::Error(e) => e,
        other => panic!("{formula} produced {other:?}, expected an error"),
    }
}

// --- dispatch -------------------------------------------------------------

#[test]
fn every_scoped_name_is_routed() {
    // Guards the single delegating arm in eval.rs: if the arm is dropped or
    // a name is misspelled here, these become #NAME? instead of answers.
    let s = nums(&[1.0, 2.0, 2.0, 3.0, 8.0]);
    for f in [
        "=MEDIAN(A1:A5)",
        "=MODE(A1:A5)",
        "=STDEV.P(A1:A5)",
        "=STDEV.S(A1:A5)",
        "=VAR.P(A1:A5)",
        "=VAR.S(A1:A5)",
        "=PERCENTILE.INC(A1:A5,0.5)",
        "=QUARTILE.INC(A1:A5,1)",
        "=RANK(2,A1:A5)",
        "=LARGE(A1:A5,1)",
        "=SMALL(A1:A5,1)",
    ] {
        let v = ev(f, &s);
        assert!(
            !matches!(v, Value::Error(ErrorKind::Name)),
            "{f} was not routed to the stats module: {v:?}"
        );
    }
}

// --- MEDIAN ---------------------------------------------------------------

#[test]
fn median_odd_and_even() {
    // Deliberately unsorted so a "return the middle element as stored"
    // implementation fails.
    let s = nums(&[7.0, 1.0, 5.0, 3.0, 9.0]);
    assert_eq!(num("=MEDIAN(A1:A5)", &s), 5.0);

    let s = nums(&[7.0, 1.0, 5.0, 3.0]);
    assert_eq!(num("=MEDIAN(A1:A4)", &s), 4.0); // (3 + 5) / 2

    let s = nums(&[42.0]);
    assert_eq!(num("=MEDIAN(A1:A1)", &s), 42.0);
}

#[test]
fn median_uses_selection_not_a_sort() {
    // Selection leaves the buffer only partitioned. If someone swaps the
    // implementation for `sort()` + index, the slice comes back fully
    // ordered and this fails — which is the point: it pins the algorithm,
    // not just the answer.
    let mut vals: Vec<f64> = (0..1001).map(|i| ((i * 7919) % 1001) as f64).collect();
    let med = median_of(&mut vals).unwrap();
    assert_eq!(med, 500.0, "median of 0..=1000 is 500");
    let sorted = vals.windows(2).all(|w| w[0] <= w[1]);
    assert!(
        !sorted,
        "buffer came back fully sorted; the no-full-sort property is gone"
    );
}

#[test]
fn median_skips_text_and_blanks() {
    // 1, <text>, <blank>, 3 -> median is 2. Coercing text/blank to 0 would
    // give 0.5, and counting them as values would give 0.5 or 1.
    let mut s = col_a(&[Some(1.0), None, None, Some(3.0)]);
    s.set_text(CellRef::new(1, 0), "hello");
    assert_eq!(num("=MEDIAN(A1:A4)", &s), 2.0);
    assert_eq!(num("=COUNT(A1:A4)", &s), 2.0, "sanity: only two numerics");
}

#[test]
fn empty_input_is_num_error() {
    let s = Sheet::new("t");
    for f in [
        "=MEDIAN(A1:A10)",
        "=MODE(A1:A10)",
        "=STDEV.P(A1:A10)",
        "=STDEV.S(A1:A10)",
        "=VAR.P(A1:A10)",
        "=VAR.S(A1:A10)",
        "=PERCENTILE.INC(A1:A10,0.5)",
        "=QUARTILE.INC(A1:A10,1)",
        "=LARGE(A1:A10,1)",
        "=SMALL(A1:A10,1)",
    ] {
        assert_eq!(err(f, &s), ErrorKind::Num, "{f}");
    }
}

// --- MODE -----------------------------------------------------------------

#[test]
fn mode_picks_most_frequent() {
    let s = nums(&[4.0, 1.0, 2.0, 2.0, 3.0, 2.0, 4.0]);
    assert_eq!(num("=MODE(A1:A7)", &s), 2.0);
}

#[test]
fn mode_ties_go_to_first_occurrence() {
    // 3 and 5 both appear twice; Excel returns the one seen first.
    let s = nums(&[5.0, 3.0, 3.0, 5.0]);
    assert_eq!(num("=MODE(A1:A4)", &s), 5.0);
}

#[test]
fn mode_with_no_repeat_is_na() {
    let s = nums(&[1.0, 2.0, 3.0, 4.0]);
    assert_eq!(err("=MODE(A1:A4)", &s), ErrorKind::NotAvailable);
}

// --- variance / stdev -----------------------------------------------------

#[test]
fn variance_matches_known_values() {
    // 2, 4, 4, 4, 5, 5, 7, 9: the textbook example.
    // population variance 4, population stdev 2, sample variance 32/7.
    let s = nums(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
    assert!((num("=VAR.P(A1:A8)", &s) - 4.0).abs() < 1e-12);
    assert!((num("=STDEV.P(A1:A8)", &s) - 2.0).abs() < 1e-12);
    assert!((num("=VAR.S(A1:A8)", &s) - 32.0 / 7.0).abs() < 1e-12);
    assert!((num("=STDEV.S(A1:A8)", &s) - (32.0f64 / 7.0).sqrt()).abs() < 1e-12);
}

#[test]
fn variance_is_numerically_stable() {
    // Five values around 1e9 differing by 1. True sample variance is 2.5,
    // true population variance is 2.0.
    let vals: Vec<f64> = (0..5).map(|i| 1e9 + i as f64).collect();
    let s = nums(&vals);

    // What the naive E[x^2] - E[x]^2 form produces on exactly this input.
    // Computed here rather than asserted from memory, so the test proves it
    // instead of claiming it.
    let n = vals.len() as f64;
    let sum: f64 = vals.iter().sum();
    let sum_sq: f64 = vals.iter().map(|v| v * v).sum();
    let naive_pop = sum_sq / n - (sum / n) * (sum / n);
    let naive_sample = (sum_sq - sum * sum / n) / (n - 1.0);
    assert_eq!(
        naive_pop, 0.0,
        "the naive population form must actually fail here, or this test \
         does not distinguish the two methods"
    );
    assert_eq!(
        naive_sample, 0.0,
        "the naive sample form must actually fail here, or this test does \
         not distinguish the two methods"
    );

    // Welford gets it right.
    assert!(
        (num("=VAR.S(A1:A5)", &s) - 2.5).abs() < 1e-9,
        "VAR.S was {}, want 2.5 (naive gives {naive_sample})",
        num("=VAR.S(A1:A5)", &s)
    );
    assert!((num("=VAR.P(A1:A5)", &s) - 2.0).abs() < 1e-9);
    assert!((num("=STDEV.S(A1:A5)", &s) - 2.5f64.sqrt()).abs() < 1e-9);
    assert!((num("=STDEV.P(A1:A5)", &s) - 2.0f64.sqrt()).abs() < 1e-9);
}

#[test]
fn variance_skips_text_and_blanks() {
    // 2, <text>, <blank>, 4, 4, 4, 5, 5, 7, 9 -> same as the textbook set.
    let mut s = col_a(&[
        Some(2.0),
        None,
        None,
        Some(4.0),
        Some(4.0),
        Some(4.0),
        Some(5.0),
        Some(5.0),
        Some(7.0),
        Some(9.0),
    ]);
    s.set_text(CellRef::new(1, 0), "n/a");
    assert!((num("=VAR.P(A1:A10)", &s) - 4.0).abs() < 1e-12);
}

#[test]
fn sample_variance_of_one_value_is_div_zero() {
    let s = nums(&[5.0]);
    assert_eq!(err("=VAR.S(A1:A1)", &s), ErrorKind::DivZero);
    assert_eq!(err("=STDEV.S(A1:A1)", &s), ErrorKind::DivZero);
    assert_eq!(num("=VAR.P(A1:A1)", &s), 0.0);
}

// --- PERCENTILE.INC / QUARTILE.INC ---------------------------------------

#[test]
fn percentile_boundaries_and_interpolation() {
    // 1, 2, 3, 4: sorted rank = k * (n - 1) = k * 3.
    let s = nums(&[4.0, 1.0, 3.0, 2.0]);
    assert_eq!(num("=PERCENTILE.INC(A1:A4,0)", &s), 1.0, "k=0 is the min");
    assert_eq!(num("=PERCENTILE.INC(A1:A4,1)", &s), 4.0, "k=1 is the max");
    // pos = 1.5 -> halfway between sorted[1]=2 and sorted[2]=3.
    assert_eq!(num("=PERCENTILE.INC(A1:A4,0.5)", &s), 2.5);
    // pos = 0.75 -> 1 + 0.75 * (2 - 1) = 1.75. Rounding to a whole rank
    // would give 1 or 2; truncating would give 1.
    assert_eq!(num("=PERCENTILE.INC(A1:A4,0.25)", &s), 1.75);
    // pos = 2.25 -> 3 + 0.25 * (4 - 3) = 3.25.
    assert_eq!(num("=PERCENTILE.INC(A1:A4,0.75)", &s), 3.25);
    // pos = 1.0 exactly -> lands on a rank, no interpolation.
    assert_eq!(num("=PERCENTILE.INC(A1:A4,0.3333333333333333)", &s), 2.0);
}

#[test]
fn percentile_out_of_range_is_num() {
    let s = nums(&[1.0, 2.0, 3.0, 4.0]);
    assert_eq!(err("=PERCENTILE.INC(A1:A4,-0.1)", &s), ErrorKind::Num);
    assert_eq!(err("=PERCENTILE.INC(A1:A4,1.1)", &s), ErrorKind::Num);
}

#[test]
fn quartile_matches_percentile() {
    // 1..4: Q0=1, Q1=1.75, Q2=2.5, Q3=3.25, Q4=4 — the same points
    // PERCENTILE.INC produces at 0, .25, .5, .75, 1.
    let s = nums(&[4.0, 1.0, 3.0, 2.0]);
    assert_eq!(num("=QUARTILE.INC(A1:A4,0)", &s), 1.0);
    assert_eq!(num("=QUARTILE.INC(A1:A4,1)", &s), 1.75);
    assert_eq!(num("=QUARTILE.INC(A1:A4,2)", &s), 2.5);
    assert_eq!(num("=QUARTILE.INC(A1:A4,3)", &s), 3.25);
    assert_eq!(num("=QUARTILE.INC(A1:A4,4)", &s), 4.0);
    assert_eq!(err("=QUARTILE.INC(A1:A4,5)", &s), ErrorKind::Num);
    assert_eq!(err("=QUARTILE.INC(A1:A4,-1)", &s), ErrorKind::Num);
}

#[test]
fn percentile_over_a_large_shuffled_range_is_exact() {
    // 1000 values in scrambled order; every decile must be exact, which a
    // partition-order-dependent bug would break.
    let vals: Vec<f64> = (0..1000).map(|i| ((i * 337) % 1000) as f64).collect();
    let s = nums(&vals);
    for d in 0..=10 {
        let k = d as f64 / 10.0;
        let got = num(&format!("=PERCENTILE.INC(A1:A1000,{k})"), &s);
        let want = k * 999.0;
        assert!(
            (got - want).abs() < 1e-9,
            "decile {d}: got {got}, want {want}"
        );
    }
}

// --- LARGE / SMALL --------------------------------------------------------

#[test]
fn large_and_small() {
    let s = nums(&[7.0, 1.0, 5.0, 3.0, 9.0]);
    assert_eq!(num("=LARGE(A1:A5,1)", &s), 9.0);
    assert_eq!(num("=LARGE(A1:A5,2)", &s), 7.0);
    assert_eq!(num("=LARGE(A1:A5,5)", &s), 1.0);
    assert_eq!(num("=SMALL(A1:A5,1)", &s), 1.0);
    assert_eq!(num("=SMALL(A1:A5,2)", &s), 3.0);
    assert_eq!(num("=SMALL(A1:A5,5)", &s), 9.0);
}

#[test]
fn large_small_k_out_of_range_is_num() {
    let s = nums(&[7.0, 1.0, 5.0, 3.0, 9.0]);
    for f in [
        "=LARGE(A1:A5,0)",
        "=LARGE(A1:A5,6)",
        "=LARGE(A1:A5,-1)",
        "=SMALL(A1:A5,0)",
        "=SMALL(A1:A5,6)",
        "=SMALL(A1:A5,-1)",
    ] {
        assert_eq!(err(f, &s), ErrorKind::Num, "{f}");
    }
}

#[test]
fn large_small_ignore_text_cells_when_bounding_k() {
    // Two numerics among five cells: k=3 must be #NUM!, not a read of a
    // text cell coerced to zero.
    let mut s = col_a(&[Some(10.0), None, Some(20.0), None, None]);
    s.set_text(CellRef::new(1, 0), "x");
    s.set_text(CellRef::new(3, 0), "y");
    assert_eq!(num("=LARGE(A1:A5,1)", &s), 20.0);
    assert_eq!(num("=SMALL(A1:A5,2)", &s), 20.0);
    assert_eq!(err("=LARGE(A1:A5,3)", &s), ErrorKind::Num);
}

// --- RANK -----------------------------------------------------------------

#[test]
fn rank_descending_by_default() {
    let s = nums(&[10.0, 20.0, 30.0, 40.0, 50.0]);
    assert_eq!(num("=RANK(50,A1:A5)", &s), 1.0);
    assert_eq!(num("=RANK(30,A1:A5)", &s), 3.0);
    assert_eq!(num("=RANK(10,A1:A5)", &s), 5.0);
}

#[test]
fn rank_ascending_with_order_arg() {
    let s = nums(&[10.0, 20.0, 30.0, 40.0, 50.0]);
    assert_eq!(num("=RANK(10,A1:A5,1)", &s), 1.0);
    assert_eq!(num("=RANK(30,A1:A5,1)", &s), 3.0);
    assert_eq!(num("=RANK(50,A1:A5,1)", &s), 5.0);
    // 0 means descending, and must differ from the ascending answer or the
    // order argument is being ignored.
    assert_eq!(num("=RANK(10,A1:A5,0)", &s), 5.0);
}

#[test]
fn rank_ties_share_the_top_rank() {
    // Excel: equal values all take the best rank, and the next distinct
    // value skips. 30, 30, 20, 10 -> both 30s rank 1, 20 ranks 3.
    let s = nums(&[30.0, 30.0, 20.0, 10.0]);
    assert_eq!(num("=RANK(30,A1:A4)", &s), 1.0);
    assert_eq!(num("=RANK(20,A1:A4)", &s), 3.0);
    assert_eq!(num("=RANK(10,A1:A4)", &s), 4.0);
}

#[test]
fn rank_of_absent_value_is_na() {
    let s = nums(&[10.0, 20.0, 30.0]);
    assert_eq!(err("=RANK(25,A1:A3)", &s), ErrorKind::NotAvailable);
}

#[test]
fn rank_skips_text_cells() {
    let mut s = col_a(&[Some(10.0), None, Some(30.0)]);
    s.set_text(CellRef::new(1, 0), "20");
    // If "20" were coerced, RANK(30,...) would still be 1, so probe the
    // text value itself: it must be absent, not ranked.
    assert_eq!(err("=RANK(20,A1:A3)", &s), ErrorKind::NotAvailable);
    assert_eq!(num("=RANK(10,A1:A3)", &s), 2.0);
}

// --- errors and edges -----------------------------------------------------

#[test]
fn error_cells_propagate() {
    let mut s = nums(&[1.0, 2.0, 3.0]);
    s.set(CellRef::new(1, 0), Value::Error(ErrorKind::DivZero));
    for f in [
        "=MEDIAN(A1:A3)",
        "=MODE(A1:A3)",
        "=VAR.P(A1:A3)",
        "=STDEV.S(A1:A3)",
        "=PERCENTILE.INC(A1:A3,0.5)",
        "=LARGE(A1:A3,1)",
        "=RANK(1,A1:A3)",
    ] {
        assert_eq!(err(f, &s), ErrorKind::DivZero, "{f}");
    }
}

#[test]
fn bare_arguments_work_alongside_ranges() {
    let s = nums(&[1.0, 2.0]);
    // 1, 2 from the range plus the literals 3 and 4.
    assert_eq!(num("=MEDIAN(A1:A2,3,4)", &s), 2.5);
    assert_eq!(num("=VAR.P(A1:A2,3,4)", &s), 1.25);
}

#[test]
fn wrong_arity_is_value_error() {
    let s = nums(&[1.0, 2.0, 3.0]);
    for f in [
        "=MEDIAN()",
        "=PERCENTILE.INC(A1:A3)",
        "=LARGE(A1:A3)",
        "=RANK(1)",
        "=RANK(1,A1:A3,0,9)",
    ] {
        assert_eq!(err(f, &s), ErrorKind::Value, "{f}");
    }
}

// --- the buffer cap -------------------------------------------------------

#[test]
fn buffer_cap_refuses_rather_than_truncating() {
    // The real cap is MAX_BUFFERED_VALUES; injecting a small one exercises
    // the same refusal path without materialising 16M cells. Truncating
    // instead would return a plausible, wrong median.
    let s = nums(&[1.0, 2.0, 3.0, 4.0, 5.0]);
    let args = match parse("=MEDIAN(A1:A5)").unwrap() {
        Expr::Call(_, args) => args,
        other => panic!("expected a call, got {other:?}"),
    };
    assert_eq!(collect_capped(&args, &s, 3).unwrap_err(), ErrorKind::Num);
    assert_eq!(collect_capped(&args, &s, 5).unwrap().len(), 5);
    // The documented promise: the cap covers the 10M-row column the issue
    // names, so that case answers rather than refusing.
    const _: () = assert!(MAX_BUFFERED_VALUES >= 10_000_000);
}
