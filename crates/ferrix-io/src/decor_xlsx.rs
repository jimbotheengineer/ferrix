//! Cell decoration on the xlsx side: borders, alignment, wrap, rotation.
//!
//! Issue #28. This module answers two questions and nothing else:
//!
//! 1. **How does a [`CellDecor`] become an OOXML cell format?**
//!    [`decor_format`] builds the `rust_xlsxwriter` [`Format`] that carries a
//!    decoration into `xl/styles.xml`, and [`write_decor`] applies it to the
//!    cells a scope covers.
//! 2. **What does NOT survive, and how is the user told?**
//!    [`decor_survives_xlsx`] and [`decor_xlsx_loss`], in exactly the shape
//!    `rule_survives_xlsx` established — because the alternative is the user
//!    discovering it after opening the file in Excel.
//!
//! ## Why the writes are bounded
//!
//! A decoration is stored per COLUMN or per RANGE, and applying one to a
//! 200M-row column must not write 200M cell formats. `rust_xlsxwriter` has a
//! real column-level format (`set_column_format`), which is one `<col>`
//! element whatever the row count — so a column-scope decoration is written
//! as ONE record. A range-scope decoration has no such shortcut in the format
//! (OOXML genuinely stores alignment per cell), so it is written per cell and
//! **capped**: see [`MAX_RANGE_CELLS`]. A range past the cap is reported as
//! lossy rather than expanded, because expanding it is how an export turns
//! into an out-of-memory kill.

use ferrix_core::{
    Border, BorderStyle, CellDecor, Diagonal, HAlign, SheetFormat, Side, TableRange, VAlign,
};
use rust_xlsxwriter::{Format, FormatAlign, FormatBorder, FormatDiagonalBorder, Worksheet};

use crate::table_xlsx::to_color;

/// Largest number of cells a RANGE-scope decoration will be expanded to.
///
/// OOXML has no range-level alignment: `wrapText` and friends live on the
/// cell format, so a decorated rectangle must be written cell by cell. That
/// is fine for the rectangles users actually select and catastrophic for a
/// 200M-row one, so the expansion is capped and anything past it is REPORTED
/// through [`decor_xlsx_loss`] instead of being silently truncated or, worse,
/// attempted.
///
/// A column-scope decoration is not affected: it goes out as one `<col>`
/// record however tall the column is.
pub const MAX_RANGE_CELLS: u64 = 1_000_000;

/// Map a Ferrix border style onto Excel's.
///
/// Every one of the seven styles issue #28 lists has an exact OOXML
/// counterpart, so this is total and lossless — which is why border style is
/// absent from [`decor_xlsx_loss`].
fn border_style(s: BorderStyle) -> FormatBorder {
    match s {
        BorderStyle::None => FormatBorder::None,
        BorderStyle::Thin => FormatBorder::Thin,
        BorderStyle::Medium => FormatBorder::Medium,
        BorderStyle::Thick => FormatBorder::Thick,
        BorderStyle::Double => FormatBorder::Double,
        BorderStyle::Dotted => FormatBorder::Dotted,
        BorderStyle::Dashed => FormatBorder::Dashed,
    }
}

/// The `Format` an OOXML cell needs to carry `d`.
///
/// Returns `None` when the decoration says nothing, so an undecorated sheet
/// adds no style records at all.
pub fn decor_format(d: &CellDecor) -> Option<Format> {
    if d.is_empty() {
        return None;
    }
    let mut f = Format::new();

    // --- borders ---
    //
    // Each side is set independently, matching the model: a decoration that
    // sets only the bottom must not draw the other three.
    let side = |f: Format, s: Side, b: Border| -> Format {
        let st = border_style(b.style);
        let f = match s {
            Side::Left => f.set_border_left(st),
            Side::Right => f.set_border_right(st),
            Side::Top => f.set_border_top(st),
            Side::Bottom => f.set_border_bottom(st),
        };
        match b.color {
            Some(c) => match s {
                Side::Left => f.set_border_left_color(to_color(c)),
                Side::Right => f.set_border_right_color(to_color(c)),
                Side::Top => f.set_border_top_color(to_color(c)),
                Side::Bottom => f.set_border_bottom_color(to_color(c)),
            },
            None => f,
        }
    };
    for s in Side::ALL {
        if let Some(b) = d.borders[s.index()] {
            f = side(f, s, b);
        }
    }
    if let Some((b, dir)) = d.diagonal {
        f = f
            .set_border_diagonal(border_style(b.style))
            .set_border_diagonal_type(match dir {
                Diagonal::Up => FormatDiagonalBorder::BorderUp,
                Diagonal::Down => FormatDiagonalBorder::BorderDown,
                Diagonal::Both => FormatDiagonalBorder::BorderUpDown,
            });
        if let Some(c) = b.color {
            f = f.set_border_diagonal_color(to_color(c));
        }
    }

    // --- alignment ---
    //
    // `HAlign::General` writes NO horizontal alignment, which is what
    // "general" means in OOXML too — an absent attribute, not a value.
    match d.h_align {
        Some(HAlign::Left) => f = f.set_align(FormatAlign::Left),
        Some(HAlign::Center) => f = f.set_align(FormatAlign::Center),
        Some(HAlign::Right) => f = f.set_align(FormatAlign::Right),
        Some(HAlign::Justify) => f = f.set_align(FormatAlign::Justify),
        Some(HAlign::General) | None => {}
    }
    match d.v_align {
        Some(VAlign::Top) => f = f.set_align(FormatAlign::Top),
        Some(VAlign::Center) => f = f.set_align(FormatAlign::VerticalCenter),
        Some(VAlign::Bottom) => f = f.set_align(FormatAlign::Bottom),
        None => {}
    }
    if let Some(i) = d.indent {
        f = f.set_indent(i);
    }
    if d.wrap == Some(true) {
        f = f.set_text_wrap();
    }
    if d.shrink == Some(true) {
        f = f.set_shrink();
    }
    // Rotation: OOXML's `textRotation` is 0..=90 for counter-clockwise and
    // 91..=180 for clockwise, where 91 is -1 degree. `rust_xlsxwriter` takes
    // the signed -90..=90 form directly and does that encoding itself, which
    // is the same convention the model uses — so this is a pass-through
    // rather than a re-derivation that could disagree.
    if let Some(r) = d.rotation.filter(|r| *r != 0) {
        f = f.set_rotation(r);
    }
    Some(f)
}

/// Is every part of `d` representable in xlsx?
///
/// The [`crate::table_xlsx::rule_survives_xlsx`] convention: the editor asks
/// BEFORE exporting so the user learns in Ferrix that something will not make
/// the trip, rather than discovering it in Excel.
///
/// Borders (all seven styles, per side, with colour and diagonal), both
/// alignments, indent, wrap, shrink and rotation all map exactly, so a plain
/// decoration always survives. The exceptions are enumerated in
/// [`decor_xlsx_loss`].
pub fn decor_survives_xlsx(d: &CellDecor) -> bool {
    decor_xlsx_loss(d).is_empty()
}

/// Everything about `d` that xlsx cannot carry, one human-readable line each.
///
/// Empty means the decoration round-trips exactly. Deliberately a list rather
/// than a bool, because "your formatting will change" is not actionable and
/// "indent is dropped on rotated text, because Excel ignores it there" is.
pub fn decor_xlsx_loss(d: &CellDecor) -> Vec<String> {
    let mut out = Vec::new();

    // Excel ignores `indent` on any cell whose text is rotated — the
    // attribute is written and then has no effect, which is worse than a
    // refusal because the file claims something Excel will not do.
    if d.indent.is_some_and(|i| i > 0) && d.rotation_deg() != 0 {
        out.push(
            "Indent is ignored by Excel on rotated text, so the indent will not appear".into(),
        );
    }
    // Wrap and shrink are mutually exclusive in Excel too, and Excel resolves
    // it silently in wrap's favour. Ferrix resolves it the same way (see
    // `CellDecor::shrinks`), so the file is honest — but the user asked for
    // both and only gets one, and should be told which.
    if d.wrap == Some(true) && d.shrink == Some(true) {
        out.push("Wrap text and shrink to fit cannot both apply; wrap wins".into());
    }
    // `justify` on a single-line cell has no visible effect in either
    // application. The value is preserved, but the user should know they will
    // not see it until the cell also wraps.
    if d.h_align == Some(HAlign::Justify) && d.wrap != Some(true) {
        out.push("Justify alignment has no visible effect without wrap text".into());
    }
    out
}

/// Apply every decoration on `fmt` to the worksheet.
///
/// `rows` and `cols` bound the sheet's real extent, so a decoration is never
/// written outside the cells that exist — an alignment on a cell with no
/// value produces a `<c>` element Excel did not previously have.
///
/// Returns the ranges that were too large to expand; the caller reports them.
pub fn write_decor(
    ws: &mut Worksheet,
    fmt: &SheetFormat,
    rows: usize,
    cols: usize,
) -> Result<Vec<TableRange>, rust_xlsxwriter::XlsxError> {
    let mut skipped = Vec::new();
    if !fmt.has_decor() {
        return Ok(skipped);
    }

    // --- column scope: ONE record per column, whatever the row count ---
    //
    // This is what keeps a 200M-row decorated column a few bytes in the file
    // rather than 200M cell formats. `set_column_format` writes the `<col>`
    // element's `style` attribute, which is exactly the same mechanism Excel
    // itself uses for "format this whole column".
    for (col, cf) in fmt.columns() {
        if cf.decor.is_empty() || col as usize > crate::xlsx::XLSX_MAX_COLS {
            continue;
        }
        if let Some(f) = decor_format(&cf.decor) {
            ws.set_column_format(col as u16, &f)?;
        }
    }

    // --- range scope: per cell, capped ---
    for rf in fmt.ranges() {
        if rf.decor.is_empty() {
            continue;
        }
        let r0 = rf.range.first_row;
        let r1 = rf.range.last_row.min(rows.saturating_sub(1) as u32);
        let c0 = rf.range.first_col;
        let c1 = rf.range.last_col.min(cols.saturating_sub(1) as u32);
        if r0 > r1 || c0 > c1 {
            continue;
        }
        let cells = (r1 - r0 + 1) as u64 * (c1 - c0 + 1) as u64;
        if cells > MAX_RANGE_CELLS {
            // Reported, never attempted. Expanding this is how the export
            // process gets killed.
            skipped.push(rf.range);
            continue;
        }
        let Some(f) = decor_format(&rf.decor) else {
            continue;
        };
        for r in r0..=r1 {
            for c in c0..=c1 {
                ws.set_cell_format(r, c as u16, &f)?;
            }
        }
    }

    // --- per-cell overrides ---
    for (cell, ov) in fmt.overrides() {
        if ov.decor.is_empty()
            || cell.row as usize >= rows.max(1)
            || cell.col as usize > crate::xlsx::XLSX_MAX_COLS
        {
            continue;
        }
        if let Some(f) = decor_format(&ov.decor) {
            ws.set_cell_format(cell.row, cell.col as u16, &f)?;
        }
    }
    Ok(skipped)
}

#[cfg(test)]
mod tests;
