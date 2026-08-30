//! Unit tests for the Page Setup dialog's pure form-resolution logic (#37).
//!
//! These cover the text<->model conversions the dialog does on OK: scaling,
//! repeat rows/cols, and the column-letter round trip. The dialog's *painting*
//! and its OK-button wiring are covered by the headless harness in
//! `harness.rs`, which drives the real egui widgets.

use super::*;
use ferrix_core::page::{PageSetup, Scaling};

fn state() -> PageSetupState {
    PageSetupState::from_setup(&PageSetup::default())
}

#[test]
fn column_names_round_trip() {
    for c in [0u32, 1, 25, 26, 27, 51, 52, 701, 702] {
        assert_eq!(parse_col_name(&col_name(c)), Some(c), "col {c}");
    }
    assert_eq!(col_name(0), "A");
    assert_eq!(col_name(26), "AA");
    assert_eq!(parse_col_name("A"), Some(0));
    assert_eq!(parse_col_name("aa"), Some(26));
    assert_eq!(parse_col_name(""), None);
    assert_eq!(parse_col_name("A1"), None);
}

#[test]
fn percent_scaling_resolves() {
    let mut s = state();
    s.fit_mode = false;
    s.percent = "80".into();
    s.resolve().unwrap();
    assert_eq!(s.setup.scaling, Scaling::Percent(80));
}

#[test]
fn zero_percent_is_rejected() {
    let mut s = state();
    s.fit_mode = false;
    s.percent = "0".into();
    assert!(s.resolve().is_err());
}

#[test]
fn fit_to_resolves_with_blank_axis_as_none() {
    let mut s = state();
    s.fit_mode = true;
    s.fit_wide = "1".into();
    s.fit_tall = "".into();
    s.resolve().unwrap();
    assert_eq!(
        s.setup.scaling,
        Scaling::FitTo {
            wide: Some(1),
            tall: None
        }
    );
}

#[test]
fn repeat_rows_are_stored_zero_based() {
    let mut s = state();
    s.repeat_rows = "1:2".into();
    s.resolve().unwrap();
    // 1-based "1:2" in the UI -> 0-based (0, 1) in the model.
    assert_eq!(s.setup.repeat_rows, Some((0, 1)));
}

#[test]
fn repeat_cols_parse_letters() {
    let mut s = state();
    s.repeat_cols = "A:B".into();
    s.resolve().unwrap();
    assert_eq!(s.setup.repeat_cols, Some((0, 1)));
}

#[test]
fn empty_repeat_clears_the_range() {
    let mut setup = PageSetup {
        repeat_rows: Some((0, 3)),
        repeat_cols: Some((0, 1)),
        ..PageSetup::default()
    };
    setup.scaling = Scaling::Percent(100);
    let mut s = PageSetupState::from_setup(&setup);
    // Pre-filled from the setup...
    assert_eq!(s.repeat_rows, "1:4");
    assert_eq!(s.repeat_cols, "A:B");
    // ...then cleared by the user and committed.
    s.repeat_rows = "".into();
    s.repeat_cols = "".into();
    s.resolve().unwrap();
    assert_eq!(s.setup.repeat_rows, None);
    assert_eq!(s.setup.repeat_cols, None);
}

#[test]
fn reversed_repeat_range_is_rejected() {
    let mut s = state();
    s.repeat_rows = "5:2".into();
    assert!(s.resolve().is_err());
}

#[test]
fn garbage_repeat_range_is_rejected() {
    let mut s = state();
    s.repeat_rows = "abc".into();
    assert!(s.resolve().is_err());
    let mut s2 = state();
    s2.repeat_cols = "9:9".into();
    assert!(s2.resolve().is_err());
}

#[test]
fn prefill_reflects_percent_scaling() {
    let setup = PageSetup {
        scaling: Scaling::Percent(75),
        ..PageSetup::default()
    };
    let s = PageSetupState::from_setup(&setup);
    assert!(!s.fit_mode);
    assert_eq!(s.percent, "75");
}
