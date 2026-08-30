//! Reading dynamic-array formula regions from `xl/worksheets/sheetN.xml` (#27 P4).
//!
//! ## Why this reads the raw XML rather than going through calamine
//!
//! calamine's `worksheet_formula` yields only the formula TEXT at each cell —
//! it drops the `t="array"` marker and the `ref` attribute that say a formula
//! is a dynamic array and how far it spilled. Those two facts are exactly what
//! distinguishes `=D1:D3` written as an ordinary formula from the same text
//! written as a spilling dynamic array: on reopen the first is a `#VALUE!`
//! scalar, the second paints D-column values down three cells. So the writer
//! emits `<f t="array" ref="A1:A3">` ([`crate::xlsx`] via
//! `write_dynamic_array_formula`) and this reader recovers the host cell and
//! its spill rectangle from the same element, using the shared, safeguarded
//! package reader so the zip-slip / declared-size / part-budget checks apply
//! once (see [`crate::table_xlsx::read_package_for`]).
//!
//! ## What it does NOT do
//!
//! It does not re-evaluate anything. The recovered rectangle lets the workbook
//! mark the host as a dynamic-array producer; the array VALUES come from
//! re-running the formula through the P1/P2 spill machinery, so the file only
//! needs to carry the host formula and the fact that it spilled — never a
//! frozen copy of every projected cell.

use std::path::Path;

use ferrix_core::CellRef;

use crate::safeguard::Limits;
use crate::xlsx::XlsxError;

/// One dynamic-array formula host recovered from a worksheet: the anchor cell
/// and the inclusive rectangle its `ref` attribute claimed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ArrayFormulaRegion {
    /// The host cell — the top-left of the `ref` rectangle, where the `<f>`
    /// lives.
    pub host: CellRef,
    /// Rows the spill covers (>= 1).
    pub rows: u32,
    /// Columns the spill covers (>= 1).
    pub cols: u32,
}

/// Every dynamic-array formula region on one sheet, keyed by sheet index in
/// tab order — the same index [`crate::xlsx::import_xlsx_full`] yields.
#[derive(Clone, Debug, Default)]
pub struct ImportedArrayFormulas {
    pub sheet_index: usize,
    pub regions: Vec<ArrayFormulaRegion>,
}

/// Read the `<f t="array" ref="...">` regions from every worksheet in `path`.
pub fn import_array_formulas(
    path: impl AsRef<Path>,
) -> Result<Vec<ImportedArrayFormulas>, XlsxError> {
    import_array_formulas_guarded(path, &Limits::measured())
}

/// [`import_array_formulas`] under explicit safeguard limits.
pub fn import_array_formulas_guarded(
    path: impl AsRef<Path>,
    limits: &Limits,
) -> Result<Vec<ImportedArrayFormulas>, XlsxError> {
    let path = path.as_ref();
    let disp = path.display().to_string();
    let parts = crate::table_xlsx::read_package_for(path, limits)?;
    let sheet_paths = crate::table_xlsx::worksheet_paths_for(&parts, &disp)?;

    let mut out = Vec::new();
    for (sheet_index, sp) in sheet_paths.iter().enumerate() {
        let Some(xml) = parts.get(sp) else { continue };
        let regions = scan_sheet(xml, &disp, sp)?;
        if !regions.is_empty() {
            out.push(ImportedArrayFormulas {
                sheet_index,
                regions,
            });
        }
    }
    Ok(out)
}

/// Scan one worksheet part for array-formula regions.
///
/// The element of interest is `<f t="array" ref="A1:A3">`, always the first
/// child of its `<c r="A1">` cell. `t` is not always literally `"array"`
/// first among the attributes, so both are read from the `<f>` and the host is
/// taken from the enclosing `<c>`'s `r`. Any `<f>` without `t="array"` (an
/// ordinary formula) or without a parseable `ref` is ignored — calamine has
/// already carried the formula text for those.
fn scan_sheet(xml: &[u8], disp: &str, part: &str) -> Result<Vec<ArrayFormulaRegion>, XlsxError> {
    use quick_xml::events::Event as E;
    let mut out = Vec::new();
    // The cell whose `<c>` we are currently inside, so an `<f>`'s host is the
    // enclosing cell rather than the `ref` — Excel anchors the array at `<c r>`
    // and `ref`'s top-left agrees, but reading the host from `<c>` is robust to
    // a writer that emits a `ref` not starting at the anchor.
    let mut current_cell: Option<CellRef> = None;

    crate::safeguard::scan_part(xml, disp, part, None, |ev| {
        match ev {
            E::Start(e) if e.local_name().as_ref() == b"c" => {
                current_cell =
                    crate::table_xlsx::attr_for(e, b"r").and_then(|r| CellRef::from_a1(&r));
            }
            E::End(e) if e.local_name().as_ref() == b"c" => {
                current_cell = None;
            }
            E::Start(e) | E::Empty(e) if e.local_name().as_ref() == b"f" => {
                let is_array = crate::table_xlsx::attr_for(e, b"t")
                    .is_some_and(|t| t.eq_ignore_ascii_case("array"));
                if !is_array {
                    return Ok(());
                }
                let Some(reef) = crate::table_xlsx::attr_for(e, b"ref") else {
                    return Ok(());
                };
                let Some((host, rows, cols)) = parse_ref_rect(&reef) else {
                    return Ok(());
                };
                // Prefer the enclosing cell as the host, falling back to the
                // ref's top-left when (pathologically) there is no `<c r>`.
                let host = current_cell.unwrap_or(host);
                out.push(ArrayFormulaRegion { host, rows, cols });
            }
            _ => {}
        }
        Ok(())
    })?;

    Ok(out)
}

/// Parse a `ref` like `A1:A10` or a single `A1` into (top-left, rows, cols).
///
/// A single-cell `ref` is a 1x1 region — a dynamic array that happened to
/// spill one cell. Corners are normalized so a `ref` written in either order
/// yields the same rectangle.
fn parse_ref_rect(reef: &str) -> Option<(CellRef, u32, u32)> {
    let (a, b) = match reef.split_once(':') {
        Some((a, b)) => (CellRef::from_a1(a.trim())?, CellRef::from_a1(b.trim())?),
        None => {
            let c = CellRef::from_a1(reef.trim())?;
            (c, c)
        }
    };
    let top = CellRef::new(a.row.min(b.row), a.col.min(b.col));
    let rows = a.row.max(b.row) - top.row + 1;
    let cols = a.col.max(b.col) - top.col + 1;
    Some((top, rows, cols))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ref_rect_reads_a_column_range() {
        let (top, rows, cols) = parse_ref_rect("A1:A10").unwrap();
        assert_eq!(top, CellRef::new(0, 0));
        assert_eq!((rows, cols), (10, 1));
    }

    #[test]
    fn parse_ref_rect_reads_a_block_and_a_single_cell() {
        let (top, rows, cols) = parse_ref_rect("B2:D4").unwrap();
        assert_eq!(top, CellRef::new(1, 1));
        assert_eq!((rows, cols), (3, 3));

        // A single-cell ref is a 1x1 region.
        let (top, rows, cols) = parse_ref_rect("C5").unwrap();
        assert_eq!(top, CellRef::new(4, 2));
        assert_eq!((rows, cols), (1, 1));
    }

    #[test]
    fn parse_ref_rect_normalizes_reversed_corners() {
        let forward = parse_ref_rect("A1:C3").unwrap();
        let reversed = parse_ref_rect("C3:A1").unwrap();
        assert_eq!(forward, reversed);
    }

    #[test]
    fn parse_ref_rect_rejects_garbage() {
        assert!(parse_ref_rect("").is_none());
        assert!(parse_ref_rect("not-a-ref").is_none());
        assert!(parse_ref_rect("A1:").is_none());
    }
}
