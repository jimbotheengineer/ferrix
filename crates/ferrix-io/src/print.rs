//! Streaming print export: PDF and single-file HTML.
//!
//! ## Why this exists
//!
//! Analysts email PDFs. Issue #37: without a print path, "exporting an
//! analysis" means taking a screenshot. This turns a paginated sheet — laid
//! out by [`ferrix_core::page::Paginator`] — into a PDF a colleague can open,
//! or a self-contained HTML `<table>` that pastes into an email.
//!
//! ## Constraints (the same rule as every other exporter)
//!
//! **Peak memory is bounded and independent of row count.** The paginator
//! yields one [`Page`] at a time (~24 bytes each), and this module renders one
//! page into one reused buffer and streams it straight to a `BufWriter`. A
//! 1M-row PDF never holds the document — it holds one page's worth of cell
//! strings plus a `Vec<u64>` of page-object byte offsets (bounded by the page
//! count, which the large-job guard caps long before it matters).
//!
//! ## Why the PDF is hand-written
//!
//! A PDF that carries *extractable text* needs only: a Catalog, a Pages tree,
//! one standard-14 font (Helvetica needs no embedding), and per page a `/Page`
//! object plus an uncompressed content stream of `BT … (text) Tj … ET`
//! operators. That is a few hundred lines and no dependency, versus pulling a
//! font-shaping PDF crate into a spreadsheet. The streams are left uncompressed
//! on purpose: the bytes a reader renders are the bytes the value was written
//! as, so "did cell X land on page N" is verifiable by reading the file, not by
//! trusting a rasteriser.
//!
//! Writes go to a temp sibling and are renamed into place, so a crash or a full
//! disk leaves the previous file intact rather than a truncated one.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use ferrix_core::column_name;
use ferrix_core::page::{FieldContext, Page, Paginator};

use crate::export::{ExportError, ExportSource, ExportStats};

/// Ambient values the header/footer field codes and the PDF metadata resolve
/// against. The caller owns these because `ferrix-io` does not know the
/// workbook's file name, the active sheet's name, or the wall clock.
#[derive(Clone, Debug, Default)]
pub struct PrintContext {
    pub file: String,
    pub sheet: String,
    /// Already-formatted date string for `&D` (caller's locale/format).
    pub date: String,
    /// Already-formatted time string for `&T`.
    pub time: String,
}

impl PrintContext {
    /// Build the per-page field context. `page`/`pages` are filled in per page;
    /// everything else is ambient.
    fn field_ctx(&self, page: u64, pages: u64) -> FieldContext {
        FieldContext {
            page,
            pages,
            date: self.date.clone(),
            time: self.time.clone(),
            file: self.file.clone(),
            sheet: self.sheet.clone(),
        }
    }
}

/// A writer that counts the bytes it has forwarded, so the PDF cross-reference
/// table can record each object's byte offset without buffering the document.
struct Counting<W: Write> {
    inner: W,
    count: u64,
}

impl<W: Write> Counting<W> {
    fn new(inner: W) -> Self {
        Self { inner, count: 0 }
    }
    fn offset(&self) -> u64 {
        self.count
    }
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Escape a string for a PDF literal `(...)` and drop it to WinAnsi-safe ASCII.
///
/// The standard-14 Helvetica this module uses is a single-byte font; a full
/// Unicode text layer would need an embedded CID font, which is out of scope
/// for this first slice. Non-ASCII characters are rendered as `?` in the PDF
/// (the HTML export, which is UTF-8, keeps them). `(`, `)` and `\` are the
/// three bytes that would otherwise corrupt the stream and MUST be escaped.
fn pdf_escape(s: &str, out: &mut Vec<u8>) {
    for ch in s.chars() {
        let b = ch as u32;
        let c = if (32..=126).contains(&b) {
            ch as u8
        } else {
            b'?'
        };
        match c {
            b'(' | b')' | b'\\' => {
                out.push(b'\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
}

/// Format a float in points with two decimals, no exponent, for PDF operators.
fn pt(v: f32, out: &mut String) {
    use std::fmt::Write as _;
    let _ = write!(out, "{:.2}", v);
}

/// Layout constants for the rendered grid, in PDF points.
const FONT_SIZE: f32 = 9.0;
/// Left padding of text inside a cell.
const CELL_PAD: f32 = 2.0;
/// Baseline offset from the bottom of a row, so text sits inside the cell.
const BASELINE: f32 = 3.5;
/// Fixed row height on paper (the on-screen row heights are for the grid; the
/// print grid uses a uniform readable line height so a value is never clipped
/// vertically). Documented as an approximation in the PR.
const PRINT_ROW_H: f32 = 12.0;
/// Fixed column width on paper for the first slice. Honouring per-column widths
/// end-to-end is a follow-up; a uniform width keeps columns aligned and text
/// readable, and pagination already accounts for the real widths.
const PRINT_COL_W: f32 = 64.0;

/// Options controlling a print export.
#[derive(Clone, Debug, Default)]
pub struct PrintOptions {
    /// Refuse to render more than this many pages without the caller having
    /// confirmed. `Paginator::is_large` uses the same threshold; a caller that
    /// wants to proceed anyway passes `force = true`.
    pub force: bool,
}

/// The export refused to run because the job is large and `force` was not set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LargeJobRefused {
    pub pages: u64,
}

#[derive(Debug)]
pub enum PrintError {
    Io(io::Error),
    Cancelled,
    /// Too many pages; caller must re-issue with `force = true`.
    TooLarge(LargeJobRefused),
}

impl std::fmt::Display for PrintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrintError::Io(e) => write!(f, "{e}"),
            PrintError::Cancelled => write!(f, "print cancelled"),
            PrintError::TooLarge(l) => {
                write!(f, "job is {} pages; confirm before printing", l.pages)
            }
        }
    }
}

impl std::error::Error for PrintError {}

impl From<io::Error> for PrintError {
    fn from(e: io::Error) -> Self {
        PrintError::Io(e)
    }
}

impl From<ExportError> for PrintError {
    fn from(e: ExportError) -> Self {
        match e {
            ExportError::Io(e) => PrintError::Io(e),
            ExportError::Cancelled => PrintError::Cancelled,
        }
    }
}

/// A temp path next to `path`, so the final rename stays on one filesystem.
fn temp_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".printing");
    PathBuf::from(s)
}

/// Rows repeated at the top of every page, as an inclusive range or empty.
fn repeat_row_range(p: &Paginator) -> Option<(u32, u32)> {
    p.setup().repeat_rows
}
fn repeat_col_range(p: &Paginator) -> Option<(u32, u32)> {
    p.setup().repeat_cols
}

/// The columns rendered on a page: the repeated columns first, then the page's
/// own columns with any that fall inside the repeat range removed so a value is
/// not printed twice.
fn cols_for(page: &Page, repeat: Option<(u32, u32)>) -> Vec<u32> {
    let mut cols: Vec<u32> = Vec::new();
    if let Some((a, b)) = repeat {
        cols.extend(a..=b);
    }
    for c in page.first_col..=page.last_col {
        if let Some((a, b)) = repeat {
            if c >= a && c <= b {
                continue;
            }
        }
        cols.push(c);
    }
    cols
}

/// The rows rendered on a page: repeated rows first, then the page's own rows,
/// de-duplicated the same way.
fn rows_for(page: &Page, repeat: Option<(u32, u32)>) -> Vec<u32> {
    let mut rows: Vec<u32> = Vec::new();
    if let Some((a, b)) = repeat {
        rows.extend(a..=b);
    }
    for r in page.first_row..=page.last_row {
        if let Some((a, b)) = repeat {
            if r >= a && r <= b {
                continue;
            }
        }
        rows.push(r);
    }
    rows
}

/// Build one page's content stream (the `BT … ET` text plus optional gridlines
/// and headings) into `buf`. Bounded by the cells on this one page.
#[allow(clippy::too_many_arguments)]
fn page_content<S: ExportSource + ?Sized>(
    source: &S,
    page: &Page,
    setup_gridlines: bool,
    setup_headings: bool,
    rep_rows: Option<(u32, u32)>,
    rep_cols: Option<(u32, u32)>,
    header_lines: &[String; 3],
    footer_lines: &[String; 3],
    page_w: f32,
    page_h: f32,
    margin_left: f32,
    margin_top: f32,
    margin_bottom: f32,
    buf: &mut Vec<u8>,
) {
    buf.clear();

    let rows = rows_for(page, rep_rows);
    let cols = cols_for(page, rep_cols);

    // Heading band (column letters / row numbers) costs one row + one column.
    let heading_rows = if setup_headings { 1u32 } else { 0 };
    let heading_cols = if setup_headings { 1u32 } else { 0 };

    // Content origin: below the top margin and the header band.
    let header_band = if header_lines.iter().any(|s| !s.is_empty()) {
        FONT_SIZE + 4.0
    } else {
        0.0
    };
    let top = page_h - margin_top - header_band;
    let left = margin_left;

    let mut s = String::new();

    // --- Header band, top of page ---
    write_band(
        &mut s,
        header_lines,
        page_w,
        page_h - margin_top - FONT_SIZE,
    );

    // --- Grid text ---
    s.push_str("BT\n/F1 ");
    pt(FONT_SIZE, &mut s);
    s.push_str(" Tf\n");
    let _ = buf; // s is the stream; buf is filled at the end.

    // Column x positions.
    let x_of = |ci: usize| left + (heading_cols as f32 + ci as f32) * PRINT_COL_W;
    let y_of = |ri: usize| top - (heading_rows as f32 + ri as f32 + 1.0) * PRINT_ROW_H + BASELINE;

    // Column headings (letters) along the top.
    if setup_headings {
        for (ci, c) in cols.iter().enumerate() {
            emit_text(
                &mut s,
                x_of(ci) + CELL_PAD,
                top - PRINT_ROW_H + BASELINE,
                &column_name(*c),
            );
        }
        for (ri, r) in rows.iter().enumerate() {
            emit_text(&mut s, left + CELL_PAD, y_of(ri), &(r + 1).to_string());
        }
    }

    // Cells.
    for (ri, r) in rows.iter().enumerate() {
        for (ci, c) in cols.iter().enumerate() {
            let text = source.display(ferrix_core::CellRef::new(*r, *c));
            if text.is_empty() {
                continue;
            }
            emit_text(&mut s, x_of(ci) + CELL_PAD, y_of(ri), &text);
        }
    }
    s.push_str("ET\n");

    // --- Footer band, bottom of page ---
    write_band(
        &mut s,
        footer_lines,
        page_w,
        margin_bottom - FONT_SIZE + 2.0,
    );

    // --- Gridlines ---
    if setup_gridlines {
        let n_rows = rows.len() as f32 + heading_rows as f32;
        let n_cols = cols.len() as f32 + heading_cols as f32;
        let grid_left = left;
        let grid_top = top;
        let grid_right = left + n_cols * PRINT_COL_W;
        let grid_bottom = top - n_rows * PRINT_ROW_H;
        s.push_str("0.6 w\n0.7 0.7 0.7 RG\n");
        for i in 0..=(n_rows as usize) {
            let y = grid_top - i as f32 * PRINT_ROW_H;
            line(&mut s, grid_left, y, grid_right, y);
        }
        for j in 0..=(n_cols as usize) {
            let x = grid_left + j as f32 * PRINT_COL_W;
            line(&mut s, x, grid_top, x, grid_bottom);
        }
    }

    buf.extend_from_slice(s.as_bytes());
}

/// Emit a `(text) Tj` at an absolute position via a `Td` from origin.
fn emit_text(s: &mut String, x: f32, y: f32, text: &str) {
    s.push_str("1 0 0 1 ");
    pt(x, s);
    s.push(' ');
    pt(y, s);
    s.push_str(" Tm\n(");
    let mut esc = Vec::new();
    pdf_escape(text, &mut esc);
    // Safe: pdf_escape restricts to ASCII 32-126.
    s.push_str(std::str::from_utf8(&esc).unwrap_or(""));
    s.push_str(") Tj\n");
}

/// Draw a line via `m … l S`.
fn line(s: &mut String, x0: f32, y0: f32, x1: f32, y1: f32) {
    pt(x0, s);
    s.push(' ');
    pt(y0, s);
    s.push_str(" m ");
    pt(x1, s);
    s.push(' ');
    pt(y1, s);
    s.push_str(" l S\n");
}

/// Write a three-part band (left / center / right) at baseline `y`.
fn write_band(s: &mut String, parts: &[String; 3], page_w: f32, y: f32) {
    if parts.iter().all(|p| p.is_empty()) {
        return;
    }
    s.push_str("BT\n/F1 ");
    pt(FONT_SIZE, s);
    s.push_str(" Tf\n");
    // Left.
    if !parts[0].is_empty() {
        emit_text(s, 36.0, y, &parts[0]);
    }
    // Center (approximate centering by string length; no font metrics).
    if !parts[1].is_empty() {
        let approx = parts[1].chars().count() as f32 * FONT_SIZE * 0.5;
        emit_text(s, (page_w - approx) / 2.0, y, &parts[1]);
    }
    // Right (approximate right edge).
    if !parts[2].is_empty() {
        let approx = parts[2].chars().count() as f32 * FONT_SIZE * 0.5;
        emit_text(s, page_w - 36.0 - approx, y, &parts[2]);
    }
    s.push_str("ET\n");
}

/// Stream a paginated sheet to `path` as a PDF.
///
/// One page is rendered and written at a time; peak memory is one page's
/// content stream plus a `Vec<u64>` of object offsets. `progress(done, total)`
/// pages and `should_cancel` are polled once per page.
pub fn export_pdf<S, P, C>(
    path: &Path,
    source: &S,
    paginator: &Paginator,
    ctx: &PrintContext,
    opts: &PrintOptions,
    mut progress: P,
    mut should_cancel: C,
) -> Result<ExportStats, PrintError>
where
    S: ExportSource + ?Sized,
    P: FnMut(u64, u64),
    C: FnMut() -> bool,
{
    let total_pages = paginator.page_count();
    if paginator.is_large() && !opts.force {
        return Err(PrintError::TooLarge(LargeJobRefused { pages: total_pages }));
    }

    let start = std::time::Instant::now();
    let setup = paginator.setup();
    let (page_w, page_h) = setup.paper_size();
    let m = &setup.margins;

    let tmp = temp_sibling(path);
    let cleanup = |w: Counting<BufWriter<File>>| {
        drop(w);
        let _ = std::fs::remove_file(&tmp);
    };

    let file = File::create(&tmp)?;
    let mut w = Counting::new(BufWriter::with_capacity(1 << 20, file));

    // Object numbering: 1 Catalog, 2 Pages, 3 Font. Then per page a Page object
    // and a Contents object. offsets[0] is unused (objects are 1-based).
    // A page needs 2 objects, so total objects = 3 + 2 * pages.
    let obj_count = 3 + 2 * total_pages;
    let mut offsets: Vec<u64> = vec![0; (obj_count + 1) as usize];

    w.write_all(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n")?;

    // Reused buffers.
    let mut content: Vec<u8> = Vec::with_capacity(8192);
    let mut page_ids: Vec<u64> = Vec::new();

    // --- Page + content objects, streamed one at a time ---
    let mut page_no = 0u64;
    for page in paginator.pages() {
        if should_cancel() {
            cleanup(w);
            return Err(PrintError::Cancelled);
        }
        page_no += 1;
        progress(page_no - 1, total_pages);

        let fctx = ctx.field_ctx(page.number, total_pages);
        let header_lines = setup.header.render(&fctx);
        let footer_lines = setup.footer.render(&fctx);

        page_content(
            source,
            &page,
            setup.gridlines,
            setup.headings,
            repeat_row_range(paginator),
            repeat_col_range(paginator),
            &header_lines,
            &footer_lines,
            page_w,
            page_h,
            m.left,
            m.top,
            m.bottom,
            &mut content,
        );

        // Object ids: page object then its contents object.
        let page_obj = 4 + (page_no - 1) * 2;
        let content_obj = page_obj + 1;
        page_ids.push(page_obj);

        // Page object.
        offsets[page_obj as usize] = w.offset();
        let mut hdr = String::new();
        use std::fmt::Write as _;
        let _ = write!(
            hdr,
            "{page_obj} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {:.2} {:.2}] \
             /Resources << /Font << /F1 3 0 R >> >> /Contents {content_obj} 0 R >>\nendobj\n",
            page_w, page_h
        );
        w.write_all(hdr.as_bytes())?;

        // Contents object (uncompressed stream).
        offsets[content_obj as usize] = w.offset();
        let mut cs = String::new();
        let _ = write!(
            cs,
            "{content_obj} 0 obj\n<< /Length {} >>\nstream\n",
            content.len()
        );
        w.write_all(cs.as_bytes())?;
        w.write_all(&content)?;
        w.write_all(b"\nendstream\nendobj\n")?;
    }

    // --- Catalog (1), Pages (2), Font (3) ---
    offsets[1] = w.offset();
    w.write_all(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n")?;

    offsets[2] = w.offset();
    {
        use std::fmt::Write as _;
        let mut kids = String::from("2 0 obj\n<< /Type /Pages /Count ");
        let _ = write!(kids, "{total_pages} /Kids [");
        for (i, id) in page_ids.iter().enumerate() {
            if i > 0 {
                kids.push(' ');
            }
            let _ = write!(kids, "{id} 0 R");
        }
        kids.push_str("] >>\nendobj\n");
        w.write_all(kids.as_bytes())?;
    }

    offsets[3] = w.offset();
    w.write_all(
        b"3 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica \
          /Encoding /WinAnsiEncoding >>\nendobj\n",
    )?;

    // --- Cross-reference table ---
    let xref_off = w.offset();
    {
        use std::fmt::Write as _;
        let mut xref = String::new();
        let _ = write!(xref, "xref\n0 {}\n", obj_count + 1);
        xref.push_str("0000000000 65535 f \n");
        for obj in 1..=obj_count {
            let _ = writeln!(xref, "{:010} 00000 n ", offsets[obj as usize]);
        }
        let _ = write!(
            xref,
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            obj_count + 1,
            xref_off
        );
        w.write_all(xref.as_bytes())?;
    }

    let bytes = w.offset();
    w.flush()?;
    drop(w);

    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    progress(total_pages, total_pages);

    Ok(ExportStats {
        rows: source.row_count(),
        cols: source.col_count(),
        bytes,
        millis: start.elapsed().as_millis(),
    })
}

/// Escape a string for HTML text/attribute content.
fn html_escape(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

/// Stream a paginated sheet to `path` as a single self-contained HTML file:
/// one `<table>` per page, inline styles, `page-break-after` between pages so
/// the browser's own print reproduces the pagination.
pub fn export_html<S, P, C>(
    path: &Path,
    source: &S,
    paginator: &Paginator,
    ctx: &PrintContext,
    opts: &PrintOptions,
    mut progress: P,
    mut should_cancel: C,
) -> Result<ExportStats, PrintError>
where
    S: ExportSource + ?Sized,
    P: FnMut(u64, u64),
    C: FnMut() -> bool,
{
    let total_pages = paginator.page_count();
    if paginator.is_large() && !opts.force {
        return Err(PrintError::TooLarge(LargeJobRefused { pages: total_pages }));
    }

    let start = std::time::Instant::now();
    let setup = paginator.setup();
    let tmp = temp_sibling(path);

    let file = File::create(&tmp)?;
    let mut w = Counting::new(BufWriter::with_capacity(1 << 20, file));

    let border = if setup.gridlines {
        "border:1px solid #bbb;"
    } else {
        ""
    };

    let mut head = String::new();
    head.push_str("<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\n");
    head.push_str("<title>");
    html_escape(&ctx.sheet, &mut head);
    head.push_str(
        "</title>\n</head><body style=\"font-family:Helvetica,Arial,sans-serif;font-size:9pt;\">\n",
    );
    w.write_all(head.as_bytes())?;

    let rep_rows = repeat_row_range(paginator);
    let rep_cols = repeat_col_range(paginator);

    let mut buf = String::with_capacity(8192);
    let mut page_no = 0u64;
    for page in paginator.pages() {
        if should_cancel() {
            drop(w);
            let _ = std::fs::remove_file(&tmp);
            return Err(PrintError::Cancelled);
        }
        page_no += 1;
        progress(page_no - 1, total_pages);

        let fctx = ctx.field_ctx(page.number, total_pages);
        let header_lines = setup.header.render(&fctx);
        let footer_lines = setup.footer.render(&fctx);

        let rows = rows_for(&page, rep_rows);
        let cols = cols_for(&page, rep_cols);

        buf.clear();
        // Page wrapper with a print page break after each page but the last.
        let brk = if page_no < total_pages {
            "page-break-after:always;"
        } else {
            ""
        };
        buf.push_str(&format!("<section style=\"{brk}\">\n"));

        write_html_band(&mut buf, &header_lines, "header");

        buf.push_str("<table style=\"border-collapse:collapse;\">\n");
        // Column headings.
        if setup.headings {
            buf.push_str("<tr>");
            buf.push_str(&format!("<th style=\"{border}background:#eee;\"></th>"));
            for c in &cols {
                buf.push_str(&format!("<th style=\"{border}background:#eee;\">"));
                html_escape(&column_name(*c), &mut buf);
                buf.push_str("</th>");
            }
            buf.push_str("</tr>\n");
        }
        for r in &rows {
            buf.push_str("<tr>");
            if setup.headings {
                buf.push_str(&format!("<th style=\"{border}background:#eee;\">"));
                buf.push_str(&(r + 1).to_string());
                buf.push_str("</th>");
            }
            for c in &cols {
                buf.push_str(&format!("<td style=\"{border}padding:1px 4px;\">"));
                let text = source.display(ferrix_core::CellRef::new(*r, *c));
                html_escape(&text, &mut buf);
                buf.push_str("</td>");
            }
            buf.push_str("</tr>\n");
        }
        buf.push_str("</table>\n");

        write_html_band(&mut buf, &footer_lines, "footer");
        buf.push_str("</section>\n");
        w.write_all(buf.as_bytes())?;
    }

    w.write_all(b"</body></html>\n")?;
    let bytes = w.offset();
    w.flush()?;
    drop(w);

    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    progress(total_pages, total_pages);

    Ok(ExportStats {
        rows: source.row_count(),
        cols: source.col_count(),
        bytes,
        millis: start.elapsed().as_millis(),
    })
}

fn write_html_band(buf: &mut String, parts: &[String; 3], kind: &str) {
    if parts.iter().all(|p| p.is_empty()) {
        return;
    }
    buf.push_str(&format!(
        "<div class=\"{kind}\" style=\"display:flex;justify-content:space-between;color:#444;\">"
    ));
    for p in parts {
        buf.push_str("<span>");
        html_escape(p, buf);
        buf.push_str("</span>");
    }
    buf.push_str("</div>\n");
}

#[cfg(test)]
mod tests;
