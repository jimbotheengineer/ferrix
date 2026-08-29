//! A small PDF reader, for tests only.
//!
//! This is the other half of an honest verification: [`super::PdfDoc`] writes
//! a file, and this parses it back **through the structures a viewer uses** —
//! `startxref` → cross-reference table → catalog → page tree → each page's
//! content stream. If the xref offsets are wrong, or `/Count` disagrees with
//! `/Kids`, or a page's `/Contents` points at nothing, parsing fails here.
//!
//! A test that instead searched the raw bytes for a cell's text would pass on
//! a file no reader can open. That is the specific way a hand-rolled PDF
//! writer goes wrong, so the test path must not share the writer's
//! assumptions.

use std::collections::HashMap;

/// One parsed page: its media box and the text runs found in its content.
#[derive(Debug, Clone)]
pub struct ParsedPage {
    pub media: (f32, f32),
    /// Text runs, in the order they were drawn, with their PDF-space
    /// (bottom-up) baseline positions.
    pub texts: Vec<TextRun>,
    /// Number of `re ... f` fill operations, i.e. painted rectangles.
    pub fills: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextRun {
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub font: u8,
    pub text: String,
}

impl ParsedPage {
    /// Every text run's string, in draw order.
    pub fn strings(&self) -> Vec<&str> {
        self.texts.iter().map(|t| t.text.as_str()).collect()
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.texts.iter().any(|t| t.text == needle)
    }

    /// The whole page's text, joined — for substring assertions.
    pub fn joined(&self) -> String {
        self.texts
            .iter()
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug)]
pub struct ParsedPdf {
    pub pages: Vec<ParsedPage>,
}

impl ParsedPdf {
    pub fn page(&self, one_based: usize) -> &ParsedPage {
        &self.pages[one_based - 1]
    }

    /// The 1-based page numbers whose text contains `needle`.
    pub fn pages_containing(&self, needle: &str) -> Vec<usize> {
        self.pages
            .iter()
            .enumerate()
            .filter(|(_, p)| p.contains(needle))
            .map(|(i, _)| i + 1)
            .collect()
    }
}

/// Parse `bytes` as a PDF, following the same path a viewer would.
///
/// Returns `Err` with a description when the file is structurally invalid,
/// which is the assertion tests actually care about.
pub fn parse(bytes: &[u8]) -> Result<ParsedPdf, String> {
    if !bytes.starts_with(b"%PDF-") {
        return Err("missing %PDF header".into());
    }
    let tail_start = bytes.len().saturating_sub(2048);
    // Scan the tail as raw bytes, not UTF-8. A PDF's second line is a
    // deliberate binary marker, so any file small enough for the tail window
    // to reach the header is not valid UTF-8 — and treating that as a parse
    // failure would reject every short document.
    let tail = ascii(&bytes[tail_start..]);
    let sx = tail.rfind("startxref").ok_or("no startxref")?;
    let after = &tail[sx + "startxref".len()..];
    let xref_pos: usize = after
        .split_whitespace()
        .next()
        .ok_or("startxref has no offset")?
        .parse()
        .map_err(|_| "startxref offset is not a number")?;
    if xref_pos >= bytes.len() {
        return Err(format!(
            "startxref {xref_pos} is past end of file ({})",
            bytes.len()
        ));
    }

    let offsets = parse_xref(bytes, xref_pos)?;

    // Trailer dictionary: find /Root.
    let trailer_at = find_from(bytes, xref_pos, b"trailer").ok_or("no trailer keyword")?;
    let trailer = ascii(&bytes[trailer_at..(trailer_at + 200).min(bytes.len())]);
    let root_id = dict_ref(&trailer, "/Root").ok_or("trailer has no /Root")?;

    let catalog = object_at(bytes, &offsets, root_id)?;
    let pages_id = dict_ref(&catalog, "/Pages").ok_or("catalog has no /Pages")?;
    let page_tree = object_at(bytes, &offsets, pages_id)?;

    let declared: usize = dict_int(&page_tree, "/Count").ok_or("page tree has no /Count")? as usize;
    let kids = dict_refs(&page_tree, "/Kids").ok_or("page tree has no /Kids")?;
    if kids.len() != declared {
        return Err(format!(
            "/Count is {declared} but /Kids has {} entries",
            kids.len()
        ));
    }

    let mut pages = Vec::with_capacity(kids.len());
    for kid in kids {
        let page = object_at(bytes, &offsets, kid)?;
        let media = dict_numbers(&page, "/MediaBox")
            .filter(|v| v.len() == 4)
            .map(|v| (v[2], v[3]))
            .ok_or("page has no usable /MediaBox")?;
        let contents_id = dict_ref(&page, "/Contents").ok_or("page has no /Contents")?;
        let stream = stream_at(bytes, &offsets, contents_id)?;
        let (texts, fills) = parse_content(&stream);
        pages.push(ParsedPage {
            media,
            texts,
            fills,
        });
    }
    Ok(ParsedPdf { pages })
}

/// Read the classic cross-reference table into `id -> byte offset`.
fn parse_xref(bytes: &[u8], at: usize) -> Result<HashMap<u32, usize>, String> {
    let head = ascii(&bytes[at..(at + 64).min(bytes.len())]);
    if !head.starts_with("xref") {
        return Err(format!(
            "startxref does not point at 'xref' (found {head:?})"
        ));
    }
    let mut cursor = at + 4;
    // Subsection header: "<first> <count>".
    let header = ascii(&bytes[cursor..(cursor + 64).min(bytes.len())]);
    let mut it = header.split_whitespace();
    let first: u32 = it
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or("xref subsection has no first id")?;
    let count: u32 = it
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or("xref subsection has no count")?;
    // Advance past the two numbers and the newline after them.
    let nl = find_from(bytes, cursor, b"\n").ok_or("xref header not terminated")?;
    let nl2 = find_from(bytes, nl + 1, b"\n").ok_or("xref subsection not terminated")?;
    cursor = nl2 + 1;

    let mut map = HashMap::new();
    for i in 0..count {
        let end = cursor + 20;
        if end > bytes.len() {
            return Err(format!("xref entry {i} runs past end of file"));
        }
        let entry = ascii(&bytes[cursor..end]);
        if entry.len() != 20 {
            return Err(format!("xref entry {i} is {} bytes, not 20", entry.len()));
        }
        let kind = entry.as_bytes()[17];
        if kind == b'n' {
            let off: usize = entry[0..10]
                .parse()
                .map_err(|_| format!("xref entry {i} offset is not a number"))?;
            map.insert(first + i, off);
        }
        cursor = end;
    }
    Ok(map)
}

/// The dictionary text of object `id`, verified to start with `<id> 0 obj`.
fn object_at(bytes: &[u8], offsets: &HashMap<u32, usize>, id: u32) -> Result<String, String> {
    let off = *offsets
        .get(&id)
        .ok_or(format!("object {id} is not in the xref table"))?;
    if off >= bytes.len() {
        return Err(format!("object {id} offset {off} is past end of file"));
    }
    let expect = format!("{id} 0 obj");
    let head = ascii(&bytes[off..(off + expect.len() + 8).min(bytes.len())]);
    if !head.starts_with(&expect) {
        return Err(format!(
            "xref says object {id} is at {off}, but that is {head:?}"
        ));
    }
    let end = find_from(bytes, off, b"endobj").ok_or(format!("object {id} has no endobj"))?;
    Ok(ascii(&bytes[off + expect.len()..end]))
}

/// The raw bytes between `stream`/`endstream` of object `id`, length-checked
/// against its `/Length`.
fn stream_at(bytes: &[u8], offsets: &HashMap<u32, usize>, id: u32) -> Result<Vec<u8>, String> {
    let off = *offsets
        .get(&id)
        .ok_or(format!("stream object {id} is not in the xref table"))?;
    let head_end = find_from(bytes, off, b"stream\n").ok_or("stream object has no stream")?;
    let dict = ascii(&bytes[off..head_end]);
    let len = dict_int(&dict, "/Length").ok_or("stream has no /Length")? as usize;
    let start = head_end + b"stream\n".len();
    if start + len > bytes.len() {
        return Err("stream /Length runs past end of file".into());
    }
    // The bytes after the declared length must be the endstream marker; if
    // /Length is wrong the file is broken even though the text may look fine.
    let after = ascii(&bytes[start + len..(start + len + 12).min(bytes.len())]);
    if !after.trim_start().starts_with("endstream") {
        return Err(format!(
            "/Length {len} does not reach endstream (found {after:?})"
        ));
    }
    Ok(bytes[start..start + len].to_vec())
}

/// Pull text runs and fill counts out of a content stream.
fn parse_content(stream: &[u8]) -> (Vec<TextRun>, usize) {
    let s = ascii(stream);
    let mut texts = Vec::new();
    // A fill is an `re` path immediately followed by `f` on its own line.
    let mut fill_count = 0usize;
    let lines: Vec<&str> = s.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.ends_with(" re") && lines.get(i + 1).map(|l| l.trim()) == Some("f") {
            fill_count += 1;
        }
    }

    let mut i = 0usize;
    while let Some(bt) = s[i..].find("BT\n") {
        let start = i + bt;
        let et = match s[start..].find("ET") {
            Some(e) => start + e,
            None => break,
        };
        let block = &s[start..et];
        let mut size = 0.0f32;
        let mut font = 0u8;
        let (mut x, mut y) = (0.0f32, 0.0f32);
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("/F") {
                // "/F1 9.00 Tf"
                let mut it = rest.split_whitespace();
                if let Some(f) = it.next() {
                    font = f.parse().unwrap_or(0);
                }
                if let Some(sz) = it.next() {
                    size = sz.parse().unwrap_or(0.0);
                }
            } else if line.ends_with(" Tm") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 7 {
                    x = parts[4].parse().unwrap_or(0.0);
                    y = parts[5].parse().unwrap_or(0.0);
                }
            } else if let Some(open) = line.find('(') {
                if let Some(close) = line.rfind(") Tj") {
                    if close > open {
                        texts.push(TextRun {
                            x,
                            y,
                            size,
                            font,
                            text: unescape(&line[open + 1..close]),
                        });
                    }
                }
            }
        }
        i = et + 2;
    }
    (texts, fill_count)
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Lossy ASCII view of a byte range — content streams are ASCII by
/// construction here, and a lossy conversion keeps offsets stable.
fn ascii(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

fn find_from(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// `/Key 12 0 R` → 12
fn dict_ref(dict: &str, key: &str) -> Option<u32> {
    let at = dict.find(key)?;
    let rest = &dict[at + key.len()..];
    let mut it = rest.split_whitespace();
    let id: u32 = it.next()?.parse().ok()?;
    Some(id)
}

/// `/Key 3` → 3
fn dict_int(dict: &str, key: &str) -> Option<i64> {
    let at = dict.find(key)?;
    let rest = &dict[at + key.len()..];
    rest.split_whitespace().next()?.parse().ok()
}

/// `/Kids [7 0 R 9 0 R]` → [7, 9]
fn dict_refs(dict: &str, key: &str) -> Option<Vec<u32>> {
    let at = dict.find(key)?;
    let rest = &dict[at + key.len()..];
    let open = rest.find('[')?;
    let close = rest.find(']')?;
    let inner = &rest[open + 1..close];
    let toks: Vec<&str> = inner.split_whitespace().collect();
    let mut out = Vec::new();
    for chunk in toks.chunks(3) {
        if chunk.len() == 3 && chunk[2] == "R" {
            out.push(chunk[0].parse().ok()?);
        }
    }
    Some(out)
}

/// `/MediaBox [0 0 612 792]` → [0.0, 0.0, 612.0, 792.0]
fn dict_numbers(dict: &str, key: &str) -> Option<Vec<f32>> {
    let at = dict.find(key)?;
    let rest = &dict[at + key.len()..];
    let open = rest.find('[')?;
    let close = rest.find(']')?;
    rest[open + 1..close]
        .split_whitespace()
        .map(|t| t.parse().ok())
        .collect()
}
