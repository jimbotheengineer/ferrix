//! Cross-family composition tests for the formula function library.
//!
//! Each of the text (#24), date (#25) and statistical (#26) families landed on
//! its own branch, in its own module, behind its own guarded arm in
//! `eval_call`'s match. Every one of those branches was individually green —
//! and no single one of them could exercise the thing that only exists after
//! all three are merged: **one dispatch table shared by three modules**.
//!
//! That seam is exactly where a silent defect would live. `eval_call` matches
//! guard arms IN ORDER, so if two families claimed the same name, or if one
//! family's `is_*_fn` predicate were over-broad, the earlier arm would swallow
//! the later one's call and the loser's own tests would still pass in isolation
//! — they never route through the merged match.
//!
//! What these assert if the merge were broken: a swallowed name evaluates in
//! the wrong module and returns `#NAME?` or a wrong-typed result, and a
//! genuinely composed expression returns the wrong number. Nothing here would
//! pass against a dispatch table that lost a family.

use ferrix_core::{CellRef, ErrorKind, Sheet, Value};

use crate::{eval, parse};

fn v(sheet: &Sheet, f: &str) -> Value {
    eval(
        &parse(f).unwrap_or_else(|e| panic!("parse {f}: {e}")),
        sheet,
    )
}

fn num(sheet: &Sheet, f: &str) -> f64 {
    match v(sheet, f) {
        Value::Number(n) => n,
        other => panic!("{f} = {other:?}, wanted a number"),
    }
}

fn text(sheet: &Sheet, f: &str) -> String {
    match v(sheet, f) {
        Value::Text(id) => sheet.resolve(id).to_string(),
        other => panic!("{f} = {other:?}, wanted text"),
    }
}

/// A1:A5 numbers, B1 text, C1 a date serial.
fn fixture() -> Sheet {
    let mut s = Sheet::new("compose");
    for (i, n) in [10.0, 20.0, 30.0, 40.0, 50.0].iter().enumerate() {
        s.set(CellRef::new(i as u32, 0), Value::Number(*n));
    }
    s.set_text(CellRef::new(0, 1), "  widget  ");
    // 2024-03-15 as an Excel serial.
    s.set(CellRef::new(0, 2), Value::Number(45366.0));
    s
}

/// The three families must all still be reachable through the ONE match in
/// `eval_call`. If a merge dropped an arm, or an earlier guard swallowed a
/// later family's names, the corresponding line here returns `#NAME?`.
#[test]
fn all_three_function_families_are_reachable_through_one_dispatch() {
    let s = fixture();

    // text (#24)
    assert_eq!(text(&s, "=TRIM(B1)"), "widget");
    // date (#25)
    assert_eq!(num(&s, "=YEAR(C1)"), 2024.0);
    // statistics (#26)
    assert_eq!(num(&s, "=MEDIAN(A1:A5)"), 30.0);
    // and the pre-existing built-ins were not displaced by the new guard arms
    assert_eq!(num(&s, "=SUM(A1:A5)"), 150.0);
    assert_eq!(num(&s, "=COUNTIF(A1:A5,\">20\")"), 3.0);
}

/// No name may be claimed by two families. A guard arm that matched too
/// broadly would win for every name it over-claimed, and the losing family's
/// own tests could never see it — they do not route through this match.
#[test]
fn no_function_name_is_claimed_by_more_than_one_family() {
    // Every name the three modules answer for, checked pairwise.
    let names: Vec<&str> = [
        "LEFT",
        "RIGHT",
        "MID",
        "LEN",
        "UPPER",
        "LOWER",
        "PROPER",
        "TRIM",
        "CLEAN",
        "SUBSTITUTE",
        "REPLACE",
        "FIND",
        "SEARCH",
        "CONCAT",
        "CONCATENATE",
        "TEXTJOIN",
        "TEXT",
        "VALUE",
        "REPT",
        "TODAY",
        "NOW",
        "DATE",
        "YEAR",
        "MONTH",
        "DAY",
        "HOUR",
        "MINUTE",
        "SECOND",
        "WEEKDAY",
        "EOMONTH",
        "EDATE",
        "DATEDIF",
        "DAYS",
        "NETWORKDAYS",
        "MEDIAN",
        "MODE",
        "STDEV.P",
        "STDEV.S",
        "VAR.P",
        "VAR.S",
        "PERCENTILE.INC",
        "QUARTILE.INC",
        "RANK",
        "LARGE",
        "SMALL",
    ]
    .to_vec();

    for name in names {
        let claims = [
            ("text", crate::text::is_text_fn(name)),
            ("datetime", crate::datetime::is_date_fn(name)),
            ("stats", crate::stats::is_stat_fn(name)),
        ];
        let owners: Vec<&str> = claims.iter().filter(|(_, y)| *y).map(|(m, _)| *m).collect();
        assert_eq!(
            owners.len(),
            1,
            "{name} is claimed by {owners:?}; exactly one module must own each name, \
             because eval_call's guard arms match in order and the first claimant wins"
        );
    }
}

/// The families must compose as ARGUMENTS to one another, not merely coexist.
/// Each of these routes a value out of one module and into another through the
/// shared `Value` representation.
#[test]
fn the_families_compose_as_arguments_to_one_another() {
    let s = fixture();

    // stats -> text: a computed median rendered by the text formatter.
    assert_eq!(text(&s, "=TEXT(MEDIAN(A1:A5),\"0.00\")"), "30.00");

    // date -> stats: MEDIAN over date serials is itself a date serial, and
    // YEAR of it round-trips. (One-element range keeps the expectation exact.)
    assert_eq!(num(&s, "=YEAR(MEDIAN(C1:C1))"), 2024.0);

    // text -> stats: LEN feeding an aggregate.
    assert_eq!(num(&s, "=LARGE(A1:A5,LEN(\"ab\"))"), 40.0);

    // date -> text: a date part concatenated into a string.
    assert_eq!(text(&s, "=CONCAT(\"Y\",TEXT(YEAR(C1),\"0\"))"), "Y2024");
}

/// An unknown name must still fall through all three guard arms to `#NAME?`.
/// If any family's predicate returned true too eagerly this would come back as
/// some other error, or worse, a plausible-looking value.
#[test]
fn an_unknown_function_still_falls_through_to_a_name_error() {
    let s = fixture();
    assert_eq!(v(&s, "=NOTAFUNCTION(A1)"), Value::Error(ErrorKind::Name));
    assert_eq!(v(&s, "=LEFTISH(B1,2)"), Value::Error(ErrorKind::Name));
}
