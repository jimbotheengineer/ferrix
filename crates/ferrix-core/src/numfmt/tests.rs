//! Tests for the Excel number-format engine.
//!
//! Every expectation here is a string Excel itself produces. They are written
//! as exact equality rather than "contains a comma" because the whole purpose
//! of this module is byte-agreement with another program — a loose assertion
//! would pass while showing the user something different from the workbook
//! they imported.

use super::*;

fn r(code: &str, v: f64) -> String {
    NumFmt::parse(code).render(v)
}

// ------------------------------------------------------------ basic numbers

#[test]
fn zero_placeholder_pads_to_its_width() {
    assert_eq!(r("0", 5.0), "5");
    assert_eq!(r("00", 5.0), "05");
    assert_eq!(r("0000", 42.0), "0042");
    // `0.00` forces two decimals even when the value has none.
    assert_eq!(r("0.00", 1.5), "1.50");
    assert_eq!(r("0.00", 3.0), "3.00");
}

#[test]
fn hash_placeholder_shows_nothing_for_absent_digits() {
    // This is the whole difference between `0` and `#`: a `#` that has no
    // digit to show shows nothing, so `#.##` on 1.5 is "1.5" not "1.50".
    assert_eq!(r("#.##", 1.5), "1.5");
    assert_eq!(r("#.##", 1.0), "1");
    assert_eq!(r("0.##", 1.0), "1");
    // A lone `#` renders an integer zero as empty, matching Excel.
    assert_eq!(r("#", 0.0), "");
    assert_eq!(r("0", 0.0), "0");
}

#[test]
fn rounding_happens_at_the_declared_precision() {
    assert_eq!(r("0.0", 1.25), "1.3");
    assert_eq!(r("0", 1.5), "2");
    assert_eq!(r("0", 2.5), "3");
    assert_eq!(r("0.00", 1.005), "1.00"); // binary f64, matches Excel
}

// ------------------------------------------------------------ grouping

#[test]
fn thousands_separators_group_by_three() {
    assert_eq!(r("#,##0", 1234.0), "1,234");
    assert_eq!(r("#,##0", 1234567.0), "1,234,567");
    assert_eq!(r("#,##0", 100.0), "100");
    assert_eq!(r("#,##0.00", 1234567.891), "1,234,567.89");
    // Exactly three digits must not gain a leading separator.
    assert_eq!(r("#,##0", 999.0), "999");
    assert_eq!(r("#,##0", 1000.0), "1,000");
}

#[test]
fn trailing_comma_scales_by_a_thousand_each() {
    // `#,##0,` displays thousands; `#,##0,,` displays millions. This is the
    // idiom every financial model uses and it looks like a typo if you do not
    // know it, so it is worth pinning.
    assert_eq!(r("#,##0,", 1_234_567.0), "1,235");
    assert_eq!(r("#,##0,,", 1_234_567_890.0), "1,235");
    assert_eq!(r("0.0,,", 2_500_000.0), "2.5");
}

// ------------------------------------------------------------ percent

#[test]
fn percent_multiplies_by_one_hundred() {
    assert_eq!(r("0%", 0.5), "50%");
    assert_eq!(r("0.0%", 0.1234), "12.3%");
    assert_eq!(r("0%", 1.0), "100%");
}

// ------------------------------------------------------------ sections

#[test]
fn two_sections_split_positive_and_negative() {
    let f = NumFmt::parse("0.00;(0.00)");
    assert_eq!(f.render(5.0), "5.00");
    // The negative section spells its own parentheses, so no minus is added:
    // "(-5.00)" would be the bug.
    assert_eq!(f.render(-5.0), "(5.00)");
    // With only two sections, zero uses the positive one.
    assert_eq!(f.render(0.0), "0.00");
}

#[test]
fn three_sections_give_zero_its_own_form() {
    let f = NumFmt::parse("0.00;-0.00;\"zero\"");
    assert_eq!(f.render(1.0), "1.00");
    assert_eq!(f.render(-1.0), "-1.00");
    assert_eq!(f.render(0.0), "zero");
}

#[test]
fn a_single_section_still_signs_negatives() {
    // No negative section means Excel supplies the minus itself. Dropping it
    // would render -5 and 5 identically, which is a data misrepresentation.
    assert_eq!(r("0.00", -5.0), "-5.00");
    assert_eq!(r("#,##0", -1234.0), "-1,234");
}

#[test]
fn the_fourth_section_formats_text_only() {
    let f = NumFmt::parse("0.00;-0.00;0;\"[\"@\"]\"");
    assert_eq!(f.render_text("hi"), "[hi]");
    // A numeric section must never be applied to text.
    assert_eq!(f.render(1.0), "1.00");
}

#[test]
fn text_passes_through_when_no_text_section_exists() {
    let f = NumFmt::parse("0.00");
    assert_eq!(f.render_text("hello"), "hello");
}

// ------------------------------------------------------------ colours

#[test]
fn colour_tokens_are_extracted_per_section() {
    let f = NumFmt::parse("[Blue]#,##0.00;[Red](#,##0.00)");
    assert_eq!(f.color_for(1.0), Some(FmtColor::Blue));
    assert_eq!(f.color_for(-1.0), Some(FmtColor::Red));
    // The colour token must not leak into the rendered text.
    assert_eq!(f.render(-1234.5), "(1,234.50)");
}

#[test]
fn indexed_colour_tokens_parse() {
    let f = NumFmt::parse("[Color 3]0");
    assert_eq!(f.color_for(1.0), Some(FmtColor::Red));
}

// ------------------------------------------------------------ conditions

#[test]
fn conditional_sections_select_by_predicate() {
    // Excel: first matching condition wins, last section is the else.
    let f = NumFmt::parse("[>=1000]#,##0,\"k\";[>0]0.00;\"neg\"");
    assert_eq!(f.render(5000.0), "5k");
    assert_eq!(f.render(12.5), "12.50");
    assert_eq!(f.render(-3.0), "neg");
}

// ------------------------------------------------------------ literals

#[test]
fn quoted_and_escaped_literals_are_emitted_verbatim() {
    assert_eq!(r("\"$\"#,##0", 1234.0), "$1,234");
    assert_eq!(r("0\" units\"", 7.0), "7 units");
    assert_eq!(r("\\$0", 5.0), "$5");
}

#[test]
fn a_semicolon_inside_a_literal_is_not_a_section_break() {
    // This is the parser bug that would silently split a code into the wrong
    // number of sections and change which one a value renders through.
    let f = NumFmt::parse("\"a;b\"0");
    assert_eq!(f.render(1.0), "a;b1");
}

#[test]
fn currency_blocks_keep_the_symbol_and_drop_the_locale_id() {
    // `[$€-407]` means "euro sign, German locale". The symbol is content; the
    // locale id is not.
    assert_eq!(r("[$\u{20ac}-407]#,##0.00", 1234.5), "\u{20ac}1,234.50");
    // A bare locale id contributes nothing visible.
    assert_eq!(r("[$-409]#,##0", 1234.0), "1,234");
}

// ------------------------------------------------------------ scientific

#[test]
fn scientific_notation_renders_with_a_signed_exponent() {
    assert_eq!(r("0.00E+00", 12345.0), "1.23E+04");
    assert_eq!(r("0.00E+00", 0.00012), "1.20E-04");
    assert_eq!(r("0.00E+00", 0.0), "0.00E+00");
}

// ------------------------------------------------------------ dates

#[test]
fn date_tokens_render_the_calendar_parts() {
    // Serial 45000 is 2023-03-15 under the 1900 system Ferrix already models.
    let s = 45000.0;
    assert_eq!(r("yyyy-mm-dd", s), "2023-03-15");
    assert_eq!(r("m/d/yyyy", s), "3/15/2023");
    assert_eq!(r("dd/mm/yy", s), "15/03/23");
    assert_eq!(r("mmm yyyy", s), "Mar 2023");
    assert_eq!(r("mmmm d, yyyy", s), "March 15, 2023");
}

#[test]
fn m_means_minute_next_to_an_hour_and_month_otherwise() {
    // The single most consequential ambiguity in Excel's format language:
    // getting it backwards turns a timestamp's minutes into a month.
    let s = 45000.5; // noon
    assert_eq!(r("h:mm", s), "12:00");
    assert_eq!(r("mm/dd", s), "03/15");
    // Between an hour and a second, `m` is unambiguously minutes.
    assert_eq!(r("h:m:s", s), "12:0:0");
}

#[test]
fn twelve_hour_clock_folds_the_hour_and_marks_the_half() {
    let morning = 45000.0 + 9.0 / 24.0;
    let evening = 45000.0 + 21.0 / 24.0;
    assert_eq!(r("h:mm AM/PM", morning), "9:00 AM");
    assert_eq!(r("h:mm AM/PM", evening), "9:00 PM");
    // Midnight and noon are the cases a naive `% 12` gets wrong (0 vs 12).
    assert_eq!(r("h AM/PM", 45000.0), "12 AM");
    assert_eq!(r("h AM/PM", 45000.5), "12 PM");
}

#[test]
fn elapsed_tokens_are_not_clamped_to_a_clock() {
    // `[h]` over three days is 72, not 0. That is the entire reason the
    // bracketed forms exist, and clamping them silently loses whole days.
    assert_eq!(r("[h]", 3.0), "72");
    assert_eq!(r("[m]", 1.0), "1440");
    assert_eq!(r("[s]", 1.0), "86400");
}

// ------------------------------------------------------------ robustness

#[test]
fn an_unmodelled_code_renders_as_a_plain_number_not_as_nothing() {
    // Fraction formats are not modelled. The requirement is that the value
    // still appears: showing nothing, or showing a wrong number, would both be
    // worse than showing an unformatted one.
    let out = r("# ?/?", 0.5);
    assert!(
        !out.is_empty(),
        "an unmodelled code must still render the value, got empty"
    );
}

#[test]
fn parsing_never_panics_on_hostile_input() {
    // These arrive from files users did not write. A panic here takes down the
    // whole load, so every one of them must merely parse to something.
    for code in [
        "",
        ";",
        ";;;",
        "[",
        "]",
        "\"unterminated",
        "\\",
        "[Red]",
        "[>]",
        "0.00E+",
        "########################################",
        ";;;;;;;;",
        "@@@@",
        "[h]:[m]:[s]",
    ] {
        let f = NumFmt::parse(code);
        let _ = f.render(1.0);
        let _ = f.render(-1.0);
        let _ = f.render(0.0);
        let _ = f.render_text("x");
        let _ = f.render(f64::NAN);
        let _ = f.render(f64::INFINITY);
    }
}

#[test]
fn a_custom_code_reaches_the_engine_through_numberformat() {
    // The wiring test. Without this, the engine could be complete, correct,
    // and never called -- which is exactly how the order and format models
    // shipped before: fully tested, zero UI.
    use crate::table::NumberFormat;
    let f = NumberFormat::Custom("\"$\"#,##0.00".to_string());
    assert_eq!(
        f.render(1234.5),
        "$1,234.50",
        "NumberFormat::Custom must interpret the code, not fall back to a plain number"
    );

    // And a date code, to prove the date path is reachable too.
    let d = NumberFormat::Custom("yyyy-mm-dd".to_string());
    assert_eq!(d.render(45000.0), "2023-03-15");
}

#[test]
fn a_parsed_format_is_reusable_across_many_values() {
    // The performance claim in the module docs: parse once, render many. This
    // pins the API shape that makes that possible — if `render` needed `&mut`
    // or re-parsed, a 200M-row column would pay for it 200M times.
    let f = NumFmt::parse("#,##0.00");
    let mut last = String::new();
    for i in 0..1000 {
        last = f.render(i as f64 * 1.5);
    }
    assert_eq!(last, "1,498.50");
}
