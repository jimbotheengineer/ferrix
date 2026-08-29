//! Rendering a paginated sheet to PDF and to single-file HTML.
//!
//! ## What this adds to `page.rs`
//!
//! [`ferrix_core::page::Paginator`] answers *which rows and columns land on
//! which page*. This module answers *what that page looks like* — grid lines,
//! headings, repeated header rows, conditional fills, merged cells, number
//! formats, headers and footers.
//!
//! ## Streaming
//!
//! Rendering asks the paginator for pages lazily and writes each one out
//! before building the next. Nothing accumulates: peak memory is one page's
//! content stream plus the reused text buffer. That is what makes a
//! million-row PDF possible at all, and it is why [`RenderSource`] is a
//! pull interface — the renderer asks for the cells of the band it is drawing
//! rather than being handed a document.
//!
//! ## Why the cell interface is not `ExportSource`
//!
//! CSV export needs `display(cell) -> String` and nothing else. Paper needs
//! alignment, fills, bold, merges — the visual layer CSV discards. Rather
//! than widen `ExportSource` and force every implementor to answer questions
//! it has no opinion on, [`RenderSource`] extends it with defaulted methods:
//! an existing source renders as plain black-on-white text with no changes.

use std::io::{self, BufWriter, Write};
use std::path::Path;

use ferrix_core::page::{
    measure_cols, measure_rows, FieldContext, HeaderFooter, Page, PageSetup, Paginator, Points,
    DEFAULT_COL_WIDTH, DEFAULT_ROW_HEIGHT,
};
use ferrix_core::{column_name, CellRef, ColSizes, HAlign, Rgb, RowSizes, TableRange};

use crate::export::{ExportError, ExportSource};
use crate::pdf::{self, Color, Content, PdfDoc, FONT_BOLD, FONT_REGULAR};

/// A temp path next to `path`, so the final rename stays on one filesystem
/// (a cross-device rename would fall back to a copy and defeat atomicity).
fn temp_sibling(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".rendering");
    std::path::PathBuf::from(s)
}

/// Everything the renderer needs about one cell beyond its text.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct CellPaint {
    pub fill: Option<Rgb>,
    pub text_color: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub align: HAlign,
}

/// A sheet that can be drawn onto paper.
///
/// Every method has a default, so a plain [`ExportSource`] is already a valid
/// `RenderSource` — it just prints as unstyled text. Implementors override
/// only what they actually model.
pub trait RenderSource: ExportSource {
    /// Visual attributes of a cell, after conditional formatting.
    fn paint(&self, _cell: CellRef) -> CellPaint {
        CellPaint::default()
    }

    /// The merged region covering `cell`, if any.
    fn merge_at(&self, _cell: CellRef) -> Option<TableRange> {
        None
    }

    /// Whether `cell` is covered by a merge but is not its anchor. Covered
    /// cells draw nothing: the anchor paints across the whole region.
    fn is_merge_covered(&self, cell: CellRef) -> bool {
        match self.merge_at(cell) {
            Some(r) => !(r.first_row == cell.row && r.first_col == cell.col),
            None => false,
        }
    }

    /// Name shown in `&A` and in the HTML title.
    fn sheet_name(&self) -> String {
        "Sheet1".to_string()
    }
}

/// Rendering knobs that are not part of [`PageSetup`].
#[derive(Clone, Debug)]
pub struct RenderOptions {
    /// Print area, inclusive and 0-based. `None` prints the used extent.
    pub print_area: Option<TableRange>,
    /// Base font size in points.
    pub font_size: f32,
    /// Values for `&D`, `&T` and `&F`. Injected rather than read from the
    /// clock so a rendered file is reproducible and testable.
    pub fields: FieldContext,
    /// Render the first row as a bold header band. Independent of
    /// `PageSetup::repeat_rows`, which repeats *sheet* rows.
    pub column_headers: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            print_area: None,
            font_size: 9.0,
            fields: FieldContext::default(),
            column_headers: true,
        }
    }
}

/// Why a render refused to run or could not finish.
#[derive(Debug)]
pub enum RenderError {
    Io(io::Error),
    Cancelled,
    /// The job exceeds [`ferrix_core::page::LARGE_JOB_PAGES`] and the caller
    /// did not confirm. Carries the count so the prompt can quote it.
    ///
    /// This is a *refusal*, not a warning printed after the fact: by the time
    /// a 200,000-page file exists the user has already lost the disk space
    /// and the wait.
    TooManyPages(u64),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Io(e) => write!(f, "{e}"),
            RenderError::Cancelled => write!(f, "render cancelled"),
            RenderError::TooManyPages(n) => {
                write!(f, "this would produce {n} pages; confirm before rendering")
            }
        }
    }
}

impl std::error::Error for RenderError {}

impl From<io::Error> for RenderError {
    fn from(e: io::Error) -> Self {
        RenderError::Io(e)
    }
}

impl From<RenderError> for ExportError {
    fn from(e: RenderError) -> Self {
        match e {
            RenderError::Io(e) => ExportError::Io(e),
            RenderError::Cancelled => ExportError::Cancelled,
            RenderError::TooManyPages(n) => ExportError::Io(io::Error::other(format!(
                "refusing to render {n} pages without confirmation"
            ))),
        }
    }
}

/// What a render produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderStats {
    pub pages: u64,
    pub bytes: u64,
    pub rows: u64,
}

/// Resolve the extent to print: the print area if set, else the used range.
fn extent<S: RenderSource + ?Sized>(sheet: &S, opts: &RenderOptions) -> ((u32, u32), (u32, u32)) {
    match opts.print_area {
        Some(r) => ((r.first_row, r.last_row), (r.first_col, r.last_col)),
        None => {
            let rows = sheet.row_count().max(1) as u32 - 1;
            let cols = sheet.col_count().max(1) as u32 - 1;
            ((0, rows), (0, cols))
        }
    }
}

/// Build the paginator for a render, so callers can ask
/// [`Paginator::page_count`] *before* committing to the export.
pub fn plan<S: RenderSource + ?Sized>(
    sheet: &S,
    setup: &PageSetup,
    opts: &RenderOptions,
    row_sizes: &RowSizes,
    col_sizes: &ColSizes,
) -> Paginator {
    let (rows, cols) = extent(sheet, opts);
    Paginator::new(setup.clone(), rows, cols, row_sizes, col_sizes)
}

fn rgb_to_color(c: Rgb) -> Color {
    Color(c.0, c.1, c.2)
}

/// Points of vertical space a header or footer band occupies when non-empty.
const BAND_H: Points = 18.0;
/// Grid line colour, matching the on-screen grid's light grey.
const GRID: Color = Color(200, 200, 200);
/// Padding inside a cell, each side.
const PAD: Points = 3.0;

/// Geometry of one rendered page, shared by the PDF and HTML paths so the two
/// cannot drift into disagreeing about where a cell sits.
struct Layout {
    /// x offset of each column in the band, and its width.
    cols: Vec<(u32, Points, Points)>,
    /// y offset of each row in the band, and its height.
    rows: Vec<(u32, Points, Points)>,
    /// Rows repeated from `repeat_rows`, drawn above the band's own rows.
    repeat: Vec<(u32, Points, Points)>,
    height: Points,
}

impl Layout {
    fn build(
        page: &Page,
        setup: &PageSetup,
        row_sizes: &RowSizes,
        col_sizes: &ColSizes,
        origin: (Points, Points),
    ) -> Layout {
        let mut cols = Vec::new();
        let mut x = origin.0;
        for c in page.first_col..=page.last_col {
            if col_sizes.is_hidden(c) {
                continue;
            }
            let w = col_sizes.width_of(c).unwrap_or(DEFAULT_COL_WIDTH);
            cols.push((c, x, w));
            x += w;
        }

        let mut y = origin.1;
        let mut repeat = Vec::new();
        if let Some((a, b)) = setup.repeat_rows {
            // Skip repetition on the page that already contains those rows,
            // or the header would print twice.
            let already = page.first_row <= a;
            if !already {
                for r in a..=b {
                    if row_sizes.is_hidden(r) {
                        continue;
                    }
                    let h = row_sizes.height_or(r, DEFAULT_ROW_HEIGHT);
                    repeat.push((r, y, h));
                    y += h;
                }
            }
        }

        let mut rows = Vec::new();
        for r in page.first_row..=page.last_row {
            if row_sizes.is_hidden(r) {
                continue;
            }
            let h = row_sizes.height_or(r, DEFAULT_ROW_HEIGHT);
            rows.push((r, y, h));
            y += h;
            if r == u32::MAX {
                break;
            }
        }

        Layout {
            height: y - origin.1,
            cols,
            rows,
            repeat,
        }
    }

    /// Every drawn row, repeated header rows first.
    fn all_rows(&self) -> impl Iterator<Item = &(u32, Points, Points)> {
        self.repeat.iter().chain(self.rows.iter())
    }
}

/// Where a cell's text starts, given its alignment and measured width.
fn text_x(align: HAlign, cell_x: Points, cell_w: Points, text_w: Points, numeric: bool) -> Points {
    let effective = match align {
        // `General` is the type-driven default the grid already applies:
        // numbers right, everything else left. Paper must match the screen.
        HAlign::General => {
            if numeric {
                HAlign::Right
            } else {
                HAlign::Left
            }
        }
        HAlign::Justify => HAlign::Left,
        other => other,
    };
    match effective {
        HAlign::Right => cell_x + cell_w - PAD - text_w,
        HAlign::Center => cell_x + (cell_w - text_w) / 2.0,
        _ => cell_x + PAD,
    }
}

/// Does this text read as a number? Used only to pick `General` alignment.
fn looks_numeric(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    let t = t.strip_prefix(['-', '+', '$']).unwrap_or(t);
    let t = t.strip_suffix('%').unwrap_or(t);
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
        && t.chars().any(|c| c.is_ascii_digit())
}

/// Render `sheet` to a PDF at `path`.
///
/// `confirm_large` is consulted only when the job exceeds
/// [`ferrix_core::page::LARGE_JOB_PAGES`]; returning false aborts before any
/// bytes are written.
#[allow(clippy::too_many_arguments)]
pub fn render_pdf<S, P, C>(
    path: &Path,
    sheet: &S,
    setup: &PageSetup,
    opts: &RenderOptions,
    row_sizes: &RowSizes,
    col_sizes: &ColSizes,
    confirm_large: bool,
    mut progress: P,
    mut should_cancel: C,
) -> Result<RenderStats, RenderError>
where
    S: RenderSource + ?Sized,
    P: FnMut(u64, u64),
    C: FnMut() -> bool,
{
    let paginator = plan(sheet, setup, opts, row_sizes, col_sizes);
    let total = paginator.page_count();
    if paginator.is_large() && !confirm_large {
        return Err(RenderError::TooManyPages(total));
    }

    let media = setup.paper_size();
    // Write to a temp sibling and rename into place at the end. A crash, a full
    // disk, or a cancel then leaves the PREVIOUS file intact rather than a
    // truncated or deleted one — the failure mode that silently destroys the
    // export the user was replacing.
    let tmp = temp_sibling(path);
    let file = std::fs::File::create(&tmp)?;
    let mut doc = PdfDoc::new(BufWriter::with_capacity(1 << 20, file), media)?;

    let printable = setup.printable();
    let origin = (setup.margins.left, setup.margins.top);
    let header_band = if setup.header.is_empty() { 0.0 } else { BAND_H };

    // ONE content buffer for the whole document. This is the allocation that
    // would otherwise track page count.
    let mut content = Content::new(media.1);
    let mut rows_drawn: u64 = 0;

    for (i, page) in paginator.pages().enumerate() {
        if i % 16 == 0 {
            if should_cancel() {
                drop(doc);
                let _ = std::fs::remove_file(&tmp);
                return Err(RenderError::Cancelled);
            }
            progress(i as u64, total);
        }
        content.reset(media.1);
        let body_origin = (origin.0, origin.1 + header_band);
        let layout = Layout::build(&page, setup, row_sizes, col_sizes, body_origin);

        draw_page(
            &mut content,
            sheet,
            setup,
            opts,
            &layout,
            &page,
            total,
            printable,
            origin,
        );
        rows_drawn += layout.rows.len() as u64;
        doc.add_page(&content)?;
    }

    let bytes = doc.finish()?;
    // Windows will not rename onto an existing file; remove the old one first.
    // Only reached after a fully-written temp, so the destination is replaced
    // atomically from the reader's point of view.
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    progress(total, total);
    Ok(RenderStats {
        pages: total,
        bytes,
        rows: rows_drawn,
    })
}

#[allow(clippy::too_many_arguments)]
fn draw_page<S: RenderSource + ?Sized>(
    c: &mut Content,
    sheet: &S,
    setup: &PageSetup,
    opts: &RenderOptions,
    layout: &Layout,
    page: &Page,
    total: u64,
    printable: (Points, Points),
    origin: (Points, Points),
) {
    let size = opts.font_size;
    let ctx = FieldContext {
        page: page.number,
        pages: total,
        sheet: sheet.sheet_name(),
        ..opts.fields.clone()
    };

    // Fills first, then grid, then text: a later fill would hide the text
    // under it, which is exactly the bug that makes a "styled" export
    // useless.
    for (r, y, h) in layout.all_rows() {
        for (col, x, w) in &layout.cols {
            let cell = CellRef::new(*r, *col);
            if sheet.is_merge_covered(cell) {
                continue;
            }
            let p = sheet.paint(cell);
            if let Some(fill) = p.fill {
                let (fw, fh) = merged_span(sheet, cell, layout, *x, *y, *w, *h);
                c.fill_rect(*x, *y, fw, fh, rgb_to_color(fill));
            }
        }
    }

    if setup.gridlines {
        for (_, x, _) in &layout.cols {
            c.line(*x, origin.1, *x, origin.1 + layout.height, 0.5, GRID);
        }
        let right = layout
            .cols
            .last()
            .map(|(_, x, w)| x + w)
            .unwrap_or(origin.0);
        c.line(right, origin.1, right, origin.1 + layout.height, 0.5, GRID);
        let left = layout.cols.first().map(|(_, x, _)| *x).unwrap_or(origin.0);
        for (_, y, _) in layout.all_rows() {
            c.line(left, *y, right, *y, 0.5, GRID);
        }
        let bottom = layout
            .all_rows()
            .last()
            .map(|(_, y, h)| y + h)
            .unwrap_or(origin.1);
        c.line(left, bottom, right, bottom, 0.5, GRID);
    }

    for (r, y, h) in layout.all_rows() {
        for (col, x, w) in &layout.cols {
            let cell = CellRef::new(*r, *col);
            if sheet.is_merge_covered(cell) {
                continue;
            }
            let text = sheet.display(cell);
            if text.is_empty() {
                continue;
            }
            let p = sheet.paint(cell);
            let (span_w, _) = merged_span(sheet, cell, layout, *x, *y, *w, *h);
            let font = if p.bold { FONT_BOLD } else { FONT_REGULAR };
            let tw = pdf::text_width(&text, size, p.bold);
            let tx = text_x(p.align, *x, span_w, tw, looks_numeric(&text));
            let baseline = y + h - (h - size * 0.72) / 2.0 - size * 0.18;
            let color = p.text_color.map(rgb_to_color).unwrap_or(Color::BLACK);
            // Clip to the cell so an overlong value cannot bleed into its
            // neighbour, matching the grid.
            c.clipped(*x, *y, span_w, *h, |c| {
                c.text(tx, baseline, size, font, color, &text);
            });
        }
    }

    draw_band(
        c,
        &setup.header,
        &ctx,
        origin.1 + BAND_H * 0.7,
        origin,
        printable,
        size,
    );
    draw_band(
        c,
        &setup.footer,
        &ctx,
        origin.1 + printable.1 - BAND_H * 0.3,
        origin,
        printable,
        size,
    );
}

/// How far a merged cell's anchor extends within this page's layout.
///
/// Clamped to the page: a merge that straddles a page break paints only the
/// part on this page, which is what Excel does and what keeps the drawing
/// inside the printable area.
fn merged_span<S: RenderSource + ?Sized>(
    sheet: &S,
    cell: CellRef,
    layout: &Layout,
    x: Points,
    y: Points,
    w: Points,
    h: Points,
) -> (Points, Points) {
    let Some(range) = sheet.merge_at(cell) else {
        return (w, h);
    };
    let right = layout
        .cols
        .iter()
        .filter(|(c, _, _)| *c >= cell.col && *c <= range.last_col)
        .map(|(_, cx, cw)| cx + cw)
        .fold(x + w, f32::max);
    let bottom = layout
        .all_rows()
        .filter(|(r, _, _)| *r >= cell.row && *r <= range.last_row)
        .map(|(_, ry, rh)| ry + rh)
        .fold(y + h, f32::max);
    (right - x, bottom - y)
}

fn draw_band(
    c: &mut Content,
    hf: &HeaderFooter,
    ctx: &FieldContext,
    baseline: Points,
    origin: (Points, Points),
    printable: (Points, Points),
    size: f32,
) {
    if hf.is_empty() {
        return;
    }
    let [left, center, right] = hf.render(ctx);
    if !left.is_empty() {
        c.text(origin.0, baseline, size, FONT_REGULAR, Color::BLACK, &left);
    }
    if !center.is_empty() {
        let w = pdf::text_width(&center, size, false);
        let x = origin.0 + (printable.0 - w) / 2.0;
        c.text(x, baseline, size, FONT_REGULAR, Color::BLACK, &center);
    }
    if !right.is_empty() {
        let w = pdf::text_width(&right, size, false);
        let x = origin.0 + printable.0 - w;
        c.text(x, baseline, size, FONT_REGULAR, Color::BLACK, &right);
    }
}

// ------------------------------------------------------------------ HTML ==

/// Escape text for HTML body content and attribute values.
fn html_escape(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

/// Render `sheet` to a single self-contained HTML file.
///
/// One `<table>` per page, with inline styles and no external references, so
/// the file can be emailed as-is. Written streaming for the same reason the
/// PDF is: the row buffer is reused and nothing accumulates.
#[allow(clippy::too_many_arguments)]
pub fn render_html<S, P, C>(
    path: &Path,
    sheet: &S,
    setup: &PageSetup,
    opts: &RenderOptions,
    row_sizes: &RowSizes,
    col_sizes: &ColSizes,
    confirm_large: bool,
    mut progress: P,
    mut should_cancel: C,
) -> Result<RenderStats, RenderError>
where
    S: RenderSource + ?Sized,
    P: FnMut(u64, u64),
    C: FnMut() -> bool,
{
    let paginator = plan(sheet, setup, opts, row_sizes, col_sizes);
    let total = paginator.page_count();
    if paginator.is_large() && !confirm_large {
        return Err(RenderError::TooManyPages(total));
    }

    // Temp sibling + rename, so a cancel or crash preserves the prior file.
    let tmp = temp_sibling(path);
    let file = std::fs::File::create(&tmp)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    let mut written: u64 = 0;
    let mut buf = String::with_capacity(64 * 1024);

    let name = sheet.sheet_name();
    buf.push_str("<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>");
    html_escape(&mut buf, &name);
    buf.push_str(
        "</title><style>\
body{font-family:Helvetica,Arial,sans-serif;font-size:9pt;margin:0;background:#f4f4f4}\
.page{background:#fff;margin:12px auto;padding:16px;box-shadow:0 1px 4px rgba(0,0,0,.2);\
page-break-after:always}\
table{border-collapse:collapse}\
td{padding:1px 4px;white-space:nowrap;overflow:hidden}\
.hf{display:flex;justify-content:space-between;color:#555;font-size:8pt;margin:4px 0}\
</style></head><body>\n",
    );
    w.write_all(buf.as_bytes())?;
    written += buf.len() as u64;

    let border = if setup.gridlines {
        "border:1px solid #c8c8c8;"
    } else {
        ""
    };
    let mut rows_drawn: u64 = 0;

    for (i, page) in paginator.pages().enumerate() {
        if i % 16 == 0 {
            if should_cancel() {
                drop(w);
                let _ = std::fs::remove_file(&tmp);
                return Err(RenderError::Cancelled);
            }
            progress(i as u64, total);
        }
        let layout = Layout::build(&page, setup, row_sizes, col_sizes, (0.0, 0.0));
        let ctx = FieldContext {
            page: page.number,
            pages: total,
            sheet: name.clone(),
            ..opts.fields.clone()
        };

        buf.clear();
        let (pw, _) = setup.paper_size();
        buf.push_str(&format!(
            "<div class=\"page\" style=\"width:{:.0}pt\">\n",
            pw - setup.margins.left - setup.margins.right
        ));
        html_band(&mut buf, &setup.header, &ctx);
        buf.push_str("<table>\n");

        for (r, _, h) in layout.all_rows() {
            buf.push_str(&format!("<tr style=\"height:{h:.0}pt\">"));
            for (col, _, cw) in &layout.cols {
                let cell = CellRef::new(*r, *col);
                if sheet.is_merge_covered(cell) {
                    continue;
                }
                let p = sheet.paint(cell);
                let text = sheet.display(cell);
                let mut span = String::new();
                if let Some(range) = sheet.merge_at(cell) {
                    let cs = (range.last_col - range.first_col + 1).min(
                        layout
                            .cols
                            .last()
                            .map(|(c, _, _)| c - cell.col + 1)
                            .unwrap_or(1),
                    );
                    let rs = range.last_row - range.first_row + 1;
                    if cs > 1 {
                        span.push_str(&format!(" colspan=\"{cs}\""));
                    }
                    if rs > 1 {
                        span.push_str(&format!(" rowspan=\"{rs}\""));
                    }
                }
                let align = match p.align {
                    HAlign::Right => "right",
                    HAlign::Center => "center",
                    HAlign::Left | HAlign::Justify => "left",
                    HAlign::General => {
                        if looks_numeric(&text) {
                            "right"
                        } else {
                            "left"
                        }
                    }
                };
                buf.push_str(&format!(
                    "<td{span} style=\"{border}width:{cw:.0}pt;text-align:{align};"
                ));
                if let Some(f) = p.fill {
                    buf.push_str(&format!("background:#{:02x}{:02x}{:02x};", f.0, f.1, f.2));
                }
                if let Some(t) = p.text_color {
                    buf.push_str(&format!("color:#{:02x}{:02x}{:02x};", t.0, t.1, t.2));
                }
                if p.bold {
                    buf.push_str("font-weight:bold;");
                }
                if p.italic {
                    buf.push_str("font-style:italic;");
                }
                buf.push_str("\">");
                html_escape(&mut buf, &text);
                buf.push_str("</td>");
            }
            buf.push_str("</tr>\n");
        }
        buf.push_str("</table>\n");
        html_band(&mut buf, &setup.footer, &ctx);
        buf.push_str("</div>\n");

        w.write_all(buf.as_bytes())?;
        written += buf.len() as u64;
        rows_drawn += layout.rows.len() as u64;
    }

    let tail = "</body></html>\n";
    w.write_all(tail.as_bytes())?;
    written += tail.len() as u64;
    w.flush()?;
    drop(w);
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    progress(total, total);

    Ok(RenderStats {
        pages: total,
        bytes: written,
        rows: rows_drawn,
    })
}

fn html_band(buf: &mut String, hf: &HeaderFooter, ctx: &FieldContext) {
    if hf.is_empty() {
        return;
    }
    let [l, c, r] = hf.render(ctx);
    buf.push_str("<div class=\"hf\"><span>");
    html_escape(buf, &l);
    buf.push_str("</span><span>");
    html_escape(buf, &c);
    buf.push_str("</span><span>");
    html_escape(buf, &r);
    buf.push_str("</span></div>\n");
}

/// A spreadsheet-style column letter, re-exported so UI code building a
/// print preview does not have to reach into `ferrix-core` for it.
pub fn header_letter(col: u32) -> String {
    column_name(col)
}

/// Total content size of a print job, for the fit-to-page maths.
pub fn content_size(
    rows: (u32, u32),
    cols: (u32, u32),
    row_sizes: &RowSizes,
    col_sizes: &ColSizes,
) -> (Points, Points) {
    (
        measure_cols(col_sizes, cols.0, cols.1),
        measure_rows(row_sizes, rows.0, rows.1),
    )
}

#[cfg(test)]
mod tests;
