//! Tests for sheet-level formatting.
//!
//! The scale claims — that storage and resolution cost are independent of row
//! count — are asserted in `tests/format_scale.rs`, which needs a counting
//! allocator and so lives outside the unit tests.

use super::*;
use crate::table::{CellStyle, DateStyle};

fn cell(row: u32, col: u32) -> CellRef {
    CellRef::new(row, col)
}

fn plan_of(f: &SheetFormat, col: u32) -> Vec<PlanEntry<'_>> {
    let mut p = Vec::new();
    f.plan(col, &mut p);
    p
}

/// Resolve with no window scan — the common case, and the one that must work
/// without ever touching a row other than this one.
fn style_at(f: &SheetFormat, c: CellRef, v: &Value) -> CellStyle {
    let plan = plan_of(f, c.col);
    f.resolve(c, v, "", &plan, &[])
}

fn style_text(f: &SheetFormat, c: CellRef, s: &str) -> CellStyle {
    let plan = plan_of(f, c.col);
    f.resolve(c, &Value::Empty, s, &plan, &[])
}

// ------------------------------------------------------------ manual colour --

#[test]
fn a_manual_column_colour_applies_to_every_row_including_unwritten_ones() {
    let mut f = SheetFormat::new();
    f.set_column_manual(
        3,
        ManualStyle {
            fill: Some(Rgb(10, 20, 30)),
            text: None,
            typography: Default::default(),
        },
    );
    // Row 0 and row 199,999,999 must agree: the rule is about the column, not
    // about rows that happen to exist today.
    for row in [0u32, 5, 199_999_999] {
        let s = style_at(&f, cell(row, 3), &Value::Number(1.0));
        assert_eq!(s.fill, Some(Rgb(10, 20, 30)), "row {row} missed the fill");
    }
    assert!(
        style_at(&f, cell(0, 4), &Value::Number(1.0)).is_plain(),
        "a neighbouring column must be untouched"
    );
}

#[test]
fn a_manual_colour_covers_non_numeric_cells_too() {
    let mut f = SheetFormat::new();
    f.set_column_manual(
        0,
        ManualStyle {
            fill: Some(Rgb(1, 2, 3)),
            text: Some(Rgb(4, 5, 6)),
            typography: Default::default(),
        },
    );
    // Text, bools and empties are all coloured — a fill is not a numeric idea.
    for v in [
        Value::Empty,
        Value::Bool(true),
        Value::Text(StrIdStub::ZERO),
    ] {
        let s = style_at(&f, cell(1, 0), &v);
        assert_eq!(s.fill, Some(Rgb(1, 2, 3)), "value {v:?} was skipped");
        assert_eq!(s.text, Some(Rgb(4, 5, 6)));
    }
}

/// A tiny local alias so the test above can name a `Value::Text` without
/// building an arena.
struct StrIdStub;
impl StrIdStub {
    const ZERO: crate::arena::StrId = crate::arena::StrId(0);
}

#[test]
fn a_manual_selection_colour_is_one_entry_whatever_the_selection_size() {
    let mut f = SheetFormat::new();
    f.set_range_manual(
        TableRange::new(0, 1, 199_999_999, 1),
        ManualStyle {
            fill: Some(Rgb(9, 9, 9)),
            text: None,
            typography: Default::default(),
        },
    );
    assert_eq!(f.ranges().len(), 1);
    assert_eq!(
        style_at(&f, cell(100_000_000, 1), &Value::Number(0.0)).fill,
        Some(Rgb(9, 9, 9))
    );
}

#[test]
fn a_range_rule_stops_at_the_range_edges() {
    let mut f = SheetFormat::new();
    f.set_range_manual(
        TableRange::new(10, 2, 20, 4),
        ManualStyle {
            fill: Some(Rgb(7, 7, 7)),
            text: None,
            typography: Default::default(),
        },
    );
    let n = Value::Number(1.0);
    assert!(style_at(&f, cell(9, 3), &n).is_plain(), "row above");
    assert_eq!(style_at(&f, cell(10, 3), &n).fill, Some(Rgb(7, 7, 7)));
    assert_eq!(style_at(&f, cell(20, 3), &n).fill, Some(Rgb(7, 7, 7)));
    assert!(style_at(&f, cell(21, 3), &n).is_plain(), "row below");
    assert!(style_at(&f, cell(15, 5), &n).is_plain(), "column right");
}

#[test]
fn clearing_a_manual_colour_removes_the_entry_rather_than_leaving_an_inert_one() {
    let mut f = SheetFormat::new();
    f.set_column_manual(
        0,
        ManualStyle {
            fill: Some(Rgb(1, 1, 1)),
            text: None,
            typography: Default::default(),
        },
    );
    assert_eq!(f.rule_count(), 1);
    f.set_column_manual(0, ManualStyle::default());
    assert_eq!(f.rule_count(), 0);
    assert!(f.is_empty(), "an emptied column must not keep a live key");
}

// ---------------------------------------------------------- value-driven ----

#[test]
fn negative_red_positive_green_needs_no_per_cell_storage() {
    let mut f = SheetFormat::new();
    f.push_column_rule(0, presets::sign_colors());

    let red = Rgb(0xC0, 0x28, 0x28);
    let green = Rgb(0x1E, 0x88, 0x3C);
    assert_eq!(
        style_at(&f, cell(0, 0), &Value::Number(-1.0)).text,
        Some(red)
    );
    assert_eq!(
        style_at(&f, cell(1, 0), &Value::Number(12.5)).text,
        Some(green)
    );
    assert_eq!(
        style_at(&f, cell(2, 0), &Value::Number(0.0)).text,
        None,
        "zero is neither, and gets no colour unless one is configured"
    );
    // The whole feature is one rule and zero per-cell entries.
    assert_eq!(f.override_count(), 0);
    assert_eq!(f.rule_count(), 1);
}

#[test]
fn sign_colouring_sets_text_not_fill() {
    let mut f = SheetFormat::new();
    f.push_column_rule(0, presets::negative_red());
    let s = style_at(&f, cell(0, 0), &Value::Number(-3.0));
    assert!(s.text.is_some());
    assert!(
        s.fill.is_none(),
        "sign colouring is typographic; filling every negative cell would \
         drown the sheet"
    );
}

#[test]
fn a_user_threshold_fires_only_on_the_matching_side() {
    let mut f = SheetFormat::new();
    f.push_column_rule(2, presets::above(100.0));
    assert!(style_at(&f, cell(0, 2), &Value::Number(99.0)).is_plain());
    assert!(
        style_at(&f, cell(0, 2), &Value::Number(100.0)).is_plain(),
        "Gt is strict"
    );
    assert!(!style_at(&f, cell(0, 2), &Value::Number(101.0)).is_plain());
}

#[test]
fn thresholds_take_any_comparison_the_user_picks() {
    let mut f = SheetFormat::new();
    f.push_column_rule(
        0,
        ConditionalRule::Threshold {
            op: CmpOp::Le,
            value: -5.0,
            fill: Rgb(1, 2, 3),
            text: Rgb(4, 5, 6),
        },
    );
    assert_eq!(
        style_at(&f, cell(0, 0), &Value::Number(-5.0)).fill,
        Some(Rgb(1, 2, 3)),
        "Le includes the bound"
    );
    assert!(style_at(&f, cell(0, 0), &Value::Number(-4.9)).is_plain());
}

#[test]
fn text_contains_matches_case_insensitively_and_ignores_numbers() {
    let mut f = SheetFormat::new();
    f.push_column_rule(0, presets::contains("err"));
    let plan = plan_of(&f, 0);
    assert!(
        SheetFormat::plan_needs_text(&plan),
        "the caller must be told to resolve text"
    );
    assert!(!style_text(&f, cell(0, 0), "Server ERROR 500").is_plain());
    assert!(style_text(&f, cell(0, 0), "fine").is_plain());
    // A numeric cell has no text; the rule simply does not fire.
    assert!(style_at(&f, cell(0, 0), &Value::Number(1.0)).is_plain());
}

#[test]
fn colour_scales_and_bars_need_a_window_and_say_nothing_without_one() {
    let mut f = SheetFormat::new();
    f.push_column_rule(0, presets::color_scale());
    f.push_column_rule(0, presets::data_bar());
    let plan = plan_of(&f, 0);
    assert!(SheetFormat::plan_needs_window(&plan));

    // No evals supplied: both rules no-op rather than guessing an extent.
    assert!(f
        .resolve(cell(0, 0), &Value::Number(5.0), "", &plan, &[])
        .is_plain());

    // With a window, the scale places the value in it.
    let evals = [
        RuleEval {
            extent: Some((0.0, 10.0)),
            cut: None,
        },
        RuleEval {
            extent: Some((0.0, 10.0)),
            cut: None,
        },
    ];
    let s = f.resolve(cell(0, 0), &Value::Number(10.0), "", &plan, &evals);
    assert_eq!(s.fill, Some(Rgb(0x53, 0x8D, 0xD5)), "top of the scale");
    assert_eq!(s.bar.map(|(t, _)| t), Some(1.0));
}

#[test]
fn top_n_uses_a_rank_cut_over_the_window() {
    let mut values = vec![5.0, 1.0, 9.0, 3.0, 7.0];
    let rule = presets::top_n(2);
    let eval = RuleEval::for_rule(&rule, &mut values);
    // The 2 largest are 9 and 7, so the cut sits at 7.
    assert_eq!(eval.cut, Some(7.0));

    let mut f = SheetFormat::new();
    f.push_column_rule(0, rule);
    let plan = plan_of(&f, 0);
    let evals = [eval];
    assert!(!f
        .resolve(cell(0, 0), &Value::Number(9.0), "", &plan, &evals)
        .is_plain());
    assert!(f
        .resolve(cell(0, 0), &Value::Number(5.0), "", &plan, &evals)
        .is_plain());
}

#[test]
fn bottom_n_cuts_from_the_other_end() {
    let mut values = vec![5.0, 1.0, 9.0, 3.0, 7.0];
    let rule = presets::bottom_n(2);
    let eval = RuleEval::for_rule(&rule, &mut values);
    assert_eq!(eval.cut, Some(3.0), "the 2 smallest are 1 and 3");
}

#[test]
fn a_rank_request_larger_than_the_window_is_clamped_not_a_panic() {
    let mut values = vec![4.0, 2.0];
    let eval = RuleEval::for_rule(&presets::top_n(500), &mut values);
    assert_eq!(eval.cut, Some(2.0), "every value qualifies");
    // And an empty window is simply unanswerable.
    assert_eq!(RuleEval::for_rule(&presets::top_n(3), &mut []).cut, None);
}

// ----------------------------------------------------------------- ordering --

#[test]
fn a_later_rule_overrides_an_earlier_one() {
    let mut f = SheetFormat::new();
    f.push_column_rule(0, presets::below(0.0)); // red
    f.push_column_rule(
        0,
        ConditionalRule::Threshold {
            op: CmpOp::Lt,
            value: -100.0,
            fill: Rgb(0, 0, 0),
            text: Rgb(255, 255, 255),
        },
    );
    // -500 matches both; the later rule wins, as Excel does it.
    assert_eq!(
        style_at(&f, cell(0, 0), &Value::Number(-500.0)).fill,
        Some(Rgb(0, 0, 0))
    );
    // -5 matches only the first.
    assert_eq!(
        style_at(&f, cell(0, 0), &Value::Number(-5.0)).fill,
        Some(Rgb(0xFF, 0xC7, 0xCE))
    );
}

#[test]
fn reordering_changes_which_rule_wins() {
    let mut f = SheetFormat::new();
    f.push_column_rule(0, presets::below(0.0));
    f.push_column_rule(
        0,
        ConditionalRule::Threshold {
            op: CmpOp::Lt,
            value: -100.0,
            fill: Rgb(0, 0, 0),
            text: Rgb(255, 255, 255),
        },
    );
    assert!(f.move_column_rule(0, 1, -1), "move the specific rule first");
    // Now the general rule is last, so it wins on -500 instead.
    assert_eq!(
        style_at(&f, cell(0, 0), &Value::Number(-500.0)).fill,
        Some(Rgb(0xFF, 0xC7, 0xCE))
    );
}

#[test]
fn reordering_at_the_edges_is_a_no_op_rather_than_an_error() {
    let mut f = SheetFormat::new();
    f.push_column_rule(0, presets::negative_red());
    f.push_column_rule(0, presets::above(1.0));
    assert!(!f.move_column_rule(0, 0, -1), "already first");
    assert!(!f.move_column_rule(0, 1, 1), "already last");
    assert!(!f.move_column_rule(0, 9, 1), "out of range");
    assert!(!f.move_column_rule(7, 0, 1), "no such column");
    assert_eq!(f.rule_count(), 2, "nothing was lost");
}

#[test]
fn moving_a_rule_several_places_lands_where_asked() {
    let mut f = SheetFormat::new();
    for v in [1.0, 2.0, 3.0, 4.0] {
        f.push_column_rule(0, presets::above(v));
    }
    assert!(f.move_column_rule(0, 0, 2));
    let labels: Vec<String> = f
        .column(0)
        .unwrap()
        .rules
        .iter()
        .map(|r| r.label())
        .collect();
    assert_eq!(
        labels,
        vec![
            "Value > 2".to_string(),
            "Value > 3".into(),
            "Value > 1".into(),
            "Value > 4".into()
        ]
    );
}

#[test]
fn deleting_a_rule_leaves_the_rest_in_order() {
    let mut f = SheetFormat::new();
    for v in [1.0, 2.0, 3.0] {
        f.push_column_rule(0, presets::above(v));
    }
    let removed = f.remove_column_rule(0, 1).expect("rule 1 exists");
    assert_eq!(removed.label(), "Value > 2");
    let labels: Vec<String> = f
        .column(0)
        .unwrap()
        .rules
        .iter()
        .map(|r| r.label())
        .collect();
    assert_eq!(labels, vec!["Value > 1".to_string(), "Value > 3".into()]);
    assert!(f.remove_column_rule(0, 9).is_none(), "out of range");
}

#[test]
fn range_rules_are_evaluated_after_column_rules() {
    let mut f = SheetFormat::new();
    f.set_column_manual(
        0,
        ManualStyle {
            fill: Some(Rgb(1, 1, 1)),
            text: None,
            typography: Default::default(),
        },
    );
    f.set_range_manual(
        TableRange::new(5, 0, 6, 0),
        ManualStyle {
            fill: Some(Rgb(2, 2, 2)),
            text: None,
            typography: Default::default(),
        },
    );
    let n = Value::Number(0.0);
    assert_eq!(style_at(&f, cell(4, 0), &n).fill, Some(Rgb(1, 1, 1)));
    assert_eq!(
        style_at(&f, cell(5, 0), &n).fill,
        Some(Rgb(2, 2, 2)),
        "a range is more specific than a column and comes later in the plan"
    );
}

// ----------------------------------------------------------- cell overrides --

#[test]
fn a_per_cell_override_beats_every_rule_and_is_the_only_per_cell_storage() {
    let mut f = SheetFormat::new();
    f.push_column_rule(0, presets::sign_colors());
    f.set_cell_override(
        cell(7, 0),
        CellOverride {
            manual: ManualStyle {
                fill: Some(Rgb(255, 255, 0)),
                text: Some(Rgb(0, 0, 0)),
                typography: Default::default(),
            },
            format: None,
        },
    );
    let s = style_at(&f, cell(7, 0), &Value::Number(-1.0));
    assert_eq!(s.fill, Some(Rgb(255, 255, 0)));
    assert_eq!(
        s.text,
        Some(Rgb(0, 0, 0)),
        "the override wins over the rule"
    );
    // The neighbouring row still gets the rule and stores nothing.
    assert_eq!(
        style_at(&f, cell(8, 0), &Value::Number(-1.0)).text,
        Some(Rgb(0xC0, 0x28, 0x28))
    );
    assert_eq!(f.override_count(), 1, "exactly one cell was singled out");
}

#[test]
fn clearing_an_override_removes_its_entry() {
    let mut f = SheetFormat::new();
    f.set_cell_override(
        cell(1, 1),
        CellOverride {
            manual: ManualStyle {
                fill: Some(Rgb(1, 2, 3)),
                text: None,
                typography: Default::default(),
            },
            format: None,
        },
    );
    assert_eq!(f.override_count(), 1);
    f.set_cell_override(cell(1, 1), CellOverride::default());
    assert_eq!(
        f.override_count(),
        0,
        "the map must not accumulate inert rows"
    );
}

// --------------------------------------------------------- number formats ----

#[test]
fn a_column_number_format_applies_down_the_whole_column() {
    let mut f = SheetFormat::new();
    f.set_column_format(
        1,
        NumberFormat::Currency {
            symbol: "$".into(),
            places: 2,
        },
    );
    let fmt = f
        .number_format(cell(199_999_999, 1))
        .expect("format resolves");
    assert_eq!(fmt.render(1234.5), "$1,234.50");
    assert!(
        f.number_format(cell(0, 0)).is_none(),
        "other columns unaffected"
    );
}

#[test]
fn every_configurable_number_format_renders() {
    // The shapes the editor offers, checked end to end so a UI change cannot
    // silently offer something that renders as a bare number.
    let cases = [
        (NumberFormat::Decimal { places: 3 }, 1.5, "1.500"),
        (
            NumberFormat::Thousands { places: 0 },
            1234567.0,
            "1,234,567",
        ),
        (
            NumberFormat::Currency {
                symbol: "€".into(),
                places: 2,
            },
            -99.5,
            "-€99.50",
        ),
        (NumberFormat::Percent { places: 1 }, 0.256, "25.6%"),
        (NumberFormat::Date(DateStyle::Iso), 45000.0, "2023-03-15"),
    ];
    for (fmt, v, want) in cases {
        assert_eq!(fmt.render(v), want, "{fmt:?} rendered wrong");
    }
}

#[test]
fn a_range_format_overrides_the_column_format_and_a_cell_override_beats_both() {
    let mut f = SheetFormat::new();
    f.set_column_format(0, NumberFormat::Decimal { places: 0 });
    let i = f.push_range(RangeFormat::new(TableRange::new(0, 0, 10, 0)));
    f.range_mut(i).unwrap().format = Some(NumberFormat::Percent { places: 1 });

    assert_eq!(
        f.number_format(cell(50, 0)),
        Some(&NumberFormat::Decimal { places: 0 })
    );
    assert_eq!(
        f.number_format(cell(5, 0)),
        Some(&NumberFormat::Percent { places: 1 })
    );

    f.set_cell_override(
        cell(5, 0),
        CellOverride {
            manual: ManualStyle::default(),
            format: Some(NumberFormat::Thousands { places: 2 }),
        },
    );
    assert_eq!(
        f.number_format(cell(5, 0)),
        Some(&NumberFormat::Thousands { places: 2 })
    );
}

#[test]
fn setting_general_clears_a_column_format() {
    let mut f = SheetFormat::new();
    f.set_column_format(0, NumberFormat::Percent { places: 0 });
    assert!(f.number_format(cell(0, 0)).is_some());
    f.set_column_format(0, NumberFormat::General);
    assert!(
        f.number_format(cell(0, 0)).is_none(),
        "General means 'no format', not 'a format called General'"
    );
    assert!(f.is_empty());
}

// ------------------------------------------------------------------- plans ---

#[test]
fn a_plan_only_gathers_rules_that_can_touch_the_column() {
    let mut f = SheetFormat::new();
    f.push_column_rule(0, presets::negative_red());
    f.push_column_rule(1, presets::above(5.0));
    f.push_range(RangeFormat::new(TableRange::new(0, 0, 9, 0)).with_rule(presets::data_bar()));

    assert_eq!(plan_of(&f, 0).len(), 2, "column 0's rule plus the range's");
    assert_eq!(plan_of(&f, 1).len(), 1);
    assert_eq!(plan_of(&f, 9).len(), 0);
}

#[test]
fn a_plan_reports_whether_the_expensive_inputs_are_needed() {
    let mut f = SheetFormat::new();
    f.push_column_rule(0, presets::negative_red());
    let plan = plan_of(&f, 0);
    assert!(
        !SheetFormat::plan_needs_text(&plan) && !SheetFormat::plan_needs_window(&plan),
        "a sign rule needs neither a string nor a scan"
    );
}

#[test]
fn heap_cost_tracks_rule_count_not_row_count() {
    let mut a = SheetFormat::new();
    a.set_range_manual(
        TableRange::new(0, 0, 9, 0),
        ManualStyle {
            fill: Some(Rgb(1, 2, 3)),
            text: None,
            typography: Default::default(),
        },
    );
    let mut b = SheetFormat::new();
    b.set_range_manual(
        TableRange::new(0, 0, 199_999_999, 0),
        ManualStyle {
            fill: Some(Rgb(1, 2, 3)),
            text: None,
            typography: Default::default(),
        },
    );
    assert_eq!(
        a.heap_bytes(),
        b.heap_bytes(),
        "a 200M-row range must cost exactly what a 10-row one does"
    );
}
