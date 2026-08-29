//! Rich clipboard interchange: the HTML `<table>` flavour Excel speaks, plus
//! the Paste Special vocabulary that goes with it.
//!
//! # Why HTML at all
//!
//! [`crate::tsv`] carries *text*. Copy a currency column out of Ferrix, paste
//! it into Excel, and the money signs, the decimal places, the bold header and
//! the column widths are all gone — the numbers land, everything that made
//! them readable does not. Excel itself does not have this problem when
//! copying between its own windows, because it puts several *flavours* on the
//! clipboard at once and the receiver picks the richest one it understands.
//! The rich flavour every spreadsheet and browser agrees on is an HTML
//! `<table>`.
//!
//! So this module does two things:
//!
//! * [`to_html`] renders a [`ClipBlock`] as a `<table>` carrying number
//!   formats, fills, text colours, typography and column widths; and
//! * [`from_html`] reads one back, understanding both what Excel writes
//!   (`mso-number-format`, inline CSS, `<col width>`) and the extra
//!   `data-ferrix-*` attributes Ferrix adds so a Ferrix -> Ferrix round trip is
//!   lossless where a Ferrix -> Excel one can only be faithful.
//!
//! [`parse_clipboard`] is the entry point a paste should use: it prefers the
//! HTML flavour whenever the payload looks like HTML and falls back to TSV
//! otherwise, which is exactly the preference order the issue asks for.
//!
//! # PLATFORM LIMITATION — read this before believing the round trip
//!
//! egui/eframe expose the system clipboard as **plain text only**
//! (`Context::copy_text` / `Event::Paste(String)`). There is no multi-flavour
//! clipboard API in eframe, so Ferrix cannot register `CF_HTML` beside
//! `CF_UNICODETEXT` the way a native Win32 or Cocoa app would. What the UI
//! actually does, therefore, is:
//!
//! * on copy, put **TSV** on the text clipboard (so Excel and every other
//!   consumer keep working exactly as before) and keep the HTML rendering
//!   available for the in-process round trip and for tests; and
//! * on paste, sniff the incoming text — if it looks like an HTML table, parse
//!   it as one; otherwise parse it as TSV.
//!
//! The consequence is honest and worth stating plainly: **pasting rich content
//! FROM Excel works** whenever the text arriving on the clipboard is HTML, and
//! **copying rich content TO Excel does not**, because Ferrix has no way to
//! advertise the HTML flavour. Everything in this module is pure and unit
//! tested, so the day eframe grows a flavoured clipboard — or a native
//! clipboard crate is added deliberately — only the wiring changes.
//!
//! # Scale
//!
//! A [`ClipBlock`] is bounded by the copied rectangle, which the UI already
//! caps against the memory budget. Formats read out of a paste are converted
//! to **rectangles** before they reach [`crate::SheetFormat`]
//! ([`merge_rectangles`]), so pasting one format across a 100k-cell region
//! costs one range entry rather than 100k cell overrides.

use crate::format::{FontFamily, ManualStyle, Typography};
use crate::table::{NumberFormat, Rgb};

/// One cell on the clipboard: what it shows, what produced it, how it is
/// formatted.
///
/// `formula` holds the SOURCE TEXT, never a parsed tree. See
/// [`crate::tsv`]'s neighbours in `ferrix-formula`: the parser discards the
/// `$` markers, so a formula that makes a round trip through an AST comes back
/// with every absolute reference silently relativised.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ClipCell {
    /// Display text — what the user saw in the cell.
    pub text: String,
    /// Formula source, if this cell held one.
    pub formula: Option<String>,
    /// Number format in effect on the cell.
    pub format: Option<NumberFormat>,
    /// Fill, text colour and typography in effect on the cell.
    pub style: ManualStyle,
    /// Where this cell was copied FROM, when it came from a Ferrix sheet.
    ///
    /// A pasted formula's references are offset by the distance between this
    /// and where the cell lands, which is what makes `=A1+B1` copied three
    /// rows down read `=A4+B4`. `None` — anything arriving from outside, where
    /// there is no source coordinate to measure from — means the formula is
    /// written exactly as it was received rather than being shifted by a
    /// guessed amount.
    pub origin: Option<crate::CellRef>,
}

impl ClipCell {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            ..Default::default()
        }
    }

    /// Is there any formatting to carry? Drives whether a `<td>` needs
    /// attributes at all, keeping the common case small.
    pub fn is_plain(&self) -> bool {
        self.formula.is_none() && self.format.is_none() && self.style.is_empty()
    }

    /// The cell's value as a number, for the arithmetic paste operations.
    ///
    /// Reads the DISPLAY TEXT, because that is all the clipboard carries. A
    /// cell showing `1,234.50` under a thousands format is not `f64`-parsable
    /// as written, so grouping separators are stripped first — otherwise
    /// "Paste Special > Add" would silently skip every formatted number, which
    /// is precisely the case a user reaches for it.
    pub fn as_number(&self) -> Option<f64> {
        parse_loose_number(&self.text)
    }
}

/// Parse a spreadsheet-looking number: optional currency symbol, thousands
/// separators, trailing percent, parenthesised negatives.
fn parse_loose_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (t, paren_negative) = match t.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        Some(inner) => (inner, true),
        None => (t, false),
    };
    let (t, percent) = match t.strip_suffix('%') {
        Some(head) => (head, true),
        None => (t, false),
    };
    // Keep digits, sign, and the decimal point; drop grouping and currency.
    let mut cleaned = String::with_capacity(t.len());
    for (i, ch) in t.trim().chars().enumerate() {
        match ch {
            '0'..='9' | '.' => cleaned.push(ch),
            '-' | '+' if i == 0 || cleaned.is_empty() => cleaned.push(ch),
            ',' | ' ' | '\u{a0}' | '\u{2009}' => {}
            // A currency symbol may only lead.
            _ if cleaned.is_empty() => {}
            _ => return None,
        }
    }
    let mut n: f64 = cleaned.parse().ok()?;
    if percent {
        n /= 100.0;
    }
    if paren_negative {
        n = -n;
    }
    Some(n)
}

/// A rectangular clipboard payload.
///
/// Row-major and dense: the rectangle a user copied is small by construction
/// (the UI caps it against the measured memory budget before ever building
/// one), so a dense block is simpler and faster than a sparse map and cannot
/// disagree with itself about its own width.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ClipBlock {
    rows: usize,
    cols: usize,
    cells: Vec<ClipCell>,
    /// Source column widths in points, one per column, `None` where the
    /// source column had no explicit width. Carried so "Paste Special >
    /// Column Widths" has something to paste.
    pub col_widths: Vec<Option<f32>>,
}

impl ClipBlock {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            cells: vec![ClipCell::default(); rows * cols],
            col_widths: vec![None; cols],
        }
    }

    /// Build from plain display strings — the shape [`crate::tsv`] produces.
    pub fn from_text_grid(grid: &[Vec<String>]) -> Self {
        let rows = grid.len();
        let cols = grid.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut b = Self::new(rows, cols);
        for (r, row) in grid.iter().enumerate() {
            for (c, s) in row.iter().enumerate() {
                b.set(r, c, ClipCell::text(s.clone()));
            }
        }
        b
    }

    #[inline]
    pub fn rows(&self) -> usize {
        self.rows
    }

    #[inline]
    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0 || self.cols == 0
    }

    /// Total cells, for the caller's memory cap.
    pub fn cell_count(&self) -> u64 {
        self.rows as u64 * self.cols as u64
    }

    #[inline]
    pub fn get(&self, row: usize, col: usize) -> Option<&ClipCell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.cells.get(row * self.cols + col)
    }

    #[inline]
    pub fn set(&mut self, row: usize, col: usize, cell: ClipCell) {
        if row < self.rows && col < self.cols {
            self.cells[row * self.cols + col] = cell;
        }
    }

    /// Display strings only, for TSV rendering and for callers that do not
    /// care about formatting.
    pub fn to_text_grid(&self) -> Vec<Vec<String>> {
        (0..self.rows)
            .map(|r| {
                (0..self.cols)
                    .map(|c| self.get(r, c).map(|x| x.text.clone()).unwrap_or_default())
                    .collect()
            })
            .collect()
    }

    /// Swap rows and columns — "Paste Special > Transpose".
    ///
    /// Column widths do NOT survive a transpose, and are dropped rather than
    /// reinterpreted: after a transpose the source's column widths describe
    /// what are now ROWS, and applying them to the destination's columns would
    /// resize the wrong axis. Excel drops them here too.
    pub fn transposed(&self) -> Self {
        let mut out = Self::new(self.cols, self.rows);
        for r in 0..self.rows {
            for c in 0..self.cols {
                if let Some(cell) = self.get(r, c) {
                    out.set(c, r, cell.clone());
                }
            }
        }
        out
    }
}

// ================================================================== options ==

/// Which aspects of the clipboard a paste applies.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PasteWhat {
    /// Everything: values or formulas as copied, plus formats.
    #[default]
    All,
    /// Values only — a formula lands as the number it evaluated to.
    Values,
    /// Formula source where there was one, values elsewhere.
    Formulas,
    /// Number formats and styling only; cell contents untouched.
    Formats,
    /// Source column widths only; contents and formats untouched.
    ColumnWidths,
}

impl PasteWhat {
    /// Does this mode write cell contents?
    pub fn writes_contents(self) -> bool {
        matches!(
            self,
            PasteWhat::All | PasteWhat::Values | PasteWhat::Formulas
        )
    }

    /// Does this mode write formats or styling?
    pub fn writes_formats(self) -> bool {
        matches!(self, PasteWhat::All | PasteWhat::Formats)
    }

    /// Does this mode write column widths?
    pub fn writes_widths(self) -> bool {
        matches!(self, PasteWhat::ColumnWidths)
    }

    /// Stable identifier for status text and command ids.
    pub fn label(self) -> &'static str {
        match self {
            PasteWhat::All => "All",
            PasteWhat::Values => "Values",
            PasteWhat::Formulas => "Formulas",
            PasteWhat::Formats => "Formats",
            PasteWhat::ColumnWidths => "Column Widths",
        }
    }
}

/// The arithmetic combination applied between the clipboard and what is
/// already in the destination.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PasteOp {
    #[default]
    None,
    Add,
    Subtract,
    Multiply,
    Divide,
}

impl PasteOp {
    pub fn label(self) -> &'static str {
        match self {
            PasteOp::None => "None",
            PasteOp::Add => "Add",
            PasteOp::Subtract => "Subtract",
            PasteOp::Multiply => "Multiply",
            PasteOp::Divide => "Divide",
        }
    }

    /// Combine the destination's current value with the clipboard's.
    ///
    /// `None` means "this pair cannot be combined arithmetically" — either
    /// side is non-numeric, or it is a division by zero. The caller leaves the
    /// destination alone in that case rather than writing an error over data
    /// the user did not ask to touch. Excel writes `#VALUE!` here; refusing is
    /// the safer half of that trade and is the same instinct as
    /// `MergeError::Overlaps` refusing rather than absorbing.
    ///
    /// An EMPTY destination is treated as zero, which is what makes
    /// "Add" over a blank column behave as a plain paste rather than as a
    /// no-op.
    pub fn apply(self, dest: Option<f64>, src: Option<f64>) -> Option<f64> {
        let (d, s) = (dest.unwrap_or(0.0), src?);
        match self {
            PasteOp::None => Some(s),
            PasteOp::Add => Some(d + s),
            PasteOp::Subtract => Some(d - s),
            PasteOp::Multiply => Some(d * s),
            // Division by zero is refused rather than producing an infinity
            // that would then format as a meaningless "inf".
            PasteOp::Divide => (s != 0.0).then_some(d / s),
        }
    }
}

/// Warning attached to a transposing paste.
///
/// A transpose moves formulas to cells whose row/column relationship to their
/// references is not the one they were written with, so their offsets are
/// applied along the axes they actually travelled and the result may not be
/// what the author meant. Excel warns here too. Reported rather than silently
/// applied, in the same spirit as `rule_survives_xlsx`: the user learns in the
/// editor, not after the numbers are wrong.
pub const TRANSPOSE_NOTE: &str =
    "transposed — formulas were offset along the axes they moved, check their references";

/// The full Paste Special request.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PasteOptions {
    pub what: PasteWhat,
    pub op: PasteOp,
    /// Swap rows and columns.
    pub transpose: bool,
    /// Leave the destination alone wherever the clipboard cell is blank,
    /// instead of clearing it.
    pub skip_blanks: bool,
}

impl PasteOptions {
    /// The plain Ctrl+V request.
    pub fn plain() -> Self {
        Self::default()
    }

    pub fn values() -> Self {
        Self {
            what: PasteWhat::Values,
            ..Self::default()
        }
    }

    /// Does this request differ from a plain paste in a way worth naming in
    /// the status line?
    pub fn is_special(&self) -> bool {
        self.what != PasteWhat::All
            || self.op != PasteOp::None
            || self.transpose
            || self.skip_blanks
    }

    /// A short human description, for the status line.
    pub fn describe(&self) -> String {
        let mut parts = vec![self.what.label().to_string()];
        if self.op != PasteOp::None {
            parts.push(self.op.label().to_string());
        }
        if self.transpose {
            parts.push("Transpose".into());
        }
        if self.skip_blanks {
            parts.push("Skip Blanks".into());
        }
        parts.join(" · ")
    }
}

// ===================================================================== HTML ==

/// Does this clipboard payload look like the HTML flavour?
///
/// Deliberately narrow: it must contain a `<table` tag. A stray `<` in a text
/// paste, or a snippet of HTML with no table in it, is data and must go down
/// the TSV path — misreading a user's text as markup would eat their content.
pub fn looks_like_html(text: &str) -> bool {
    // Bounded scan: only the head of a large paste is inspected, so sniffing a
    // 100MB text paste costs the same as sniffing a small one.
    let head_len = text
        .char_indices()
        .nth(4096)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let head = text[..head_len].to_ascii_lowercase();
    head.contains("<table")
}

/// Render a block as an HTML `<table>`, the flavour Excel understands.
pub fn to_html(block: &ClipBlock) -> String {
    let mut s = String::with_capacity(block.cell_count() as usize * 24 + 128);
    s.push_str(
        "<html><head><meta charset=\"utf-8\"></head><body>\n\
         <table border=\"0\" cellpadding=\"0\" cellspacing=\"0\">\n",
    );
    if block.col_widths.iter().any(|w| w.is_some()) {
        s.push_str("<colgroup>");
        for w in &block.col_widths {
            match w {
                // Two spellings on purpose. `width` is bare and therefore
                // PIXELS, which is what Excel reads off a <col>; the `style`
                // carries the same width in points with an explicit unit,
                // which is what Ferrix reads back. Emitting only the bare
                // attribute would make a Ferrix -> Ferrix round trip shrink
                // every column by the 0.75 px-to-pt factor, once per trip.
                Some(pt) => s.push_str(&format!(
                    "<col width=\"{}\" style=\"width:{}pt\">",
                    round2(pt / 0.75),
                    round2(*pt)
                )),
                None => s.push_str("<col>"),
            }
        }
        s.push_str("</colgroup>\n");
    }
    for r in 0..block.rows() {
        s.push_str("<tr>");
        for c in 0..block.cols() {
            let empty = ClipCell::default();
            let cell = block.get(r, c).unwrap_or(&empty);
            s.push_str("<td");
            if let Some(f) = &cell.format {
                // Excel's own attribute for a cell's number format, and the
                // one it emits when copying. Quoting matches what Excel
                // writes so a code containing a semicolon survives.
                s.push_str(&format!(
                    " style=\"mso-number-format:'{}'",
                    escape_attr(&f.to_code())
                ));
                push_css_style(&mut s, &cell.style, false);
                s.push('"');
                // Ferrix's own copy, unambiguous and not subject to Excel's
                // quoting rules, so a Ferrix -> Ferrix round trip is exact.
                s.push_str(&format!(
                    " data-ferrix-numfmt=\"{}\"",
                    escape_attr(&f.to_code())
                ));
            } else if !cell.style.is_empty() {
                s.push_str(" style=\"");
                push_css_style(&mut s, &cell.style, true);
                s.push('"');
            }
            if let Some(f) = &cell.formula {
                s.push_str(&format!(" data-ferrix-formula=\"{}\"", escape_attr(f)));
                // The source coordinate travels with the formula, so a paste
                // through the text clipboard can still offset references by
                // the distance actually moved. Only emitted alongside a
                // formula, because nothing else consults it.
                if let Some(o) = cell.origin {
                    s.push_str(&format!(" data-ferrix-origin=\"{},{}\"", o.row, o.col));
                }
            }
            s.push('>');
            s.push_str(&escape_text(&cell.text));
            s.push_str("</td>");
        }
        s.push_str("</tr>\n");
    }
    s.push_str("</table>\n</body></html>");
    s
}

/// Append the CSS declarations for a style. `first` says whether the
/// declaration list is still empty, so separators land correctly after an
/// `mso-number-format` that is already there.
fn push_css_style(out: &mut String, style: &ManualStyle, first: bool) {
    let mut need_sep = !first;
    let mut sep = |out: &mut String| {
        if need_sep {
            out.push(';');
        }
        need_sep = true;
    };
    if let Some(fill) = style.fill {
        sep(out);
        out.push_str(&format!("background-color:#{}", fill.to_hex()));
    }
    if let Some(text) = style.text {
        sep(out);
        out.push_str(&format!("color:#{}", text.to_hex()));
    }
    let t = &style.typography;
    if let Some(b) = t.bold {
        sep(out);
        out.push_str(if b {
            "font-weight:bold"
        } else {
            "font-weight:normal"
        });
    }
    if let Some(i) = t.italic {
        sep(out);
        out.push_str(if i {
            "font-style:italic"
        } else {
            "font-style:normal"
        });
    }
    // Underline and strikethrough share one CSS property, so they are emitted
    // together — writing two `text-decoration` declarations would mean the
    // second silently wins and one of the two switches vanished.
    if t.underline.is_some() || t.strikethrough.is_some() {
        let mut deco = Vec::new();
        if t.underline == Some(true) {
            deco.push("underline");
        }
        if t.strikethrough == Some(true) {
            deco.push("line-through");
        }
        sep(out);
        out.push_str("text-decoration:");
        let joined = deco.join(" ");
        out.push_str(if joined.is_empty() { "none" } else { &joined });
    }
    if let Some(f) = t.family {
        sep(out);
        out.push_str(match f {
            FontFamily::Monospace => "font-family:monospace",
            FontFamily::Proportional => "font-family:sans-serif",
        });
    }
    if let Some(pt) = t.size {
        sep(out);
        out.push_str(&format!("font-size:{}pt", round2(pt)));
    }
}

fn round2(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\n' => out.push_str("<br>"),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Decode the entity subset a spreadsheet or browser actually emits.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != '&' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // Bounded lookahead: an unterminated `&` is literal text, not the
        // start of a scan to the end of a 10MB document.
        let end = (i + 1..bytes.len().min(i + 12)).find(|&j| bytes[j] == ';');
        let Some(end) = end else {
            out.push('&');
            i += 1;
            continue;
        };
        let name: String = bytes[i + 1..end].iter().collect();
        let decoded = match name.as_str() {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            other => other
                .strip_prefix('#')
                .and_then(
                    |d| match d.strip_prefix('x').or_else(|| d.strip_prefix('X')) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => d.parse::<u32>().ok(),
                    },
                )
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                i = end + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

/// Parse the HTML clipboard flavour into a block.
///
/// Returns `None` when there is no table to read, so the caller can fall back
/// to TSV rather than producing a plausible-looking empty paste.
///
/// This is a deliberately small tolerant scanner rather than a real HTML
/// parser: the input is machine-generated table markup from a spreadsheet, and
/// pulling in a full HTML5 parser to read `<td>`s would be a large dependency
/// for a strictly smaller job. Anything it does not understand degrades to
/// text, never to a panic.
pub fn from_html(html: &str) -> Option<ClipBlock> {
    let lower = html.to_ascii_lowercase();
    let table_start = lower.find("<table")?;
    // Only the first table: Excel emits one, and a stray nested table's rows
    // must not be spliced into the outer one's grid.
    let table_end = lower[table_start..]
        .find("</table")
        .map(|i| table_start + i)
        .unwrap_or(html.len());
    let body = &html[table_start..table_end];
    let lower_body = &lower[table_start..table_end];

    let widths = parse_colgroup(html, &lower, table_start);

    let mut rows: Vec<Vec<ClipCell>> = Vec::new();
    let mut pos = 0usize;
    while let Some(tr) = lower_body[pos..].find("<tr") {
        let row_start = pos + tr;
        let row_body_start = match lower_body[row_start..].find('>') {
            Some(i) => row_start + i + 1,
            None => break,
        };
        let row_end = lower_body[row_body_start..]
            .find("</tr")
            .map(|i| row_body_start + i)
            .unwrap_or(lower_body.len());
        rows.push(parse_row(
            &body[row_body_start..row_end],
            &lower_body[row_body_start..row_end],
        ));
        pos = row_end.max(row_body_start);
        if pos >= lower_body.len() {
            break;
        }
    }
    if rows.is_empty() {
        return None;
    }

    // Ragged input is padded to a rectangle, matching `tsv::from_tsv`, so a
    // caller can index without bounds checks.
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return None;
    }
    let mut block = ClipBlock::new(rows.len(), cols);
    for (r, row) in rows.into_iter().enumerate() {
        for (c, cell) in row.into_iter().enumerate() {
            block.set(r, c, cell);
        }
    }
    for (i, w) in widths.into_iter().enumerate() {
        if i < block.col_widths.len() {
            block.col_widths[i] = w;
        }
    }
    Some(block)
}

fn parse_colgroup(html: &str, lower: &str, from: usize) -> Vec<Option<f32>> {
    let mut out = Vec::new();
    let end = lower[from..]
        .find("</table")
        .map(|i| from + i)
        .unwrap_or(html.len());
    let region = &lower[from..end];
    let mut pos = 0usize;
    while let Some(i) = region[pos..].find("<col") {
        let start = pos + i;
        // `<colgroup` also starts with `<col`; skip it rather than reading it
        // as a zero-width column and shifting every later width left by one.
        if region[start..].starts_with("<colgroup") {
            pos = start + 4;
            continue;
        }
        let tag_end = match region[start..].find('>') {
            Some(j) => start + j,
            None => break,
        };
        let tag = &html[from + start..from + tag_end];
        // An explicit CSS unit wins over the bare `width` attribute, which is
        // pixels by definition and therefore always the lossier reading.
        let w = style_prop(tag, "width")
            .and_then(|v| parse_len_pt(&v))
            .or_else(|| attr(tag, "width").and_then(|v| parse_len_pt(&v)));
        out.push(w);
        pos = tag_end;
    }
    out
}

/// Read a CSS/HTML length as points. Bare numbers on a `<col width>` are
/// pixels in the HTML that spreadsheets emit; `pt` is taken at face value.
fn parse_len_pt(s: &str) -> Option<f32> {
    let t = s.trim();
    if let Some(v) = t.strip_suffix("pt") {
        return v.trim().parse::<f32>().ok().filter(|v| *v > 0.0);
    }
    if let Some(v) = t.strip_suffix("px") {
        return v
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| *v > 0.0)
            .map(|px| px * 0.75);
    }
    t.parse::<f32>()
        .ok()
        .filter(|v| *v > 0.0)
        .map(|px| px * 0.75)
}

fn parse_row(body: &str, lower: &str) -> Vec<ClipCell> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let td = lower[pos..].find("<td");
        let th = lower[pos..].find("<th");
        let next = match (td, th) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let Some(rel) = next else {
            break;
        };
        let start = pos + rel;
        let tag_end = match lower[start..].find('>') {
            Some(i) => start + i,
            None => break,
        };
        let tag = &body[start..tag_end];
        let close = if lower[start..].starts_with("<th") {
            "</th"
        } else {
            "</td"
        };
        let content_start = tag_end + 1;
        let content_end = lower[content_start..]
            .find(close)
            .map(|i| content_start + i)
            .unwrap_or(lower.len());
        out.push(parse_cell(tag, &body[content_start..content_end]));
        // A horizontally spanning cell occupies the columns it covers, so the
        // cells after it stay under the right headers instead of sliding left.
        let span = attr(tag, "colspan")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 4096);
        for _ in 1..span {
            out.push(ClipCell::default());
        }
        pos = content_end.max(content_start);
        if pos >= lower.len() {
            break;
        }
    }
    out
}

fn parse_cell(tag: &str, content: &str) -> ClipCell {
    let mut cell = ClipCell {
        text: strip_tags(content),
        ..Default::default()
    };

    // Formula: Ferrix's own attribute first, then the one Excel writes.
    cell.formula = attr(tag, "data-ferrix-formula")
        .or_else(|| attr(tag, "x:fmla"))
        .map(|f| unescape(&f))
        .filter(|f| f.starts_with('='));

    // Source coordinate, so a pasted formula can be offset by the distance
    // actually travelled. Only meaningful beside a formula.
    cell.origin = attr(tag, "data-ferrix-origin").and_then(|v| {
        let (r, c) = v.split_once(',')?;
        Some(crate::CellRef::new(
            r.trim().parse().ok()?,
            c.trim().parse().ok()?,
        ))
    });

    // Number format: Ferrix's unambiguous attribute wins over the CSS one,
    // because `mso-number-format` is quoted and escaped by whoever wrote it.
    let code = attr(tag, "data-ferrix-numfmt")
        .map(|c| unescape(&c))
        .or_else(|| {
            style_prop(tag, "mso-number-format").map(|c| {
                let c = c.trim();
                let c = c
                    .strip_prefix('\'')
                    .and_then(|r| r.strip_suffix('\''))
                    .unwrap_or(c);
                unescape(&c.replace("\\.", "."))
            })
        });
    cell.format = code
        .filter(|c| !c.is_empty())
        .map(|c| NumberFormat::from_code(&c))
        .filter(|f| *f != NumberFormat::General);

    // Styling.
    let mut style = ManualStyle::default();
    if let Some(v) = style_prop(tag, "background-color").or_else(|| style_prop(tag, "background")) {
        style.fill = parse_css_color(&v);
    }
    if let Some(v) = style_prop(tag, "color") {
        style.text = parse_css_color(&v);
    }
    let mut ty = Typography::default();
    if let Some(v) = style_prop(tag, "font-weight") {
        let v = v.trim().to_ascii_lowercase();
        // Numeric weights are what a browser emits; 600+ is bold by CSS.
        let bold = v == "bold" || v == "bolder" || v.parse::<u32>().is_ok_and(|n| n >= 600);
        ty.bold = Some(bold);
    }
    if let Some(v) = style_prop(tag, "font-style") {
        ty.italic = Some(v.trim().eq_ignore_ascii_case("italic"));
    }
    if let Some(v) = style_prop(tag, "text-decoration") {
        let v = v.to_ascii_lowercase();
        ty.underline = Some(v.contains("underline"));
        ty.strikethrough = Some(v.contains("line-through"));
    }
    if let Some(v) = style_prop(tag, "font-family") {
        let v = v.to_ascii_lowercase();
        ty.family = Some(
            if v.contains("monospace")
                || v.contains("courier")
                || v.contains("consolas")
                || v.contains("mono")
            {
                FontFamily::Monospace
            } else {
                FontFamily::Proportional
            },
        );
    }
    if let Some(v) = style_prop(tag, "font-size") {
        // Clamped, so a 200pt paste cannot produce a sheet that draws over
        // itself — the same bound `clamp_font_pt` puts on the toolbar.
        ty.size = parse_len_pt(&v).map(crate::format::clamp_font_pt);
    }
    // A <th> is bold in every renderer; carrying that means an Excel header
    // row pastes in looking like a header row.
    if tag.to_ascii_lowercase().starts_with("<th") && ty.bold.is_none() {
        ty.bold = Some(true);
    }
    style.typography = ty;
    cell.style = style;
    cell
}

fn parse_css_color(v: &str) -> Option<Rgb> {
    let t = v.trim();
    if let Some(hex) = t.strip_prefix('#') {
        // `#abc` shorthand expands each nibble, the way CSS defines it.
        if hex.len() == 3 {
            let mut full = String::with_capacity(6);
            for ch in hex.chars() {
                full.push(ch);
                full.push(ch);
            }
            return Rgb::from_hex(&full);
        }
        return Rgb::from_hex(hex);
    }
    if let Some(rest) = t.strip_prefix("rgb(").or_else(|| t.strip_prefix("RGB(")) {
        let rest = rest.strip_suffix(')')?;
        let parts: Vec<u8> = rest
            .split(',')
            .filter_map(|p| p.trim().parse::<u8>().ok())
            .collect();
        if parts.len() >= 3 {
            return Some(Rgb(parts[0], parts[1], parts[2]));
        }
    }
    // Bare hex with no `#`, which xlsx-flavoured markup sometimes writes.
    Rgb::from_hex(t)
}

/// Value of an attribute on a tag, quoted or bare.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{}=", name.to_ascii_lowercase());
    let mut from = 0usize;
    while let Some(i) = lower[from..].find(&needle) {
        let at = from + i;
        // Must be preceded by whitespace, or `x:fmla=` would match `fmla=`
        // inside a longer attribute name.
        let ok = at == 0
            || lower[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace());
        if !ok {
            from = at + needle.len();
            continue;
        }
        let vstart = at + needle.len();
        let rest = &tag[vstart..];
        let value = match rest.chars().next() {
            Some('"') => rest[1..].split('"').next().unwrap_or("").to_string(),
            Some('\'') => rest[1..].split('\'').next().unwrap_or("").to_string(),
            _ => rest
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or("")
                .to_string(),
        };
        return Some(value);
    }
    None
}

/// One declaration out of a tag's `style` attribute.
fn style_prop(tag: &str, prop: &str) -> Option<String> {
    let style = attr(tag, "style")?;
    // `mso-number-format:'#,##0.00'` may itself contain semicolons inside its
    // quotes, so declarations are split outside quotes only.
    let mut decls = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in style.chars() {
        match ch {
            '\'' | '"' if quote == Some(ch) => {
                quote = None;
                cur.push(ch);
            }
            '\'' | '"' if quote.is_none() => {
                quote = Some(ch);
                cur.push(ch);
            }
            ';' if quote.is_none() => decls.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    decls.push(cur);
    for d in decls {
        let Some((k, v)) = d.split_once(':') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case(prop) {
            return Some(unescape(v.trim()));
        }
    }
    None
}

/// Inner text of a `<td>`: tags removed, entities decoded, `<br>` a newline.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for ch in s.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let t = tag.trim().to_ascii_lowercase();
                if t.starts_with("br") || t.starts_with("/p") || t.starts_with("/div") {
                    out.push('\n');
                }
            }
            _ if in_tag => tag.push(ch),
            _ => out.push(ch),
        }
    }
    unescape(out.trim())
}

// ================================================================== parsing ==

/// Read whatever arrived on the clipboard, preferring the HTML flavour.
///
/// This is the preference order issue #30 asks for: when Excel puts an HTML
/// `<table>` on the clipboard beside its plain text, the table is the richer
/// payload and is what a paste should read.
pub fn parse_clipboard(text: &str) -> ClipBlock {
    if looks_like_html(text) {
        if let Some(b) = from_html(text) {
            return b;
        }
        // Fall through: something contained `<table` but had no rows we could
        // read. Treating it as text is lossy but never silently empty.
    }
    ClipBlock::from_text_grid(&crate::tsv::from_tsv(text))
}

// =============================================================== rectangles ==

/// Collapse a per-cell attribute grid into maximal rectangles.
///
/// **This is the scale invariant for a formatted paste.** Formatting is stored
/// per column or per range, never per cell, so pasting one number format over
/// a 100k-cell region must produce ONE range entry — not 100k cell overrides,
/// which would be a per-cell format store by another name.
///
/// `keys` is row-major, `rows * cols` long. `None` means "no attribute here".
/// The returned rectangles are disjoint, cover exactly the non-`None` cells,
/// and each carries the index of its key in `keys`.
///
/// Greedy horizontal-then-vertical growth: not the minimum possible number of
/// rectangles (that problem is NP-hard), but linear in the grid and optimal
/// for the case that actually matters — a uniform format over a block, which
/// collapses to exactly one.
pub fn merge_rectangles<K: PartialEq>(keys: &[Option<K>], rows: usize, cols: usize) -> Vec<Rect> {
    let mut used = vec![false; rows * cols];
    let mut out = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let i = r * cols + c;
            if used[i] || keys.get(i).is_none_or(|k| k.is_none()) {
                continue;
            }
            let key = keys[i].as_ref();
            // Grow right while the key matches and the cell is free.
            let mut last_col = c;
            while last_col + 1 < cols {
                let j = r * cols + last_col + 1;
                if used[j] || keys[j].as_ref() != key {
                    break;
                }
                last_col += 1;
            }
            // Grow down while the WHOLE row segment matches.
            let mut last_row = r;
            'grow: while last_row + 1 < rows {
                for cc in c..=last_col {
                    let j = (last_row + 1) * cols + cc;
                    if used[j] || keys[j].as_ref() != key {
                        break 'grow;
                    }
                }
                last_row += 1;
            }
            for rr in r..=last_row {
                for cc in c..=last_col {
                    used[rr * cols + cc] = true;
                }
            }
            out.push(Rect {
                first_row: r,
                first_col: c,
                last_row,
                last_col,
                key_index: i,
            });
        }
    }
    out
}

/// A rectangle produced by [`merge_rectangles`], in block-local coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub first_row: usize,
    pub first_col: usize,
    pub last_row: usize,
    pub last_col: usize,
    /// Index into the `keys` slice of a cell carrying this rectangle's value.
    pub key_index: usize,
}

impl Rect {
    pub fn cells(&self) -> usize {
        (self.last_row - self.first_row + 1) * (self.last_col - self.first_col + 1)
    }
}

#[cfg(test)]
mod tests;
