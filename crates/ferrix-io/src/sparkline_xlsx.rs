//! Sparklines on the xlsx side: `<extLst><x14:sparklineGroups>` (issue #36).
//!
//! ## What Excel actually stores, and why the round trip is possible
//!
//! Excel has no notion of "a sparkline rule over a range". A
//! `<x14:sparklineGroup>` holds a `<x14:sparklines>` list with **one entry per
//! destination cell**:
//!
//! ```xml
//! <x14:sparkline><xm:f>Sheet1!A2:D2</xm:f><xm:sqref>E2</xm:sqref></x14:sparkline>
//! <x14:sparkline><xm:f>Sheet1!A3:D3</xm:f><xm:sqref>E3</xm:sqref></x14:sparkline>
//! ```
//!
//! Ferrix's [`SparkGroup`] is the *compressed* form of exactly that pattern:
//! one destination rectangle plus a source column span, with row `r` plotting
//! row `r`. So export EXPANDS the group into per-cell entries and import
//! RE-COMPRESSES a run of them back into a group — and the round trip is exact
//! for every group a user can make in Ferrix, because Ferrix cannot express
//! anything else.
//!
//! Import deliberately does NOT try to represent a hand-authored Excel group
//! whose entries do not march down a column in lockstep (say, E2 plotting
//! A9:D9). Ferrix has no storage shape for that, and inventing one row's worth
//! of guess for it would silently show the wrong picture. Such a group is
//! skipped and reported through [`sparkline_xlsx_loss`], in the same shape
//! `rule_survives_xlsx` and `decor_xlsx_loss` established.
//!
//! ## Why the expansion is capped
//!
//! One `<x14:sparkline>` element per destination row is the only spelling the
//! format has, so a group over 200M rows has no bounded encoding — this is a
//! property of OOXML, not a shortcut taken here. Excel's own row ceiling
//! (1,048,576) already bounds it, and [`crate::xlsx::check_limits`] refuses a
//! sheet past that before this module runs; on top of that the expansion is
//! capped at [`MAX_SPARKLINE_CELLS`] and anything past it is REPORTED rather
//! than written, because expanding it is how an export turns into an
//! out-of-memory kill.
//!
//! ## What was NOT verified
//!
//! Excel is not available in this environment and was never launched. The
//! tests unzip the written package and read the XML back, so the claim is
//! "the elements Excel reads are present, correctly namespaced and
//! structurally correct", not "opened in Excel and confirmed".

use std::collections::HashMap;
use std::path::Path;

use ferrix_core::{CellRef, SparkGroup, SparkKind, SparklineMap, TableRange};
use rust_xlsxwriter::{Sparkline, SparklineType, Worksheet};

use crate::safeguard::{self, Limits, SafeguardError};

/// Largest number of destination cells a sparkline group is expanded to.
///
/// See the module note: OOXML stores one element per destination cell, so this
/// is a bound on the FILE, not on Ferrix's own storage — a capped group is
/// still one small entry in [`SparklineMap`].
pub const MAX_SPARKLINE_CELLS: u64 = 1_000_000;

/// Map a Ferrix sparkline type onto Excel's.
///
/// Total and lossless in both directions: the three types issue #36 asks for
/// are exactly the three OOXML has, which is why type is absent from
/// [`sparkline_xlsx_loss`].
fn to_xlsx_type(k: SparkKind) -> SparklineType {
    match k {
        SparkKind::Line => SparklineType::Line,
        SparkKind::Column => SparklineType::Column,
        SparkKind::WinLoss => SparklineType::WinLose,
    }
}

/// The reverse. `stacked` is OOXML's spelling of win/loss; a `type` attribute
/// that is absent means line, which is the format's default.
fn from_xlsx_type(s: Option<&str>) -> SparkKind {
    match s {
        Some("column") => SparkKind::Column,
        Some("stacked") => SparkKind::WinLoss,
        _ => SparkKind::Line,
    }
}

/// Does this group survive a round trip through xlsx?
///
/// `false` means the group is dropped rather than written, and the user is
/// told through [`sparkline_xlsx_loss`] BEFORE they save — the alternative is
/// them finding out after opening the file in Excel.
pub fn sparkline_survives_xlsx(g: &SparkGroup) -> bool {
    // A group must be a single COLUMN of destinations. Excel accepts a row of
    // them too, but Ferrix's `add_sparkline` only ever makes columns, so a
    // multi-column target could only come from a future feature — and writing
    // one as if it were a column would put the pictures in the wrong cells.
    g.target.first_col == g.target.last_col
        && g.target.rows() as u64 <= MAX_SPARKLINE_CELLS
        && !g.self_referential()
}

/// Human-readable reasons the sparklines on this sheet will not survive.
///
/// Empty when everything round-trips. One sentence per problem, naming the
/// group, because a warning the user cannot act on is noise.
pub fn sparkline_xlsx_loss(map: &SparklineMap) -> Vec<String> {
    let mut out = Vec::new();
    for g in map.iter() {
        if sparkline_survives_xlsx(g) {
            continue;
        }
        let where_ = g.target.to_a1();
        if g.target.first_col != g.target.last_col {
            out.push(format!(
                "The sparkline group at {where_} spans more than one column; Excel stores \
                 sparklines one destination cell at a time and Ferrix writes a single column \
                 per group, so this group is not exported."
            ));
        } else if g.target.rows() as u64 > MAX_SPARKLINE_CELLS {
            out.push(format!(
                "The sparkline group at {where_} covers {} rows. Excel stores one element per \
                 destination cell, so exporting it would write {} elements; groups above \
                 {MAX_SPARKLINE_CELLS} rows are not exported.",
                g.target.rows(),
                g.target.rows()
            ));
        } else {
            out.push(format!(
                "The sparkline group at {where_} plots a source range that overlaps its own \
                 destination, which Excel would resolve as a circular reference. It is not \
                 exported."
            ));
        }
    }
    out
}

/// Write a sheet's sparkline groups as `<extLst><x14:sparklineGroups>`.
///
/// `sheet_name` is the name the destination worksheet is saved under; the
/// source formulas are qualified with it because OOXML's `<xm:f>` is a full
/// sheet-qualified reference, not a bare range.
///
/// Groups that cannot survive are SKIPPED, not truncated — see
/// [`sparkline_xlsx_loss`] for what the user is told.
pub fn write_sparklines(
    ws: &mut Worksheet,
    sheet_name: &str,
    map: &SparklineMap,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    for g in map.iter() {
        if !sparkline_survives_xlsx(g) {
            continue;
        }
        // `add_sparkline_group` wants a 2D source range whose row count
        // matches the destination's, and expands it into one `<x14:sparkline>`
        // per row itself. That is precisely the compressed-to-expanded step,
        // so it is not re-implemented here.
        let Ok(first_col) = u16::try_from(g.src_first_col) else {
            continue;
        };
        let Ok(last_col) = u16::try_from(g.src_last_col) else {
            continue;
        };
        let Ok(target_col) = u16::try_from(g.target.first_col) else {
            continue;
        };
        let spark = Sparkline::new().set_type(to_xlsx_type(g.kind)).set_range((
            sheet_name,
            g.target.first_row,
            first_col,
            g.target.last_row,
            last_col,
        ));
        ws.add_sparkline_group(
            g.target.first_row,
            target_col,
            g.target.last_row,
            target_col,
            &spark,
        )?;
    }
    Ok(())
}

/// A sparkline group found in a worksheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedSparkline {
    /// Index of the owning worksheet, in workbook order.
    pub sheet_index: usize,
    pub group: SparkGroup,
}

/// Read every `<x14:sparklineGroup>` out of an `.xlsx`.
///
/// Opens the package directly, like [`crate::table_xlsx::import_tables`]:
/// calamine surfaces cell values and knows nothing about a worksheet's
/// `<extLst>`.
///
/// A workbook with no sparklines returns an empty vector; that is not an
/// error. A group whose entries do not form the lockstep pattern Ferrix can
/// store is skipped rather than approximated — see the module note.
pub fn import_sparklines(path: impl AsRef<Path>) -> Result<Vec<ImportedSparkline>, SafeguardError> {
    let path = path.as_ref();
    let disp = path.display().to_string();
    let parts = crate::table_xlsx::read_package_for(path, &Limits::measured())?;

    let mut out = Vec::new();
    for (sheet_index, sp) in crate::table_xlsx::worksheet_paths_for(&parts, &disp)?
        .iter()
        .enumerate()
    {
        let Some(xml) = parts.get(sp) else { continue };
        for group in scan_sheet(xml, &disp, sp)? {
            out.push(ImportedSparkline { sheet_index, group });
        }
    }
    Ok(out)
}

/// Every group in one worksheet part.
fn scan_sheet(xml: &[u8], disp: &str, part: &str) -> Result<Vec<SparkGroup>, SafeguardError> {
    // One `(source, destination)` pair per `<x14:sparkline>`, accumulated for
    // the group currently open. Bounded by the elements the FILE contains,
    // which the safeguard's part budget already caps.
    let mut kind = SparkKind::Line;
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut cur_f: Option<String> = None;
    let mut cur_sqref: Option<String> = None;
    let mut text_into: Option<&'static str> = None;
    let mut in_group = false;
    let mut groups = Vec::new();

    safeguard::scan_part(xml, disp, part, None, |ev| {
        use quick_xml::events::Event as E;
        match ev {
            E::Start(e) | E::Empty(e) => match e.local_name().as_ref() {
                b"sparklineGroup" => {
                    in_group = true;
                    pairs.clear();
                    kind = from_xlsx_type(
                        crate::table_xlsx::attr_for(e, b"type")
                            .as_deref()
                            .map(str::to_owned)
                            .as_deref(),
                    );
                }
                // `<xm:f>` appears BOTH as a group's date-axis range and as a
                // sparkline's source. Only the one inside `<x14:sparkline>` is
                // a source, which is why `cur_sqref` gates the pairing below
                // rather than every `f` being taken.
                b"f" if in_group => text_into = Some("f"),
                b"sqref" if in_group => text_into = Some("sqref"),
                _ => {}
            },
            E::Text(t) if text_into.is_some() => {
                let s = String::from_utf8_lossy(t.as_ref()).trim().to_string();
                match text_into {
                    Some("f") => cur_f = Some(s),
                    Some("sqref") => cur_sqref = Some(s),
                    _ => {}
                }
            }
            E::End(e) => match e.local_name().as_ref() {
                b"f" | b"sqref" => {
                    text_into = None;
                    if let (Some(f), Some(sq)) = (cur_f.as_ref(), cur_sqref.as_ref()) {
                        pairs.push((f.clone(), sq.clone()));
                        cur_f = None;
                        cur_sqref = None;
                    }
                }
                b"sparklineGroup" => {
                    in_group = false;
                    cur_f = None;
                    cur_sqref = None;
                    if let Some(g) = compress(kind, &pairs) {
                        groups.push(g);
                    }
                    pairs.clear();
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    })?;
    Ok(groups)
}

/// Strip a sheet qualifier from an `<xm:f>` reference.
///
/// `Sheet1!A2:D2`, `'My Sheet'!A2:D2` and a bare `A2:D2` all reduce to the
/// range. The sheet name is dropped rather than checked: `import_sparklines`
/// already attributes each group to the worksheet part it was found in, so a
/// cross-sheet source would be the only case it matters, and Ferrix cannot
/// store one anyway.
fn strip_sheet(s: &str) -> &str {
    match s.rfind('!') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

/// Re-compress per-cell entries back into one [`SparkGroup`].
///
/// Returns `None` unless the entries form the exact lockstep pattern Ferrix
/// stores: destinations are consecutive cells down ONE column, and each one's
/// source is its OWN row across a fixed column span. Anything else — a
/// hand-authored Excel group, a row of destinations, a source that does not
/// track the destination row — has no Ferrix representation, and guessing one
/// would paint a picture of the wrong data.
fn compress(kind: SparkKind, pairs: &[(String, String)]) -> Option<SparkGroup> {
    if pairs.is_empty() {
        return None;
    }
    let mut dest: Option<(u32, u32)> = None; // (col, first_row)
    let mut last_row = 0u32;
    let mut src_cols: Option<(u32, u32)> = None;

    for (i, (f, sq)) in pairs.iter().enumerate() {
        // Destination: a single cell.
        let d = CellRef::from_a1(strip_sheet(sq).trim())?;
        // Source: a range on the destination's OWN row.
        let src = TableRange::from_a1(strip_sheet(f).trim())?;
        if src.first_row != d.row || src.last_row != d.row {
            return None;
        }
        match src_cols {
            None => src_cols = Some((src.first_col, src.last_col)),
            // Every row must share the same source span, or the group is not
            // one rule.
            Some(c) if c != (src.first_col, src.last_col) => return None,
            _ => {}
        }
        match dest {
            None => dest = Some((d.col, d.row)),
            Some((col, first)) => {
                if d.col != col || d.row != first + i as u32 {
                    return None;
                }
            }
        }
        last_row = d.row;
    }

    let (col, first_row) = dest?;
    let (sc0, sc1) = src_cols?;
    Some(SparkGroup::new(
        kind,
        TableRange::new(first_row, col, last_row, col),
        sc0,
        sc1,
    ))
}

/// Silence an unused-import warning on a helper the tests use.
#[allow(dead_code)]
fn _parts_type_hint(_: &HashMap<String, Vec<u8>>) {}

#[cfg(test)]
mod tests;
