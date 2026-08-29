//! End-to-end resource safeguards, driven through the public import API.
//!
//! The unit tests in `safeguard/tests.rs` exercise the primitives directly.
//! These go the other way: they build a **real** `.xlsx` with the real
//! exporter, damage it in one specific way, and then assert on what
//! `import_xlsx` / `import_defined_names` / `import_tables` actually do.
//!
//! That distinction matters. A safeguard module that is perfect but not
//! wired into the importers protects nothing, and a unit test on the module
//! cannot tell the difference. Everything here goes through the same entry
//! points the application calls.
//!
//! ## The assertion standard
//!
//! For each case: the SPECIFIC error variant, the NAME of the failing part,
//! and — where the criterion is about partial state — a control proving the
//! undamaged file imports the full expected content. A test that only checks
//! `is_err()` passes against an importer that rejects everything, which is
//! not the behaviour anyone wants.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use ferrix_core::{CancelToken, CellRef, Sheet, Value};
use ferrix_io::safeguard::{Limits, SafeguardError};
use ferrix_io::xlsx::{
    export_xlsx, import_defined_names, import_xlsx, import_xlsx_guarded, XlsxError,
};

/// Fixtures live under `benchdata/` in this clone and are removed on drop,
/// as the agent guide requires. Only this test's own directory is touched —
/// peers may be running.
struct Fixtures(PathBuf);

impl Fixtures {
    fn new(tag: &str) -> Self {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let d = root.join("benchdata").join(format!("e2e-{tag}-{uniq}"));
        std::fs::create_dir_all(&d).expect("create fixture dir");
        Self(d)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A three-sheet-shaped workbook with known, countable content, written by
/// the real exporter so the fixture is a genuine `.xlsx`.
fn write_workbook(path: &Path) -> usize {
    let mut s = Sheet::new("Data");
    let rows = 200u32;
    for r in 0..rows {
        s.set(CellRef::new(r, 0), Value::Number(f64::from(r)));
        s.set(CellRef::new(r, 1), Value::Number(f64::from(r) * 2.0));
    }
    export_xlsx(path, &s, "Data").expect("export fixture");
    (rows * 2) as usize
}

/// Rebuild an archive with one named part replaced by `body`.
///
/// Repacking rather than byte-patching in place: a real `.xlsx` is deflated,
/// so overwriting bytes would corrupt the compressed stream rather than
/// producing the specific damage each test is about.
fn repack_with(src: &Path, dst: &Path, part: &str, body: &[u8]) {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(src).expect("open src")).expect("zip");
    let out = std::fs::File::create(dst).expect("create dst");
    let mut w = zip::ZipWriter::new(out);
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .collect();
    for name in names {
        let mut buf = Vec::new();
        zip.by_name(&name)
            .expect("entry")
            .read_to_end(&mut buf)
            .expect("read entry");
        w.start_file(&name, opts).expect("start");
        if name == part {
            w.write_all(body).expect("write replacement");
        } else {
            w.write_all(&buf).expect("write entry");
        }
    }
    w.finish().expect("finish");
}

/// Read one part out of an archive.
fn part_bytes(path: &Path, part: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut buf = Vec::new();
    zip.by_name(part).unwrap().read_to_end(&mut buf).unwrap();
    buf
}

// ------------------------------------------------------ the control ---

#[test]
fn an_undamaged_workbook_imports_completely() {
    // Every test below asserts that a DAMAGED file is refused. Without this
    // control they would all pass against an importer that refuses
    // everything, which is the classic way a safeguard suite certifies a
    // broken feature.
    let fx = Fixtures::new("control");
    let p = fx.path("ok.xlsx");
    let expected_cells = write_workbook(&p);

    let sheets = import_xlsx(&p).expect("an undamaged workbook must import");
    assert_eq!(sheets.len(), 1, "one worksheet expected");
    let (name, sheet) = &sheets[0];
    assert_eq!(name, "Data");
    // Assert on CONTENT, not just that something came back.
    assert_eq!(sheet.get(CellRef::new(0, 0)), Value::Number(0.0));
    assert_eq!(sheet.get(CellRef::new(199, 1)), Value::Number(398.0));
    let counted = (0..200u32)
        .flat_map(|r| (0..2u32).map(move |c| CellRef::new(r, c)))
        .filter(|&cell| !sheet.get(cell).is_empty())
        .count();
    assert_eq!(
        counted, expected_cells,
        "every exported cell must come back"
    );
}

// ------------------------------------------------------- truncation ---

#[test]
fn a_truncated_worksheet_is_refused_by_name_rather_than_imported_short() {
    // THE defect this issue is about. The old readers matched
    // `Ok(Eof) | Err(_) => break`, so a sheet cut in half imported as a
    // SHORTER sheet with no error anywhere — silent data loss presented as
    // success. Here the same file must produce a named error instead.
    let fx = Fixtures::new("truncated-sheet");
    let good = fx.path("good.xlsx");
    write_workbook(&good);

    // Find the worksheet part and cut it in half.
    let sheet_part = {
        let zip = zip::ZipArchive::new(std::fs::File::open(&good).unwrap()).unwrap();
        let found = zip
            .file_names()
            .find(|n| n.contains("worksheets/") && n.ends_with(".xml"))
            .expect("a worksheet part must exist")
            .to_string();
        found
    };
    let full = part_bytes(&good, &sheet_part);
    assert!(full.len() > 1000, "fixture sheet part is implausibly small");
    let cut = fx.path("cut.xlsx");
    repack_with(&good, &cut, &sheet_part, &full[..full.len() / 2]);

    let err = import_xlsx(&cut)
        .expect_err("a worksheet truncated at half its length must not import as a shorter sheet");
    // It must name something specific — not a bare "could not read file".
    let msg = err.to_string();
    assert!(
        msg.contains("sheet") || msg.contains("xl/"),
        "the error must identify the failing part, got: {msg}"
    );
}

#[test]
fn a_truncated_workbook_part_is_refused_naming_that_part() {
    // `xl/workbook.xml` carries the sheet list and the defined names. A
    // truncated one used to yield a workbook with FEWER names and no
    // complaint — and because `localSheetId` indexes the sheet order, a
    // short `<sheets>` list also silently re-scopes names onto wrong sheets.
    let fx = Fixtures::new("truncated-workbook");
    let good = fx.path("good.xlsx");
    write_workbook(&good);

    // Control: the undamaged file reads its (empty) name table cleanly.
    import_defined_names(&good).expect("undamaged workbook.xml must parse");

    let full = part_bytes(&good, "xl/workbook.xml");
    let cut = fx.path("cut.xlsx");
    repack_with(&good, &cut, "xl/workbook.xml", &full[..full.len() * 2 / 3]);

    let err =
        import_defined_names(&cut).expect_err("a truncated workbook.xml must not parse as valid");
    let XlsxError::Safeguard(sg) = &err else {
        panic!("expected a Safeguard error, got {err:?}");
    };
    assert!(
        matches!(sg, SafeguardError::MalformedXml { .. }),
        "expected MalformedXml, got {sg:?}"
    );
    assert_eq!(
        sg.part(),
        Some("xl/workbook.xml"),
        "the failing part must be named: {sg}"
    );
}

#[test]
fn a_file_truncated_at_arbitrary_points_never_panics() {
    // Whole-file truncation, which usually destroys the zip central
    // directory rather than the XML. Every cut must yield a Result.
    let fx = Fixtures::new("file-truncation");
    let good = fx.path("good.xlsx");
    write_workbook(&good);
    let bytes = std::fs::read(&good).unwrap();

    for frac in [1usize, 2, 3, 5, 7, 9] {
        let cut = bytes.len() * frac / 10;
        let p = fx.path(&format!("cut{frac}.xlsx"));
        std::fs::write(&p, &bytes[..cut]).unwrap();
        // Reaching the next line at all is the assertion: a panic inside
        // zip, calamine or quick-xml would unwind out of this test.
        let r = import_xlsx(&p);
        if let Ok(sheets) = r {
            // A truncated file must never import as a *complete* one. If
            // some prefix happens to parse, it must not claim the full 200
            // rows of the original.
            for (_, s) in &sheets {
                assert!(
                    s.get(CellRef::new(199, 1)) != Value::Number(398.0),
                    "a file cut to {frac}0% claimed to contain the last row of the original"
                );
            }
        }
    }
}

// -------------------------------------------------- entity expansion ---

#[test]
fn a_billion_laughs_workbook_is_refused_at_import() {
    // End to end: the payload goes into a real package and the public import
    // API must refuse it, naming the part that carried it.
    let fx = Fixtures::new("laughs");
    let good = fx.path("good.xlsx");
    write_workbook(&good);

    let payload = br#"<?xml version="1.0"?>
<!DOCTYPE lolz [
 <!ENTITY lol "lol">
 <!ENTITY lol1 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
 <!ENTITY lol2 "&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;">
 <!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">
]>
<workbook><sheets><sheet name="&lol3;" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
    let evil = fx.path("laughs.xlsx");
    repack_with(&good, &evil, "xl/workbook.xml", payload);

    let err = import_defined_names(&evil).expect_err("an entity-bearing workbook must be refused");
    let XlsxError::Safeguard(sg) = &err else {
        panic!("expected a Safeguard error, got {err:?}");
    };
    assert!(
        matches!(sg, SafeguardError::EntityDeclaration { .. }),
        "expected EntityDeclaration, got {sg:?}"
    );
    assert_eq!(sg.part(), Some("xl/workbook.xml"));
    assert!(
        err.to_string().contains("Entity expansion is refused"),
        "the message must explain what was refused: {err}"
    );
}

// ------------------------------------------------------- bounded memory ---

#[test]
fn an_import_over_the_memory_limit_fails_cleanly_and_keeps_nothing() {
    // The criterion: a file exceeding the bound fails cleanly with partial
    // state discarded. Driven through the public guarded entry point with a
    // deliberately tiny budget, so no machine-scale fixture is needed.
    let fx = Fixtures::new("memlimit");
    let p = fx.path("wb.xlsx");
    write_workbook(&p);

    // A budget far below what this workbook declares.
    let tiny = Limits::with_bytes(64, 64);
    let err = match import_xlsx_guarded(&p, &tiny, None) {
        Err(e) => e,
        Ok(sheets) => panic!(
            "a workbook over the memory limit must be refused, got {} sheets",
            sheets.len()
        ),
    };
    let XlsxError::Safeguard(sg) = &err else {
        panic!("expected a Safeguard error, got {err:?}");
    };
    assert!(
        sg.is_pre_extraction() || matches!(sg, SafeguardError::PartTooLarge { .. }),
        "the refusal must happen before or at the part read, got {sg:?}"
    );

    // The control: the SAME file imports completely under a real budget, so
    // the refusal above is the limit's doing and not a broken fixture.
    let sheets = import_xlsx_guarded(&p, &Limits::measured(), None)
        .expect("the same workbook must import under a measured budget");
    assert_eq!(sheets.len(), 1);
    assert_eq!(
        sheets[0].sheet.get(CellRef::new(199, 1)),
        Value::Number(398.0),
        "the control import must contain the full sheet"
    );
}

// ---------------------------------------------------------- cancellation ---

#[test]
fn a_cancelled_import_returns_no_sheets_at_all() {
    // "Cancelling leaves a consistent state" means the caller gets nothing,
    // not a truncated prefix of the workbook that happened to finish.
    let fx = Fixtures::new("cancel");
    let p = fx.path("wb.xlsx");
    write_workbook(&p);

    let token = CancelToken::new();
    token.cancel();
    let err = match import_xlsx_guarded(&p, &Limits::measured(), Some(&token)) {
        Err(e) => e,
        Ok(sheets) => panic!(
            "a cancelled import must not return a workbook, got {} sheets",
            sheets.len()
        ),
    };
    let XlsxError::Safeguard(sg) = &err else {
        panic!("expected a Safeguard error, got {err:?}");
    };
    assert!(
        matches!(sg, SafeguardError::Cancelled { .. }),
        "expected Cancelled, got {sg:?}"
    );
    assert!(
        sg.part().is_some(),
        "the cancellation must say where it stopped"
    );

    // The control: reset the token and the identical call succeeds in full.
    // Without this the test would pass against an importer that always
    // reports cancellation.
    token.reset();
    let sheets = import_xlsx_guarded(&p, &Limits::measured(), Some(&token))
        .expect("an uncancelled import must succeed");
    assert_eq!(sheets.len(), 1);
    assert_eq!(
        sheets[0].sheet.get(CellRef::new(199, 1)),
        Value::Number(398.0),
        "the uncancelled import must contain the whole sheet"
    );
}

// -------------------------------------------------------------- zip bomb ---

#[test]
fn a_bomb_disguised_as_a_workbook_is_refused_before_extraction() {
    // A package whose worksheet part is 32 MB of zeroes: a few kilobytes on
    // disk, refused from the central directory alone.
    let fx = Fixtures::new("bomb-e2e");
    let p = fx.path("bomb.xlsx");
    {
        let f = std::fs::File::create(&p).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file("xl/workbook.xml", opts).unwrap();
        w.write_all(&vec![0u8; 32 << 20]).unwrap();
        w.finish().unwrap();
    }
    let on_disk = std::fs::metadata(&p).unwrap().len();
    assert!(on_disk < 1 << 20, "fixture is not a bomb: {on_disk} bytes");

    let err = import_xlsx(&p).expect_err("a decompression bomb must be refused");
    let XlsxError::Safeguard(sg) = &err else {
        panic!("expected a Safeguard error, got {err:?}");
    };
    assert!(
        sg.is_pre_extraction(),
        "a bomb must be refused BEFORE extraction, got {sg:?}"
    );
}
