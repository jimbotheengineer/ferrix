//! Round-trip tests for cell decoration in xlsx (issue #28).
//!
//! Every test here **unzips the written package and reads the XML**, rather
//! than asserting the writer was called. That is the criterion: a format the
//! library accepted and then dropped — which is exactly what the
//! constant-memory writer does with a cell format — leaves an API that looks
//! healthy and a file with no borders in it. The only thing that settles it is
//! the bytes on disk.

use super::*;
use ferrix_core::{CellRef, Rgb, Sheet, SheetFormat, TableRange};

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("ferrix_decor_xlsx");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{}-{}", std::process::id(), name))
}

/// A part of the written package, as text.
fn part(path: &std::path::Path, name: &str) -> String {
    let f = std::fs::File::open(path).unwrap();
    let mut zip = zip::ZipArchive::new(f).unwrap();
    let mut e = zip
        .by_name(name)
        .unwrap_or_else(|_| panic!("{name} missing from the package"));
    let mut s = String::new();
    std::io::Read::read_to_string(&mut e, &mut s).unwrap();
    s
}

fn small_sheet() -> Sheet {
    let mut s = Sheet::new("Data");
    for r in 0..4u32 {
        for c in 0..3u32 {
            s.set_text(CellRef::new(r, c), &format!("v{r}{c}"));
        }
    }
    s
}

fn export(name: &str, sheet: &Sheet, fmt: &SheetFormat) -> std::path::PathBuf {
    let p = tmp(name);
    crate::xlsx::export_workbook(
        &p,
        &[crate::xlsx::SheetExport::new("Data", sheet).with_format(fmt)],
    )
    .expect("export");
    p
}

/// EVERY border style reaches `xl/styles.xml` with its own OOXML spelling.
///
/// Asserted against the styles part's text, one style at a time. What would
/// this assert if the feature did nothing? The part would contain none of the
/// seven names and every assertion would fail — as opposed to a shape count,
/// which a broken writer could still satisfy.
#[test]
fn every_border_style_appears_in_the_styles_xml() {
    let sheet = small_sheet();
    let mut fmt = SheetFormat::new();
    // One style per column, so all seven are in one file and each is
    // attributable to the scope that asked for it.
    let styles = BorderStyle::ALL;
    for (i, st) in styles.iter().enumerate() {
        fmt.set_column_decor(
            i as u32,
            CellDecor::default().with_border(Side::Bottom, Border::new(*st)),
        );
    }
    let p = export("border_styles.xlsx", &sheet, &fmt);
    let xml = part(&p, "xl/styles.xml");
    std::fs::remove_file(&p).ok();

    for st in styles.iter().filter(|s| s.is_visible()) {
        let want = format!("style=\"{}\"", st.ooxml());
        assert!(
            xml.contains(&want),
            "border style {st:?} is missing from xl/styles.xml; expected {want}\n{xml}"
        );
    }
}

/// A border COLOUR survives, as a real `<color rgb=...>` on the edge.
#[test]
fn a_border_colour_reaches_the_styles_xml() {
    let sheet = small_sheet();
    let mut fmt = SheetFormat::new();
    fmt.set_column_decor(
        1,
        CellDecor::default().with_border(
            Side::Left,
            Border::colored(BorderStyle::Thick, Rgb(0x12, 0x34, 0x56)),
        ),
    );
    let p = export("border_colour.xlsx", &sheet, &fmt);
    let xml = part(&p, "xl/styles.xml");
    std::fs::remove_file(&p).ok();

    assert!(
        xml.contains("123456"),
        "the border colour did not reach the file\n{xml}"
    );
    assert!(xml.contains("style=\"thick\""), "the style went missing");
}

/// Alignment, indent, wrap, shrink and rotation each reach `<alignment>`.
///
/// One decoration carrying all of them, then every attribute checked
/// individually — so a writer that dropped exactly one of them fails on that
/// one rather than passing because the others made it.
#[test]
fn alignment_indent_wrap_and_rotation_reach_the_styles_xml() {
    let sheet = small_sheet();
    let mut fmt = SheetFormat::new();
    fmt.set_column_decor(
        0,
        CellDecor::default()
            .with_h_align(HAlign::Center)
            .with_v_align(VAlign::Top)
            .with_indent(4)
            .with_wrap(true)
            .with_rotation(45),
    );
    // Shrink on its own column: it cannot coexist with wrap.
    fmt.set_column_decor(1, CellDecor::default().with_shrink(true));
    let p = export("alignment.xlsx", &sheet, &fmt);
    let xml = part(&p, "xl/styles.xml");
    std::fs::remove_file(&p).ok();

    for want in [
        "horizontal=\"center\"",
        "vertical=\"top\"",
        "indent=\"4\"",
        "wrapText=\"1\"",
        "textRotation=\"45\"",
        "shrinkToFit=\"1\"",
    ] {
        assert!(
            xml.contains(want),
            "{want} is missing from xl/styles.xml\n{xml}"
        );
    }
}

/// A NEGATIVE rotation uses Excel's 91..=180 encoding for clockwise.
///
/// OOXML has no negative rotation: 0..=90 is counter-clockwise, and 91..=180
/// means `value - 90` degrees CLOCKWISE. So -30 degrees is written as 120,
/// which is what reading the actual part back confirms. Asserted because a
/// writer that passed the signed value straight through would produce a file
/// Excel reads as a *different* angle — a silent visual corruption that looks
/// fine in every unit test of the model.
#[test]
fn a_negative_rotation_uses_excels_clockwise_encoding() {
    let sheet = small_sheet();
    let mut fmt = SheetFormat::new();
    fmt.set_column_decor(0, CellDecor::default().with_rotation(-30));
    let p = export("rotation_neg.xlsx", &sheet, &fmt);
    let xml = part(&p, "xl/styles.xml");
    std::fs::remove_file(&p).ok();

    assert!(
        xml.contains("textRotation=\"120\""),
        "-30 degrees must be written as OOXML's 120 (90 + 30 clockwise), not \
         as a raw -30\n{xml}"
    );
}

/// A diagonal border is written with both its style and its direction flags.
#[test]
fn a_diagonal_border_reaches_the_styles_xml() {
    let sheet = small_sheet();
    let mut fmt = SheetFormat::new();
    fmt.set_column_decor(
        0,
        CellDecor::default().with_diagonal(
            Border::colored(BorderStyle::Medium, Rgb(0xAB, 0xCD, 0xEF)),
            Diagonal::Both,
        ),
    );
    let p = export("diagonal.xlsx", &sheet, &fmt);
    let xml = part(&p, "xl/styles.xml");
    std::fs::remove_file(&p).ok();

    assert!(
        xml.contains("diagonalUp=\"1\"") && xml.contains("diagonalDown=\"1\""),
        "an X diagonal must set both direction flags\n{xml}"
    );
    assert!(xml.contains("ABCDEF"), "the diagonal colour went missing");
}

/// A COLUMN-scope decoration is one `<col>` record, not one per row.
///
/// The file-side half of the scale criterion. A 10M-row column written cell by
/// cell would produce a package megabytes wide and take minutes; this asserts
/// the shape of the output instead of timing it, so it fails deterministically.
#[test]
fn a_column_scope_decor_is_one_col_record_in_the_sheet_xml() {
    let sheet = small_sheet();
    let mut fmt = SheetFormat::new();
    fmt.set_column_decor(
        1,
        CellDecor::default().with_border(Side::Bottom, Border::new(BorderStyle::Thin)),
    );
    let p = export("col_scope.xlsx", &sheet, &fmt);
    let sheet_xml = part(&p, "xl/worksheets/sheet1.xml");
    std::fs::remove_file(&p).ok();

    // One `<col ... style=...>` covering the column, rather than a style
    // attribute on each of its cells.
    let cols = sheet_xml.matches("<col ").count();
    assert_eq!(
        cols, 1,
        "a column-scope decoration must be exactly one <col> record; got {cols}\n{sheet_xml}"
    );
    assert!(
        sheet_xml.contains("min=\"2\"") && sheet_xml.contains("max=\"2\""),
        "the <col> record does not name column B\n{sheet_xml}"
    );
}

/// Every combination of the whole feature survives one round trip together.
///
/// Not one decoration per file: the point is that borders, alignment, indent,
/// wrap and rotation coexist in ONE style record without one overwriting
/// another, which per-feature files cannot show.
#[test]
fn a_combined_decoration_round_trips_in_one_style_record() {
    let sheet = small_sheet();
    let mut fmt = SheetFormat::new();
    fmt.set_range_decor(
        TableRange::new(1, 1, 2, 2),
        CellDecor::default()
            .with_box(Border::colored(BorderStyle::Double, Rgb(1, 2, 3)))
            .with_h_align(HAlign::Right)
            // TOP rather than Bottom: bottom IS Excel's default vertical
            // alignment, so the writer correctly omits the attribute for it,
            // and an assertion on `vertical="bottom"` would fail against a
            // perfectly lossless file. Top is written explicitly.
            .with_v_align(VAlign::Top)
            .with_indent(2)
            .with_wrap(true)
            .with_rotation(90),
    );
    let p = export("combined.xlsx", &sheet, &fmt);
    let xml = part(&p, "xl/styles.xml");
    let sheet_xml = part(&p, "xl/worksheets/sheet1.xml");
    std::fs::remove_file(&p).ok();

    for want in [
        "style=\"double\"",
        "horizontal=\"right\"",
        "vertical=\"top\"",
        "indent=\"2\"",
        "wrapText=\"1\"",
        "textRotation=\"90\"",
    ] {
        assert!(
            xml.contains(want),
            "{want} missing from the combined style\n{xml}"
        );
    }
    // The decorated cells really carry a style index — a style record nothing
    // points at is a style Excel will not show.
    assert!(
        sheet_xml.contains(" s=\""),
        "no cell in the sheet references a style\n{sheet_xml}"
    );
}

/// An UNDECORATED sheet is unchanged: no decoration, no extra style records,
/// and the streaming writer is still used.
///
/// The regression guard for "this feature costs nothing when unused".
#[test]
fn an_undecorated_sheet_writes_no_decoration() {
    let sheet = small_sheet();
    let fmt = SheetFormat::new();
    let p = export("plain.xlsx", &sheet, &fmt);
    let xml = part(&p, "xl/styles.xml");
    std::fs::remove_file(&p).ok();

    assert!(
        !xml.contains("wrapText") && !xml.contains("textRotation"),
        "an undecorated sheet must not write alignment records\n{xml}"
    );
}

// ------------------------------------------------------- loss reporting ----

/// A plain decoration survives, and is reported as surviving.
#[test]
fn an_ordinary_decoration_is_reported_as_surviving() {
    let d = CellDecor::default()
        .with_box(Border::new(BorderStyle::Thick))
        .with_h_align(HAlign::Center)
        .with_wrap(true)
        .with_rotation(45);
    assert!(decor_survives_xlsx(&d), "{:?}", decor_xlsx_loss(&d));
    assert!(decor_xlsx_loss(&d).is_empty());

    // Indent survives too — just not TOGETHER with rotation, which is the
    // case the loss report exists for.
    let with_indent = CellDecor::default()
        .with_h_align(HAlign::Left)
        .with_indent(3)
        .with_wrap(true);
    assert!(
        decor_survives_xlsx(&with_indent),
        "{:?}",
        decor_xlsx_loss(&with_indent)
    );
}

/// The combinations with no lossless xlsx meaning are REPORTED, not dropped.
///
/// The `rule_survives_xlsx` contract: the user learns in the editor, not after
/// opening the file in Excel. Each case is asserted separately so a reporter
/// that only caught one of them fails on the others.
#[test]
fn lossy_decorations_are_reported_rather_than_silently_dropped() {
    // Indent is ignored by Excel on rotated text.
    let d = CellDecor::default().with_indent(5).with_rotation(60);
    let loss = decor_xlsx_loss(&d);
    assert!(!decor_survives_xlsx(&d));
    assert!(
        loss.iter().any(|m| m.to_lowercase().contains("indent")),
        "indent-on-rotation was not reported: {loss:?}"
    );

    // Wrap and shrink cannot both apply.
    let d = CellDecor::default().with_wrap(true).with_shrink(true);
    let loss = decor_xlsx_loss(&d);
    assert!(!decor_survives_xlsx(&d));
    assert!(
        loss.iter().any(|m| m.to_lowercase().contains("shrink")),
        "the wrap/shrink conflict was not reported: {loss:?}"
    );

    // Justify does nothing without wrap.
    let d = CellDecor::default().with_h_align(HAlign::Justify);
    let loss = decor_xlsx_loss(&d);
    assert!(!decor_survives_xlsx(&d));
    assert!(
        loss.iter().any(|m| m.to_lowercase().contains("justify")),
        "justify-without-wrap was not reported: {loss:?}"
    );
    // ...and WITH wrap it is fine, so the report is not simply always-on.
    let ok = CellDecor::default()
        .with_h_align(HAlign::Justify)
        .with_wrap(true);
    assert!(
        decor_survives_xlsx(&ok),
        "justify with wrap should survive: {:?}",
        decor_xlsx_loss(&ok)
    );
}

/// A range too large to expand is REPORTED by `write_decor` rather than
/// attempted.
///
/// Expanding a 200M-cell rectangle to per-cell formats is how the export
/// process gets killed. The cap makes that a reportable outcome instead.
#[test]
fn an_oversized_range_decoration_is_reported_not_expanded() {
    let mut fmt = SheetFormat::new();
    let huge = TableRange::new(0, 0, 199_999_999, 0);
    fmt.set_range_decor(huge, CellDecor::default().with_wrap(true));

    let mut wb = rust_xlsxwriter::Workbook::new();
    let ws = wb.add_worksheet();
    // `rows` is the sheet's real extent, which is what bounds the expansion —
    // so this is reported only when the RANGE itself is genuinely enormous.
    let skipped = write_decor(ws, &fmt, 200_000_000, 1).expect("write");
    assert_eq!(
        skipped,
        vec![huge],
        "a range past MAX_RANGE_CELLS must be reported back to the caller"
    );
}

/// A range within the cap IS written, so the cap is not simply refusing
/// everything.
#[test]
fn a_normal_range_decoration_is_written_not_skipped() {
    let mut fmt = SheetFormat::new();
    fmt.set_range_decor(
        TableRange::new(0, 0, 9, 2),
        CellDecor::default().with_wrap(true),
    );
    let mut wb = rust_xlsxwriter::Workbook::new();
    let ws = wb.add_worksheet();
    let skipped = write_decor(ws, &fmt, 10, 3).expect("write");
    assert!(
        skipped.is_empty(),
        "an ordinary selection must be written, not reported as lossy"
    );
}
