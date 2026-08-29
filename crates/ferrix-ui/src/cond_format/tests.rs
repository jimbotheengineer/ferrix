//! Tests for the conditional-formatting editor's model layer.
//!
//! These exercise the parts that do not need a frame: target scoping, the form
//! round-trip, the preview splice, and the xlsx warning. The behaviour that
//! only shows up through the real app — a rule changing what is PAINTED, and
//! Cancel leaving the sheet byte-identical — is tested in `harness.rs`, which
//! drives the actual `FerrixApp`.

use super::*;
use ferrix_core::CellRef;

fn cell(r: u32, c: u32) -> CellRef {
    CellRef::new(r, c)
}

#[test]
fn every_rule_variant_round_trips_through_the_form() {
    // If a variant is added to `ConditionalRule` and not to the editor, the
    // form silently loses it: `from_rule` would fall through to a default and
    // `to_rule` would build something else. Round-tripping every variant is
    // what makes that a failing test rather than a missing dialog page.
    let rules = vec![
        ConditionalRule::Threshold {
            op: CmpOp::Le,
            value: -12.5,
            fill: Rgb(1, 2, 3),
            text: Rgb(4, 5, 6),
        },
        ConditionalRule::ColorScale2 {
            min: Rgb(7, 8, 9),
            max: Rgb(10, 11, 12),
        },
        ConditionalRule::ColorScale3 {
            min: Rgb(1, 1, 1),
            mid: Rgb(2, 2, 2),
            max: Rgb(3, 3, 3),
        },
        ConditionalRule::DataBar {
            color: Rgb(9, 9, 9),
        },
        ConditionalRule::Sign {
            negative: Some(Rgb(200, 0, 0)),
            positive: None,
            zero: Some(Rgb(0, 0, 200)),
        },
        ConditionalRule::TopBottom {
            top: false,
            n: 7,
            fill: Rgb(20, 21, 22),
            text: Rgb(23, 24, 25),
        },
        ConditionalRule::TextContains {
            needle: "urgent".into(),
            fill: Rgb(30, 31, 32),
            text: Rgb(33, 34, 35),
        },
        ConditionalRule::Manual {
            fill: Some(Rgb(40, 41, 42)),
            text: None,
            typography: Typography {
                bold: Some(true),
                ..Default::default()
            },
        },
    ];
    assert_eq!(
        rules.len(),
        RuleKind::ALL.len(),
        "every RuleKind must have a fixture here, and vice versa"
    );
    for r in rules {
        let back = RuleForm::from_rule(&r).to_rule();
        assert_eq!(back, r, "form lost information for {r:?}");
    }
}

#[test]
fn topbottom_and_text_rules_warn_about_xlsx_and_the_rest_do_not() {
    // The warning is sourced from the exporter's own predicate, so this also
    // pins that the editor and the exporter agree about which rules are lossy.
    let lossy = ConditionalRule::TopBottom {
        top: true,
        n: 5,
        fill: Rgb(0, 0, 0),
        text: Rgb(0, 0, 0),
    };
    let w = xlsx_warning(&lossy).expect("TopBottom has no xlsx mapping and must warn");
    assert!(
        w.contains("DROPPED") && w.contains("xlsx"),
        "the warning must say what actually happens on export: {w}"
    );
    assert!(xlsx_warning(&ferrix_core::format::presets::contains("x")).is_some());

    // A threshold DOES survive, so warning about it would train the user to
    // ignore the warning.
    assert_eq!(
        xlsx_warning(&ferrix_core::format::presets::above(1.0)),
        None
    );
    assert_eq!(
        xlsx_warning(&ferrix_core::format::presets::color_scale()),
        None
    );
    assert_eq!(
        xlsx_warning(&ferrix_core::format::presets::sign_colors()),
        None
    );
}

#[test]
fn preview_does_not_touch_the_store_and_shows_the_pending_rule() {
    let mut fmt = SheetFormat::new();
    fmt.push_column_rule(2, ferrix_core::format::presets::negative_red());
    let before = fmt.clone();

    let mut st = CondFormatState::new_rule(CondTarget::Column(2));
    st.form.kind = RuleKind::Threshold;
    st.form.value = 100.0;

    let previewed = st.preview_format(&fmt).expect("preview is on by default");
    assert_eq!(fmt, before, "building a preview must not mutate the store");
    assert_eq!(
        previewed.column_rules(2).len(),
        2,
        "the preview shows the pending rule alongside the stored one"
    );
    assert_eq!(
        previewed.column_rules(2)[1],
        st.form.to_rule(),
        "a NEW rule previews appended — i.e. winning — which is where OK puts it"
    );

    // Preview off means paint the store as it is: no clone, nothing spliced.
    st.preview = false;
    assert_eq!(st.preview_format(&fmt), None);
}

#[test]
fn editing_previews_in_place_rather_than_appended() {
    // An edit that previewed appended would show the user the wrong answer
    // whenever the rule being edited is not already the last one.
    let mut fmt = SheetFormat::new();
    fmt.push_column_rule(0, ferrix_core::format::presets::above(10.0));
    fmt.push_column_rule(0, ferrix_core::format::presets::below(-10.0));

    let mut st = CondFormatState::new_rule(CondTarget::Column(0));
    st.mode = CondMode::Edit(0);
    st.form = RuleForm::from_rule(&ferrix_core::format::presets::above(10.0));
    st.form.value = 999.0;

    let p = st.preview_format(&fmt).unwrap();
    assert_eq!(p.column_rules(0).len(), 2, "editing must not add a rule");
    assert_eq!(
        p.column_rules(0)[0],
        st.form.to_rule(),
        "the edited rule previews in its own slot"
    );
    assert_eq!(
        p.column_rules(0)[1],
        ferrix_core::format::presets::below(-10.0),
        "the rule that was not edited is untouched"
    );
}

#[test]
fn manage_mode_never_previews() {
    // The rules Manage lists are already stored and already painted. Splicing
    // one in again would double-apply it and show a fill nobody asked for.
    let mut fmt = SheetFormat::new();
    fmt.push_column_rule(1, ferrix_core::format::presets::data_bar());
    let st = CondFormatState::manage(CondTarget::Column(1));
    assert_eq!(st.preview_rule(), None);
    assert_eq!(st.preview_format(&fmt), None);
}

#[test]
fn a_range_target_stores_one_entry_however_many_rows_it_spans() {
    // The scale invariant, at the editor's own boundary.
    let mut fmt = SheetFormat::new();
    let t = CondTarget::Range(TableRange::new(0, 3, 199_999_999, 3));
    t.push(&mut fmt, ferrix_core::format::presets::negative_red());
    t.push(&mut fmt, ferrix_core::format::presets::data_bar());

    assert_eq!(fmt.rule_count(), 2, "two rules, not 400 million");
    assert_eq!(fmt.ranges().len(), 1, "both rules share ONE range entry");
    assert_eq!(
        fmt.override_count(),
        0,
        "nothing may land in per-cell storage"
    );
    assert!(
        fmt.heap_bytes() < 4096,
        "a 200M-row rule must not cost real memory, got {}",
        fmt.heap_bytes()
    );
}

#[test]
fn replace_keeps_precedence_position_and_push_does_not() {
    let mut fmt = SheetFormat::new();
    let t = CondTarget::Column(5);
    t.push(&mut fmt, ferrix_core::format::presets::above(1.0));
    t.push(&mut fmt, ferrix_core::format::presets::below(1.0));

    let swap = ferrix_core::format::presets::top_n(3);
    assert!(t.replace(&mut fmt, 0, swap.clone()));
    assert_eq!(t.rules(&fmt)[0], swap, "an edit stays where it was");
    assert_eq!(t.rules(&fmt).len(), 2);

    // Out-of-range replace is refused rather than appending silently.
    assert!(!t.replace(&mut fmt, 9, swap.clone()));
    assert_eq!(t.rules(&fmt).len(), 2);
}

#[test]
fn move_rule_reorders_and_reports_whether_it_moved() {
    let mut fmt = SheetFormat::new();
    let t = CondTarget::Column(0);
    let a = ferrix_core::format::presets::above(1.0);
    let b = ferrix_core::format::presets::below(1.0);
    t.push(&mut fmt, a.clone());
    t.push(&mut fmt, b.clone());

    assert!(t.move_rule(&mut fmt, 0, 1), "0 -> 1 must move");
    assert_eq!(t.rules(&fmt), &[b, a][..]);
    // Already at the end: nothing to do, and it says so rather than lying.
    assert!(!t.move_rule(&mut fmt, 1, 1));
}

#[test]
fn range_and_column_scopes_are_distinct_lists() {
    // A rule on B1:B10 must not appear in the whole-column list, or deleting
    // "the column's rule" would silently delete somebody else's range rule.
    let mut fmt = SheetFormat::new();
    let range = CondTarget::Range(TableRange::new(0, 1, 9, 1));
    let col = CondTarget::Column(1);
    range.push(&mut fmt, ferrix_core::format::presets::above(5.0));

    assert_eq!(range.rules(&fmt).len(), 1);
    assert_eq!(
        col.rules(&fmt).len(),
        0,
        "scopes must not bleed into each other"
    );

    col.push(&mut fmt, ferrix_core::format::presets::data_bar());
    assert_eq!(range.rules(&fmt).len(), 1);
    assert_eq!(col.rules(&fmt).len(), 1);
    assert_eq!(fmt.rule_count(), 2);
}

#[test]
fn a_selection_becomes_a_range_and_widen_makes_it_a_column() {
    let t = CondTarget::from_selection(cell(2, 4), cell(40, 4));
    assert_eq!(t, CondTarget::Range(TableRange::new(2, 4, 40, 4)));
    assert_eq!(t.widen(), CondTarget::Column(4));
    assert!(t.label().contains("E3"), "label was {}", t.label());
    assert_eq!(CondTarget::Column(4).label(), "column E");
}

#[test]
fn inert_rules_are_refused_before_they_can_be_saved() {
    // Each of these is a rule that can NEVER match anything. Saving one gives
    // the user a rule in the list that visibly does nothing, which reads as a
    // broken feature.
    let mut f = RuleForm {
        kind: RuleKind::TextContains,
        needle: "   ".into(),
        ..Default::default()
    };
    assert!(f.problem().is_some(), "an empty needle matches nothing");
    f.needle = "ok".into();
    assert_eq!(f.problem(), None);

    let f = RuleForm {
        kind: RuleKind::Sign,
        negative: None,
        positive: None,
        zero: None,
        ..Default::default()
    };
    assert!(f.problem().is_some(), "a Sign with no colours does nothing");

    let f = RuleForm {
        kind: RuleKind::Manual,
        manual_fill: None,
        manual_text: None,
        ..Default::default()
    };
    assert!(f.problem().is_some());

    // A threshold no row currently meets is legal — the data may change.
    let f = RuleForm {
        kind: RuleKind::Threshold,
        value: f64::MAX,
        ..Default::default()
    };
    assert_eq!(f.problem(), None);
}

#[test]
fn topbottom_n_is_clamped_rather_than_allowed_to_be_zero() {
    let f = RuleForm {
        kind: RuleKind::TopBottom,
        n: 0,
        ..Default::default()
    };
    match f.to_rule() {
        ConditionalRule::TopBottom { n, .. } => assert_eq!(n, 1),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn a_threshold_rule_changes_the_resolved_style_of_matching_cells_only() {
    // The editor's whole purpose, asserted on the resolved CellStyle rather
    // than on the rule appearing in a list: a rule that stores fine but does
    // not change how a cell resolves is a dead feature.
    let mut fmt = SheetFormat::new();
    let t = CondTarget::Column(0);
    let mut form = RuleForm {
        kind: RuleKind::Threshold,
        op: CmpOp::Gt,
        value: 50.0,
        ..Default::default()
    };
    form.fill = Rgb(0xAB, 0xCD, 0xEF);
    t.push(&mut fmt, form.to_rule());

    let mut plan = Vec::new();
    fmt.plan(0, &mut plan);
    let hit = fmt.resolve(
        cell(0, 0),
        &ferrix_core::Value::Number(99.0),
        "",
        &plan,
        &[],
    );
    let miss = fmt.resolve(cell(1, 0), &ferrix_core::Value::Number(1.0), "", &plan, &[]);

    assert_eq!(
        hit.fill,
        Some(Rgb(0xAB, 0xCD, 0xEF)),
        "99 > 50 must be filled"
    );
    assert_eq!(
        miss.fill, None,
        "1 > 50 is false; the cell must be untouched"
    );
    assert!(miss.is_plain());
}

#[test]
fn a_later_rule_wins_and_reordering_flips_which_one_that_is() {
    // Two rules that both match 5. Whichever is LAST is what the user sees,
    // which is the fact the Manage list's ▲/▼ exists to control.
    let mut fmt = SheetFormat::new();
    let t = CondTarget::Column(0);
    let red = ConditionalRule::Threshold {
        op: CmpOp::Gt,
        value: 0.0,
        fill: Rgb(0xFF, 0, 0),
        text: Rgb(0, 0, 0),
    };
    let blue = ConditionalRule::Threshold {
        op: CmpOp::Gt,
        value: 0.0,
        fill: Rgb(0, 0, 0xFF),
        text: Rgb(0, 0, 0),
    };
    t.push(&mut fmt, red.clone());
    t.push(&mut fmt, blue.clone());

    let resolved = |f: &SheetFormat| {
        let mut plan = Vec::new();
        f.plan(0, &mut plan);
        f.resolve(cell(0, 0), &ferrix_core::Value::Number(5.0), "", &plan, &[])
            .fill
    };

    assert_eq!(resolved(&fmt), Some(Rgb(0, 0, 0xFF)), "the LAST rule wins");
    assert!(t.move_rule(&mut fmt, 1, -1), "move blue earlier");
    assert_eq!(
        resolved(&fmt),
        Some(Rgb(0xFF, 0, 0)),
        "after the reorder the OTHER rule wins — the order is the behaviour"
    );
}

#[test]
fn manual_of_carries_the_typography_switches() {
    let f = RuleForm {
        kind: RuleKind::Manual,
        manual_fill: Some(Rgb(1, 2, 3)),
        manual_text: None,
        bold: true,
        italic: false,
        ..Default::default()
    };
    let m = manual_of(&f);
    assert_eq!(m.fill, Some(Rgb(1, 2, 3)));
    assert_eq!(m.typography.bold, Some(true));
    assert_eq!(m.typography.italic, None, "an unset switch must inherit");
}
