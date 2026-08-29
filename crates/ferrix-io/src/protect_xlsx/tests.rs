//! Round-trip tests for protection in `.xlsx`.
//!
//! These write a real package, unzip it, assert on the OOXML, and re-import.
//! **Excel was never launched** — nothing here proves Excel accepts the file.
//! What is proven is that the elements Excel reads are present with the
//! expected attributes, and that Ferrix reads back exactly what it wrote.

use super::*;
use ferrix_core::{CellRef, Sheet, Value};

use crate::xlsx::{export_workbook_full, SheetExport};

/// A temp path that deletes itself.
struct TempXlsx(std::path::PathBuf);

impl TempXlsx {
    fn new(tag: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!("ferrix-prot-{tag}-{n}.xlsx")))
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
    s.set(CellRef::new(0, 0), Value::Number(1.0));
    s.set(CellRef::new(1, 0), Value::Number(2.0));
    s.set(CellRef::new(0, 1), Value::Number(3.0));
    s.set(CellRef::new(1, 1), Value::Number(4.0));
    s
}

fn write(tag: &str, prot: &SheetProtection, wbp: &WorkbookProtection) -> TempXlsx {
    let tmp = TempXlsx::new(tag);
    let sheet = demo_sheet();
    let ex = SheetExport::new("Data", &sheet).with_protection(prot);
    export_workbook_full(tmp.path(), &[ex], &ferrix_formula::NameTable::new(), wbp)
        .expect("export");
    tmp
}

// ------------------------------------------------------------- sheet part --

#[test]
fn a_protected_sheet_writes_a_sheet_protection_element() {
    let mut p = SheetProtection::new();
    p.protect(Allowances::default(), PasswordHash::NONE);
    let tmp = write("basic", &p, &WorkbookProtection::new());
    let xml = part(tmp.path(), "xl/worksheets/sheet1.xml").expect("worksheet part");
    assert!(
        xml.contains("<sheetProtection"),
        "no <sheetProtection> in the emitted worksheet:\n{xml}"
    );
    assert!(xml.contains("sheet=\"1\""), "protection not switched on");
}

#[test]
fn an_unprotected_sheet_writes_no_protection_element() {
    // The negative: a file that gains a protection element it never asked for
    // would change meaning on every save.
    let p = SheetProtection::new();
    let tmp = write("none", &p, &WorkbookProtection::new());
    let xml = part(tmp.path(), "xl/worksheets/sheet1.xml").expect("worksheet part");
    assert!(!xml.contains("<sheetProtection"), "{xml}");
    assert!(!xml.contains("<protectedRange"), "{xml}");
}

#[test]
fn unlocked_ranges_become_protected_range_elements() {
    let mut p = SheetProtection::new();
    p.unlock_range(TableRange::new(1, 1, 3, 2));
    p.protect(Allowances::default(), PasswordHash::NONE);
    let tmp = write("ranges", &p, &WorkbookProtection::new());
    let xml = part(tmp.path(), "xl/worksheets/sheet1.xml").expect("worksheet part");
    assert!(xml.contains("<protectedRange"), "{xml}");
    assert!(
        xml.contains("B2:C4"),
        "the unlocked rectangle must be named in sqref:\n{xml}"
    );
}

#[test]
fn allowances_round_trip_through_the_file() {
    // Assert on the flags one at a time, so an exporter that wrote a constant
    // set of attributes cannot pass.
    let mut p = SheetProtection::new();
    p.protect(
        Allowances {
            sort: true,
            insert_rows: true,
            format_cells: false,
            delete_columns: false,
            use_autofilter: true,
            ..Allowances::default()
        },
        PasswordHash::NONE,
    );
    let tmp = write("allow", &p, &WorkbookProtection::new());

    let back = import_protection(tmp.path()).expect("import");
    assert_eq!(back.len(), 1, "one protected sheet expected");
    let a = back[0].protection.allow();
    assert!(a.sort, "sort was allowed and must come back allowed");
    assert!(a.insert_rows);
    assert!(a.use_autofilter);
    assert!(!a.format_cells, "format_cells was denied and must stay so");
    assert!(!a.delete_columns);
    assert!(back[0].protection.is_enabled());
}

#[test]
fn unlocked_ranges_survive_the_round_trip_as_editable_cells() {
    // The decisive assertion is on BEHAVIOUR, not on the XML: after the trip,
    // the cell inside the range must be editable and the one outside must not.
    let mut p = SheetProtection::new();
    p.unlock_range(TableRange::new(1, 1, 3, 2));
    p.protect(Allowances::default(), PasswordHash::NONE);
    let tmp = write("rt-ranges", &p, &WorkbookProtection::new());

    let back = import_protection(tmp.path()).expect("import");
    let q = &back[0].protection;
    assert_eq!(q.deny_edit(CellRef::new(2, 1)), None, "B3 was unlocked");
    assert!(
        q.deny_edit(CellRef::new(0, 0)).is_some(),
        "A1 was never unlocked and must still be refused"
    );
}

// ---------------------------------------------------------- the hash trip --

#[test]
fn an_imported_password_hash_is_not_stripped_by_re_export() {
    // THE round-trip criterion. A file arrives carrying a password hash;
    // Ferrix must write the SAME hash back out, byte for byte, without ever
    // having seen the password.
    let original = PasswordHash::from_raw(0x83AF); // hash of "password"
    let mut p = SheetProtection::new();
    p.protect(Allowances::default(), original);
    let tmp = write("hash", &p, &WorkbookProtection::new());

    let xml = part(tmp.path(), "xl/worksheets/sheet1.xml").expect("worksheet part");
    assert!(
        xml.contains("password=\"83AF\""),
        "the imported hash must be written verbatim, not dropped or rehashed:\n{xml}"
    );

    let back = import_protection(tmp.path()).expect("import");
    assert_eq!(
        back[0].protection.hash(),
        original,
        "hash changed across the trip"
    );
    // And it still answers to the user's real password, because it is the
    // same sixteen bits.
    assert!(back[0].protection.hash().verify("password"));
}

#[test]
fn a_protected_sheet_without_a_password_writes_no_password_attribute() {
    let mut p = SheetProtection::new();
    p.protect(Allowances::default(), PasswordHash::NONE);
    let tmp = write("nopw", &p, &WorkbookProtection::new());
    let xml = part(tmp.path(), "xl/worksheets/sheet1.xml").expect("worksheet part");
    assert!(
        !xml.contains("password="),
        "a passwordless sheet must not gain a password attribute:\n{xml}"
    );
    let back = import_protection(tmp.path()).expect("import");
    assert!(back[0].protection.hash().is_none());
}

// ------------------------------------------------------- workbook element --

#[test]
fn workbook_structure_protection_round_trips() {
    let mut w = WorkbookProtection::new();
    w.protect_structure(PasswordHash::of("shhh"));
    let mut p = SheetProtection::new();
    p.protect(Allowances::default(), PasswordHash::NONE);
    let tmp = write("wbprot", &p, &w);

    let xml = part(tmp.path(), "xl/workbook.xml").expect("workbook part");
    assert!(xml.contains("<workbookProtection"), "{xml}");
    assert!(xml.contains("lockStructure=\"1\""), "{xml}");
    assert!(
        xml.find("<workbookProtection").unwrap() < xml.find("<sheets").unwrap(),
        "OOXML sequences the element before <sheets>:\n{xml}"
    );

    let back = import_workbook_protection(tmp.path()).expect("import");
    assert!(back.structure_locked());
    assert_eq!(back.hash(), PasswordHash::of("shhh"));
    assert!(back.deny(ferrix_core::StructureOp::RenameSheet).is_some());
}

#[test]
fn injecting_workbook_protection_leaves_every_other_part_intact() {
    // The injection rebuilds the zip. If it corrupted or dropped a part, the
    // sheet's own data would be the first casualty — so this reads it back.
    let mut w = WorkbookProtection::new();
    w.protect_structure(PasswordHash::NONE);
    let p = SheetProtection::new();
    let tmp = write("intact", &p, &w);

    let sheets = crate::xlsx::import_xlsx(tmp.path()).expect("re-import the values");
    assert_eq!(sheets.len(), 1);
    let s = &sheets[0].1;
    assert_eq!(s.get(CellRef::new(0, 0)), Value::Number(1.0));
    assert_eq!(s.get(CellRef::new(1, 1)), Value::Number(4.0));
    // And the workbook part really did change.
    let xml = part(tmp.path(), "xl/workbook.xml").expect("workbook part");
    assert!(xml.contains("lockStructure=\"1\""));
}

#[test]
fn a_workbook_with_no_structure_protection_gains_no_element() {
    let p = SheetProtection::new();
    let tmp = write("wbnone", &p, &WorkbookProtection::new());
    let xml = part(tmp.path(), "xl/workbook.xml").expect("workbook part");
    assert!(!xml.contains("workbookProtection"), "{xml}");
    let back = import_workbook_protection(tmp.path()).expect("import");
    assert!(!back.structure_locked());
    assert!(!back.is_active());
}

// -------------------------------------------------------------- odds/ends --

#[test]
fn sqref_tokens_parse_as_ranges_and_bare_cells() {
    assert_eq!(
        parse_sqref_token("B2:C4"),
        Some(TableRange::new(1, 1, 3, 2))
    );
    assert_eq!(parse_sqref_token("D7"), Some(TableRange::new(6, 3, 6, 3)));
    assert_eq!(parse_sqref_token("not-a-ref"), None);
}

#[test]
fn a_file_with_no_protection_imports_as_no_entries() {
    let p = SheetProtection::new();
    let tmp = write("empty", &p, &WorkbookProtection::new());
    assert!(
        import_protection(tmp.path()).expect("import").is_empty(),
        "an ordinary workbook must produce no protection entries at all"
    );
}
