//! High-throughput CSV ingest.
//!
//! Strategy for large files:
//! 1. `mmap` the file so the OS pages it in — no read syscall per buffer, and
//!    the page cache is shared rather than duplicated into a heap buffer.
//! 2. Split the byte range into one chunk per core at *record boundaries*
//!    (respecting quotes), so chunks can be parsed with zero coordination.
//! 3. Parse each chunk into per-chunk column vectors on a worker thread.
//! 4. Concatenate chunk columns in order.
//!
//! Type inference is per-column and done during parse: a column stays numeric
//! until a non-numeric field appears, at which point it degrades to text.

use std::fs::File;
use std::path::Path;

use ferrix_core::{Column, Sheet, Value};
use memmap2::Mmap;
use rayon::prelude::*;

#[derive(Debug, thiserror::Error)]
pub enum CsvError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("file is empty")]
    Empty,
}

#[derive(Clone, Copy, Debug)]
pub struct CsvOptions {
    pub delimiter: u8,
    pub has_headers: bool,
    /// Cap on rows read; `None` means the whole file.
    pub max_rows: Option<usize>,
    /// Quote character (issue #31). RFC 4180 says `"`, but exports from other
    /// tools use `'`, and a file quoted with `'` parsed as if it were quoted
    /// with `"` splits every embedded delimiter into a spurious column.
    pub quote: u8,
    /// Records discarded before anything else — a title block or export
    /// banner ahead of the real header row (issue #31).
    pub skip_rows: usize,
    /// Source encoding. `None` means "the bytes are UTF-8", which is the
    /// historical behaviour and stays the zero-cost path: only a `Some` that
    /// is not UTF-8 makes the loader transcode.
    pub encoding: Option<&'static encoding_rs::Encoding>,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            has_headers: true,
            max_rows: None,
            quote: b'"',
            skip_rows: 0,
            encoding: None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct LoadStats {
    pub rows: usize,
    pub cols: usize,
    pub bytes: usize,
    pub parse_millis: u128,
    pub chunks: usize,
}

impl LoadStats {
    /// Throughput in MB/s — the number that tells us if ingest is on target.
    pub fn throughput_mbps(&self) -> f64 {
        if self.parse_millis == 0 {
            return f64::INFINITY;
        }
        (self.bytes as f64 / 1_048_576.0) / (self.parse_millis as f64 / 1000.0)
    }
}

/// One chunk's worth of parsed data: raw string slices kept as owned bytes,
/// resolved into the sheet's arena during the merge phase.
struct ChunkResult {
    /// Column-major: `fields[col][row]`.
    fields: Vec<Vec<FieldValue>>,
    rows: usize,
}

/// A parsed field before arena interning.
enum FieldValue {
    Empty,
    Number(f64),
    Bool(bool),
    Text(String),
}

/// Load a CSV file into a `Sheet`.
pub fn load_csv(path: &Path, opts: CsvOptions) -> Result<(Sheet, LoadStats), CsvError> {
    let file = File::open(path)?;
    // SAFETY: we only read from the mapping, and the file is not truncated
    // underneath us during the load.
    let mmap = unsafe { Mmap::map(&file)? };
    let raw: &[u8] = &mmap;
    if raw.is_empty() {
        return Err(CsvError::Empty);
    }
    let mapped_bytes = raw.len();

    // Transcode ONLY when the caller named a non-UTF-8 encoding (issue #31).
    // The default path is untouched: `Cow::Borrowed` over the mapping, no
    // copy, no allocation, exactly the bytes the parser saw before.
    //
    // Scale note: a transcode does materialise the file as UTF-8 in heap, so
    // it costs roughly one extra file-size. That is acceptable HERE and only
    // here — `load_csv` is the in-RAM path, taken only for files the budget
    // already admitted at ~1.2x (see `lib.rs::should_use_mmap`). The
    // out-of-core converter does not transcode, which is why the wizard's
    // encoding override is documented as applying to the in-RAM path.
    let decoded = decode_to_utf8(raw, opts.encoding);
    let data: &[u8] = &decoded;

    let start = std::time::Instant::now();
    let mut cursor = 0usize;

    // A preamble is discarded before anything else: the header row is the
    // first record AFTER it, not the first record in the file.
    for _ in 0..opts.skip_rows {
        if cursor >= data.len() {
            break;
        }
        let end = find_record_end(data, cursor, opts.quote);
        cursor = skip_newline(data, end);
    }

    // Headers come from the first remaining record, parsed on this thread.
    let mut headers: Vec<String> = Vec::new();
    if opts.has_headers && cursor < data.len() {
        let end = find_record_end(data, cursor, opts.quote);
        let line = &data[cursor..end];
        headers = split_record(line, opts.delimiter, opts.quote)
            .into_iter()
            .map(|f| String::from_utf8_lossy(&f).trim().to_string())
            .collect();
        cursor = skip_newline(data, end);
    }

    let body = &data[cursor.min(data.len())..];
    let n_chunks = rayon::current_num_threads().max(1);
    let bounds = chunk_bounds_quoted(body, n_chunks, opts.quote);

    // Parse chunks in parallel; each produces column-major fields.
    let results: Vec<ChunkResult> = bounds
        .par_iter()
        .map(|&(s, e)| parse_chunk(&body[s..e], opts.delimiter, opts.quote))
        .collect();

    // Merge: build the sheet by concatenating chunks in order.
    let n_cols = results
        .iter()
        .map(|r| r.fields.len())
        .max()
        .unwrap_or(0)
        .max(headers.len());

    let mut sheet = Sheet::new(
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Sheet1".into()),
    );

    let total_rows: usize = results.iter().map(|r| r.rows).sum();

    for col_idx in 0..n_cols {
        let mut column = Column::with_capacity(total_rows);
        for chunk in &results {
            match chunk.fields.get(col_idx) {
                Some(vals) => {
                    for v in vals {
                        match v {
                            FieldValue::Empty => column.push(Value::Empty),
                            FieldValue::Number(n) => column.push(Value::Number(*n)),
                            FieldValue::Bool(b) => column.push(Value::Bool(*b)),
                            FieldValue::Text(s) => {
                                let id = sheet.arena.intern(s);
                                column.push(Value::Text(id));
                            }
                        }
                    }
                }
                None => {
                    // Ragged row: this chunk had fewer columns. Pad.
                    for _ in 0..chunk.rows {
                        column.push(Value::Empty);
                    }
                }
            }
        }
        column.shrink_to_fit();
        sheet.push_column(column);
    }

    if !headers.is_empty() {
        sheet.set_headers(headers);
    }

    let stats = LoadStats {
        rows: sheet.row_count(),
        cols: sheet.col_count(),
        // The SOURCE size, not the transcoded size — throughput is about how
        // fast the file on disk was consumed.
        bytes: mapped_bytes,
        parse_millis: start.elapsed().as_millis(),
        chunks: bounds.len(),
    };

    Ok((sheet, stats))
}

/// Bytes as UTF-8, borrowing when no conversion is needed (issue #31).
///
/// `None` and `UTF-8` both borrow. Anything else transcodes; malformed input
/// becomes U+FFFD rather than an error, because refusing to open a file over
/// one bad byte helps nobody.
fn decode_to_utf8<'a>(
    raw: &'a [u8],
    encoding: Option<&'static encoding_rs::Encoding>,
) -> std::borrow::Cow<'a, [u8]> {
    let Some(enc) = encoding else {
        return std::borrow::Cow::Borrowed(raw);
    };
    if enc == encoding_rs::UTF_8 {
        // Still strip a BOM: left in place it becomes part of the first
        // header cell, so `id` arrives as `\u{feff}id` and no formula
        // referencing that column ever matches.
        return match raw.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
            Some(rest) => std::borrow::Cow::Borrowed(rest),
            None => std::borrow::Cow::Borrowed(raw),
        };
    }
    match enc.decode(raw) {
        (std::borrow::Cow::Borrowed(s), _, _) => std::borrow::Cow::Borrowed(s.as_bytes()),
        (std::borrow::Cow::Owned(s), _, _) => std::borrow::Cow::Owned(s.into_bytes()),
    }
}

/// Split `data` into approximately `n` chunks that each begin at a record
/// boundary.
///
/// Correctness note: quote state cannot be guessed from a local window — a
/// `"` anywhere earlier flips the meaning of every later newline. So we make
/// ONE linear pass tracking exact quote parity and emit boundaries as we
/// reach them. That pass is a simple byte loop (multiple GB/s) and is cheap
/// relative to the parallel parse it enables.
pub(crate) fn chunk_bounds(data: &[u8], n: usize) -> Vec<(usize, usize)> {
    chunk_bounds_quoted(data, n, b'"')
}

/// `chunk_bounds` with a configurable quote character (issue #31).
///
/// The quote character is a parameter rather than a constant because parity
/// has to be tracked with the SAME character the field splitter will use. A
/// file quoted with `'` chunked as if it were quoted with `"` puts boundaries
/// inside quoted fields, which is the exact class of corruption the
/// single-pass design exists to avoid.
pub(crate) fn chunk_bounds_quoted(data: &[u8], n: usize, quote: u8) -> Vec<(usize, usize)> {
    if data.is_empty() {
        return vec![];
    }
    if n <= 1 || data.len() < 1 << 20 {
        return vec![(0, data.len())];
    }
    let target = (data.len() / n).max(1);
    let mut bounds = Vec::with_capacity(n);
    let mut start = 0usize;
    let mut next_target = target;
    let mut in_quotes = false;

    for (i, &b) in data.iter().enumerate() {
        if b == quote {
            in_quotes = !in_quotes;
            continue;
        }
        // Guard collapsed into the condition; a failed guard is the same
        // no-op as before.
        if b == b'\n' && !in_quotes && i + 1 > next_target && i + 1 > start {
            bounds.push((start, i + 1));
            start = i + 1;
            while next_target <= start {
                next_target += target;
            }
            if bounds.len() + 1 >= n {
                break;
            }
        }
    }
    if start < data.len() {
        bounds.push((start, data.len()));
    }
    bounds
}

/// Index just past the end of the record starting at `from` (exclusive of the
/// newline itself).
fn find_record_end(data: &[u8], from: usize, quote: u8) -> usize {
    let mut i = from;
    let mut in_quotes = false;
    while i < data.len() {
        let b = data[i];
        if b == quote {
            in_quotes = !in_quotes;
        } else if b == b'\n' && !in_quotes {
            // Trim a preceding CR for CRLF files.
            return if i > from && data[i - 1] == CR {
                i - 1
            } else {
                i
            };
        }
        i += 1;
    }
    data.len()
}

/// The carriage return byte, named so a patch tool cannot mangle the escape.
const CR: u8 = 13;

fn skip_newline(data: &[u8], mut pos: usize) -> usize {
    if pos < data.len() && data[pos] == CR {
        pos += 1;
    }
    if pos < data.len() && data[pos] == b'\n' {
        pos += 1;
    }
    pos
}

/// Split one record into fields, handling RFC-4180 quoting and doubled-quote
/// escapes with a configurable quote character.
fn split_record(line: &[u8], delim: u8, quote: u8) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut field = Vec::new();
    let mut i = 0;
    let mut in_quotes = false;
    while i < line.len() {
        let b = line[i];
        if in_quotes {
            if b == quote {
                if i + 1 < line.len() && line[i + 1] == quote {
                    field.push(quote);
                    i += 2;
                    continue;
                }
                in_quotes = false;
            } else {
                field.push(b);
            }
        } else if b == quote {
            in_quotes = true;
        } else if b == delim {
            out.push(std::mem::take(&mut field));
        } else if b != CR {
            field.push(b);
        }
        i += 1;
    }
    out.push(field);
    out
}

/// Parse a chunk into column-major typed fields.
fn parse_chunk(data: &[u8], delim: u8, quote: u8) -> ChunkResult {
    let mut fields: Vec<Vec<FieldValue>> = Vec::new();
    let mut rows = 0usize;
    let mut pos = 0usize;

    while pos < data.len() {
        let end = find_record_end(data, pos, quote);
        let line = &data[pos..end];
        pos = skip_newline(data, if end < data.len() { end } else { data.len() });
        // A trailing newline yields one empty record; skip it.
        if line.is_empty() {
            if pos >= data.len() {
                break;
            }
            continue;
        }
        let cells = split_record(line, delim, quote);
        if fields.len() < cells.len() {
            // A later row is wider: back-fill the new columns with Empty.
            let pad = rows;
            fields.resize_with(cells.len(), || {
                let mut v = Vec::new();
                for _ in 0..pad {
                    v.push(FieldValue::Empty);
                }
                v
            });
        }
        for (c, cell) in cells.iter().enumerate() {
            fields[c].push(infer(cell));
        }
        // Rows narrower than the running width get padded.
        for f in fields.iter_mut().skip(cells.len()) {
            f.push(FieldValue::Empty);
        }
        rows += 1;
    }

    ChunkResult { fields, rows }
}

/// Infer a field's type. Numeric parsing is the hot path, so we reject
/// obviously non-numeric bytes before calling the float parser.
#[inline]
fn infer(raw: &[u8]) -> FieldValue {
    let s = raw.trim_ascii();
    if s.is_empty() {
        return FieldValue::Empty;
    }
    if looks_numeric(s) {
        if let Ok(txt) = std::str::from_utf8(s) {
            if let Ok(n) = txt.parse::<f64>() {
                return FieldValue::Number(n);
            }
        }
    }
    match s {
        b"true" | b"TRUE" | b"True" => return FieldValue::Bool(true),
        b"false" | b"FALSE" | b"False" => return FieldValue::Bool(false),
        _ => {}
    }
    FieldValue::Text(String::from_utf8_lossy(s).into_owned())
}

/// Cheap pre-filter: a numeric field starts with a digit, sign, or dot.
#[inline]
fn looks_numeric(s: &[u8]) -> bool {
    matches!(s[0], b'0'..=b'9' | b'-' | b'+' | b'.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::CellRef;
    use std::io::Write;

    fn write_temp(name: &str, content: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ferrix_test_{name}.csv"));
        let mut f = File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn split_handles_quotes_and_escapes() {
        let fields = split_record(br#"a,"b,c","say ""hi""",d"#, b',', b'"');
        let as_str: Vec<String> = fields
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect();
        assert_eq!(as_str, vec!["a", "b,c", r#"say "hi""#, "d"]);
    }

    #[test]
    fn split_handles_empty_fields() {
        let fields = split_record(b"a,,c,", b',', b'"');
        assert_eq!(fields.len(), 4);
        assert!(fields[1].is_empty());
        assert!(fields[3].is_empty());
    }

    #[test]
    fn type_inference() {
        assert!(matches!(infer(b""), FieldValue::Empty));
        assert!(matches!(infer(b"  "), FieldValue::Empty));
        assert!(matches!(infer(b"42"), FieldValue::Number(n) if n == 42.0));
        assert!(matches!(infer(b"-3.5"), FieldValue::Number(n) if n == -3.5));
        assert!(matches!(infer(b"1e6"), FieldValue::Number(n) if n == 1e6));
        assert!(matches!(infer(b"TRUE"), FieldValue::Bool(true)));
        assert!(matches!(infer(b"false"), FieldValue::Bool(false)));
        assert!(matches!(infer(b"hello"), FieldValue::Text(_)));
        // Leading-zero identifiers must not silently become numbers... they do
        // parse as numbers, which matches Excel's behaviour.
        assert!(matches!(infer(b"007"), FieldValue::Number(n) if n == 7.0));
    }

    #[test]
    fn loads_simple_csv() {
        let p = write_temp(
            "simple",
            "id,name,score\n1,alice,90.5\n2,bob,85\n3,carol,\n",
        );
        let (sheet, stats) = load_csv(&p, CsvOptions::default()).unwrap();
        assert_eq!(stats.rows, 3);
        assert_eq!(stats.cols, 3);
        assert_eq!(sheet.headers(), &["id", "name", "score"]);
        assert_eq!(sheet.get(CellRef::new(0, 0)), Value::Number(1.0));
        assert_eq!(sheet.display(CellRef::new(1, 1)), "bob");
        assert_eq!(sheet.get(CellRef::new(0, 2)), Value::Number(90.5));
        assert_eq!(sheet.get(CellRef::new(2, 2)), Value::Empty);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn handles_crlf() {
        let p = write_temp("crlf", "a,b\r\n1,2\r\n3,4\r\n");
        let (sheet, stats) = load_csv(&p, CsvOptions::default()).unwrap();
        assert_eq!(stats.rows, 2);
        assert_eq!(sheet.get(CellRef::new(0, 1)), Value::Number(2.0));
        assert_eq!(sheet.get(CellRef::new(1, 0)), Value::Number(3.0));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn handles_ragged_rows() {
        let p = write_temp("ragged", "a,b,c\n1\n2,3\n4,5,6\n");
        let (sheet, stats) = load_csv(&p, CsvOptions::default()).unwrap();
        assert_eq!(stats.rows, 3);
        assert_eq!(sheet.get(CellRef::new(0, 0)), Value::Number(1.0));
        assert_eq!(sheet.get(CellRef::new(0, 1)), Value::Empty);
        assert_eq!(sheet.get(CellRef::new(2, 2)), Value::Number(6.0));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn no_header_mode() {
        let p = write_temp("nohdr", "1,2\n3,4\n");
        let opts = CsvOptions {
            has_headers: false,
            ..Default::default()
        };
        let (sheet, stats) = load_csv(&p, opts).unwrap();
        assert_eq!(stats.rows, 2);
        assert_eq!(sheet.get(CellRef::new(0, 0)), Value::Number(1.0));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn parallel_chunks_preserve_row_order() {
        // Large enough to trigger multi-chunk parsing.
        let mut content = String::from("n\n");
        for i in 0..300_000 {
            content.push_str(&format!("{i}\n"));
        }
        let p = write_temp("order", &content);
        let (sheet, stats) = load_csv(&p, CsvOptions::default()).unwrap();
        assert_eq!(stats.rows, 300_000);
        assert!(
            stats.chunks > 1,
            "expected parallel chunking, got {}",
            stats.chunks
        );
        // Order must be exactly preserved across the chunk merge.
        for probe in [0usize, 1, 99_999, 150_000, 299_999] {
            assert_eq!(
                sheet.get(CellRef::new(probe as u32, 0)),
                Value::Number(probe as f64),
                "row {probe} out of order"
            );
        }
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn quoted_newline_survives_chunking() {
        let mut content = String::from("a,b\n");
        for i in 0..50_000 {
            content.push_str(&format!("{i},\"line one\nline two\"\n"));
        }
        let p = write_temp("qnl", &content);
        let (sheet, stats) = load_csv(&p, CsvOptions::default()).unwrap();
        assert_eq!(stats.rows, 50_000, "embedded newlines split records");
        assert_eq!(sheet.display(CellRef::new(0, 1)), "line one\nline two");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn empty_file_errors() {
        let p = write_temp("empty", "");
        assert!(matches!(
            load_csv(&p, CsvOptions::default()),
            Err(CsvError::Empty)
        ));
        let _ = std::fs::remove_file(p);
    }
}
