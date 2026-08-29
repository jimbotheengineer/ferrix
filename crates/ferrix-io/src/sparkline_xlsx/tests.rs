//! Round-trip tests for sparklines in xlsx (issue #36).
//!
//! Every test here **unzips the written package and reads the XML**, in the
//! shape `decor_xlsx/tests.rs` established. Asserting the writer was called
//! proves nothing: a library that accepted an element and then dropped it
//! leaves a healthy-looking API and a file with no sparklines in it. The bytes
//! on disk are the only thing that settles it.
//!
//! Excel was never launched here, so these prove well-formed, correctly
//! namespaced OOXML — not that Excel accepts it.

use super::*;
use ferrix_core::{Sheet, SparkGroup, SparkKind, SparklineMap, TableRange, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("ferrix_spark_xlsx");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{}-{name}", std::process::id()))
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

/// Four numeric columns over four rows, with a fifth destination column.
fn numeric_sheet() -> Sheet {
    let mut s = Sheet::new("Data");
    for r in 0..4u32 {
        for c in 0..4u32 {
            s.set(CellRef::new(r, c), Value::Number((r * 4 + c) as f64));
        }
    }
    s
}

fn group(kind: SparkKind) -> SparkGroup {
    SparkGroup::new(kind, TableRange::new(0, 4, 3, 4), 0, 3)
}

fn export(name: &str, sheet: &Sheet, map: &SparklineMap) -> std::path::PathBuf {
    let p = tmp(name);
    crate::xlsx::export_workbook(
        &p,
        &[crate::xlsx::SheetExport::new("Data", sheet).with_sparklines(map)],
    )
    .expect("export");
    p
}

/// The criterion: the group reaches `<extLst><x14:sparklineGroups>` in the
/// file, with one `<x14:sparkline>` per destination row.
///
/// What would this assert if export did nothing? The part would contain no
/// `sparklineGroup` at all and the first assertion fails.
#[test]
fn a_group_is_written_into_the_worksheet_extlst() {
    let mut map = SparklineMap::new();
    map.add(group(SparkKind::Line));
    let p = export("write_line.xlsx", &numeric_sheet(), &map);
    let xml = part(&p, "xl/worksheets/sheet1.xml");

    assert!(
        xml.contains("<extLst>"),
        "the sparklines live inside <extLst>; got:\n{xml}"
    );
    assert!(
        xml.contains("x14:sparklineGroups"),
        "the group element must be present; got:\n{xml}"
    );
    // One <x14:sparkline> per destination row: the expansion the format
    // requires. Four rows, four elements.
    assert_eq!(
        xml.matches("<x14:sparkline>").count(),
        4,
        "one element per destination cell, for 4 rows"
    );
    // Row 1's source is row 1's cells, not a fixed range repeated.
    assert!(
        xml.contains("Data!A1:D1") && xml.contains("Data!A4:D4"),
        "each row's source must be its OWN row; got:\n{xml}"
    );
    assert!(
        xml.contains("<xm:sqref>E1</xm:sqref>") && xml.contains("<xm:sqref>E4</xm:sqref>"),
        "destinations must be the group's column, one cell per row; got:\n{xml}"
    );
    let _ = std::fs::remove_file(&p);
}

/// The type reaches the file, and the three types are distinguishable there.
///
/// A writer that ignored the type would emit three identical parts and the
/// `assert_ne` pair fails.
#[test]
fn each_type_is_written_with_its_own_ooxml_spelling() {
    let sheet = numeric_sheet();

    let mut line = SparklineMap::new();
    line.add(group(SparkKind::Line));
    let pl = export("type_line.xlsx", &sheet, &line);
    let xl = part(&pl, "xl/worksheets/sheet1.xml");

    let mut col = SparklineMap::new();
    col.add(group(SparkKind::Column));
    let pc = export("type_col.xlsx", &sheet, &col);
    let xc = part(&pc, "xl/worksheets/sheet1.xml");

    let mut wl = SparklineMap::new();
    wl.add(group(SparkKind::WinLoss));
    let pw = export("type_wl.xlsx", &sheet, &wl);
    let xw = part(&pw, "xl/worksheets/sheet1.xml");

    // Line is OOXML's default and writes NO type attribute; column and
    // win/loss ("stacked") each write their own.
    assert!(
        !xl.contains(r#"type="column""#) && !xl.contains(r#"type="stacked""#),
        "line is the format's default and writes no type"
    );
    assert!(xc.contains(r#"type="column""#), "column spells itself");
    assert!(
        xw.contains(r#"type="stacked""#),
        "win/loss is OOXML's 'stacked'"
    );
    assert_ne!(xl, xc, "line and column must not write identical XML");
    assert_ne!(xc, xw, "column and win/loss must not write identical XML");

    for p in [pl, pc, pw] {
        let _ = std::fs::remove_file(&p);
    }
}

/// Export then import returns the SAME group, for every type.
///
/// This is the round trip proper. The compression step is where a bug would
/// hide — it is the only part of the pipeline with a chance to invent a
/// plausible-but-wrong answer — so the assertion is on equality of the whole
/// `SparkGroup`, not on any one field.
#[test]
fn every_type_survives_the_round_trip_unchanged() {
    for kind in SparkKind::ALL {
        let mut map = SparklineMap::new();
        map.add(group(kind));
        let p = export(
            &format!("roundtrip_{}.xlsx", kind.label().replace('/', "")),
            &numeric_sheet(),
            &map,
        );

        let back = import_sparklines(&p).expect("import");
        assert_eq!(
            back.len(),
            1,
            "{kind:?}: exactly one group must come back, got {back:?}"
        );
        assert_eq!(back[0].sheet_index, 0);
        assert_eq!(
            back[0].group,
            group(kind),
            "{kind:?}: the group must come back byte-identical in meaning"
        );
        let _ = std::fs::remove_file(&p);
    }
}

/// A workbook with no sparklines imports as an empty list, not an error.
///
/// Without this the round-trip test above could pass while `import_sparklines`
/// hallucinated a group into every file it read.
#[test]
fn a_workbook_without_sparklines_imports_nothing() {
    let p = tmp("no_sparks.xlsx");
    crate::xlsx::export_workbook(
        &p,
        &[crate::xlsx::SheetExport::new("Data", &numeric_sheet())],
    )
    .expect("export");
    let back = import_sparklines(&p).expect("import");
    assert!(
        back.is_empty(),
        "a file with no sparklines must import none, got {back:?}"
    );
    let _ = std::fs::remove_file(&p);
}

/// A group that cannot survive is REPORTED, never silently dropped.
///
/// The repo's convention (`rule_survives_xlsx`, `decor_xlsx_loss`) is that the
/// user learns in the editor, not after opening the file in Excel.
#[test]
fn an_unexportable_group_is_reported_with_a_reason() {
    // A source overlapping its own destination: Excel would read it as a
    // circular reference.
    let mut map = SparklineMap::new();
    let bad = SparkGroup::new(SparkKind::Line, TableRange::new(0, 2, 3, 2), 0, 3);
    map.add(bad);
    assert!(!sparkline_survives_xlsx(&bad));

    let loss = sparkline_xlsx_loss(&map);
    assert_eq!(loss.len(), 1, "one problem, one sentence: {loss:?}");
    assert!(
        loss[0].contains("circular"),
        "the reason must be actionable, got {:?}",
        loss[0]
    );

    // And it is genuinely not written, rather than written broken.
    let p = export("bad_group.xlsx", &numeric_sheet(), &map);
    let xml = part(&p, "xl/worksheets/sheet1.xml");
    assert!(
        !xml.contains("x14:sparklineGroups"),
        "a group that cannot survive must not be written at all"
    );
    let _ = std::fs::remove_file(&p);

    // A healthy group reports nothing, so the check above cannot be passing
    // because `sparkline_xlsx_loss` always returns something.
    let mut ok = SparklineMap::new();
    ok.add(group(SparkKind::Column));
    assert!(sparkline_xlsx_loss(&ok).is_empty());
}

/// An Excel group Ferrix cannot represent is SKIPPED, not approximated.
///
/// Here every destination plots the same fixed source row, which is legal in
/// Excel and has no Ferrix shape. Compressing it into "row r plots row r"
/// would show the wrong data in three of the four cells, plausibly enough that
/// nobody would notice.
#[test]
fn a_group_ferrix_cannot_represent_is_skipped_rather_than_guessed() {
    let pairs: Vec<(String, String)> = (1..=4)
        .map(|r| ("Data!A9:D9".to_string(), format!("E{r}")))
        .collect();
    assert!(
        compress(SparkKind::Line, &pairs).is_none(),
        "a fixed source across moving destinations has no Ferrix representation"
    );

    // A destination ROW rather than a column: also unrepresentable.
    let row_pairs: Vec<(String, String)> = ["A1:A4", "B1:B4"]
        .iter()
        .zip(["E1", "F1"])
        .map(|(f, s)| (format!("Data!{f}"), s.to_string()))
        .collect();
    assert!(compress(SparkKind::Line, &row_pairs).is_none());

    // And the lockstep pattern IS accepted, so the two assertions above are
    // not passing because `compress` always returns None.
    let good: Vec<(String, String)> = (1..=4)
        .map(|r| (format!("Data!A{r}:D{r}"), format!("E{r}")))
        .collect();
    assert_eq!(
        compress(SparkKind::Line, &good),
        Some(SparkGroup::new(
            SparkKind::Line,
            TableRange::new(0, 4, 3, 4),
            0,
            3
        ))
    );
}

/// Two groups on one sheet both survive, and stay distinct.
///
/// A round trip that merged them, or kept only the last, would pass every
/// single-group test above.
#[test]
fn two_groups_on_one_sheet_both_survive() {
    let mut map = SparklineMap::new();
    map.add(SparkGroup::new(
        SparkKind::Line,
        TableRange::new(0, 4, 3, 4),
        0,
        1,
    ));
    map.add(SparkGroup::new(
        SparkKind::Column,
        TableRange::new(0, 5, 3, 5),
        2,
        3,
    ));
    let p = export("two_groups.xlsx", &numeric_sheet(), &map);

    let mut back = import_sparklines(&p).expect("import");
    back.sort_by_key(|i| i.group.target.first_col);
    assert_eq!(back.len(), 2, "both groups must come back, got {back:?}");
    assert_eq!(back[0].group.kind, SparkKind::Line);
    assert_eq!(back[0].group.src_last_col, 1);
    assert_eq!(back[1].group.kind, SparkKind::Column);
    assert_eq!(back[1].group.src_first_col, 2);
    let _ = std::fs::remove_file(&p);
}

/// Ferrix's own storage stays one entry however the file spells it.
///
/// The expansion is a property of OOXML. Import must NOT bring that expansion
/// back into memory — a million-row group read from a file would otherwise
/// become a million entries, which is the scale invariant failing on the
/// import path rather than the paint path.
#[test]
fn import_recompresses_rather_than_keeping_one_entry_per_row() {
    let mut map = SparklineMap::new();
    map.add(group(SparkKind::Line));
    let p = export("recompress.xlsx", &numeric_sheet(), &map);

    let back = import_sparklines(&p).expect("import");
    assert_eq!(
        back.len(),
        1,
        "4 file elements must re-compress to ONE group, got {}",
        back.len()
    );
    assert_eq!(back[0].group.target.rows(), 4, "covering all four rows");
    let _ = std::fs::remove_file(&p);
}
