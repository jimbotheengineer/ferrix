//! Round-trip tests for Excel table interoperability.
//!
//! These write a real `.xlsx`, unzip it, and assert on the OOXML. That is the
//! strongest verification available here — **Excel itself was never opened**,
//! and no claim in this file should be read as "Excel accepted it". What is
//! proven is that the parts Excel reads are present, well-formed, and that
//! re-importing reproduces every rule.

use super::*;
use ferrix_core::{
    CellRef, ColumnType, ConditionalRule, DateStyle, NumberFormat, Predicate, Sheet, Table,
    TableColumn, TableRange, Validation, ValidationRule, Value,
};

use crate::xlsx::export_xlsx_with_tables;

/// A temp path that deletes itself, so a failing test does not leave litter.
struct TempXlsx(std::path::PathBuf);

impl TempXlsx {
    fn new(tag: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!("ferrix-tbl-{tag}-{n}.xlsx")))
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

/// Read one part out of the written package as a string.
fn part(path: &std::path::Path, name: &str) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(f).ok()?;
    let mut e = zip.by_name(name).ok()?;
    let mut s = String::new();
    std::io::Read::read_to_string(&mut e, &mut s).ok()?;
    Some(s)
}

fn part_names(path: &std::path::Path) -> Vec<String> {
    let f = std::fs::File::open(path).unwrap();
    let mut zip = zip::ZipArchive::new(f).unwrap();
    (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .collect()
}

/// A three-column sheet with a header row, matching `demo_table`'s range.
fn demo_sheet() -> Sheet {
    let mut s = Sheet::new("Data");
    for (c, h) in ["region", "amount", "status"].iter().enumerate() {
        s.set_text(CellRef::new(0, c as u32), h);
    }
    let regions = ["north", "south", "east", "west"];
    for r in 1..=20u32 {
        s.set_text(CellRef::new(r, 0), regions[(r as usize - 1) % 4]);
        s.set(CellRef::new(r, 1), Value::Number(r as f64 * 10.0));
        s.set_text(
            CellRef::new(r, 2),
            if r % 2 == 0 { "open" } else { "closed" },
        );
    }
    s
}

/// A table exercising every scope area at once: types, each validation shape,
/// number formats, each conditional rule, and an active filter.
fn demo_table() -> Table {
    Table::new("SalesData", TableRange::new(0, 0, 20, 2)).with_columns(vec![
        TableColumn::new("region")
            .typed(ColumnType::Text)
            .validated(
                Validation::new(ValidationRule::OneOf(vec![
                    "north".into(),
                    "south".into(),
                    "east".into(),
                    "west".into(),
                ]))
                .message("pick a region"),
            )
            .filtered(Predicate::ValueList(vec!["north".into()])),
        TableColumn::new("amount")
            .typed(ColumnType::Number)
            .validated(Validation::new(ValidationRule::Between {
                min: 0.0,
                max: 1000.0,
            }))
            .formatted(NumberFormat::Currency {
                symbol: "$".into(),
                places: 2,
            })
            .with_conditional(ConditionalRule::DataBar {
                color: Rgb(0x63, 0x8E, 0xC6),
            }),
        TableColumn::new("status")
            .typed(ColumnType::Text)
            .validated(Validation::new(ValidationRule::Regex(
                r"(open|closed)".into(),
            ))),
    ])
}

// ------------------------------------------------------------ the tag codec --

#[test]
fn tag_roundtrips_every_extension_field() {
    let col = TableColumn::new("c")
        .typed(ColumnType::Date)
        .validated(Validation::new(ValidationRule::Regex(r"^\d{4}$".into())).allow_empty(false))
        .filtered(Predicate::Text {
            needle: "a|b:c%d,e".into(),
            case_sensitive: true,
            whole_cell: false,
        });
    let tag = ferrix_tag(&col).expect("column has extension state");
    let back = parse_ferrix_tag(&tag).expect("parses");
    assert_eq!(back.ctype, Some(ColumnType::Date));
    assert_eq!(back.rule, Some(ValidationRule::Regex(r"^\d{4}$".into())));
    assert!(back.require_value);
    // The separators inside the needle must survive escaping.
    assert_eq!(
        back.filter,
        Some(Predicate::Text {
            needle: "a|b:c%d,e".into(),
            case_sensitive: true,
            whole_cell: false
        })
    );
}

#[test]
fn tag_encodes_every_predicate_shape() {
    let cases = [
        Predicate::Blank,
        Predicate::NonBlank,
        Predicate::Compare {
            op: CmpOp::Ge,
            value: -12.5,
        },
        Predicate::Between {
            min: 1.0,
            max: 99.0,
        },
        Predicate::ValueList(vec!["a,b".into(), "c|d".into()]),
        Predicate::Text {
            needle: "x".into(),
            case_sensitive: false,
            whole_cell: true,
        },
    ];
    for p in cases {
        let col = TableColumn::new("c").filtered(p.clone());
        let tag = ferrix_tag(&col).unwrap();
        assert_eq!(parse_ferrix_tag(&tag).unwrap().filter, Some(p));
    }
}

#[test]
fn a_plain_column_produces_no_tag() {
    // Nothing to say means nothing written — an Excel-authored file stays
    // free of Ferrix noise.
    assert_eq!(ferrix_tag(&TableColumn::new("c")), None);
}

#[test]
fn non_ferrix_prompts_are_ignored() {
    assert_eq!(parse_ferrix_tag("Enter a value between 1 and 10"), None);
    assert_eq!(parse_ferrix_tag(""), None);
    // A future version's tag is not mistaken for this one.
    assert_eq!(parse_ferrix_tag("fx9|t=number"), None);
}

#[test]
fn unknown_tag_fields_are_skipped_not_fatal() {
    let t = parse_ferrix_tag("fx1|zzz=1|t=number").unwrap();
    assert_eq!(t.ctype, Some(ColumnType::Number));
}

// ---------------------------------------------------------- emitted OOXML --

#[test]
fn export_emits_a_real_table_part() {
    let f = TempXlsx::new("part");
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[demo_table()]).unwrap();

    let names = part_names(f.path());
    assert!(
        names.iter().any(|n| n == "xl/tables/table1.xml"),
        "no table part in {names:?} — Excel would see a plain range"
    );

    let xml = part(f.path(), "xl/tables/table1.xml").unwrap();
    assert!(xml.contains(r#"displayName="SalesData""#), "{xml}");
    assert!(xml.contains(r#"ref="A1:C21""#), "{xml}");
    assert!(xml.contains("<autoFilter"), "filters must be declared");
    assert!(xml.contains("<tableStyleInfo"), "{xml}");
    assert!(xml.contains(r#"showRowStripes="1""#), "banding lost");
    // The header captions Excel shows in the filter dropdowns.
    for h in ["region", "amount", "status"] {
        assert!(
            xml.contains(&format!(r#"name="{h}""#)),
            "missing column {h}"
        );
    }
}

#[test]
fn the_table_part_is_wired_into_the_package() {
    // A table part nobody references is invisible to Excel. The relationship
    // and the content-type override are what make it real.
    let f = TempXlsx::new("rels");
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[demo_table()]).unwrap();

    let sheet = part(f.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(sheet.contains("<tableParts"), "worksheet does not claim it");
    assert!(sheet.contains("<tablePart"), "{sheet}");

    let rels = part(f.path(), "xl/worksheets/_rels/sheet1.xml.rels").unwrap();
    assert!(rels.contains("tables/table1.xml"), "{rels}");

    let ct = part(f.path(), "[Content_Types].xml").unwrap();
    assert!(
        ct.contains("/xl/tables/table1.xml"),
        "no content-type override; Excel would reject the package"
    );
}

#[test]
fn validation_becomes_a_native_data_validation_element() {
    let f = TempXlsx::new("dv");
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[demo_table()]).unwrap();
    let sheet = part(f.path(), "xl/worksheets/sheet1.xml").unwrap();

    assert!(sheet.contains("<dataValidations"), "{sheet}");
    // The list rule must be a real Excel dropdown, not a comment.
    assert!(sheet.contains(r#"type="list""#), "{sheet}");
    assert!(sheet.contains("north,south,east,west"), "{sheet}");
    // The numeric bounds must be a real decimal rule with both endpoints.
    // Excel omits `operator` when it is "between", which is the default, so
    // the two formulas are what identify the shape.
    assert!(sheet.contains(r#"type="decimal""#), "{sheet}");
    assert!(sheet.contains("<formula1>0</formula1>"), "{sheet}");
    assert!(sheet.contains("<formula2>1000</formula2>"), "{sheet}");
    // Validation applies to the data rows only — never the header.
    assert!(sheet.contains(r#"sqref="A2:A21""#), "{sheet}");
}

#[test]
fn the_extension_tag_rides_along_invisibly() {
    let f = TempXlsx::new("tag");
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[demo_table()]).unwrap();
    let sheet = part(f.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(sheet.contains(r#"promptTitle="Ferrix""#), "{sheet}");
    // showInputMessage defaults off in the written element, so Excel never
    // pops a tooltip carrying our payload at the user.
    assert!(
        !sheet.contains(r#"showInputMessage="1""#),
        "the tag must not be displayed to an Excel user"
    );
}

#[test]
fn conditional_formats_become_cf_rule_elements() {
    let f = TempXlsx::new("cf");
    let mut t = demo_table();
    t.columns[1].conditional = vec![
        ConditionalRule::ColorScale2 {
            min: Rgb(0xFF, 0xFF, 0xFF),
            max: Rgb(0x63, 0xBE, 0x7B),
        },
        ConditionalRule::Threshold {
            op: CmpOp::Gt,
            value: 150.0,
            fill: Rgb(0xFF, 0xC7, 0xCE),
            text: Rgb(0x9C, 0x00, 0x06),
        },
    ];
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[t]).unwrap();
    let sheet = part(f.path(), "xl/worksheets/sheet1.xml").unwrap();

    assert!(sheet.contains("<conditionalFormatting"), "{sheet}");
    assert!(sheet.contains(r#"type="colorScale""#), "{sheet}");
    assert!(sheet.contains(r#"type="cellIs""#), "{sheet}");
    assert!(sheet.contains(r#"operator="greaterThan""#), "{sheet}");
}

#[test]
fn data_bars_are_emitted() {
    let f = TempXlsx::new("bar");
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[demo_table()]).unwrap();
    let sheet = part(f.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(sheet.contains(r#"type="dataBar""#), "{sheet}");
}

#[test]
fn number_formats_reach_the_styles_part() {
    let f = TempXlsx::new("numfmt");
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[demo_table()]).unwrap();
    let styles = part(f.path(), "xl/styles.xml").unwrap();
    assert!(styles.contains("<numFmts"), "{styles}");
    assert!(
        styles.contains("#,##0.00"),
        "currency format lost: {styles}"
    );
}

#[test]
fn an_unmodelled_format_string_reaches_the_file_verbatim() {
    // The data-loss guard, end to end: a format Ferrix cannot parse must
    // still be the exact bytes in styles.xml.
    let exotic = "0.000E+00";
    let f = TempXlsx::new("custom");
    let mut t = demo_table();
    t.columns[1].format = NumberFormat::Custom(exotic.into());
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[t]).unwrap();
    let styles = part(f.path(), "xl/styles.xml").unwrap();
    assert!(
        styles.contains(exotic),
        "custom format string was mangled or dropped: {styles}"
    );
}

// ------------------------------------------------------------- round trips --

#[test]
fn a_ferrix_table_reimports_with_every_rule_intact() {
    // The headline acceptance criterion: nothing is lost in either direction.
    let f = TempXlsx::new("roundtrip");
    let original = demo_table();
    export_xlsx_with_tables(
        f.path(),
        &demo_sheet(),
        "Data",
        std::slice::from_ref(&original),
    )
    .unwrap();

    let imported = import_tables(f.path()).unwrap();
    assert_eq!(imported.len(), 1, "exactly one table expected");
    let got = &imported[0].table;
    assert_eq!(imported[0].sheet_index, 0);

    // Identity.
    assert_eq!(got.name, original.name);
    assert_eq!(got.range, original.range);
    assert_eq!(got.header_row, original.header_row);
    assert_eq!(got.autofilter, original.autofilter);
    assert_eq!(got.banded_rows, original.banded_rows);
    assert_eq!(got.columns.len(), original.columns.len());

    for (i, (a, b)) in original.columns.iter().zip(&got.columns).enumerate() {
        assert_eq!(b.name, a.name, "column {i} name");
        assert_eq!(b.ctype, a.ctype, "column {i} type");
        assert_eq!(b.validation.rule, a.validation.rule, "column {i} rule");
        assert_eq!(b.format, a.format, "column {i} number format");
        assert_eq!(b.conditional, a.conditional, "column {i} conditional rules");
        assert_eq!(b.filter, a.filter, "column {i} filter");
    }
}

#[test]
fn every_validation_shape_survives_a_round_trip() {
    let rules = [
        (
            ValidationRule::Between {
                min: -5.0,
                max: 5.5,
            },
            ColumnType::Number,
        ),
        (
            ValidationRule::NotBetween { min: 1.0, max: 2.0 },
            ColumnType::Number,
        ),
        (
            ValidationRule::Compare {
                op: CmpOp::Gt,
                value: 0.0,
            },
            ColumnType::Number,
        ),
        (
            ValidationRule::Compare {
                op: CmpOp::Le,
                value: 100.0,
            },
            ColumnType::Number,
        ),
        (
            ValidationRule::OneOf(vec!["a".into(), "b".into()]),
            ColumnType::Text,
        ),
        (
            ValidationRule::TextLength { min: 2, max: 8 },
            ColumnType::Text,
        ),
        (ValidationRule::Regex("[0-9]+".into()), ColumnType::Text),
        (ValidationRule::Unique, ColumnType::Text),
        (ValidationRule::None, ColumnType::Date),
        (ValidationRule::None, ColumnType::Bool),
        (ValidationRule::None, ColumnType::Any),
    ];

    for (rule, ctype) in rules {
        let f = TempXlsx::new("rules");
        let t =
            Table::new("T", TableRange::new(0, 0, 5, 0)).with_columns(vec![TableColumn::new("c")
                .typed(ctype)
                .validated(Validation::new(rule.clone()))]);
        let mut s = Sheet::new("S");
        s.set_text(CellRef::new(0, 0), "c");
        for r in 1..=5u32 {
            s.set(CellRef::new(r, 0), Value::Number(r as f64));
        }
        export_xlsx_with_tables(f.path(), &s, "S", &[t]).unwrap();

        let got = import_tables(f.path()).unwrap();
        assert_eq!(got.len(), 1, "rule {rule:?} lost its whole table");
        let col = &got[0].table.columns[0];
        assert_eq!(col.validation.rule, rule, "rule {rule:?} did not survive");
        assert_eq!(col.ctype, ctype, "type for rule {rule:?} did not survive");
    }
}

#[test]
fn every_number_format_survives_a_round_trip() {
    let formats = [
        NumberFormat::Decimal { places: 3 },
        NumberFormat::Thousands { places: 0 },
        NumberFormat::Currency {
            symbol: "$".into(),
            places: 2,
        },
        NumberFormat::Percent { places: 1 },
        NumberFormat::Date(DateStyle::Iso),
        NumberFormat::Custom("0.000E+00".into()),
        NumberFormat::Custom(r#"[$€-407]#,##0.00"#.into()),
    ];
    for fmt in formats {
        let f = TempXlsx::new("fmt");
        let t = Table::new("T", TableRange::new(0, 0, 3, 0))
            .with_columns(vec![TableColumn::new("c").formatted(fmt.clone())]);
        let mut s = Sheet::new("S");
        s.set_text(CellRef::new(0, 0), "c");
        for r in 1..=3u32 {
            s.set(CellRef::new(r, 0), Value::Number(r as f64));
        }
        export_xlsx_with_tables(f.path(), &s, "S", &[t]).unwrap();
        let got = import_tables(f.path()).unwrap();
        assert_eq!(
            got[0].table.columns[0].format, fmt,
            "format {fmt:?} did not survive the round trip"
        );
    }
}

#[test]
fn every_conditional_rule_survives_a_round_trip() {
    let rules = [
        ConditionalRule::ColorScale2 {
            min: Rgb(0xFF, 0xFF, 0xFF),
            max: Rgb(0x63, 0xBE, 0x7B),
        },
        ConditionalRule::ColorScale3 {
            min: Rgb(0xF8, 0x69, 0x6B),
            mid: Rgb(0xFF, 0xEB, 0x84),
            max: Rgb(0x63, 0xBE, 0x7B),
        },
        ConditionalRule::DataBar {
            color: Rgb(0x63, 0x8E, 0xC6),
        },
        ConditionalRule::Threshold {
            op: CmpOp::Lt,
            value: 42.0,
            fill: Rgb(0xFF, 0xC7, 0xCE),
            text: Rgb(0x9C, 0x00, 0x06),
        },
    ];
    for rule in rules {
        let f = TempXlsx::new("cfrt");
        let t = Table::new("T", TableRange::new(0, 0, 3, 0))
            .with_columns(vec![TableColumn::new("c").with_conditional(rule.clone())]);
        let mut s = Sheet::new("S");
        s.set_text(CellRef::new(0, 0), "c");
        for r in 1..=3u32 {
            s.set(CellRef::new(r, 0), Value::Number(r as f64));
        }
        export_xlsx_with_tables(f.path(), &s, "S", &[t]).unwrap();
        let got = import_tables(f.path()).unwrap();
        assert_eq!(
            got[0].table.columns[0].conditional,
            vec![rule.clone()],
            "conditional rule {rule:?} did not survive"
        );
    }
}

#[test]
fn active_filters_survive_a_round_trip() {
    for pred in [
        Predicate::ValueList(vec!["north".into(), "south".into()]),
        Predicate::Compare {
            op: CmpOp::Gt,
            value: 100.0,
        },
        Predicate::Between {
            min: 0.0,
            max: 50.0,
        },
        Predicate::Text {
            needle: "op".into(),
            case_sensitive: false,
            whole_cell: false,
        },
        Predicate::NonBlank,
    ] {
        let f = TempXlsx::new("filt");
        let mut t = demo_table();
        t.columns[0].filter = Some(pred.clone());
        export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[t]).unwrap();
        let got = import_tables(f.path()).unwrap();
        assert_eq!(
            got[0].table.columns[0].filter,
            Some(pred.clone()),
            "filter {pred:?} did not survive"
        );
    }
}

#[test]
fn a_multi_sheet_workbook_attributes_tables_to_the_right_sheet() {
    let f = TempXlsx::new("multi");
    let s = demo_sheet();
    let t = demo_table();
    let mut t2 = Table::new("Second", TableRange::new(0, 0, 20, 2));
    t2.columns[1].ctype = ColumnType::Number;

    crate::xlsx::export_workbook(
        f.path(),
        &[
            crate::xlsx::SheetExport::new("First", &s),
            crate::xlsx::SheetExport::new("Data", &s).with_tables(std::slice::from_ref(&t)),
            crate::xlsx::SheetExport::new("Third", &s).with_tables(std::slice::from_ref(&t2)),
        ],
    )
    .unwrap();

    let got = import_tables(f.path()).unwrap();
    assert_eq!(got.len(), 2);
    let by_name: std::collections::HashMap<_, _> =
        got.iter().map(|t| (t.table.name.as_str(), t)).collect();
    assert_eq!(by_name["SalesData"].sheet_index, 1);
    assert_eq!(by_name["Second"].sheet_index, 2);
}

#[test]
fn a_workbook_with_no_tables_is_not_an_error() {
    let f = TempXlsx::new("none");
    crate::xlsx::export_xlsx(f.path(), &demo_sheet(), "Data").unwrap();
    assert!(import_tables(f.path()).unwrap().is_empty());
}

#[test]
fn cell_values_still_round_trip_alongside_the_table() {
    // The table machinery must not disturb the data it describes.
    let f = TempXlsx::new("values");
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[demo_table()]).unwrap();
    let sheets = crate::xlsx::import_xlsx(f.path()).unwrap();
    let (_, s) = &sheets[0];
    assert_eq!(s.display(CellRef::new(0, 0)), "region");
    assert_eq!(s.get(CellRef::new(1, 1)), Value::Number(10.0));
    assert_eq!(s.display(CellRef::new(20, 0)), "west");
}

#[test]
fn a_table_defined_over_a_shifted_range_keeps_its_offset() {
    // Column/row offsets are where off-by-one bugs live: validation must land
    // on the table's columns, not the sheet's first ones.
    let f = TempXlsx::new("offset");
    let mut s = Sheet::new("S");
    s.set_text(CellRef::new(3, 2), "h");
    for r in 4..=8u32 {
        s.set(CellRef::new(r, 2), Value::Number(r as f64));
    }
    let t =
        Table::new("Shifted", TableRange::new(3, 2, 8, 2)).with_columns(vec![TableColumn::new(
            "h",
        )
        .typed(ColumnType::Number)
        .validated(Validation::new(ValidationRule::Between {
            min: 0.0,
            max: 10.0,
        }))]);
    export_xlsx_with_tables(f.path(), &s, "S", &[t]).unwrap();

    let xml = part(f.path(), "xl/tables/table1.xml").unwrap();
    assert!(xml.contains(r#"ref="C4:C9""#), "{xml}");
    let sheet = part(f.path(), "xl/worksheets/sheet1.xml").unwrap();
    assert!(
        sheet.contains(r#"sqref="C5:C9""#),
        "validation misplaced: {sheet}"
    );

    let got = import_tables(f.path()).unwrap();
    assert_eq!(got[0].table.range, TableRange::new(3, 2, 8, 2));
    assert_eq!(
        got[0].table.columns[0].validation.rule,
        ValidationRule::Between {
            min: 0.0,
            max: 10.0
        }
    );
}

#[test]
fn require_value_survives_as_allow_blank() {
    let f = TempXlsx::new("blank");
    let t = Table::new("T", TableRange::new(0, 0, 3, 0)).with_columns(vec![TableColumn::new("c")
        .typed(ColumnType::Number)
        .validated(Validation::new(ValidationRule::None).allow_empty(false))]);
    let mut s = Sheet::new("S");
    s.set_text(CellRef::new(0, 0), "c");
    export_xlsx_with_tables(f.path(), &s, "S", &[t]).unwrap();
    let got = import_tables(f.path()).unwrap();
    assert!(
        !got[0].table.columns[0].validation.allow_empty,
        "a required column came back optional"
    );
}

#[test]
fn totals_row_configuration_survives() {
    let f = TempXlsx::new("totals");
    let mut t = demo_table();
    t.totals_row = true;
    t.columns[1].totals_function = Some("sum".into());
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[t]).unwrap();

    let xml = part(f.path(), "xl/tables/table1.xml").unwrap();
    assert!(xml.contains(r#"totalsRowCount="1""#), "{xml}");
    assert!(xml.contains(r#"totalsRowFunction="sum""#), "{xml}");

    let got = import_tables(f.path()).unwrap();
    assert!(got[0].table.totals_row);
    assert_eq!(
        got[0].table.columns[1].totals_function.as_deref(),
        Some("sum")
    );
}

#[test]
fn a_table_name_illegal_in_excel_is_fixed_before_writing() {
    // rust_xlsxwriter rejects an illegal displayName outright, so an
    // unsanitised name would turn into an export failure rather than a table.
    let f = TempXlsx::new("badname");
    let t = Table::new("Sales Q1 2024!", TableRange::new(0, 0, 20, 2));
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[t]).unwrap();
    let xml = part(f.path(), "xl/tables/table1.xml").unwrap();
    assert!(xml.contains(r#"displayName="Sales_Q1_2024_""#), "{xml}");
}

#[test]
fn an_imported_table_is_immediately_usable_for_filtering() {
    // Ties the interop path back to the engine: what comes out of a file
    // drives the same filter machinery as a table built in memory.
    let f = TempXlsx::new("usable");
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[demo_table()]).unwrap();
    let table = import_tables(f.path()).unwrap().remove(0).table;
    let sheets = crate::xlsx::import_xlsx(f.path()).unwrap();
    let (_, sheet) = &sheets[0];

    // demo_table filters region == "north": rows 1, 5, 9, 13, 17.
    let mask = sheet.filter_table(&table, usize::MAX);
    assert_eq!(mask.visible_rows(), 6, "5 north rows plus the header");
    assert_eq!(mask.nth_visible(1), Some(1));

    // And validation runs over it without further setup.
    let report = sheet.validate_table(&table, 100);
    assert!(report.is_clean(), "demo data should satisfy its own rules");
}

#[test]
fn imported_validation_flags_bad_data_from_the_file() {
    let f = TempXlsx::new("badrows");
    let mut s = demo_sheet();
    // Two cells that violate the imported rules.
    s.set(CellRef::new(3, 1), Value::Number(99_999.0));
    s.set_text(CellRef::new(4, 0), "antarctica");
    export_xlsx_with_tables(f.path(), &s, "Data", &[demo_table()]).unwrap();

    let table = import_tables(f.path()).unwrap().remove(0).table;
    let sheets = crate::xlsx::import_xlsx(f.path()).unwrap();
    let report = sheets[0].1.validate_table(&table, 100);
    assert_eq!(report.total, 2, "both bad cells must be flagged");
    // And the bad values are still in the file, untouched.
    assert_eq!(sheets[0].1.get(CellRef::new(3, 1)), Value::Number(99_999.0));
    assert_eq!(sheets[0].1.display(CellRef::new(4, 0)), "antarctica");
}

#[test]
fn every_emitted_part_is_well_formed_xml() {
    // The limit of what can be verified without Excel: not "Excel likes it",
    // but "every XML part parses to EOF with no error". A malformed part is
    // the failure mode that would make Excel refuse the file outright, so
    // this is the check worth having.
    let f = TempXlsx::new("wellformed");
    export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[demo_table()]).unwrap();

    let mut checked = 0;
    for name in part_names(f.path()) {
        if !name.ends_with(".xml") && !name.ends_with(".rels") {
            continue;
        }
        let body = part(f.path(), &name).unwrap_or_default();
        let mut rd = quick_xml::Reader::from_str(&body);
        let mut depth = 0i32;
        loop {
            match rd.read_event() {
                Ok(quick_xml::events::Event::Start(_)) => depth += 1,
                Ok(quick_xml::events::Event::End(_)) => depth -= 1,
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("{name} is malformed XML: {e}"),
            }
        }
        assert_eq!(depth, 0, "{name} has unbalanced elements");
        checked += 1;
    }
    assert!(checked >= 6, "expected a full package, checked {checked}");
}

#[test]
fn temp_files_are_cleaned_up() {
    let path = {
        let f = TempXlsx::new("cleanup");
        export_xlsx_with_tables(f.path(), &demo_sheet(), "Data", &[demo_table()]).unwrap();
        assert!(f.path().exists());
        f.path().to_path_buf()
    };
    assert!(!path.exists(), "the fixture must not outlive the test");
}
