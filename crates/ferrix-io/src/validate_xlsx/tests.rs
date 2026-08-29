//! Round-trip tests for sheet-range data validation (issue #41).
//!
//! These write a real `.xlsx`, unzip it, and assert on the OOXML, then
//! re-import and compare. **Excel itself was never opened**: what is proven is
//! that the `<dataValidations>` parts Excel reads are present and well formed,
//! and that Ferrix reproduces every rule across the round trip — not that
//! Excel accepts the file.

use super::*;
use ferrix_core::validate::{ErrorStyle, RangeValidation, SheetValidation, ValueDomain};
use ferrix_core::{CmpOp, Sheet, TableRange, ValidationRule, Value};

struct TempXlsx(std::path::PathBuf);

impl TempXlsx {
    fn new(tag: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!("ferrix-dv-{tag}-{n}.xlsx")))
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempXlsx {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn part(path: &std::path::Path, name: &str) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(f).ok()?;
    let mut e = zip.by_name(name).ok()?;
    let mut s = String::new();
    std::io::Read::read_to_string(&mut e, &mut s).ok()?;
    Some(s)
}

fn demo_sheet() -> Sheet {
    let mut s = Sheet::new("Data");
    s.set_headers(vec!["Region".into(), "Qty".into(), "Note".into()]);
    for r in 0..6u32 {
        s.set_text(ferrix_core::CellRef::new(r, 0), "North");
        s.set(
            ferrix_core::CellRef::new(r, 1),
            Value::Number(r as f64 + 1.0),
        );
        s.set_text(ferrix_core::CellRef::new(r, 2), "ok");
    }
    s
}

fn export(path: &std::path::Path, sheet: &Sheet, v: &SheetValidation) {
    crate::xlsx::export_workbook(
        path,
        &[crate::xlsx::SheetExport::new("Data", sheet).with_validation(v)],
    )
    .expect("export");
}

fn reimport(path: &std::path::Path) -> SheetValidation {
    let got = import_sheet_validation(path).expect("import");
    assert_eq!(got.len(), 1, "one sheet carries validation");
    assert_eq!(got[0].sheet_index, 0);
    got[0].validation.clone()
}

// ------------------------------------------------------------- the six ----

#[test]
fn a_list_rule_round_trips_and_writes_a_dropdown() {
    let mut v = SheetValidation::new();
    v.push(
        RangeValidation::list(
            TableRange::new(1, 0, 500, 0),
            vec!["North".into(), "South".into(), "East".into()],
        )
        .with_message("Pick a region from the list"),
    )
    .unwrap();

    let t = TempXlsx::new("list");
    export(t.path(), &demo_sheet(), &v);

    let xml = part(t.path(), "xl/worksheets/sheet1.xml").expect("sheet part");
    assert!(
        xml.contains("<dataValidations"),
        "the worksheet must carry a dataValidations element, got:\n{}",
        &xml[..xml.len().min(600)]
    );
    assert!(
        xml.contains("type=\"list\""),
        "a list validation, got:\n{xml}"
    );
    assert!(
        xml.contains("A2:A501"),
        "the rule's own range, not the sheet's extent, got:\n{xml}"
    );
    assert!(
        xml.contains("North,South,East"),
        "the allowed values reach the file, got:\n{xml}"
    );
    assert!(
        !xml.contains("showDropDown=\"1\""),
        "showDropDown=\"1\" HIDES the dropdown in OOXML; it must be absent, \
         got:\n{xml}"
    );

    let back = reimport(t.path());
    assert_eq!(back.len(), 1);
    let r = back.get(0).unwrap();
    assert_eq!(r.domain, ValueDomain::List);
    assert_eq!(
        r.list_values(),
        Some(&["North".to_string(), "South".to_string(), "East".to_string()][..])
    );
    assert_eq!(r.range, TableRange::new(1, 0, 500, 0));
    assert_eq!(r.message.as_deref(), Some("Pick a region from the list"));
    assert!(r.show_dropdown);
}

#[test]
fn a_hidden_dropdown_survives_as_hidden() {
    let mut v = SheetValidation::new();
    v.push(
        RangeValidation::list(TableRange::new(0, 0, 9, 0), vec!["Yes".into()]).with_dropdown(false),
    )
    .unwrap();
    let t = TempXlsx::new("nodrop");
    export(t.path(), &demo_sheet(), &v);
    let xml = part(t.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(
        xml.contains("showDropDown=\"1\""),
        "suppressing the dropdown is spelled showDropDown=\"1\", got:\n{xml}"
    );
    assert!(!reimport(t.path()).get(0).unwrap().show_dropdown);
}

#[test]
fn a_whole_number_rule_round_trips() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::new(
        TableRange::new(1, 1, 100, 1),
        ValueDomain::WholeNumber,
        ValidationRule::Between {
            min: 1.0,
            max: 100.0,
        },
    ))
    .unwrap();
    let t = TempXlsx::new("whole");
    export(t.path(), &demo_sheet(), &v);
    let xml = part(t.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(xml.contains("type=\"whole\""), "got:\n{xml}");

    let r = reimport(t.path());
    let r = r.get(0).unwrap();
    assert_eq!(r.domain, ValueDomain::WholeNumber);
    assert_eq!(
        r.rule,
        ValidationRule::Between {
            min: 1.0,
            max: 100.0
        }
    );
    assert_eq!(r.range, TableRange::new(1, 1, 100, 1));
}

#[test]
fn a_decimal_comparison_round_trips_with_its_operator() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::new(
        TableRange::new(0, 1, 20, 1),
        ValueDomain::Decimal,
        ValidationRule::Compare {
            op: CmpOp::Gt,
            value: 0.5,
        },
    ))
    .unwrap();
    let t = TempXlsx::new("dec");
    export(t.path(), &demo_sheet(), &v);
    let xml = part(t.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(xml.contains("type=\"decimal\""), "got:\n{xml}");
    assert!(xml.contains("operator=\"greaterThan\""), "got:\n{xml}");

    let r = reimport(t.path());
    assert_eq!(
        r.get(0).unwrap().rule,
        ValidationRule::Compare {
            op: CmpOp::Gt,
            value: 0.5
        },
        "the operator must survive, not collapse to a bare between"
    );
}

#[test]
fn a_date_rule_round_trips() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::new(
        TableRange::new(0, 2, 50, 2),
        ValueDomain::Date,
        ValidationRule::Between {
            min: 44_000.0,
            max: 45_000.0,
        },
    ))
    .unwrap();
    let t = TempXlsx::new("date");
    export(t.path(), &demo_sheet(), &v);
    let r = reimport(t.path());
    let r = r.get(0).unwrap();
    // Serial dates are written as decimals so the bounds stay exact — the
    // same choice `table_xlsx` makes and for the same reason. What matters is
    // that the BOUNDS survive.
    assert_eq!(
        r.rule,
        ValidationRule::Between {
            min: 44_000.0,
            max: 45_000.0
        }
    );
}

#[test]
fn a_text_length_rule_round_trips() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::new(
        TableRange::new(0, 2, 30, 2),
        ValueDomain::TextLength,
        ValidationRule::TextLength { min: 2, max: 40 },
    ))
    .unwrap();
    let t = TempXlsx::new("len");
    export(t.path(), &demo_sheet(), &v);
    let xml = part(t.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(xml.contains("type=\"textLength\""), "got:\n{xml}");
    let r = reimport(t.path());
    assert_eq!(
        r.get(0).unwrap().rule,
        ValidationRule::TextLength { min: 2, max: 40 }
    );
}

#[test]
fn a_custom_formula_rule_round_trips() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::new(
        TableRange::new(0, 1, 10, 1),
        ValueDomain::Custom,
        ValidationRule::CustomFormula("=MOD(B1,2)=0".into()),
    ))
    .unwrap();
    let t = TempXlsx::new("custom");
    export(t.path(), &demo_sheet(), &v);
    let xml = part(t.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(xml.contains("type=\"custom\""), "got:\n{xml}");
    let r = reimport(t.path());
    assert_eq!(
        r.get(0).unwrap().custom_formula(),
        Some("=MOD(B1,2)=0"),
        "the leading `=` is Ferrix's spelling; Excel stores it without one, \
         and the reader must put it back"
    );
}

// ------------------------------------------------ styles and messages ----

#[test]
fn stop_and_warning_are_distinguishable_after_a_round_trip() {
    let mut v = SheetValidation::new();
    v.push(
        RangeValidation::list(TableRange::new(0, 0, 5, 0), vec!["a".into()])
            .with_style(ErrorStyle::Stop),
    )
    .unwrap();
    v.push(
        RangeValidation::list(TableRange::new(0, 1, 5, 1), vec!["b".into()])
            .with_style(ErrorStyle::Warning)
            .with_message("this looks wrong but you may keep it")
            .with_title("Check this"),
    )
    .unwrap();

    let t = TempXlsx::new("style");
    export(t.path(), &demo_sheet(), &v);
    let xml = part(t.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(xml.contains("errorStyle=\"warning\""), "got:\n{xml}");

    let back = reimport(t.path());
    assert_eq!(back.len(), 2, "both rules survive");
    let stop = back
        .rules()
        .iter()
        .find(|r| r.range.first_col == 0)
        .expect("column A rule");
    let warn = back
        .rules()
        .iter()
        .find(|r| r.range.first_col == 1)
        .expect("column B rule");
    assert!(stop.style.rejects(), "Stop must still reject");
    assert!(!warn.style.rejects(), "Warning must still allow");
    assert_eq!(
        warn.message.as_deref(),
        Some("this looks wrong but you may keep it")
    );
    assert_eq!(warn.title.as_deref(), Some("Check this"));
}

#[test]
fn allow_blank_survives() {
    let mut v = SheetValidation::new();
    v.push(
        RangeValidation::list(TableRange::new(0, 0, 5, 0), vec!["a".into()])
            .with_allow_empty(false),
    )
    .unwrap();
    v.push(
        RangeValidation::list(TableRange::new(0, 1, 5, 1), vec!["b".into()]).with_allow_empty(true),
    )
    .unwrap();
    let t = TempXlsx::new("blank");
    export(t.path(), &demo_sheet(), &v);
    let back = reimport(t.path());
    let strict = back
        .rules()
        .iter()
        .find(|r| r.range.first_col == 0)
        .unwrap();
    let lax = back
        .rules()
        .iter()
        .find(|r| r.range.first_col == 1)
        .unwrap();
    assert!(!strict.allow_empty, "allowBlank=0 must not flip to 1");
    assert!(lax.allow_empty);
}

// ------------------------------------------------------------- scale ----

/// A rule over 200M rows is ONE element, clamped to Excel's own row limit.
///
/// Asserts on the emitted XML's `sqref` rather than on the model, because the
/// failure this guards against is the writer expanding a range into per-cell
/// references — which would produce a multi-gigabyte part.
#[test]
fn a_rule_over_200m_rows_writes_one_clamped_element() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::new(
        TableRange::new(0, 0, 199_999_999, 0),
        ValueDomain::WholeNumber,
        ValidationRule::Between { min: 0.0, max: 9.0 },
    ))
    .unwrap();
    let t = TempXlsx::new("scale");
    export(t.path(), &demo_sheet(), &v);
    let xml = part(t.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert_eq!(
        xml.matches("<dataValidation ").count(),
        1,
        "one rule is one element, whatever the row count; got:\n{xml}"
    );
    assert!(
        xml.contains("A1:A1048576"),
        "the sqref must be clamped to Excel's row limit, got:\n{xml}"
    );
    let meta = std::fs::metadata(t.path()).unwrap();
    assert!(
        meta.len() < 100_000,
        "a 200M-row rule must not inflate the file; got {} bytes",
        meta.len()
    );
}

// ------------------------------------------------------------ the trap ----

/// The production export path must carry validation.
///
/// `export_workbook_full` is the variant `FerrixApp::export_xlsx_to` calls.
/// A sibling variant that silently omitted validation is exactly the bug this
/// asserts against — see the same note on protection in `protect_xlsx`.
#[test]
fn export_workbook_full_carries_sheet_validation() {
    let mut v = SheetValidation::new();
    v.push(RangeValidation::list(
        TableRange::new(0, 0, 9, 0),
        vec!["North".into()],
    ))
    .unwrap();
    let sheet = demo_sheet();
    let t = TempXlsx::new("full");
    crate::xlsx::export_workbook_full(
        t.path(),
        &[crate::xlsx::SheetExport::new("Data", &sheet).with_validation(&v)],
        &ferrix_formula::NameTable::new(),
        &ferrix_core::WorkbookProtection::new(),
    )
    .expect("export");
    let xml = part(t.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(
        xml.contains("type=\"list\""),
        "the FULL export variant — the one the menu item runs — must write \
         validation too, got:\n{xml}"
    );
    assert_eq!(reimport(t.path()).len(), 1);
}

#[test]
fn a_sheet_with_no_rules_writes_no_element() {
    let t = TempXlsx::new("none");
    export(t.path(), &demo_sheet(), &SheetValidation::new());
    let xml = part(t.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(
        !xml.contains("<dataValidation"),
        "an unvalidated sheet must cost nothing, got:\n{xml}"
    );
    assert!(import_sheet_validation(t.path()).unwrap().is_empty());
}

// --------------------------------------------------------------- loss ----

#[test]
fn a_regex_rule_is_reported_as_lossy_rather_than_silently_dropped() {
    let rule = RangeValidation::new(
        TableRange::new(0, 0, 9, 0),
        ValueDomain::Any,
        ValidationRule::Regex("[A-Z]{3}".into()),
    );
    let loss = sheet_validation_xlsx_loss(&rule);
    assert_eq!(loss.len(), 1, "the user must be told, got {loss:?}");
    assert!(loss[0].contains("regular-expression"));
}

#[test]
fn an_over_long_list_is_reported() {
    let vals: Vec<String> = (0..60).map(|i| format!("value-number-{i:03}")).collect();
    let rule = RangeValidation::list(TableRange::new(0, 0, 9, 0), vals);
    let loss = sheet_validation_xlsx_loss(&rule);
    assert_eq!(loss.len(), 1, "got {loss:?}");
    assert!(loss[0].contains("255-character"));
}

#[test]
fn an_ordinary_rule_reports_no_loss() {
    let rule = RangeValidation::list(TableRange::new(0, 0, 9, 0), vec!["a".into(), "b".into()]);
    assert!(sheet_validation_xlsx_loss(&rule).is_empty());
}
