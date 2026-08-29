//! Unit tests for the date/time function library.
//!
//! Every expected value here is an *independently known* Excel answer (serial
//! 45000 = 2023-03-15 is already pinned by `ferrix-core`'s own suite), not a
//! number read back out of this implementation. Each assertion is written so
//! that a function which did nothing — returned zero, echoed its argument, or
//! ignored an optional parameter — would fail it.

use super::*;
use crate::parser::parse;
use ferrix_core::table::{render_serial, DATE_SERIAL_MAX};
use ferrix_core::{CellRef, DateStyle, Sheet};

/// Serial anchors, cross-checked against Excel's 1900 system.
const D2023_03_15: f64 = 45_000.0; // a Wednesday
const D2023_01_31: f64 = 44_957.0;
const D2023_02_28: f64 = 44_985.0;
const D2023_03_31: f64 = 45_016.0;
const D2024_01_31: f64 = 45_322.0;
const D2024_02_29: f64 = 45_351.0;
const D2023_03_13_MON: f64 = 44_998.0;
const D2023_03_24_FRI: f64 = 45_009.0;

fn ev(formula: &str, sheet: &Sheet) -> Value {
    eval_view(&parse(formula).unwrap(), sheet)
}

/// Evaluate to a number, failing loudly with the formula text on anything else.
fn n(formula: &str, sheet: &Sheet) -> f64 {
    match ev(formula, sheet) {
        Value::Number(v) => v,
        other => panic!("{formula} produced {other:?}, expected a number"),
    }
}

fn empty() -> Sheet {
    Sheet::new("t")
}

/// Restores the wall clock when it drops, so a panicking test cannot leave a
/// frozen clock behind for the next test on this thread.
struct FrozenClock;

impl FrozenClock {
    fn at(serial: f64) -> Self {
        set_test_clock(Some(serial));
        FrozenClock
    }
}

impl Drop for FrozenClock {
    fn drop(&mut self) {
        set_test_clock(None);
    }
}

// ------------------------------------------------------------ the clock ---

#[test]
fn today_and_now_read_the_injected_clock() {
    // The whole point: an exact equality, not "TODAY() > 0" — which would
    // pass against a function that always returned 1.
    let _clock = FrozenClock::at(45_000.75);
    let s = empty();
    assert_eq!(n("=NOW()", &s), 45_000.75);
    assert_eq!(n("=TODAY()", &s), 45_000.0, "TODAY drops the time of day");
    // And the injected instant really is what the rest of the library sees.
    assert_eq!(n("=YEAR(NOW())", &s), 2023.0);
    assert_eq!(n("=HOUR(NOW())", &s), 18.0);
}

#[test]
fn releasing_the_clock_restores_the_wall_clock() {
    {
        let _clock = FrozenClock::at(1.0);
        assert_eq!(n("=TODAY()", &empty()), 1.0);
    }
    // 1900-01-01 is not today. If the override leaked, this fails.
    let today = n("=TODAY()", &empty());
    assert!(
        today > 44_000.0 && today <= DATE_SERIAL_MAX,
        "wall-clock TODAY() gave serial {today}, which is not a plausible date"
    );
}

#[test]
fn today_and_now_take_no_arguments() {
    let s = empty();
    assert_eq!(ev("=TODAY(1)", &s), Value::Error(ErrorKind::Value));
    assert_eq!(ev("=NOW(1)", &s), Value::Error(ErrorKind::Value));
}

// ------------------------------------------------------ DATE and parts ---

#[test]
fn date_builds_the_serial_excel_builds() {
    let s = empty();
    assert_eq!(n("=DATE(2023,3,15)", &s), D2023_03_15);
    assert_eq!(n("=DATE(1900,1,1)", &s), 1.0);
    assert_eq!(n("=DATE(1970,1,1)", &s), 25_569.0);
    assert_eq!(n("=DATE(9999,12,31)", &s), DATE_SERIAL_MAX.floor());
    assert_eq!(n("=DATE(2023,1,31)", &s), D2023_01_31);
    assert_eq!(n("=DATE(2024,1,31)", &s), D2024_01_31);
    assert_eq!(n("=DATE(2023,3,24)", &s), D2023_03_24_FRI);
    // Excel's two-digit-year shorthand: a year in 0..=1899 means 1900 + y,
    // so DATE(23,3,15) is *1923*, not 2023. Reproduced deliberately.
    assert_eq!(n("=DATE(23,3,15)", &s), n("=DATE(1923,3,15)", &s));
    assert_eq!(n("=DATE(0,1,1)", &s), 1.0);
}

#[test]
fn date_rolls_over_out_of_range_months_and_days() {
    let s = empty();
    // 2023-13-01 is 2024-01-01, not an error.
    assert_eq!(n("=DATE(2023,13,1)", &s), n("=DATE(2024,1,1)", &s));
    assert_eq!(n("=DATE(2023,0,1)", &s), n("=DATE(2022,12,1)", &s));
    assert_eq!(n("=DATE(2023,1,32)", &s), n("=DATE(2023,2,1)", &s));
    // 2024 is a leap year, so day 30 of February is 1 March.
    assert_eq!(n("=DATE(2024,2,30)", &s), n("=DATE(2024,3,1)", &s));
    assert_eq!(n("=DATE(2023,3,0)", &s), n("=DATE(2023,2,28)", &s));
}

#[test]
fn date_and_the_parts_are_inverses_across_the_whole_range() {
    let s = empty();
    // Sampled rather than exhaustive so the test stays fast, but it crosses
    // the phantom day, every century rule, and both ends of the range.
    for serial in [
        1.0,
        59.0,
        60.0,
        61.0,
        100.0,
        25_569.0,
        43_890.0,
        45_000.0,
        2_958_465.0,
    ] {
        let (y, m, d, ..) = serial_parts(serial);
        let back = n(&format!("=DATE({y},{m},{d})"), &s);
        assert_eq!(back, serial, "DATE round trip broke at serial {serial}");
    }
}

#[test]
fn out_of_range_dates_are_num_errors() {
    let s = empty();
    // Below serial 0 — the 1900 system has no earlier day.
    assert_eq!(ev("=DATE(1900,1,-1)", &s), Value::Error(ErrorKind::Num));
    assert_eq!(ev("=DATE(1900,1,1-5)", &s), Value::Error(ErrorKind::Num));
    // Past 9999-12-31, the last date xlsx can express.
    assert_eq!(ev("=DATE(10000,1,1)", &s), Value::Error(ErrorKind::Num));
    assert_eq!(ev("=DATE(9999,12,32)", &s), Value::Error(ErrorKind::Num));
    // A serial that is not a date at all.
    assert_eq!(ev("=YEAR(-1)", &s), Value::Error(ErrorKind::Num));
    assert_eq!(ev("=WEEKDAY(3000000)", &s), Value::Error(ErrorKind::Num));
    // Wrong arity is #VALUE!, distinct from an out-of-range value.
    assert_eq!(ev("=DATE(2023,3)", &s), Value::Error(ErrorKind::Value));
}

#[test]
fn year_month_day_decompose_a_serial() {
    let s = empty();
    assert_eq!(n("=YEAR(45000)", &s), 2023.0);
    assert_eq!(n("=MONTH(45000)", &s), 3.0);
    assert_eq!(n("=DAY(45000)", &s), 15.0);
    // The time component must not disturb the date.
    assert_eq!(n("=DAY(45000.99)", &s), 15.0);
}

#[test]
fn hour_minute_second_decompose_the_fraction() {
    let s = empty();
    assert_eq!(n("=HOUR(45000.5)", &s), 12.0);
    assert_eq!(n("=MINUTE(45000.5)", &s), 0.0);
    assert_eq!(n("=HOUR(45000.25)", &s), 6.0);
    // 13:45:30 = (13*3600 + 45*60 + 30) / 86400.
    let t = 45_000.0 + (13.0 * 3600.0 + 45.0 * 60.0 + 30.0) / 86_400.0;
    let mut sheet = empty();
    sheet.set(CellRef::new(0, 0), Value::Number(t));
    assert_eq!(n("=HOUR(A1)", &sheet), 13.0);
    assert_eq!(n("=MINUTE(A1)", &sheet), 45.0);
    assert_eq!(n("=SECOND(A1)", &sheet), 30.0);
}

// --------------------------------------------- Excel's 1900-02-29 bug ---

#[test]
fn serial_60_is_excels_phantom_1900_02_29() {
    let s = empty();
    // The invariant that ties this module to the renderer: whatever
    // render_serial prints for a serial, the functions must agree with.
    assert_eq!(render_serial(60.0, DateStyle::Iso), "1900-02-29");
    assert_eq!(n("=YEAR(60)", &s), 1900.0);
    assert_eq!(n("=MONTH(60)", &s), 2.0);
    assert_eq!(n("=DAY(60)", &s), 29.0);
    // ...and the inverse direction agrees too.
    assert_eq!(n("=DATE(1900,2,29)", &s), 60.0);
    // 59 and 61 are the real days either side of it.
    assert_eq!(render_serial(59.0, DateStyle::Iso), "1900-02-28");
    assert_eq!(render_serial(61.0, DateStyle::Iso), "1900-03-01");
    assert_eq!(n("=DATE(1900,2,28)", &s), 59.0);
    assert_eq!(n("=DATE(1900,3,1)", &s), 61.0);
    // The one-day discontinuity: 1900-03-01 minus 1900-02-28 is 2 serials,
    // because the phantom day sits between them. This is the bug, reproduced
    // on purpose so serials agree with Excel's.
    assert_eq!(n("=DAYS(DATE(1900,3,1),DATE(1900,2,28))", &s), 2.0);
}

#[test]
fn every_function_agrees_with_render_serial_about_the_calendar() {
    // Drift guard against a second calendar sneaking in: for a spread of
    // serials, YEAR/MONTH/DAY must reconstruct exactly the ISO string the
    // renderer paints into the cell.
    let s = empty();
    for serial in [
        1.0,
        32.0,
        59.0,
        60.0,
        61.0,
        366.0,
        25_569.0,
        45_000.0,
        2_958_465.0,
    ] {
        let painted = render_serial(serial, DateStyle::Iso);
        let computed = format!(
            "{:04}-{:02}-{:02}",
            n(&format!("=YEAR({serial})"), &s) as i64,
            n(&format!("=MONTH({serial})"), &s) as i64,
            n(&format!("=DAY({serial})"), &s) as i64,
        );
        assert_eq!(
            computed, painted,
            "calendar disagreement at serial {serial}"
        );
    }
}

// ---------------------------------------------------------- WEEKDAY ---

#[test]
fn weekday_return_types() {
    let s = empty();
    // 2023-03-15 is a Wednesday. Type 1: Sunday=1, so Wednesday=4.
    assert_eq!(n("=WEEKDAY(45000)", &s), 4.0);
    assert_eq!(n("=WEEKDAY(45000,1)", &s), 4.0);
    // Type 2: Monday=1, so Wednesday=3.
    assert_eq!(n("=WEEKDAY(45000,2)", &s), 3.0);
    // Type 3: Monday=0, so Wednesday=2.
    assert_eq!(n("=WEEKDAY(45000,3)", &s), 2.0);
    // The types must genuinely differ — a WEEKDAY that ignored its second
    // argument would give the same answer three times.
    assert_ne!(n("=WEEKDAY(45000,1)", &s), n("=WEEKDAY(45000,2)", &s));
    assert_ne!(n("=WEEKDAY(45000,2)", &s), n("=WEEKDAY(45000,3)", &s));
}

#[test]
fn weekday_covers_a_whole_week_in_every_type() {
    let s = empty();
    // 2023-03-13 is a Monday; walk forward seven days.
    let expected = [
        (1.0, [2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 1.0]), // Sun = 1
        (2.0, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]), // Mon = 1
        (3.0, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), // Mon = 0
    ];
    for (kind, want) in expected {
        for (i, w) in want.iter().enumerate() {
            let serial = D2023_03_13_MON + i as f64;
            assert_eq!(
                n(&format!("=WEEKDAY({serial},{kind})"), &s),
                *w,
                "WEEKDAY({serial},{kind})"
            );
        }
    }
}

#[test]
fn unknown_weekday_return_type_is_a_num_error() {
    let s = empty();
    assert_eq!(ev("=WEEKDAY(45000,4)", &s), Value::Error(ErrorKind::Num));
    assert_eq!(ev("=WEEKDAY(45000,0)", &s), Value::Error(ErrorKind::Num));
}

// ------------------------------------------------- EDATE and EOMONTH ---

#[test]
fn edate_clamps_to_the_month_end_instead_of_overflowing() {
    let s = empty();
    // The acceptance criterion: 31 Jan + 1 month is 28 Feb, NOT 3 March.
    assert_eq!(n("=EDATE(44957,1)", &s), D2023_02_28);
    assert_eq!(
        render_serial(n("=EDATE(44957,1)", &s), DateStyle::Iso),
        "2023-02-28"
    );
    // Leap year: the same shift lands on the 29th.
    assert_eq!(n("=EDATE(45322,1)", &s), D2024_02_29);
    // A day that exists in the target month is preserved exactly.
    assert_eq!(n("=EDATE(45000,1)", &s), n("=DATE(2023,4,15)", &s));
    assert_eq!(n("=EDATE(45000,-1)", &s), n("=DATE(2023,2,15)", &s));
    assert_eq!(n("=EDATE(45000,12)", &s), n("=DATE(2024,3,15)", &s));
    assert_eq!(n("=EDATE(45000,0)", &s), D2023_03_15);
    // Crossing a year boundary backwards.
    assert_eq!(n("=EDATE(45000,-14)", &s), n("=DATE(2022,1,15)", &s));
}

#[test]
fn eomonth_lands_on_the_last_day_of_the_target_month() {
    let s = empty();
    assert_eq!(n("=EOMONTH(45000,0)", &s), D2023_03_31);
    // 31 Jan + 1 month: the end of February, short month and all.
    assert_eq!(n("=EOMONTH(44957,1)", &s), D2023_02_28);
    assert_eq!(n("=EOMONTH(45322,1)", &s), D2024_02_29);
    assert_eq!(n("=EOMONTH(45000,-1)", &s), n("=DATE(2023,2,28)", &s));
    assert_eq!(n("=EOMONTH(45000,1)", &s), n("=DATE(2023,4,30)", &s));
    // Independent of the day within the source month.
    assert_eq!(n("=EOMONTH(DATE(2023,3,1),0)", &s), D2023_03_31);
    assert_eq!(n("=EOMONTH(DATE(2023,3,31),0)", &s), D2023_03_31);
    // A month with 31 days after one with 28: not an off-by-one.
    assert_eq!(
        n("=EOMONTH(DATE(2023,2,28),1)", &s),
        n("=DATE(2023,3,31)", &s)
    );
}

#[test]
fn eomonth_result_renders_as_the_month_end() {
    // The serial is only right if the painted date is right.
    let s = empty();
    for (start, shift, want) in [
        ("DATE(2023,1,31)", 1, "2023-02-28"),
        ("DATE(2024,1,31)", 1, "2024-02-29"),
        ("DATE(2023,3,15)", 0, "2023-03-31"),
        ("DATE(2023,12,1)", 1, "2024-01-31"),
        ("DATE(2023,1,1)", -1, "2022-12-31"),
    ] {
        let got = n(&format!("=EOMONTH({start},{shift})"), &s);
        assert_eq!(
            render_serial(got, DateStyle::Iso),
            want,
            "EOMONTH({start},{shift})"
        );
    }
}

// -------------------------------------------------- DAYS and DATEDIF ---

#[test]
fn days_is_a_signed_serial_difference() {
    let s = empty();
    assert_eq!(n("=DAYS(45016,45000)", &s), 16.0);
    assert_eq!(n("=DAYS(45000,45016)", &s), -16.0);
    assert_eq!(n("=DAYS(45000,45000)", &s), 0.0);
    assert_eq!(n("=DAYS(DATE(2024,1,1),DATE(2023,1,1))", &s), 365.0);
    // 2024 is a leap year, so this span is 366 days.
    assert_eq!(n("=DAYS(DATE(2025,1,1),DATE(2024,1,1))", &s), 366.0);
    // Time of day is discarded, not rounded.
    assert_eq!(n("=DAYS(45016.9,45000.1)", &s), 16.0);
}

#[test]
fn datedif_units() {
    // 2023-01-15 -> 2024-03-20. Every unit is a different, checkable number.
    let s = empty();
    let call = |u: &str| {
        n(
            &format!("=DATEDIF(DATE(2023,1,15),DATE(2024,3,20),\"{u}\")"),
            &s,
        )
    };
    assert_eq!(call("D"), 430.0);
    assert_eq!(call("M"), 14.0);
    assert_eq!(call("Y"), 1.0);
    assert_eq!(call("YM"), 2.0, "months ignoring years");
    assert_eq!(call("MD"), 5.0, "days ignoring months and years");
    // 2024-01-15 -> 2024-03-20 is 16 + 29 + 20 days.
    assert_eq!(call("YD"), 65.0, "days ignoring years");
    // Lowercase units are accepted, like Excel.
    assert_eq!(call("m"), 14.0);
}

#[test]
fn datedif_month_counting_needs_the_day_to_come_round() {
    let s = empty();
    // One day short of a whole month is zero months, not one.
    assert_eq!(
        n("=DATEDIF(DATE(2023,1,15),DATE(2023,2,14),\"M\")", &s),
        0.0
    );
    assert_eq!(
        n("=DATEDIF(DATE(2023,1,15),DATE(2023,2,15),\"M\")", &s),
        1.0
    );
    // Same rule a year out.
    assert_eq!(
        n("=DATEDIF(DATE(2023,3,15),DATE(2024,3,14),\"Y\")", &s),
        0.0
    );
    assert_eq!(
        n("=DATEDIF(DATE(2023,3,15),DATE(2024,3,15),\"Y\")", &s),
        1.0
    );
}

#[test]
fn datedif_rejects_a_reversed_interval_and_an_unknown_unit() {
    let s = empty();
    assert_eq!(
        ev("=DATEDIF(DATE(2024,1,1),DATE(2023,1,1),\"D\")", &s),
        Value::Error(ErrorKind::Num),
        "Excel refuses a backwards DATEDIF rather than returning a negative"
    );
    assert_eq!(
        ev("=DATEDIF(DATE(2023,1,1),DATE(2024,1,1),\"Q\")", &s),
        Value::Error(ErrorKind::Num)
    );
}

// ------------------------------------------------------ NETWORKDAYS ---

#[test]
fn networkdays_counts_weekdays_inclusively() {
    let s = empty();
    // Mon 2023-03-13 .. Fri 2023-03-24 is exactly two working weeks.
    assert_eq!(n("=NETWORKDAYS(44998,45009)", &s), 10.0);
    // A single weekday is 1; a single weekend day is 0.
    assert_eq!(n("=NETWORKDAYS(44998,44998)", &s), 1.0);
    assert_eq!(
        n("=NETWORKDAYS(DATE(2023,3,18),DATE(2023,3,18))", &s),
        0.0,
        "2023-03-18 is a Saturday"
    );
    // A whole Mon-Sun week is 5.
    assert_eq!(n("=NETWORKDAYS(44998,45004)", &s), 5.0);
    // Starting on a weekend does not shift the count.
    assert_eq!(
        n("=NETWORKDAYS(DATE(2023,3,11),DATE(2023,3,17))", &s),
        5.0,
        "Sat 11th through Fri 17th"
    );
    // Reversed interval is negative, as in Excel.
    assert_eq!(n("=NETWORKDAYS(45009,44998)", &s), -10.0);
}

#[test]
fn networkdays_subtracts_holidays_from_a_range() {
    let mut s = Sheet::new("t");
    // C1:C3 are holidays: a Wednesday, a Saturday, and a day outside the span.
    s.set(CellRef::new(0, 2), Value::Number(D2023_03_15)); // Wed, inside
    s.set(CellRef::new(1, 2), Value::Number(45_003.0)); // Sat 2023-03-18
    s.set(CellRef::new(2, 2), Value::Number(45_016.0)); // outside the span

    let base = n("=NETWORKDAYS(44998,45009)", &s);
    let with = n("=NETWORKDAYS(44998,45009,C1:C3)", &s);
    assert_eq!(base, 10.0);
    assert_eq!(
        with, 9.0,
        "only the in-span weekday holiday counts; the Saturday and the \
         out-of-span date must not"
    );
}

#[test]
fn a_holiday_listed_twice_is_deducted_once() {
    let mut s = Sheet::new("t");
    for r in 0..4u32 {
        s.set(CellRef::new(r, 2), Value::Number(D2023_03_15));
    }
    assert_eq!(
        n("=NETWORKDAYS(44998,45009,C1:C4)", &s),
        9.0,
        "four copies of one holiday must not remove four days"
    );
}

#[test]
fn networkdays_holidays_accept_a_single_cell_or_literal() {
    let mut s = Sheet::new("t");
    s.set(CellRef::new(0, 2), Value::Number(D2023_03_15));
    assert_eq!(n("=NETWORKDAYS(44998,45009,C1)", &s), 9.0);
    assert_eq!(n("=NETWORKDAYS(44998,45009,45000)", &s), 9.0);
}

#[test]
fn an_error_in_the_holiday_range_propagates() {
    let mut s = Sheet::new("t");
    s.set(CellRef::new(0, 2), Value::Number(D2023_03_15));
    s.set(CellRef::new(1, 2), Value::Error(ErrorKind::DivZero));
    assert_eq!(
        ev("=NETWORKDAYS(44998,45009,C1:C2)", &s),
        Value::Error(ErrorKind::DivZero),
        "a broken holiday cell must not be silently skipped"
    );
}

#[test]
fn networkdays_span_matches_a_day_by_day_count() {
    // Independent oracle: walk every day in the span and count weekdays with
    // the renderer's own weekday, not with the formula under test.
    let s = empty();
    for (a, b) in [(44_900.0, 45_100.0), (60.0, 90.0), (45_000.0, 45_000.0)] {
        let mut want = 0i64;
        for d in (a as i64)..=(b as i64) {
            let name = render_serial(d as f64, DateStyle::Iso);
            let (_, _, _, _, _, _, wd) = serial_parts(d as f64);
            assert!(!name.is_empty());
            if !matches!(wd, 0 | 6) {
                want += 1;
            }
        }
        assert_eq!(
            n(&format!("=NETWORKDAYS({a},{b})"), &s),
            want as f64,
            "NETWORKDAYS({a},{b})"
        );
    }
}

// ------------------------------------------------------- plumbing ---

#[test]
fn errors_in_arguments_propagate() {
    let mut s = Sheet::new("t");
    s.set(CellRef::new(0, 0), Value::Error(ErrorKind::DivZero));
    for f in ["=YEAR(A1)", "=WEEKDAY(A1)", "=EOMONTH(A1,1)", "=DAYS(A1,1)"] {
        assert_eq!(ev(f, &s), Value::Error(ErrorKind::DivZero), "{f}");
    }
}

#[test]
fn text_arguments_are_value_errors_not_zero() {
    let mut s = Sheet::new("t");
    s.set_text(CellRef::new(0, 0), "not a date");
    assert_eq!(ev("=YEAR(A1)", &s), Value::Error(ErrorKind::Value));
    assert_eq!(ev("=DATE(A1,1,1)", &s), Value::Error(ErrorKind::Value));
}

#[test]
fn wrong_arity_is_a_value_error() {
    let s = empty();
    for f in [
        "=DATE(2023,1)",
        "=YEAR()",
        "=YEAR(1,2)",
        "=WEEKDAY(1,2,3)",
        "=EOMONTH(1)",
        "=DAYS(1)",
        "=DATEDIF(1,2)",
        "=NETWORKDAYS(1)",
        "=NETWORKDAYS(1,2,3,4)",
    ] {
        assert_eq!(ev(f, &s), Value::Error(ErrorKind::Value), "{f}");
    }
}

#[test]
fn the_dispatcher_and_the_name_list_agree() {
    // Drift guard: `DATE_FUNCTIONS` is what the xlsx import filter trusts. If
    // it and `is_date_fn` disagree, an importable formula evaluates to #NAME?
    // (or a working function is dropped on import) with nothing failing.
    let s = empty();
    for f in DATE_FUNCTIONS {
        assert!(is_date_fn(f), "{f} is listed but not dispatched");
        // ...and it really evaluates: a call with no arguments must answer
        // something other than #NAME?.
        assert_ne!(
            ev(&format!("={f}()"), &s),
            Value::Error(ErrorKind::Name),
            "{f} dispatches but the evaluator does not know it"
        );
    }
    assert!(!is_date_fn("SUM"));
    assert!(!is_date_fn("VLOOKUP"));
    assert_eq!(ev("=DATEVALUE(\"x\")", &s), Value::Error(ErrorKind::Name));
}

#[test]
fn date_functions_compose_with_the_rest_of_the_evaluator() {
    let mut s = Sheet::new("t");
    s.set(CellRef::new(0, 0), Value::Number(D2023_03_15));
    s.set(CellRef::new(1, 0), Value::Number(D2024_01_31));
    assert_eq!(n("=YEAR(A1)+YEAR(A2)", &s), 4047.0);
    assert_eq!(n("=SUM(DAY(A1),DAY(A2))", &s), 46.0);
    assert_eq!(n("=IF(WEEKDAY(A1,2)<6,1,0)", &s), 1.0);
    assert_eq!(n("=EOMONTH(A1,0)-A1", &s), 16.0);
    assert_eq!(n("=MAX(A1:A2)", &s), D2024_01_31);
}
