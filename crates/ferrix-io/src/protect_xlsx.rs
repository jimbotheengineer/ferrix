//! Reading and writing `<sheetProtection>` / `<workbookProtection>`.
//!
//! # Not security, and the file format says so
//!
//! Both elements are lists of boolean attributes plus, optionally, a
//! `password="A1B2"` — a **sixteen-bit** hash, in plain hex, in an unencrypted
//! zip. Any reader is free to ignore all of it. Ferrix honours it because the
//! author of a shared workbook meant something by it, not because it can be
//! relied on. See `ferrix_core::protect` for the full argument, including the
//! function that manufactures a colliding password in constant time.
//!
//! # Round-tripping the password hash
//!
//! The acceptance criterion is that re-exporting a file must not strip the
//! password hash it came with. That is awkward because `rust_xlsxwriter` —
//! like every writer — takes a *password string* and hashes it, while an
//! imported file gives us only the hash. Ferrix bridges the gap with
//! [`ferrix_core::PasswordHash::matching_secret`], which derives a string that
//! hashes to the imported value, and hands *that* to the writer. The bytes in
//! the re-exported file's `password` attribute are therefore identical to the
//! bytes in the original.
//!
//! The user's actual password is not recovered and is not needed: it never
//! left their keyboard, and the file never carried it. That this works at all
//! is the point being made about how much the password is worth.
//!
//! # Why the reader opens the package directly
//!
//! calamine surfaces cell values and knows nothing about these elements —
//! the same reason `table_xlsx` reads the raw parts, and the same helpers are
//! reused.

use std::path::Path;

use ferrix_core::{Allowances, PasswordHash, SheetProtection, TableRange, WorkbookProtection};
use rust_xlsxwriter::{ProtectionOptions, Worksheet};

use crate::safeguard::Limits;
use crate::xlsx::XlsxError;

/// One worksheet's protection, as found in a file.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedProtection {
    /// Index of the owning worksheet, in workbook order.
    pub sheet_index: usize,
    pub protection: SheetProtection,
}

/// Read `<sheetProtection>` and every `<protectedRange>` from each worksheet.
///
/// Worksheets with no `<sheetProtection>` produce no entry at all, so an
/// ordinary file costs nothing.
///
/// `<protectedRange>` is Excel's "Allow Users to Edit Ranges" list, i.e. the
/// ranges that stay editable on a protected sheet — exactly what Ferrix stores
/// as its unlocked set. Per-cell `s="..."` style locking is NOT read: it would
/// mean walking every cell of a worksheet to reconstruct a per-cell flag, and
/// the scale invariant forbids a per-cell side table. See the module note in
/// `ferrix_core::protect` — locks are per range, always.
pub fn import_protection(path: impl AsRef<Path>) -> Result<Vec<ImportedProtection>, XlsxError> {
    import_protection_guarded(path, &Limits::measured())
}

/// [`import_protection`] under explicit limits.
pub fn import_protection_guarded(
    path: impl AsRef<Path>,
    limits: &Limits,
) -> Result<Vec<ImportedProtection>, XlsxError> {
    let path = path.as_ref();
    let disp = path.display().to_string();
    let parts = crate::table_xlsx::read_package_for(path, limits)?;
    let sheet_paths = crate::table_xlsx::worksheet_paths_for(&parts, &disp)?;

    let mut out = Vec::new();
    for (sheet_index, sp) in sheet_paths.iter().enumerate() {
        let Some(xml) = parts.get(sp) else { continue };
        let mut found = false;
        let mut prot = SheetProtection::new();
        let mut allow = Allowances::default();
        let mut hash = PasswordHash::NONE;
        let mut unlocked: Vec<TableRange> = Vec::new();

        crate::safeguard::scan_part(xml, &disp, sp, None, |ev| {
            use quick_xml::events::Event as E;
            let (E::Empty(e) | E::Start(e)) = ev else {
                return Ok(());
            };
            match e.local_name().as_ref() {
                b"sheetProtection" => {
                    found = true;
                    // Absent attributes take the OOXML default, which for the
                    // "cannot do this" flags is 0 = allowed. Reading a missing
                    // attribute as "denied" would silently tighten a file
                    // every time it passed through Ferrix.
                    let on = |name: &[u8]| xattr_bool(e, name);
                    allow = Allowances {
                        select_locked_cells: !on(b"selectLockedCells").unwrap_or(false),
                        select_unlocked_cells: !on(b"selectUnlockedCells").unwrap_or(false),
                        format_cells: !on(b"formatCells").unwrap_or(true),
                        insert_rows: !on(b"insertRows").unwrap_or(true),
                        insert_columns: !on(b"insertColumns").unwrap_or(true),
                        delete_rows: !on(b"deleteRows").unwrap_or(true),
                        delete_columns: !on(b"deleteColumns").unwrap_or(true),
                        sort: !on(b"sort").unwrap_or(true),
                        use_autofilter: !on(b"autoFilter").unwrap_or(true),
                    };
                    if let Some(p) = crate::table_xlsx::attr_for(e, b"password") {
                        if let Some(h) = PasswordHash::from_hex(&p) {
                            hash = h;
                        }
                    }
                }
                b"protectedRange" => {
                    // `sqref` may list several rectangles, space separated.
                    if let Some(sq) = crate::table_xlsx::attr_for(e, b"sqref") {
                        for token in sq.split_whitespace() {
                            if let Some(r) = parse_sqref_token(token) {
                                unlocked.push(r);
                            }
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        })?;

        if !found && unlocked.is_empty() {
            continue;
        }
        for r in unlocked {
            prot.unlock_range(r);
        }
        if found {
            prot.protect(allow, hash);
        }
        out.push(ImportedProtection {
            sheet_index,
            protection: prot,
        });
    }
    Ok(out)
}

/// Read `<workbookProtection>` from `xl/workbook.xml`.
pub fn import_workbook_protection(path: impl AsRef<Path>) -> Result<WorkbookProtection, XlsxError> {
    let path = path.as_ref();
    let disp = path.display().to_string();
    let parts = crate::table_xlsx::read_package_for(path, &Limits::measured())?;
    const PART: &str = "xl/workbook.xml";
    let Some(xml) = parts.get(PART) else {
        return Ok(WorkbookProtection::new());
    };
    let mut structure = false;
    let mut windows = false;
    let mut hash = PasswordHash::NONE;
    crate::safeguard::scan_part(xml, &disp, PART, None, |ev| {
        use quick_xml::events::Event as E;
        if let E::Empty(e) | E::Start(e) = ev {
            if e.local_name().as_ref() == b"workbookProtection" {
                structure = xattr_bool(e, b"lockStructure").unwrap_or(false);
                windows = xattr_bool(e, b"lockWindows").unwrap_or(false);
                if let Some(p) = crate::table_xlsx::attr_for(e, b"workbookPassword") {
                    if let Some(h) = PasswordHash::from_hex(&p) {
                        hash = h;
                    }
                }
            }
        }
        Ok(())
    })?;
    Ok(WorkbookProtection::from_parts(structure, windows, hash))
}

/// Apply a sheet's protection to a `rust_xlsxwriter` worksheet.
///
/// The password is re-supplied as [`PasswordHash::matching_secret`] so the
/// emitted `password` attribute is byte-identical to the one imported. When
/// the sheet is protected without a password, nothing is passed and no
/// attribute is written — matching what Excel does.
pub fn write_protection(
    ws: &mut Worksheet,
    prot: &SheetProtection,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    // Unlocked ranges are written whether or not protection is currently ON:
    // they are a property of the cells, and dropping them would silently
    // relock a form the next time someone turned protection back on.
    for r in prot.unlocked().ranges() {
        // Excel's column limit is 16384, which is what `ColNum` (u16) holds.
        // A Ferrix range may legally exceed it; the export path already
        // refuses oversized sheets up front, so clamping here only guards
        // against a panic on an unreachable input.
        let first_col = r.first_col.min(u16::MAX as u32) as u16;
        let last_col = r.last_col.min(u16::MAX as u32) as u16;
        ws.unprotect_range(r.first_row, first_col, r.last_row, last_col)?;
    }
    if !prot.is_enabled() {
        return Ok(());
    }
    let a = prot.allow();
    let opts = ProtectionOptions {
        select_locked_cells: a.select_locked_cells,
        select_unlocked_cells: a.select_unlocked_cells,
        format_cells: a.format_cells,
        format_columns: a.format_cells,
        format_rows: a.format_cells,
        insert_columns: a.insert_columns,
        insert_rows: a.insert_rows,
        delete_columns: a.delete_columns,
        delete_rows: a.delete_rows,
        sort: a.sort,
        use_autofilter: a.use_autofilter,
        ..ProtectionOptions::new()
    };
    ws.protect_with_options(&opts);
    if let Some(secret) = prot.hash().matching_secret() {
        // Sets `protection_hash` to exactly the imported value. See the module
        // docs: this is the round trip, and it is also the demonstration.
        ws.protect_with_password(&secret);
        ws.protect_with_options(&opts);
    }
    Ok(())
}

/// Inject `<workbookProtection>` into an already-written package.
///
/// `rust_xlsxwriter` 0.99 has no API for this element, so the one part that
/// needs it is rewritten after `save()`. Everything else in the file is
/// copied through byte for byte — the zip is rebuilt with `raw_copy_file` for
/// every other entry, so no other part is re-encoded and none can be
/// corrupted by this pass.
///
/// The element must sit immediately after `<fileVersion>`/`<workbookPr>` and
/// before `<sheets>`; OOXML's schema is sequenced, and Excel refuses a
/// workbook whose children are out of order. It is therefore inserted just
/// before `<sheets`, which is the first element guaranteed to be present and
/// to come after every element `<workbookProtection>` must follow.
pub fn inject_workbook_protection(path: &Path, prot: &WorkbookProtection) -> Result<(), XlsxError> {
    use std::io::{Read, Write};

    const PART: &str = "xl/workbook.xml";
    let disp = path.display().to_string();
    let io_err = |e: std::io::Error| XlsxError::WorkbookProtection {
        path: disp.clone(),
        detail: e.to_string(),
    };
    let zip_err = |e: zip::result::ZipError| XlsxError::WorkbookProtection {
        path: disp.clone(),
        detail: e.to_string(),
    };

    let bytes = std::fs::read(path).map_err(io_err)?;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).map_err(zip_err)?;

    // Build the replacement part first: if the marker is missing there is
    // nothing safe to do, and rewriting the archive would only risk damage.
    let mut xml = String::new();
    {
        let mut f = zip.by_name(PART).map_err(zip_err)?;
        f.read_to_string(&mut xml).map_err(io_err)?;
    }
    let Some(at) = xml.find("<sheets") else {
        return Err(XlsxError::WorkbookProtection {
            path: disp,
            detail: "xl/workbook.xml has no <sheets> element".to_string(),
        });
    };
    let mut el = String::from("<workbookProtection");
    if !prot.hash().is_none() {
        el.push_str(&format!(" workbookPassword=\"{}\"", prot.hash().to_hex()));
    }
    if prot.structure_locked() {
        el.push_str(" lockStructure=\"1\"");
    }
    if prot.windows_locked() {
        el.push_str(" lockWindows=\"1\"");
    }
    el.push_str("/>");
    let patched = format!("{}{}{}", &xml[..at], el, &xml[at..]);

    let out = std::fs::File::create(path).map_err(io_err)?;
    let mut w = zip::ZipWriter::new(out);
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i).map_err(zip_err)?;
        if entry.name() == PART {
            continue;
        }
        // Raw copy: the compressed bytes move across untouched, so nothing
        // outside the one part we mean to change can be altered.
        w.raw_copy_file(entry).map_err(zip_err)?;
    }
    w.start_file(
        PART,
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated),
    )
    .map_err(zip_err)?;
    w.write_all(patched.as_bytes()).map_err(io_err)?;
    w.finish().map_err(zip_err)?;
    Ok(())
}

/// Parse one `sqref` token: `B2:D9`, or a bare `C4`.
fn parse_sqref_token(tok: &str) -> Option<TableRange> {
    if let Some(r) = TableRange::from_a1(tok) {
        return Some(r);
    }
    let c = ferrix_core::CellRef::from_a1(tok)?;
    Some(TableRange::new(c.row, c.col, c.row, c.col))
}

/// Read an OOXML boolean attribute. `1`/`true` are true; `0`/`false` false.
fn xattr_bool(e: &quick_xml::events::BytesStart, name: &[u8]) -> Option<bool> {
    let v = crate::table_xlsx::attr_for(e, name)?;
    match v.trim() {
        "1" | "true" | "TRUE" | "True" => Some(true),
        "0" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
