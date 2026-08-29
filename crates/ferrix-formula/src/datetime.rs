//! Date and time functions.
//!
//! # Storage
//!
//! There is no date *type*. A date is an `f64` Excel serial living in
//! [`Value::Number`], exactly like every other number, which is why
//! `Value` is still 16 bytes (pinned by `value::tests::value_stays_16_bytes`).
//! "Is this a date?" is a *format* question, answered by
//! [`ferrix_core::NumberFormat::Date`], never a storage question.
//!
//! # One calendar
//!
//! Every serial<->calendar conversion in here goes through
//! [`ferrix_core::table::serial_parts`] and
//! [`ferrix_core::table::serial_from_civil`], which are inverses of each other
//! and of `render_serial`. This module deliberately contains **no calendar
//! arithmetic of its own** — a second calendar that disagreed with the
//! renderer by one day would show a date in the cell that no function agreed
//! with, and nothing would fail loudly.
//!
//! That includes Excel's deliberate 1900-02-29 bug: serial 60 is a day that
//! never existed, `render_serial(60.0)` prints `1900-02-29`, and
//! `DAY(60) = 29` here for the same reason.
//!
//! # Memory
//!
//! Nothing here allocates per row. The only allocation in the whole module is
//! `NETWORKDAYS`' holiday bitmap, which is one bit per *day in the requested
//! date span* (363 KB at the absolute limit of the 1900 date system, ~46 bytes
//! for a one-year span) and is completely independent of how many rows the
//! holiday range covers — that range is streamed, never collected.

use std::cell::Cell;

use ferrix_core::table::{days_in_month, serial_from_civil, serial_parts, DATE_SERIAL_MAX};
use ferrix_core::{CellRef, ErrorKind, Value};

use crate::eval::{eval_view, CellSource};
use crate::parser::Expr;

// --------------------------------------------------------------- the clock ---

thread_local! {
    /// Test override for "now", as an Excel serial.
    ///
    /// **This is how TODAY/NOW are made testable.** A test calls
    /// [`set_test_clock`] with a fixed serial and every subsequent `TODAY()`
    /// / `NOW()` on that thread answers from it instead of the wall clock, so
    /// an assertion can be an exact equality rather than a tautology like
    /// "TODAY() is greater than zero" (which would pass against a function
    /// that always returned 1).
    ///
    /// It is a *thread-local* on purpose: `cargo test` runs tests in parallel
    /// in one process, and a global would let one test's frozen clock leak
    /// into another's and produce failures that only reproduce under a
    /// particular scheduling. Production code never touches it, so the
    /// override costs one TLS read per call and nothing else.
    static TEST_CLOCK: Cell<Option<f64>> = const { Cell::new(None) };
}

/// Freeze (`Some`) or release (`None`) this thread's clock. See [`TEST_CLOCK`].
pub fn set_test_clock(serial: Option<f64>) {
    TEST_CLOCK.with(|c| c.set(serial));
}

/// Current instant as an Excel serial.
///
/// The wall-clock path is UTC: `std` has no timezone database, and inventing
/// one here would be a much bigger dependency than this feature justifies.
/// A user in UTC-5 therefore sees UTC in `NOW()`. This is a known, documented
/// limitation rather than an accident — see REPORT.md.
fn now_serial() -> f64 {
    if let Some(fixed) = TEST_CLOCK.with(|c| c.get()) {
        return fixed;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    // 25_569 is the serial of 1970-01-01 (agrees with `render_serial`).
    25_569.0 + secs / 86_400.0
}

// ------------------------------------------------------------- dispatching ---

/// Does this name belong to this module?
///
/// Kept as a function rather than inlined into `eval_call` so the whole date
/// feature is one added match arm in `eval.rs`.
pub fn is_date_fn(name: &str) -> bool {
    matches!(
        name,
        "TODAY"
            | "NOW"
            | "DATE"
            | "YEAR"
            | "MONTH"
            | "DAY"
            | "HOUR"
            | "MINUTE"
            | "SECOND"
            | "WEEKDAY"
            | "EOMONTH"
            | "EDATE"
            | "DATEDIF"
            | "DAYS"
            | "NETWORKDAYS"
    )
}

/// Every function name this module implements, in one place so the xlsx
/// import filter and the tests cannot drift from the dispatcher.
pub const DATE_FUNCTIONS: &[&str] = &[
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
];

/// Evaluate a date/time call. `name` is already upper-cased by the lexer.
pub fn call<S: CellSource + ?Sized>(name: &str, args: &[Expr], src: &S) -> Value {
    match dispatch(name, args, src) {
        Ok(v) => v,
        Err(e) => Value::Error(e),
    }
}

fn dispatch<S: CellSource + ?Sized>(
    name: &str,
    args: &[Expr],
    src: &S,
) -> Result<Value, ErrorKind> {
    match name {
        "TODAY" | "NOW" => {
            arity(args, 0, 0)?;
            let n = now_serial();
            // TODAY is NOW with the time of day thrown away, not a separate
            // clock read — two calls in one recalc can never straddle midnight
            // and disagree about the date.
            Ok(num(if name == "TODAY" { n.floor() } else { n }))
        }
        "DATE" => {
            arity(args, 3, 3)?;
            let y = int_arg(args, 0, src)?;
            let m = int_arg(args, 1, src)?;
            let d = int_arg(args, 2, src)?;
            Ok(num(date_serial(y, m, d)?))
        }
        "YEAR" | "MONTH" | "DAY" | "HOUR" | "MINUTE" | "SECOND" => {
            arity(args, 1, 1)?;
            let s = serial_arg(args, 0, src)?;
            let (y, mo, d, h, mi, sec, _) = serial_parts(s);
            Ok(num(f64::from(match name {
                "YEAR" => y as u32,
                "MONTH" => mo,
                "DAY" => d,
                "HOUR" => h,
                "MINUTE" => mi,
                _ => sec,
            })))
        }
        "WEEKDAY" => {
            arity(args, 1, 2)?;
            let s = serial_arg(args, 0, src)?;
            let kind = if args.len() == 2 {
                int_arg(args, 1, src)?
            } else {
                1
            };
            // `serial_parts` reports 0 = Sunday.
            let sun0 = i64::from(serial_parts(s).6);
            let mon0 = (sun0 + 6) % 7;
            Ok(num(match kind {
                1 => (sun0 + 1) as f64, // Sunday = 1 .. Saturday = 7
                2 => (mon0 + 1) as f64, // Monday = 1 .. Sunday = 7
                3 => mon0 as f64,       // Monday = 0 .. Sunday = 6
                _ => return Err(ErrorKind::Num),
            }))
        }
        "EDATE" | "EOMONTH" => {
            arity(args, 2, 2)?;
            let s = serial_arg(args, 0, src)?;
            let months = int_arg(args, 1, src)?;
            let (y, mo, d, ..) = serial_parts(s);
            let (y2, m2) = shift_month(i64::from(y), i64::from(mo), months);
            let dim = i64::from(days_in_month(clamp_year(y2)?, m2 as u32));
            // Month-end clamp. 31 Jan + 1 month is 28/29 Feb, NOT 3 March:
            // the day is clamped into the target month, never allowed to
            // overflow past its end. EOMONTH always lands on the last day.
            let day = if name == "EOMONTH" {
                dim
            } else {
                i64::from(d).min(dim)
            };
            let out =
                serial_from_civil(clamp_year(y2)?, m2 as u32, day as u32).ok_or(ErrorKind::Num)?;
            Ok(num(out))
        }
        "DAYS" => {
            arity(args, 2, 2)?;
            let end = serial_arg(args, 0, src)?.floor();
            let start = serial_arg(args, 1, src)?.floor();
            Ok(num(end - start))
        }
        "DATEDIF" => {
            arity(args, 3, 3)?;
            let start = serial_arg(args, 0, src)?.floor();
            let end = serial_arg(args, 1, src)?.floor();
            let unit = text_arg(args, 2, src)?;
            // Excel refuses a reversed interval outright rather than
            // returning a negative count.
            if start > end {
                return Err(ErrorKind::Num);
            }
            Ok(num(datedif(start, end, &unit)? as f64))
        }
        "NETWORKDAYS" => {
            arity(args, 2, 3)?;
            let start = serial_arg(args, 0, src)?.floor() as i64;
            let end = serial_arg(args, 1, src)?.floor() as i64;
            networkdays(start, end, args.get(2), src).map(num)
        }
        _ => Err(ErrorKind::Name),
    }
}

// ------------------------------------------------------------------- maths ---

#[inline]
fn num(n: f64) -> Value {
    Value::Number(n)
}

/// Shift `(y, m)` by `delta` months, keeping the month in `1..=12`.
fn shift_month(y: i64, m: i64, delta: i64) -> (i64, i64) {
    let total = y * 12 + (m - 1) + delta;
    (total.div_euclid(12), total.rem_euclid(12) + 1)
}

fn clamp_year(y: i64) -> Result<i32, ErrorKind> {
    if (1900..=9999).contains(&y) {
        Ok(y as i32)
    } else {
        Err(ErrorKind::Num)
    }
}

/// `DATE(y, m, d)` with Excel's rollover rules.
///
/// The day is applied as an offset from the 1st **in serial space**, not by
/// handing an out-of-range day to the calendar. That is what Excel does, and
/// it is the only way `DATE(1900, 2, 30)` crosses the phantom 1900-02-29 and
/// lands on 1900-03-01 (serial 61) the way Excel's does.
fn date_serial(y: i64, m: i64, d: i64) -> Result<f64, ErrorKind> {
    // Excel: a year of 0..=1899 means "1900 + that", so DATE(23,1,1) is 1923.
    let y = if (0..1900).contains(&y) { y + 1900 } else { y };
    let (y2, m2) = shift_month(y, m, 0);
    let first = serial_from_civil(clamp_year(y2)?, m2 as u32, 1).ok_or(ErrorKind::Num)?;
    let s = first + (d - 1) as f64;
    if !(0.0..=DATE_SERIAL_MAX).contains(&s) {
        return Err(ErrorKind::Num);
    }
    Ok(s)
}

/// `DATEDIF` unit semantics, matching Excel's (undocumented but stable) ones.
fn datedif(start: f64, end: f64, unit: &str) -> Result<i64, ErrorKind> {
    let (y1, m1, d1, ..) = serial_parts(start);
    let (y2, m2, d2, ..) = serial_parts(end);
    // Whole months elapsed: the month difference, minus one if the day of the
    // month has not come round yet.
    let whole_months =
        (i64::from(y2) - i64::from(y1)) * 12 + (i64::from(m2) - i64::from(m1)) - i64::from(d2 < d1);
    Ok(match unit.to_ascii_uppercase().as_str() {
        "D" => (end - start) as i64,
        "Y" => whole_months.div_euclid(12),
        "M" => whole_months,
        // Days ignoring months and years. When the end day is earlier in the
        // month than the start day, borrow the length of the month before the
        // end date — which is where those days actually came from.
        "MD" => {
            if d2 >= d1 {
                i64::from(d2) - i64::from(d1)
            } else {
                let (py, pm) = shift_month(i64::from(y2), i64::from(m2), -1);
                i64::from(days_in_month(clamp_year(py)?, pm as u32)) - i64::from(d1) + i64::from(d2)
            }
        }
        // Months ignoring years.
        "YM" => whole_months.rem_euclid(12),
        // Days ignoring years: distance from the last anniversary of `start`
        // that is on or before `end`.
        "YD" => {
            let back = i64::from((i64::from(m1), i64::from(d1)) > (i64::from(m2), i64::from(d2)));
            let anniversary = date_serial(i64::from(y2) - back, i64::from(m1), i64::from(d1))?;
            (end - anniversary) as i64
        }
        _ => return Err(ErrorKind::Num),
    })
}

/// Is this serial day a Monday-to-Friday?
#[inline]
fn is_workday(serial_day: i64) -> bool {
    // `serial_parts` reports 0 = Sunday, 6 = Saturday.
    !matches!(serial_parts(serial_day as f64).6, 0 | 6)
}

/// `NETWORKDAYS(start, end, [holidays])`.
///
/// Whole weekdays in `[start, end]` inclusive, minus any holiday that falls on
/// a weekday inside the span. Excel returns a negative count when the interval
/// runs backwards, so the span is normalised and the sign reapplied.
///
/// SCALE INVARIANT: the holiday argument is *streamed*. Nothing proportional
/// to the range's row count is ever held; the only allocation is a bitmap of
/// one bit per day in the span (`<= 363 KB` for the entire 1900 date system,
/// tens of bytes in practice), which also gives duplicate-free counting for
/// free — Excel counts a holiday listed twice only once.
fn networkdays<S: CellSource + ?Sized>(
    start: i64,
    end: i64,
    holidays: Option<&Expr>,
    src: &S,
) -> Result<f64, ErrorKind> {
    let sign = if start <= end { 1.0 } else { -1.0 };
    let (a, b) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };

    let span = b - a + 1;
    let full_weeks = span / 7;
    let mut work = full_weeks * 5;
    for i in 0..(span % 7) {
        if is_workday(a + full_weeks * 7 + i) {
            work += 1;
        }
    }

    if let Some(arg) = holidays {
        let mut seen = vec![0u8; (span as usize).div_ceil(8)];
        let mut err = None;
        for_each_serial(arg, src, &mut |v| match v {
            Ok(s) => {
                let day = s.floor() as i64;
                if day < a || day > b || !is_workday(day) {
                    return;
                }
                let idx = (day - a) as usize;
                let (byte, bit) = (idx / 8, 1u8 << (idx % 8));
                if seen[byte] & bit == 0 {
                    seen[byte] |= bit;
                    work -= 1;
                }
            }
            Err(e) => {
                err.get_or_insert(e);
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
    }

    Ok(sign * work as f64)
}

// ------------------------------------------------------------- argument I/O ---

fn arity(args: &[Expr], min: usize, max: usize) -> Result<(), ErrorKind> {
    if (min..=max).contains(&args.len()) {
        Ok(())
    } else {
        Err(ErrorKind::Value)
    }
}

fn number_arg<S: CellSource + ?Sized>(args: &[Expr], i: usize, src: &S) -> Result<f64, ErrorKind> {
    let a = args.get(i).ok_or(ErrorKind::Value)?;
    let v = eval_view(a, src);
    if let Some(e) = v.error() {
        return Err(e);
    }
    let n = v.as_number().ok_or(ErrorKind::Value)?;
    if n.is_finite() {
        Ok(n)
    } else {
        Err(ErrorKind::Num)
    }
}

/// An argument read as a date serial. Excel has no negative dates, so a
/// negative serial is `#NUM!` rather than a silently wrapped calendar.
fn serial_arg<S: CellSource + ?Sized>(args: &[Expr], i: usize, src: &S) -> Result<f64, ErrorKind> {
    let n = number_arg(args, i, src)?;
    if (0.0..=DATE_SERIAL_MAX).contains(&n) {
        Ok(n)
    } else {
        Err(ErrorKind::Num)
    }
}

fn int_arg<S: CellSource + ?Sized>(args: &[Expr], i: usize, src: &S) -> Result<i64, ErrorKind> {
    Ok(number_arg(args, i, src)?.trunc() as i64)
}

fn text_arg<S: CellSource + ?Sized>(args: &[Expr], i: usize, src: &S) -> Result<String, ErrorKind> {
    match args.get(i) {
        // A string literal has no interned id during evaluation, so it is read
        // straight off the AST rather than round-tripped through `Value`.
        Some(Expr::Text(s)) => Ok(s.clone()),
        Some(other) => match eval_view(other, src) {
            Value::Text(id) => Ok(src.resolve(id).to_string()),
            Value::Error(e) => Err(e),
            _ => Err(ErrorKind::Value),
        },
        None => Err(ErrorKind::Value),
    }
}

/// Stream every numeric value an argument contributes, reporting errors as
/// they are met. Ranges are clamped to the sheet's real extent so `A:A` costs
/// the populated rows rather than 2^20 of them.
fn for_each_serial<S: CellSource + ?Sized>(
    arg: &Expr,
    src: &S,
    f: &mut impl FnMut(Result<f64, ErrorKind>),
) {
    let feed = |v: Value, f: &mut dyn FnMut(Result<f64, ErrorKind>)| match v {
        Value::Number(n) => f(Ok(n)),
        Value::Error(e) => f(Err(e)),
        // Blanks and text in a holiday list are ignored, matching Excel.
        _ => {}
    };
    match arg {
        Expr::Range(s, e) => {
            let r1 = (e.row as usize + 1).min(src.row_count().max(1));
            for c in s.col..=e.col {
                for r in s.row as usize..r1 {
                    feed(src.get(CellRef::new(r as u32, c)), f);
                }
            }
        }
        Expr::XRange(sheet, s, e) => {
            let Some(rows) = src.row_count_in(sheet) else {
                f(Err(ErrorKind::Ref));
                return;
            };
            let r1 = (e.row as usize + 1).min(rows.max(1));
            for c in s.col..=e.col {
                for r in s.row as usize..r1 {
                    feed(src.get_in(sheet, CellRef::new(r as u32, c)), f);
                }
            }
        }
        other => feed(eval_view(other, src), f),
    }
}

#[cfg(test)]
mod tests;
