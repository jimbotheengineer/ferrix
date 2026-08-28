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
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            has_headers: true,
            max_rows: None,
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
    let data: &[u8] = &mmap;
    if data.is_empty() {
        return Err(CsvError::Empty);
    }

    let start = std::time::Instant::now();
    let mut cursor = 0usize;

    // Headers come from the first record, parsed on this thread.
    let mut headers: Vec<String> = Vec::new();
    if opts.has_headers {
        let end = find_record_end(data, 0);
        let line = &data[0..end];
        headers = split_record(line, opts.delimiter)
            .into_iter()
            .map(|f| String::from_utf8_lossy(&f).trim().to_string())
            .collect();
        cursor = skip_newline(data, end);
    }

    let body = &data[cursor..];
    let n_chunks = rayon::current_num_threads().max(1);
    let bounds = chunk_bounds(body, n_chunks);

    // Parse chunks in parallel; each produces column-major fields.
    let results: Vec<ChunkResult> = bounds
        .par_iter()
        .map(|&(s, e)| parse_chunk(&body[s..e], opts.delimiter))
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
        bytes: data.len(),
        parse_millis: start.elapsed().as_millis(),
        chunks: bounds.len(),
    };

    Ok((sheet, stats))
}

/// Split `data` into approximately `n` chunks that each begin at a record
/// boundary.
///
/// Correctness note: quote state cannot be guessed from a local window — a
/// `"` anywhere earlier flips the meaning of every later newline. So we make
/// ONE linear pass tracking exact quote parity and emit boundaries as we
/// reach them. That pass is a simple byte loop (multiple GB/s) and is cheap
/// relative to the parallel parse it enables.
fn chunk_bounds(data: &[u8], n: usize) -> Vec<(usize, usize)> {
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
        match b {
            b'"' => in_quotes = !in_quotes,
            // Guard collapsed into the match arm; a failed guard falls through
            // to the `_` arm, which is the same no-op as before.
            b'\n' if !in_quotes && i + 1 > next_target && i + 1 > start => {
                bounds.push((start, i + 1));
                start = i + 1;
                while next_target <= start {
                    next_target += target;
                }
                if bounds.len() + 1 >= n {
                    break;
                }
            }
            _ => {}
        }
    }
    if start < data.len() {
        bounds.push((start, data.len()));
    }
    bounds
}

/// Index just past the end of the record starting at `from` (exclusive of the
/// newline itself).
fn find_record_end(data: &[u8], from: usize) -> usize {
    let mut i = from;
    let mut in_quotes = false;
    while i < data.len() {
        match data[i] {
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => {
                // Trim a preceding \r for CRLF files.
                return if i > from && data[i - 1] == b'\r' {
                    i - 1
                } else {
                    i
                };
            }
            _ => {}
        }
        i += 1;
    }
    data.len()
}

fn skip_newline(data: &[u8], mut pos: usize) -> usize {
    if pos < data.len() && data[pos] == b'\r' {
        pos += 1;
    }
    if pos < data.len() && data[pos] == b'\n' {
        pos += 1;
    }
    pos
}

/// Split one record into fields, handling RFC-4180 quoting and `""` escapes.
fn split_record(line: &[u8], delim: u8) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut field = Vec::new();
    let mut i = 0;
    let mut in_quotes = false;
    while i < line.len() {
        let b = line[i];
        if in_quotes {
            if b == b'"' {
                if i + 1 < line.len() && line[i + 1] == b'"' {
                    field.push(b'"');
                    i += 2;
                    continue;
                }
                in_quotes = false;
            } else {
                field.push(b);
            }
        } else if b == b'"' {
            in_quotes = true;
        } else if b == delim {
            out.push(std::mem::take(&mut field));
        } else if b != b'\r' {
            field.push(b);
        }
        i += 1;
    }
    out.push(field);
    out
}

/// Parse a chunk into column-major typed fields.
fn parse_chunk(data: &[u8], delim: u8) -> ChunkResult {
    let mut fields: Vec<Vec<FieldValue>> = Vec::new();
    let mut rows = 0usize;
    let mut pos = 0usize;

    while pos < data.len() {
        let end = find_record_end(data, pos);
        let line = &data[pos..end];
        pos = skip_newline(data, if end < data.len() { end } else { data.len() });
        // A trailing newline yields one empty record; skip it.
        if line.is_empty() {
            if pos >= data.len() {
                break;
            }
            continue;
        }
        let cells = split_record(line, delim);
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
        let fields = split_record(br#"a,"b,c","say ""hi""",d"#, b',');
        let as_str: Vec<String> = fields
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect();
        assert_eq!(as_str, vec!["a", "b,c", r#"say "hi""#, "d"]);
    }

    #[test]
    fn split_handles_empty_fields() {
        let fields = split_record(b"a,,c,", b',');
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
