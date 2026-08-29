//! A minimal, streaming PDF writer.
//!
//! ## Why hand-rolled
//!
//! Ferrix needs exactly one thing from PDF: put text and filled rectangles on
//! a page, one page at a time, without holding the document. Every general
//! PDF crate in the ecosystem builds a document object graph in memory and
//! serialises it at the end — which is precisely the shape the scale
//! invariant forbids. A 4,000-page export must not cost 4,000 pages of RAM.
//!
//! So this writer emits each object to the sink the moment it is complete and
//! never revisits it. Peak memory is **one page's content stream** plus the
//! cross-reference table.
//!
//! ## The one thing that grows
//!
//! PDF's cross-reference table must list the byte offset of every object, and
//! the page tree must list every page. That is inherently O(pages): about 12
//! bytes per page here. A 1,000-page job — the point at which
//! [`ferrix_core::page::LARGE_JOB_PAGES`] makes the caller confirm — costs
//! ~12 KB. This is the only structure in the export path that grows with job
//! size, it is forced by the file format, and the large-job warning exists
//! partly to bound it.
//!
//! ## Scope
//!
//! Base-14 Helvetica only, no embedding, WinAnsi encoding, uncompressed
//! content streams. Uncompressed is deliberate: it keeps the crate
//! dependency-free and keeps the output readable by `pdftotext` and by the
//! test suite's own parser. Files are larger than they would be with Flate;
//! that is a trade we can revisit behind a flag.

use std::io::{self, Write};

/// Font slots the writer registers. The indices match the `/F1`..`/F4` names
/// used in content streams.
pub const FONT_REGULAR: u8 = 1;
pub const FONT_BOLD: u8 = 2;
pub const FONT_ITALIC: u8 = 3;
pub const FONT_BOLD_ITALIC: u8 = 4;

/// Object ids reserved before any page is written. Catalog, page tree, and
/// the four fonts. Pages start at 7.
const ID_CATALOG: u32 = 1;
const ID_PAGES: u32 = 2;
const ID_FIRST_FONT: u32 = 3;
const RESERVED: u32 = 6;

/// Helvetica advance widths for ASCII 32..=126, in 1/1000 em.
///
/// Taken from the Adobe AFM metrics for the base-14 font, which every
/// conforming viewer must use for a non-embedded `/Helvetica`. Without these
/// the writer cannot right-align a number or know when a value overflows its
/// cell, so it would have to guess with an average advance — and a column of
/// right-aligned numbers would visibly fail to line up.
#[rustfmt::skip]
const HELVETICA_WIDTHS: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556,
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556,
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556,
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

/// Advance for a character not in the ASCII table, in 1/1000 em.
///
/// The Latin-1 supplement is mostly accented letters whose advances match
/// their unaccented forms closely enough for layout; 556 is Helvetica's
/// lowercase average.
const FALLBACK_WIDTH: u16 = 556;

/// Helvetica-Bold runs wider than Helvetica by roughly this ratio across the
/// alphabet. Using it avoids carrying a second 95-entry table for a
/// measurement whose only consumers are alignment and overflow clipping,
/// both of which degrade gracefully. Bold text may therefore be measured a
/// few percent narrow.
const BOLD_WIDTH_RATIO: f32 = 1.06;

/// Width of `s` set in Helvetica at `size` points.
pub fn text_width(s: &str, size: f32, bold: bool) -> f32 {
    let mils: u32 = s
        .chars()
        .map(|c| {
            let code = c as u32;
            if (32..=126).contains(&code) {
                HELVETICA_WIDTHS[(code - 32) as usize] as u32
            } else {
                FALLBACK_WIDTH as u32
            }
        })
        .sum();
    let w = (mils as f32 / 1000.0) * size;
    if bold {
        w * BOLD_WIDTH_RATIO
    } else {
        w
    }
}

/// An RGB colour in 0..=255 per channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Color(pub u8, pub u8, pub u8);

impl Color {
    pub const BLACK: Color = Color(0, 0, 0);

    fn parts(self) -> (f32, f32, f32) {
        (
            self.0 as f32 / 255.0,
            self.1 as f32 / 255.0,
            self.2 as f32 / 255.0,
        )
    }
}

/// One page's drawing commands, built top-down and flipped on the way in.
///
/// Callers work in the same coordinate system as the rest of Ferrix — origin
/// top-left, y increasing downward — because every layout number in the
/// paginator is already in that space. Converting once here, at the boundary,
/// means no layout code has to remember which way up it is.
///
/// Reuse one `Content` across pages via [`Content::reset`]: the buffer then
/// settles at the size of the widest page and stops allocating.
pub struct Content {
    buf: Vec<u8>,
    page_h: f32,
}

impl Content {
    pub fn new(page_h: f32) -> Self {
        Content {
            buf: Vec::with_capacity(16 * 1024),
            page_h,
        }
    }

    /// Empty the buffer, keeping its capacity, for the next page.
    pub fn reset(&mut self, page_h: f32) {
        self.buf.clear();
        self.page_h = page_h;
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Top-down y to PDF's bottom-up y.
    fn flip(&self, y: f32) -> f32 {
        self.page_h - y
    }

    /// Fill an axis-aligned rectangle. `y` is the *top* edge.
    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let (r, g, b) = color.parts();
        let bottom = self.flip(y + h);
        let _ = write!(
            self.buf,
            "{r:.3} {g:.3} {b:.3} rg\n{x:.2} {bottom:.2} {w:.2} {h:.2} re\nf\n"
        );
    }

    /// Stroke a straight line. Both `y` values are top-down.
    pub fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: Color) {
        let (r, g, b) = color.parts();
        let (fy1, fy2) = (self.flip(y1), self.flip(y2));
        let _ = write!(
            self.buf,
            "{r:.3} {g:.3} {b:.3} RG\n{width:.2} w\n{x1:.2} {fy1:.2} m\n{x2:.2} {fy2:.2} l\nS\n"
        );
    }

    /// Draw text with its baseline at `(x, baseline_y)`, top-down.
    pub fn text(&mut self, x: f32, baseline_y: f32, size: f32, font: u8, color: Color, s: &str) {
        if s.is_empty() || size <= 0.0 {
            return;
        }
        let (r, g, b) = color.parts();
        let y = self.flip(baseline_y);
        let _ = write!(
            self.buf,
            "BT\n/F{font} {size:.2} Tf\n{r:.3} {g:.3} {b:.3} rg\n1 0 0 1 {x:.2} {y:.2} Tm\n("
        );
        escape_into(&mut self.buf, s);
        let _ = write!(self.buf, ") Tj\nET\n");
    }

    /// Run `f` with drawing clipped to the given top-down rectangle.
    ///
    /// Cell text that overruns its column must not paint over the neighbour,
    /// which is what the grid does on screen and what Excel does on paper.
    pub fn clipped(&mut self, x: f32, y: f32, w: f32, h: f32, f: impl FnOnce(&mut Self)) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let bottom = self.flip(y + h);
        let _ = write!(self.buf, "q\n{x:.2} {bottom:.2} {w:.2} {h:.2} re\nW\nn\n");
        f(self);
        let _ = writeln!(self.buf, "Q");
    }
}

/// Escape a string for a PDF literal string and map it to WinAnsi bytes.
///
/// Characters outside WinAnsi become `?` rather than being dropped: a missing
/// glyph that leaves a visible mark is a bug the user can see and report; one
/// that silently shortens a cell's text is a bug that gets discovered by a
/// customer reading the wrong number.
fn escape_into(out: &mut Vec<u8>, s: &str) {
    for c in s.chars() {
        let code = c as u32;
        match c {
            '(' | ')' | '\\' => {
                out.push(b'\\');
                out.push(c as u8);
            }
            '\n' | '\r' | '\t' => out.push(b' '),
            _ if (32..=126).contains(&code) => out.push(code as u8),
            // WinAnsi agrees with Latin-1 over A0..FF.
            _ if (160..=255).contains(&code) => out.push(code as u8),
            _ => out.push(b'?'),
        }
    }
}

/// A PDF document being written out page by page.
pub struct PdfDoc<W: Write> {
    w: W,
    pos: u64,
    /// Byte offset of each object, indexed by `id - 1`.
    offsets: Vec<u64>,
    /// Page object ids, in order, for the page tree's `/Kids`.
    kids: Vec<u32>,
    next_id: u32,
    media: (f32, f32),
}

impl<W: Write> PdfDoc<W> {
    /// Start a document whose pages are `media` points.
    pub fn new(mut w: W, media: (f32, f32)) -> io::Result<Self> {
        // The binary comment line tells transfer tools the file is not text.
        let header: &[u8] = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n";
        w.write_all(header)?;
        Ok(PdfDoc {
            w,
            pos: header.len() as u64,
            offsets: vec![0; RESERVED as usize],
            kids: Vec::new(),
            next_id: RESERVED + 1,
            media,
        })
    }

    pub fn page_count(&self) -> usize {
        self.kids.len()
    }

    fn emit(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.w.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    fn alloc(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.offsets.push(0);
        id
    }

    fn begin_object(&mut self, id: u32) -> io::Result<()> {
        self.offsets[(id - 1) as usize] = self.pos;
        let head = format!("{id} 0 obj\n");
        self.emit(head.as_bytes())
    }

    /// Append a page whose drawing commands are `content`.
    ///
    /// The content stream is written straight through; nothing about this
    /// page is retained afterwards except its object id.
    pub fn add_page(&mut self, content: &Content) -> io::Result<()> {
        let body = content.as_bytes();
        let content_id = self.alloc();
        self.begin_object(content_id)?;
        let head = format!("<< /Length {} >>\nstream\n", body.len());
        self.emit(head.as_bytes())?;
        self.emit(body)?;
        self.emit(b"\nendstream\nendobj\n")?;

        let page_id = self.alloc();
        self.begin_object(page_id)?;
        let (mw, mh) = self.media;
        let fonts = (0..4)
            .map(|i| format!("/F{} {} 0 R", i + 1, ID_FIRST_FONT + i))
            .collect::<Vec<_>>()
            .join(" ");
        let page = format!(
            "<< /Type /Page /Parent {ID_PAGES} 0 R /MediaBox [0 0 {mw:.2} {mh:.2}] \
             /Resources << /Font << {fonts} >> >> /Contents {content_id} 0 R >>\nendobj\n"
        );
        self.emit(page.as_bytes())?;
        self.kids.push(page_id);
        Ok(())
    }

    /// Write the fonts, page tree, catalog, cross-reference table and
    /// trailer. Returns the total byte length of the document.
    pub fn finish(mut self) -> io::Result<u64> {
        for (i, name) in [
            "Helvetica",
            "Helvetica-Bold",
            "Helvetica-Oblique",
            "Helvetica-BoldOblique",
        ]
        .iter()
        .enumerate()
        {
            let id = ID_FIRST_FONT + i as u32;
            self.begin_object(id)?;
            let obj = format!(
                "<< /Type /Font /Subtype /Type1 /BaseFont /{name} /Encoding /WinAnsiEncoding >>\nendobj\n"
            );
            self.emit(obj.as_bytes())?;
        }

        self.begin_object(ID_PAGES)?;
        let kids = self
            .kids
            .iter()
            .map(|id| format!("{id} 0 R"))
            .collect::<Vec<_>>()
            .join(" ");
        let pages = format!(
            "<< /Type /Pages /Count {} /Kids [{kids}] >>\nendobj\n",
            self.kids.len()
        );
        self.emit(pages.as_bytes())?;

        self.begin_object(ID_CATALOG)?;
        let cat = format!("<< /Type /Catalog /Pages {ID_PAGES} 0 R >>\nendobj\n");
        self.emit(cat.as_bytes())?;

        let xref_pos = self.pos;
        let count = self.offsets.len() + 1;
        self.emit(format!("xref\n0 {count}\n").as_bytes())?;
        // Every entry is exactly 20 bytes; readers index into the table
        // arithmetically, so a short line breaks the whole file.
        self.emit(b"0000000000 65535 f \n")?;
        for i in 0..self.offsets.len() {
            let off = self.offsets[i];
            self.emit(format!("{off:010} 00000 n \n").as_bytes())?;
        }
        let trailer = format!(
            "trailer\n<< /Size {count} /Root {ID_CATALOG} 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n"
        );
        self.emit(trailer.as_bytes())?;
        self.w.flush()?;
        Ok(self.pos)
    }
}

/// An independent reader used to verify what the writer produced.
///
/// Deliberately not part of the public API: it exists so tests can assert on
/// a rendered PDF by *parsing* it — following `startxref`, the cross-reference
/// offsets, the catalog and the page tree — rather than by grepping the bytes.
/// Grepping would pass on a file with a corrupt xref table that no viewer can
/// open, which is the failure this whole format is easiest to get wrong.
#[cfg(test)]
pub(crate) mod reader;

#[cfg(test)]
mod tests;
