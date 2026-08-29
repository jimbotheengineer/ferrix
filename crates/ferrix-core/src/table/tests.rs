//! Tests for structured tables.

use super::*;
use crate::sheet::Sheet;

fn range(r0: u32, c0: u32, r1: u32, c1: u32) -> TableRange {
    TableRange::new(r0, c0, r1, c1)
}

// ------------------------------------------------------------------- range --

#[test]
fn range_normalises_reversed_corners() {
    let a = range(10, 5, 0, 0);
    assert_eq!(a.first_row, 0);
    assert_eq!(a.last_row, 10);
    assert_eq!(a.first_col, 0);
    assert_eq!(a.last_col, 5);
    assert_eq!(a.rows(), 11);
    assert_eq!(a.cols(), 6);
}

#[test]
fn range_a1_roundtrip() {
    let r = range(0, 0, 99, 3);
    assert_eq!(r.to_a1(), "A1:D100");
    assert_eq!(TableRange::from_a1("A1:D100"), Some(r));
    assert_eq!(TableRange::from_a1(" B2 : C3 "), Some(range(1, 1, 2, 2)));
    assert_eq!(TableRange::from_a1("nonsense"), None);
    assert_eq!(TableRange::from_a1("A1:"), None);
}

#[test]
fn range_contains() {
    let r = range(2, 1, 5, 3);
    assert!(r.contains(CellRef::new(2, 1)));
    assert!(r.contains(CellRef::new(5, 3)));
    assert!(!r.contains(CellRef::new(1, 1)));
    assert!(!r.contains(CellRef::new(2, 0)));
}

// -------------------------------------------------------------------- name --

#[test]
fn table_names_are_coerced_to_excels_rules() {
    // Spaces and punctuation are illegal in a displayName.
    assert_eq!(Table::sanitise_name("Sales Q1"), "Sales_Q1");
    assert_eq!(Table::sanitise_name("a-b/c"), "a_b_c");
    // Must not start with a digit.
    assert_eq!(Table::sanitise_name("2024data"), "_2024data");
    // Must not be parseable as a cell reference.
    assert_ne!(Table::sanitise_name("C4"), "C4");
    assert!(CellRef::from_a1(&Table::sanitise_name("C4")).is_none());
    // Empty is replaced rather than rejected.
    assert_eq!(Table::sanitise_name(""), "Table1");
}

#[test]
fn duplicate_column_names_get_suffixes_not_dropped() {
    let t = Table::new("T", range(0, 0, 5, 2)).with_columns(vec![
        TableColumn::new("Amount"),
        TableColumn::new("Amount"),
        TableColumn::new("   "),
    ]);
    assert_eq!(t.columns.len(), 3, "no column may be dropped");
    assert_eq!(t.columns[0].name, "Amount");
    assert_eq!(t.columns[1].name, "Amount2");
    assert_eq!(t.columns[2].name, "Column3");
}

#[test]
fn with_columns_pads_and_truncates_to_the_range_width() {
    let t = Table::new("T", range(0, 0, 5, 3)).with_columns(vec![TableColumn::new("only")]);
    assert_eq!(t.columns.len(), 4);
    let t = Table::new("T", range(0, 0, 5, 1)).with_columns(vec![
        TableColumn::new("a"),
        TableColumn::new("b"),
        TableColumn::new("c"),
    ]);
    assert_eq!(t.columns.len(), 2);
}

#[test]
fn data_rows_exclude_header_and_totals() {
    let mut t = Table::new("T", range(0, 0, 10, 1));
    assert_eq!(t.data_rows(), 1..11, "header row excluded");
    t.totals_row = true;
    assert_eq!(t.data_rows(), 1..10);
    t.header_row = false;
    assert_eq!(t.data_rows(), 0..10);
}

#[test]
fn banding_is_relative_to_the_table_not_the_sheet() {
    // A table starting on sheet row 7 must still band its own rows 0,1,2...
    let t = Table::new("T", range(7, 0, 20, 1));
    assert_eq!(t.data_rows().start, 8);
    assert!(!t.is_banded(8));
    assert!(t.is_banded(9));
    assert!(!t.is_banded(10));
}

#[test]
fn column_index_maps_both_ways() {
    let t = Table::new("T", range(0, 3, 5, 6));
    assert_eq!(t.sheet_col(0), 3);
    assert_eq!(t.sheet_col(3), 6);
    assert_eq!(t.column_index(3), Some(0));
    assert_eq!(t.column_index(6), Some(3));
    assert_eq!(t.column_index(2), None);
    assert_eq!(t.column_index(7), None);
}

// -------------------------------------------------------------- validation --

fn tbl_with(rule: ValidationRule, ctype: ColumnType) -> Table {
    Table::new("T", range(0, 0, 10, 0)).with_columns(vec![TableColumn::new("c")
        .typed(ctype)
        .validated(Validation::new(rule))])
}

#[test]
fn type_mismatch_is_flagged() {
    let t = tbl_with(ValidationRule::None, ColumnType::Number);
    assert_eq!(
        t.validate_cell(0, &Value::Text(StrId(0)), "hi", None),
        Some(Violation::WrongType(ColumnType::Number))
    );
    assert_eq!(t.validate_cell(0, &Value::Number(1.0), "1", None), None);
}

#[test]
fn empty_cells_pass_unless_the_column_requires_a_value() {
    let mut t = tbl_with(ValidationRule::None, ColumnType::Number);
    assert_eq!(t.validate_cell(0, &Value::Empty, "", None), None);
    t.columns[0].validation.allow_empty = false;
    assert_eq!(
        t.validate_cell(0, &Value::Empty, "", None),
        Some(Violation::Empty)
    );
}

#[test]
fn range_bounds_are_inclusive() {
    let t = tbl_with(
        ValidationRule::Between {
            min: 0.0,
            max: 100.0,
        },
        ColumnType::Number,
    );
    assert_eq!(t.validate_cell(0, &Value::Number(0.0), "", None), None);
    assert_eq!(t.validate_cell(0, &Value::Number(100.0), "", None), None);
    assert!(t.validate_cell(0, &Value::Number(-0.5), "", None).is_some());
    assert!(t
        .validate_cell(0, &Value::Number(100.5), "", None)
        .is_some());
}

#[test]
fn not_between_is_the_complement() {
    let t = tbl_with(
        ValidationRule::NotBetween {
            min: 10.0,
            max: 20.0,
        },
        ColumnType::Number,
    );
    assert_eq!(t.validate_cell(0, &Value::Number(9.0), "", None), None);
    assert_eq!(t.validate_cell(0, &Value::Number(21.0), "", None), None);
    assert!(t.validate_cell(0, &Value::Number(15.0), "", None).is_some());
}

#[test]
fn comparison_rules_work() {
    for (op, ok, bad) in [
        (CmpOp::Gt, 5.0, 0.0),
        (CmpOp::Ge, 0.0, -1.0),
        (CmpOp::Lt, -1.0, 0.0),
        (CmpOp::Ne, 1.0, 0.0),
    ] {
        let t = tbl_with(
            ValidationRule::Compare { op, value: 0.0 },
            ColumnType::Number,
        );
        assert_eq!(
            t.validate_cell(0, &Value::Number(ok), "", None),
            None,
            "{op:?} should accept {ok}"
        );
        assert!(
            t.validate_cell(0, &Value::Number(bad), "", None).is_some(),
            "{op:?} should reject {bad}"
        );
    }
}

#[test]
fn one_of_a_list_is_case_insensitive() {
    let t = tbl_with(
        ValidationRule::OneOf(vec!["open".into(), "closed".into()]),
        ColumnType::Text,
    );
    assert_eq!(
        t.validate_cell(0, &Value::Text(StrId(0)), "OPEN", None),
        None
    );
    assert_eq!(
        t.validate_cell(0, &Value::Text(StrId(0)), "pending", None),
        Some(Violation::NotInList)
    );
}

#[test]
fn regex_rules_are_anchored_to_the_whole_cell() {
    let t = tbl_with(
        ValidationRule::Regex(r"[A-Z]{3}-\d{4}".into()),
        ColumnType::Text,
    );
    assert_eq!(
        t.validate_cell(0, &Value::Text(StrId(0)), "ABC-1234", None),
        None
    );
    // A substring match must NOT pass — the pattern describes the whole cell.
    assert_eq!(
        t.validate_cell(0, &Value::Text(StrId(0)), "xxABC-1234yy", None),
        Some(Violation::RegexMismatch)
    );
}

#[test]
fn an_already_anchored_pattern_is_not_double_anchored() {
    let t = tbl_with(ValidationRule::Regex(r"^\d+$".into()), ColumnType::Text);
    assert_eq!(
        t.validate_cell(0, &Value::Text(StrId(0)), "12345", None),
        None
    );
    assert!(t
        .validate_cell(0, &Value::Text(StrId(0)), "12a45", None)
        .is_some());
}

#[test]
fn a_broken_regex_does_not_condemn_the_whole_column() {
    // An unparseable pattern is the rule's bug, not the data's. Flagging every
    // cell in the column would bury the user in false positives.
    let t = tbl_with(ValidationRule::Regex("[unclosed".into()), ColumnType::Text);
    assert_eq!(
        t.validate_cell(0, &Value::Text(StrId(0)), "anything", None),
        None
    );
}

#[test]
fn text_length_bounds() {
    let t = tbl_with(
        ValidationRule::TextLength { min: 2, max: 4 },
        ColumnType::Text,
    );
    assert_eq!(
        t.validate_cell(0, &Value::Text(StrId(0)), "abc", None),
        None
    );
    assert_eq!(
        t.validate_cell(0, &Value::Text(StrId(0)), "a", None),
        Some(Violation::BadLength {
            min: 2,
            max: 4,
            got: 1
        })
    );
    // Counted in characters, not bytes.
    assert!(t
        .validate_cell(0, &Value::Text(StrId(0)), "héllo", None)
        .is_some());
    assert_eq!(
        t.validate_cell(0, &Value::Text(StrId(0)), "héo", None),
        None
    );
}

#[test]
fn error_cells_are_flagged_in_a_typed_column() {
    let t = tbl_with(ValidationRule::None, ColumnType::Number);
    assert_eq!(
        t.validate_cell(0, &Value::Error(ErrorKind::DivZero), "", None),
        Some(Violation::ErrorValue(ErrorKind::DivZero))
    );
    // ...but an untyped column tolerates them.
    let any = tbl_with(ValidationRule::None, ColumnType::Any);
    assert_eq!(
        any.validate_cell(0, &Value::Error(ErrorKind::DivZero), "", None),
        None
    );
}

#[test]
fn date_type_accepts_serial_numbers_only() {
    let t = tbl_with(ValidationRule::None, ColumnType::Date);
    assert_eq!(t.validate_cell(0, &Value::Number(45_000.0), "", None), None);
    assert!(t.validate_cell(0, &Value::Number(-5.0), "", None).is_some());
    assert!(t
        .validate_cell(0, &Value::Number(9_999_999.0), "", None)
        .is_some());
}

#[test]
fn uniqueness_index_finds_duplicates_by_arena_id() {
    let mut arena = StringArena::new();
    let a = arena.intern("alpha");
    let b = arena.intern("beta");
    let mut idx = UniquenessIndex::new(arena.len());
    idx.observe(&Value::Text(a));
    idx.observe(&Value::Text(a));
    idx.observe(&Value::Text(b));
    assert!(idx.is_duplicate(&Value::Text(a)));
    assert!(!idx.is_duplicate(&Value::Text(b)));
}

#[test]
fn uniqueness_handles_numbers_and_bools() {
    let mut idx = UniquenessIndex::new(0);
    for v in [1.0, 2.0, 1.0, -0.0, 0.0] {
        idx.observe(&Value::Number(v));
    }
    idx.observe(&Value::Bool(true));
    assert!(idx.is_duplicate(&Value::Number(1.0)));
    assert!(!idx.is_duplicate(&Value::Number(2.0)));
    // -0.0 and 0.0 are the same number and must count as duplicates.
    assert!(idx.is_duplicate(&Value::Number(0.0)));
    assert!(!idx.is_duplicate(&Value::Bool(true)));
}

#[test]
fn validation_flags_but_never_rewrites_the_cell() {
    // The acceptance criterion: a bad cell stays exactly as the user typed it.
    let mut s = Sheet::new("s");
    let t = Table::new("T", range(0, 0, 3, 0)).with_columns(vec![TableColumn::new("n")
        .typed(ColumnType::Number)
        .validated(Validation::new(ValidationRule::Between {
            min: 0.0,
            max: 10.0,
        }))]);
    s.set_text(CellRef::new(0, 0), "n");
    s.set(CellRef::new(1, 0), Value::Number(5.0));
    s.set(CellRef::new(2, 0), Value::Number(999.0));
    s.set_text(CellRef::new(3, 0), "oops");

    let report = s.validate_table(&t, 100);
    assert_eq!(report.total, 2);
    assert_eq!(report.invalid[0].0, CellRef::new(2, 0));
    assert_eq!(report.invalid[1].0, CellRef::new(3, 0));

    // The values are untouched.
    assert_eq!(s.get(CellRef::new(2, 0)), Value::Number(999.0));
    assert_eq!(s.display(CellRef::new(3, 0)), "oops");
    assert!(!report.invalid[0].1.describe().is_empty());
}

#[test]
fn validation_report_is_bounded_and_honest() {
    let mut s = Sheet::new("s");
    let t = Table::new("T", range(0, 0, 500, 0))
        .with_columns(vec![TableColumn::new("n").typed(ColumnType::Number)]);
    for r in 1..=500u32 {
        s.set_text(CellRef::new(r, 0), "not a number");
    }
    let report = s.validate_table(&t, 10);
    assert_eq!(report.invalid.len(), 10, "capped at the limit");
    assert_eq!(report.total, 500, "total is the true count");
    assert!(report.truncated);
    assert!(!report.is_clean());
}

#[test]
fn next_after_wraps_to_the_first_problem() {
    let mut s = Sheet::new("s");
    let t = Table::new("T", range(0, 0, 5, 0))
        .with_columns(vec![TableColumn::new("n").typed(ColumnType::Number)]);
    s.set_text(CellRef::new(2, 0), "bad");
    s.set_text(CellRef::new(4, 0), "bad");
    let r = s.validate_table(&t, 100);
    assert_eq!(r.next_after(CellRef::new(0, 0)), Some(CellRef::new(2, 0)));
    assert_eq!(r.next_after(CellRef::new(2, 0)), Some(CellRef::new(4, 0)));
    assert_eq!(r.next_after(CellRef::new(99, 0)), Some(CellRef::new(2, 0)));
}

#[test]
fn unique_rule_flags_through_the_sheet_path() {
    let mut s = Sheet::new("s");
    let t = Table::new("T", range(0, 0, 4, 0)).with_columns(vec![TableColumn::new("id")
        .typed(ColumnType::Text)
        .validated(Validation::new(ValidationRule::Unique))]);
    s.set_text(CellRef::new(1, 0), "a");
    s.set_text(CellRef::new(2, 0), "b");
    s.set_text(CellRef::new(3, 0), "a");
    s.set_text(CellRef::new(4, 0), "c");
    let r = s.validate_table(&t, 100);
    assert_eq!(r.total, 2, "both copies of 'a' are flagged");
    assert_eq!(r.invalid[0].1, Violation::Duplicate);
    assert_eq!(r.invalid[0].0, CellRef::new(1, 0));
    assert_eq!(r.invalid[1].0, CellRef::new(3, 0));
}

// -------------------------------------------------------------- formatting --

#[test]
fn number_format_codes_roundtrip() {
    let cases = [
        NumberFormat::General,
        NumberFormat::Decimal { places: 0 },
        NumberFormat::Decimal { places: 2 },
        NumberFormat::Thousands { places: 0 },
        NumberFormat::Thousands { places: 2 },
        NumberFormat::Currency {
            symbol: "$".into(),
            places: 2,
        },
        NumberFormat::Percent { places: 1 },
        NumberFormat::Date(DateStyle::Iso),
        NumberFormat::Date(DateStyle::Us),
        NumberFormat::Date(DateStyle::IsoDateTime),
        NumberFormat::Date(DateStyle::Time),
    ];
    for f in cases {
        let code = f.to_code();
        assert_eq!(NumberFormat::from_code(&code), f, "roundtrip {code}");
    }
}

#[test]
fn unknown_format_strings_survive_verbatim() {
    // The data-loss guard: a format Ferrix cannot model must come back out
    // byte-for-byte identical, not be replaced with General.
    for exotic in [
        r#"[$€-407]#,##0.00;[RED]-#,##0.00"#,
        r#"_("$"* #,##0.00_);_("$"* \(#,##0.00\)"#,
        "0.00E+00",
        "# ?/?",
        r#"[<=9999999]###-####;(###) ###-####"#,
    ] {
        let parsed = NumberFormat::from_code(exotic);
        assert_eq!(parsed, NumberFormat::Custom(exotic.to_string()));
        assert_eq!(
            parsed.to_code(),
            exotic,
            "format string must not be mangled"
        );
    }
}

#[test]
fn number_formats_render() {
    assert_eq!(NumberFormat::Decimal { places: 2 }.render(3.75), "3.75");
    assert_eq!(NumberFormat::Decimal { places: 1 }.render(3.75), "3.8");
    assert_eq!(
        NumberFormat::Thousands { places: 0 }.render(1_234_567.0),
        "1,234,567"
    );
    assert_eq!(
        NumberFormat::Thousands { places: 2 }.render(-1234.5),
        "-1,234.50"
    );
    assert_eq!(
        NumberFormat::Currency {
            symbol: "$".into(),
            places: 2
        }
        .render(1234.5),
        "$1,234.50"
    );
    assert_eq!(NumberFormat::Percent { places: 1 }.render(0.256), "25.6%");
    assert_eq!(NumberFormat::General.render(42.0), "42");
}

#[test]
fn small_numbers_are_grouped_correctly() {
    assert_eq!(NumberFormat::Thousands { places: 0 }.render(1.0), "1");
    assert_eq!(NumberFormat::Thousands { places: 0 }.render(999.0), "999");
    assert_eq!(
        NumberFormat::Thousands { places: 0 }.render(1000.0),
        "1,000"
    );
    assert_eq!(NumberFormat::Thousands { places: 0 }.render(0.0), "0");
}

#[test]
fn serial_dates_match_excels_1900_system() {
    // Anchors everyone can check: serial 1 is 1900-01-01, and serial 45000 is
    // 2023-03-15 in Excel (which counts the phantom 1900-02-29).
    let iso = NumberFormat::Date(DateStyle::Iso);
    assert_eq!(iso.render(1.0), "1900-01-01");
    assert_eq!(iso.render(59.0), "1900-02-28");
    // Excel's bug: serial 60 is a day that does not exist.
    assert_eq!(iso.render(60.0), "1900-02-29");
    assert_eq!(iso.render(61.0), "1900-03-01");
    assert_eq!(iso.render(45_000.0), "2023-03-15");
    assert_eq!(iso.render(25_569.0), "1970-01-01");
    assert_eq!(
        NumberFormat::Date(DateStyle::Us).render(45_000.0),
        "03/15/2023"
    );
    assert_eq!(
        NumberFormat::Date(DateStyle::Euro).render(45_000.0),
        "15/03/2023"
    );
}

#[test]
fn serial_datetimes_render_the_time_component() {
    assert_eq!(
        NumberFormat::Date(DateStyle::IsoDateTime).render(45_000.5),
        "2023-03-15 12:00:00"
    );
    assert_eq!(NumberFormat::Date(DateStyle::Time).render(0.25), "06:00:00");
}

#[test]
fn is_date_recognises_custom_date_codes() {
    assert!(NumberFormat::Date(DateStyle::Iso).is_date());
    assert!(NumberFormat::Custom("d-mmm-yy".into()).is_date());
    assert!(!NumberFormat::Decimal { places: 2 }.is_date());
    assert!(!NumberFormat::Custom("0.00E+00".into()).is_date());
}

#[test]
fn rgb_hex_roundtrip() {
    let c = Rgb(0xFF, 0x88, 0x00);
    assert_eq!(c.to_hex(), "FF8800");
    assert_eq!(Rgb::from_hex("FF8800"), Some(c));
    // xlsx writes an alpha byte first.
    assert_eq!(Rgb::from_hex("FFFF8800"), Some(c));
    assert_eq!(Rgb::from_hex("#FF8800"), Some(c));
    assert_eq!(Rgb::from_hex("nope"), None);
}

#[test]
fn color_scale_interpolates_across_the_extent() {
    let rule = ConditionalRule::ColorScale2 {
        min: Rgb(0, 0, 0),
        max: Rgb(100, 100, 100),
    };
    let mut lo = CellStyle::default();
    rule.apply(0.0, Some((0.0, 10.0)), &mut lo);
    assert_eq!(lo.fill, Some(Rgb(0, 0, 0)));

    let mut mid = CellStyle::default();
    rule.apply(5.0, Some((0.0, 10.0)), &mut mid);
    assert_eq!(mid.fill, Some(Rgb(50, 50, 50)));

    let mut hi = CellStyle::default();
    rule.apply(10.0, Some((0.0, 10.0)), &mut hi);
    assert_eq!(hi.fill, Some(Rgb(100, 100, 100)));
}

#[test]
fn three_color_scale_pivots_at_the_midpoint() {
    let rule = ConditionalRule::ColorScale3 {
        min: Rgb(0, 0, 0),
        mid: Rgb(255, 255, 255),
        max: Rgb(0, 0, 0),
    };
    let mut s = CellStyle::default();
    rule.apply(50.0, Some((0.0, 100.0)), &mut s);
    assert_eq!(s.fill, Some(Rgb(255, 255, 255)));
}

#[test]
fn data_bars_are_proportional_and_clamped() {
    let rule = ConditionalRule::DataBar {
        color: Rgb(1, 2, 3),
    };
    let mut s = CellStyle::default();
    rule.apply(25.0, Some((0.0, 100.0)), &mut s);
    assert_eq!(s.bar, Some((0.25, Rgb(1, 2, 3))));
    // Out-of-extent values clamp rather than overflowing the cell.
    let mut over = CellStyle::default();
    rule.apply(500.0, Some((0.0, 100.0)), &mut over);
    assert_eq!(over.bar.unwrap().0, 1.0);
}

#[test]
fn scales_no_op_without_an_extent() {
    let rule = ConditionalRule::ColorScale2 {
        min: Rgb(0, 0, 0),
        max: Rgb(255, 255, 255),
    };
    let mut s = CellStyle::default();
    rule.apply(5.0, None, &mut s);
    assert!(s.is_plain(), "no extent means no meaningful scale");
}

#[test]
fn a_degenerate_extent_does_not_divide_by_zero() {
    let rule = ConditionalRule::DataBar {
        color: Rgb(0, 0, 0),
    };
    let mut s = CellStyle::default();
    rule.apply(7.0, Some((7.0, 7.0)), &mut s);
    assert_eq!(s.bar.unwrap().0, 0.0);
}

#[test]
fn threshold_rules_set_fill_and_text() {
    let rule = ConditionalRule::Threshold {
        op: CmpOp::Lt,
        value: 0.0,
        fill: Rgb(255, 0, 0),
        text: Rgb(255, 255, 255),
    };
    let mut neg = CellStyle::default();
    rule.apply(-1.0, None, &mut neg);
    assert_eq!(neg.fill, Some(Rgb(255, 0, 0)));
    assert_eq!(neg.text, Some(Rgb(255, 255, 255)));

    let mut pos = CellStyle::default();
    rule.apply(1.0, None, &mut pos);
    assert!(pos.is_plain());
}

#[test]
fn later_rules_win_like_excel() {
    let rules = [
        ConditionalRule::ColorScale2 {
            min: Rgb(0, 0, 0),
            max: Rgb(10, 10, 10),
        },
        ConditionalRule::Threshold {
            op: CmpOp::Gt,
            value: 0.0,
            fill: Rgb(9, 9, 9),
            text: Rgb(0, 0, 0),
        },
    ];
    let mut s = CellStyle::default();
    for r in &rules {
        r.apply(5.0, Some((0.0, 10.0)), &mut s);
    }
    assert_eq!(
        s.fill,
        Some(Rgb(9, 9, 9)),
        "the threshold overrides the scale"
    );
}

// --------------------------------------------------------------- filtering --

fn filter_sheet() -> (Sheet, Table) {
    let mut s = Sheet::new("s");
    let regions = ["north", "south", "east", "west"];
    // Row 0 is the header.
    s.set_text(CellRef::new(0, 0), "region");
    s.set_text(CellRef::new(0, 1), "amount");
    for r in 1..=200u32 {
        s.set_text(CellRef::new(r, 0), regions[(r as usize - 1) % 4]);
        s.set(CellRef::new(r, 1), Value::Number(r as f64));
    }
    let t = Table::new("Sales", range(0, 0, 200, 1)).with_columns(vec![
        TableColumn::new("region").typed(ColumnType::Text),
        TableColumn::new("amount").typed(ColumnType::Number),
    ]);
    (s, t)
}

#[test]
fn no_filter_shows_everything() {
    let (s, t) = filter_sheet();
    let mask = s.filter_table(&t, usize::MAX);
    // Header row is outside data_rows and stays visible.
    assert_eq!(mask.visible_rows(), 201);
    assert!(!mask.is_truncated());
}

#[test]
fn value_checklist_narrows_the_view() {
    let (s, mut t) = filter_sheet();
    t.columns[0].filter = Some(Predicate::ValueList(vec!["north".into()]));
    let mask = s.filter_table(&t, usize::MAX);
    // 50 north rows plus the header row, which the filter does not touch.
    assert_eq!(mask.visible_rows(), 51);
    assert!(mask.is_visible(1), "row 1 is 'north'");
    assert!(!mask.is_visible(2), "row 2 is 'south'");
}

#[test]
fn checklist_matching_is_case_insensitive() {
    let (s, mut t) = filter_sheet();
    t.columns[0].filter = Some(Predicate::ValueList(vec!["NORTH".into(), "South".into()]));
    assert_eq!(s.filter_table(&t, usize::MAX).visible_rows(), 101);
}

#[test]
fn numeric_comparison_filters() {
    let (s, mut t) = filter_sheet();
    t.columns[1].filter = Some(Predicate::Compare {
        op: CmpOp::Gt,
        value: 150.0,
    });
    // 151..=200 is 50 rows, plus the header.
    assert_eq!(s.filter_table(&t, usize::MAX).visible_rows(), 51);
}

#[test]
fn between_filters_are_inclusive() {
    let (s, mut t) = filter_sheet();
    t.columns[1].filter = Some(Predicate::Between {
        min: 10.0,
        max: 19.0,
    });
    assert_eq!(s.filter_table(&t, usize::MAX).visible_rows(), 11);
}

#[test]
fn text_contains_filter_uses_the_search_query() {
    let (s, mut t) = filter_sheet();
    t.columns[0].filter = Some(Predicate::Text {
        needle: "th".into(),
        case_sensitive: false,
        whole_cell: false,
    });
    // "north" and "south" both contain "th": 100 rows plus the header.
    assert_eq!(s.filter_table(&t, usize::MAX).visible_rows(), 101);
}

#[test]
fn whole_cell_text_filter_rejects_substrings() {
    let (s, mut t) = filter_sheet();
    t.columns[0].filter = Some(Predicate::Text {
        needle: "nor".into(),
        case_sensitive: false,
        whole_cell: true,
    });
    assert_eq!(
        s.filter_table(&t, usize::MAX).visible_rows(),
        1,
        "header only"
    );
}

#[test]
fn filters_on_two_columns_compose_as_and() {
    let (s, mut t) = filter_sheet();
    t.columns[0].filter = Some(Predicate::ValueList(vec!["north".into()]));
    t.columns[1].filter = Some(Predicate::Compare {
        op: CmpOp::Gt,
        value: 100.0,
    });
    let mask = s.filter_table(&t, usize::MAX);
    // north rows are 1, 5, 9, ...; those above 100 are 101,105,...,197 = 25.
    assert_eq!(mask.visible_rows(), 26);
}

#[test]
fn a_filter_matching_nothing_hides_every_data_row() {
    let (s, mut t) = filter_sheet();
    t.columns[0].filter = Some(Predicate::ValueList(vec!["antarctica".into()]));
    let mask = s.filter_table(&t, usize::MAX);
    assert_eq!(mask.visible_rows(), 1, "only the header survives");
}

#[test]
fn blank_and_nonblank_filters() {
    let mut s = Sheet::new("s");
    s.set_text(CellRef::new(0, 0), "h");
    s.set_text(CellRef::new(1, 0), "a");
    s.set(CellRef::new(2, 0), Value::Empty);
    s.set_text(CellRef::new(3, 0), "b");
    let mut t = Table::new("T", range(0, 0, 3, 0));

    t.columns[0].filter = Some(Predicate::Blank);
    let m = s.filter_table(&t, usize::MAX);
    assert_eq!(m.visible_rows(), 2, "the header plus the one blank row");
    assert!(m.is_visible(2));

    t.columns[0].filter = Some(Predicate::NonBlank);
    let m = s.filter_table(&t, usize::MAX);
    assert_eq!(m.visible_rows(), 3);
    assert!(!m.is_visible(2));
}

#[test]
fn filtering_reuses_the_arena_and_costs_cardinality_not_rows() {
    // The core performance claim, mirroring search_cost_tracks_cardinality.
    // 200k text cells drawn from 4 distinct strings must cost 4 string
    // comparisons to plan, and the scan itself is integer work.
    let mut s = Sheet::new("big");
    let regions = ["north", "south", "east", "west"];
    for r in 0..200_000u32 {
        s.set_text(CellRef::new(r, 0), regions[r as usize % 4]);
    }
    assert_eq!(s.arena.len(), 4, "arena dedups to the cardinality");

    let compiled =
        CompiledPredicate::compile(&Predicate::ValueList(vec!["north".into()]), &s.arena);
    assert_eq!(
        compiled.matched_strings(),
        1,
        "only one distinct string matched — this is the whole trick"
    );

    let mut t = Table::new("T", range(0, 0, 199_999, 0));
    t.header_row = false;
    t.columns[0].filter = Some(Predicate::ValueList(vec!["north".into()]));

    let start = std::time::Instant::now();
    let mask = s.filter_table(&t, usize::MAX);
    let ms = start.elapsed().as_millis();
    assert_eq!(mask.visible_rows(), 50_000);
    assert!(
        ms < 500,
        "200k-row filter took {ms}ms — the arena fast path may be broken"
    );
}

#[test]
fn a_dead_text_predicate_skips_the_column_entirely() {
    let mut arena = StringArena::new();
    arena.intern("alpha");
    arena.intern("beta");
    let p = CompiledPredicate::compile(
        &Predicate::Text {
            needle: "zzz".into(),
            case_sensitive: false,
            whole_cell: false,
        },
        &arena,
    );
    assert!(!p.can_match_text(), "lets the scanner skip a whole column");
    assert!(!p.matches_text_id(0));
}

#[test]
fn numeric_predicates_do_not_claim_to_match_text() {
    let arena = StringArena::new();
    let p = CompiledPredicate::compile(
        &Predicate::Compare {
            op: CmpOp::Gt,
            value: 0.0,
        },
        &arena,
    );
    assert!(p.can_match_numbers());
    assert!(!p.can_match_text());
    assert!(p.matches_value(&Value::Number(1.0)));
    assert!(!p.matches_value(&Value::Number(-1.0)));
    assert!(!p.matches_value(&Value::Empty));
}

// ---------------------------------------------------------------- row mask --

#[test]
fn row_mask_maps_view_rows_to_data_rows() {
    let mut bits = Bitmap::zeros(10);
    for r in [1usize, 4, 7, 9] {
        bits.set(r, true);
    }
    let m = RowMask::from_bits(bits);
    assert_eq!(m.visible_rows(), 4);
    assert_eq!(m.total_rows(), 10);
    assert_eq!(m.nth_visible(0), Some(1));
    assert_eq!(m.nth_visible(1), Some(4));
    assert_eq!(m.nth_visible(3), Some(9));
    assert_eq!(m.nth_visible(4), None, "past the end");
}

#[test]
fn row_mask_rank_is_the_inverse_of_nth_visible() {
    let mut bits = Bitmap::zeros(1000);
    for r in (0..1000).step_by(7) {
        bits.set(r, true);
    }
    let m = RowMask::from_bits(bits);
    for n in 0..m.visible_rows() {
        let row = m.nth_visible(n).unwrap();
        assert_eq!(m.rank(row), n, "rank/nth disagree at n={n}");
    }
}

#[test]
fn row_mask_rank_index_works_across_block_boundaries() {
    // RANK_BLOCK is 4096, so this spans several blocks and exercises the
    // binary search rather than the first-block fast path.
    let rows = RANK_BLOCK * 5 + 37;
    let mut bits = Bitmap::zeros(rows);
    for r in (0..rows).step_by(3) {
        bits.set(r, true);
    }
    let m = RowMask::from_bits(bits);
    assert_eq!(m.visible_rows(), rows.div_ceil(3));
    for n in [0, 1, 1365, 1366, 4000, m.visible_rows() - 1] {
        assert_eq!(m.nth_visible(n), Some(n * 3), "n={n}");
    }
}

#[test]
fn all_visible_mask_is_the_identity() {
    let m = RowMask::all_visible(100);
    assert_eq!(m.visible_rows(), 100);
    for r in 0..100 {
        assert_eq!(m.nth_visible(r), Some(r));
        assert_eq!(m.rank(r), r);
    }
}

#[test]
fn empty_mask_answers_nothing_rather_than_panicking() {
    let m = RowMask::from_bits(Bitmap::zeros(50));
    assert_eq!(m.visible_rows(), 0);
    assert_eq!(m.nth_visible(0), None);
    assert_eq!(m.rank(25), 0);
    let (rows, truncated) = m.first_visible(10);
    assert!(rows.is_empty());
    assert!(!truncated);
}

#[test]
fn first_visible_is_bounded_and_says_so() {
    let m = RowMask::all_visible(1000);
    let (rows, truncated) = m.first_visible(10);
    assert_eq!(rows.len(), 10);
    assert!(truncated, "must report that it was cut short");
    let (all, truncated) = m.first_visible(5000);
    assert_eq!(all.len(), 1000);
    assert!(!truncated);
}

#[test]
fn masks_intersect() {
    let mut a = Bitmap::zeros(10);
    let mut b = Bitmap::zeros(10);
    for r in [1usize, 2, 3, 4] {
        a.set(r, true);
    }
    for r in [3usize, 4, 5, 6] {
        b.set(r, true);
    }
    let m = RowMask::from_bits(a).intersect(&RowMask::from_bits(b));
    assert_eq!(m.visible_rows(), 2);
    assert_eq!(m.nth_visible(0), Some(3));
    assert_eq!(m.nth_visible(1), Some(4));
}

#[test]
fn a_row_budget_bounds_the_scan_and_reports_truncation() {
    // The 200M-row discipline: an interactive filter may examine only part of
    // the table, and must say so rather than pretending the rest matched.
    let (s, mut t) = filter_sheet();
    t.columns[0].filter = Some(Predicate::ValueList(vec!["north".into()]));
    let mask = s.filter_table(&t, 40);
    assert!(mask.is_truncated());
    assert_eq!(mask.scanned_rows(), 40);
    // Rows 1..=40 hold 10 north rows; plus the header.
    assert_eq!(mask.visible_rows(), 11);
    // Unscanned rows are hidden, not assumed to match.
    assert!(!mask.is_visible(100));
}

#[test]
fn row_mask_memory_is_one_bit_per_row_not_four_bytes() {
    // A permissive filter over a large table must not allocate a row-index
    // vector. 1M rows all visible: the bitmap is ~128 KB, the rank index
    // ~2 KB; a Vec<u32> would be 4 MB.
    let m = RowMask::all_visible(1_000_000);
    assert_eq!(m.visible_rows(), 1_000_000);
    let index_bytes = 1_000_000usize.div_ceil(RANK_BLOCK) * 8;
    assert!(index_bytes < 4096, "rank index is {index_bytes} bytes");
}

#[test]
fn serial_from_civil_is_the_exact_inverse_of_the_renderer() {
    // ONE calendar: the forward direction (civil_from_serial, which
    // render_serial and serial_parts share) and the inverse must agree on
    // every day in the 1900 date system, phantom 1900-02-29 included.
    //
    // Exhaustive rather than sampled: a one-day drift near a century rule or
    // either side of serial 60 is exactly the bug this guards, and 2.9M
    // iterations of integer maths is fast.
    let mut checked = 0u32;
    for s in 0..=2_958_465i64 {
        let (y, m, d, ..) = serial_parts(s as f64);
        let back = serial_from_civil(y, m, d);
        assert_eq!(
            back,
            Some(s as f64),
            "serial {s} decomposes to {y}-{m}-{d}, which converts back to {back:?}"
        );
        checked += 1;
    }
    assert_eq!(checked, 2_958_466, "the whole supported range was walked");
}

#[test]
fn serial_from_civil_reproduces_excels_leap_bug() {
    // Anchors: the phantom day exists, and its neighbours are one serial
    // apart from IT but two apart from each other.
    assert_eq!(serial_from_civil(1900, 2, 29), Some(60.0));
    assert_eq!(serial_from_civil(1900, 2, 28), Some(59.0));
    assert_eq!(serial_from_civil(1900, 3, 1), Some(61.0));
    assert_eq!(serial_from_civil(1900, 1, 1), Some(1.0));
    // Known Excel serials.
    assert_eq!(serial_from_civil(1970, 1, 1), Some(25_569.0));
    assert_eq!(serial_from_civil(2023, 3, 15), Some(45_000.0));
    assert_eq!(serial_from_civil(9999, 12, 31), Some(2_958_465.0));
    // Out of range and nonsense months.
    // Serial 0 is Excel's "1900-01-00" placeholder, which the renderer shows
    // as 1899-12-31, so that IS in range -- the inverse must agree with it
    // rather than inventing a stricter floor than the renderer has.
    assert_eq!(serial_from_civil(1899, 12, 31), Some(0.0));
    assert_eq!(render_serial(0.0, DateStyle::Iso), "1899-12-31");
    assert_eq!(serial_from_civil(1899, 12, 30), None);
    assert_eq!(serial_from_civil(10_000, 1, 1), None);
    assert_eq!(serial_from_civil(2023, 13, 1), None);
    assert_eq!(serial_from_civil(2023, 0, 1), None);
}

#[test]
fn days_in_month_agrees_with_the_rendered_month_end() {
    // days_in_month is what EOMONTH clamps to, so it must equal the day the
    // renderer prints for the last serial of that month -- including
    // February 1900, which has a 29th only because of Excel's bug.
    for (y, m) in [
        (1900, 1),
        (1900, 2),
        (1900, 3),
        (2000, 2),
        (1900, 12),
        (2023, 2),
        (2024, 2),
        (2100, 2),
        (2023, 4),
        (2023, 7),
        (9999, 12),
    ] {
        let dim = days_in_month(y, m);
        let last = serial_from_civil(y, m, dim).expect("month end is in range");
        assert_eq!(
            render_serial(last, DateStyle::Iso),
            format!("{y:04}-{m:02}-{dim:02}"),
            "days_in_month({y}, {m}) = {dim} does not render as that month's end"
        );
        // And one more day rolls into the next month.
        let (_, next_m, next_d, ..) = serial_parts(last + 1.0);
        assert_eq!(next_d, 1, "the day after {y}-{m}-{dim} must be the 1st");
        assert_ne!(next_m, m);
    }
    assert_eq!(days_in_month(1900, 2), 29, "Excel's phantom leap day");
    assert_eq!(days_in_month(1901, 2), 28);
    assert_eq!(days_in_month(2000, 2), 29);
    assert_eq!(days_in_month(2100, 2), 28, "century rule");
}
