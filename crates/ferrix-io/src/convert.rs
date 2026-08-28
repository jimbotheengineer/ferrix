//! Streaming CSV -> `.ferrix` conversion.
//!
//! The constraint that shapes everything here: converting a 10GB CSV must not
//! need 10GB of RAM. So we never hold the whole dataset. Instead:
//!
//! 1. Read the CSV in fixed-size blocks (record-aligned, quote-aware).
//! 2. Split each block into per-core sub-chunks at exact record boundaries and
//!    parse them in parallel into per-column encoded buffers.
//! 3. Merge the sub-chunks back in source order, appending straight to
//!    per-column spill files on disk.
//! 4. Concatenate the spills into the final `.ferrix` layout.
//!
//! Peak memory is one block plus its parsed form plus the string arena —
//! bounded and independent of file size. The arena is the one thing that must
//! stay resident, which is fine because spreadsheet text is low-cardinality
//! (the 10M-row benchmark has 18 distinct strings); a pathological
//! all-unique-text column is the known worst case and is reported rather than
//! silently OOMing.
//!
//! ## Why the parallelism is shaped this way
//!
//! Parsing is the bottleneck (field splitting and `f64` parsing dominate), and
//! it is embarrassingly parallel *provided* chunks split on real record
//! boundaries. We reuse `csv::chunk_bounds`, the exact single-pass quote-aware
//! splitter, rather than any windowed heuristic — a previous 64KB quote-parity
//! guess silently corrupted records containing embedded newlines.
//!
//! Two things must stay deterministic for the output to be byte-identical to
//! the old serial converter:
//!
//! * **Row order.** `chunk_bounds` yields chunks in source order, rayon's
//!   indexed `collect` preserves that order, and the merge walks them in
//!   order. No row can overtake another.
//! * **String ids.** The arena assigns ids by first appearance in row order.
//!   Workers therefore intern into a *chunk-local* table (also first-appearance
//!   order) and emit local ids; the serial merge replays each chunk's local
//!   table into the global arena in order, producing exactly the id sequence a
//!   single-threaded scan would. Cell ids are remapped through that table.
//!   A concurrent map was considered and rejected: it would need a lock or CAS
//!   on the hot path for every text cell, and it cannot reproduce
//!   first-appearance ordering without extra synchronisation anyway.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use ferrix_core::{StringArena, ValueTag};
use rayon::prelude::*;

use crate::csv::chunk_bounds;
use crate::format::{
    align8, ColumnDesc, FormatError, Header, ABSENT, COL_DESC_BYTES, HEADER_BYTES,
};

/// Bytes of CSV to read at a time. 32MB is smaller than the old serial 64MB
/// because a block now also holds its decoded form (up to ~13 bytes per cell)
/// while it is merged; halving the block keeps total peak well inside budget
/// and still gives every core a multi-MB sub-chunk to chew on.
const BLOCK: usize = 32 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("format error: {0}")]
    Format(#[from] FormatError),
    #[error("source file is empty")]
    Empty,
    #[error(
        "string arena exceeded {limit_mb} MB — this CSV has too many distinct strings to convert"
    )]
    ArenaTooLarge { limit_mb: usize },
}

#[derive(Debug, Clone)]
pub struct ConvertStats {
    pub rows: u64,
    pub cols: usize,
    pub source_bytes: u64,
    pub output_bytes: u64,
    pub distinct_strings: usize,
    pub millis: u128,
    pub peak_block_bytes: usize,
}

impl ConvertStats {
    pub fn throughput_mbps(&self) -> f64 {
        if self.millis == 0 {
            return f64::INFINITY;
        }
        (self.source_bytes as f64 / 1_048_576.0) / (self.millis as f64 / 1000.0)
    }
}

/// Cap on arena growth. Beyond this we bail with a clear error instead of
/// consuming all memory on a file that is pathologically string-heavy.
const ARENA_LIMIT: usize = 2 << 30; // 2 GB

/// The canonical cache path for a source file: `data.csv` -> `data.ferrix`.
pub fn cache_path_for(source: &Path) -> PathBuf {
    source.with_extension("ferrix")
}

/// Is the cache present and newer than the source?
pub fn cache_is_fresh(source: &Path, cache: &Path) -> bool {
    let (Ok(s), Ok(c)) = (source.metadata(), cache.metadata()) else {
        return false;
    };
    let (Ok(sm), Ok(cm)) = (s.modified(), c.modified()) else {
        return false;
    };
    // Also require a valid header, so a truncated cache from an interrupted
    // conversion is never mistaken for a usable one.
    cm >= sm && header_is_valid(cache)
}

fn header_is_valid(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let mut buf = [0u8; HEADER_BYTES];
    if f.read_exact(&mut buf).is_err() {
        return false;
    }
    match Header::parse(&buf) {
        Ok(h) => {
            // The file must be at least as long as the header claims.
            let declared_min = h.arena_spans_off + h.arena_spans * 8;
            path.metadata()
                .map(|m| m.len() >= declared_min)
                .unwrap_or(false)
        }
        Err(_) => false,
    }
}

/// Per-column spill file, written as we stream.
struct Spill {
    tags: BufWriter<File>,
    nums: Option<BufWriter<File>>,
    strs: Option<BufWriter<File>>,
    tags_path: PathBuf,
    nums_path: PathBuf,
    strs_path: PathBuf,
    len: u64,
    /// Whether this column has yet needed a numeric or string section.
    has_nums: bool,
    has_strs: bool,
}

impl Spill {
    fn new(dir: &Path, idx: usize) -> Result<Self, ConvertError> {
        let tags_path = dir.join(format!("c{idx}.tags"));
        let nums_path = dir.join(format!("c{idx}.nums"));
        let strs_path = dir.join(format!("c{idx}.strs"));
        Ok(Self {
            tags: BufWriter::with_capacity(1 << 20, File::create(&tags_path)?),
            nums: None,
            strs: None,
            tags_path,
            nums_path,
            strs_path,
            len: 0,
            has_nums: false,
            has_strs: false,
        })
    }

    /// Lazily create the numeric section, back-filling zeros for rows already
    /// written — this is how a column that turns numeric halfway through a
    /// 200M-row file stays correct without pre-allocating.
    fn ensure_nums(&mut self) -> Result<(), ConvertError> {
        if self.has_nums {
            return Ok(());
        }
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&self.nums_path)?);
        let zeros = [0u8; 8];
        for _ in 0..self.len {
            w.write_all(&zeros)?;
        }
        self.nums = Some(w);
        self.has_nums = true;
        Ok(())
    }

    fn ensure_strs(&mut self) -> Result<(), ConvertError> {
        if self.has_strs {
            return Ok(());
        }
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&self.strs_path)?);
        let zeros = [0u8; 4];
        for _ in 0..self.len {
            w.write_all(&zeros)?;
        }
        self.strs = Some(w);
        self.has_strs = true;
        Ok(())
    }

    fn push_empty(&mut self) -> Result<(), ConvertError> {
        self.tags.write_all(&[ValueTag::Empty as u8])?;
        if let Some(n) = &mut self.nums {
            n.write_all(&0f64.to_le_bytes())?;
        }
        if let Some(s) = &mut self.strs {
            s.write_all(&0u32.to_le_bytes())?;
        }
        self.len += 1;
        Ok(())
    }

    fn push_number(&mut self, v: f64) -> Result<(), ConvertError> {
        self.ensure_nums()?;
        self.tags.write_all(&[ValueTag::Number as u8])?;
        self.nums.as_mut().unwrap().write_all(&v.to_le_bytes())?;
        if let Some(s) = &mut self.strs {
            s.write_all(&0u32.to_le_bytes())?;
        }
        self.len += 1;
        Ok(())
    }

    fn push_bool(&mut self, b: bool) -> Result<(), ConvertError> {
        self.ensure_nums()?;
        self.tags.write_all(&[ValueTag::Bool as u8])?;
        let v = if b { 1f64 } else { 0f64 };
        self.nums.as_mut().unwrap().write_all(&v.to_le_bytes())?;
        if let Some(s) = &mut self.strs {
            s.write_all(&0u32.to_le_bytes())?;
        }
        self.len += 1;
        Ok(())
    }

    fn push_text(&mut self, id: u32) -> Result<(), ConvertError> {
        self.ensure_strs()?;
        self.tags.write_all(&[ValueTag::Text as u8])?;
        self.strs.as_mut().unwrap().write_all(&id.to_le_bytes())?;
        if let Some(n) = &mut self.nums {
            n.write_all(&0f64.to_le_bytes())?;
        }
        self.len += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<FinishedSpill, ConvertError> {
        self.tags.flush()?;
        if let Some(mut n) = self.nums.take() {
            n.flush()?;
        }
        if let Some(mut s) = self.strs.take() {
            s.flush()?;
        }
        Ok(FinishedSpill {
            tags_path: self.tags_path,
            nums_path: self.has_nums.then_some(self.nums_path),
            strs_path: self.has_strs.then_some(self.strs_path),
            len: self.len,
        })
    }
}

struct FinishedSpill {
    tags_path: PathBuf,
    nums_path: Option<PathBuf>,
    strs_path: Option<PathBuf>,
    len: u64,
}

/// Convert a CSV into a `.ferrix` cache. Calls `progress` with (bytes_done,
/// bytes_total) periodically so the UI can show a bar on a multi-minute job.
pub fn convert_csv<F>(
    source: &Path,
    dest: &Path,
    delimiter: u8,
    has_headers: bool,
    mut progress: F,
) -> Result<ConvertStats, ConvertError>
where
    F: FnMut(u64, u64),
{
    let start = std::time::Instant::now();
    let source_bytes = source.metadata()?.len();
    if source_bytes == 0 {
        return Err(ConvertError::Empty);
    }

    // Spill files live beside the destination and are removed at the end.
    let scratch = dest.with_extension("ferrix-tmp");
    std::fs::create_dir_all(&scratch)?;

    let result = convert_inner(
        source,
        dest,
        &scratch,
        delimiter,
        has_headers,
        source_bytes,
        &mut progress,
    );

    // Always clean up scratch, success or failure — a failed conversion must
    // not leave gigabytes of spill files behind.
    let _ = std::fs::remove_dir_all(&scratch);
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }

    let mut stats = result?;
    stats.millis = start.elapsed().as_millis();
    Ok(stats)
}

#[allow(clippy::too_many_arguments)]
fn convert_inner<F>(
    source: &Path,
    dest: &Path,
    scratch: &Path,
    delimiter: u8,
    has_headers: bool,
    source_bytes: u64,
    progress: &mut F,
) -> Result<ConvertStats, ConvertError>
where
    F: FnMut(u64, u64),
{
    let mut file = File::open(source)?;
    let mut arena = StringArena::new();
    let mut spills: Vec<Spill> = Vec::new();
    let mut headers: Vec<String> = Vec::new();
    let mut rows: u64 = 0;

    let mut buf = vec![0u8; BLOCK];
    let mut carry: Vec<u8> = Vec::new();
    let mut consumed: u64 = 0;
    let mut first_record = true;
    let mut peak_bytes = BLOCK;

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        consumed += n as u64;

        // Prepend anything left over from the previous block.
        let mut chunk = Vec::with_capacity(carry.len() + n);
        chunk.extend_from_slice(&carry);
        chunk.extend_from_slice(&buf[..n]);
        carry.clear();

        // Find the last complete record; the tail carries to the next block.
        let split = last_record_end(&chunk);
        let (complete, tail) = chunk.split_at(split);
        carry.extend_from_slice(tail);

        parse_block_parallel(
            complete,
            delimiter,
            has_headers,
            &mut first_record,
            &mut headers,
            &mut arena,
            &mut spills,
            &mut rows,
            scratch,
            &mut peak_bytes,
        )?;

        if arena.data_bytes() > ARENA_LIMIT {
            return Err(ConvertError::ArenaTooLarge {
                limit_mb: ARENA_LIMIT >> 20,
            });
        }
        progress(consumed, source_bytes);
    }

    // Whatever is left in `carry` is a final record with no trailing newline.
    if !carry.is_empty() {
        parse_block_parallel(
            &carry,
            delimiter,
            has_headers,
            &mut first_record,
            &mut headers,
            &mut arena,
            &mut spills,
            &mut rows,
            scratch,
            &mut peak_bytes,
        )?;
    }

    // Ragged data: pad every column out to the full row count.
    for s in &mut spills {
        while s.len < rows {
            s.push_empty()?;
        }
    }

    let cols = spills.len();
    let finished: Vec<FinishedSpill> = spills
        .into_iter()
        .map(|s| s.finish())
        .collect::<Result<_, _>>()?;

    let output_bytes = assemble(dest, &finished, &arena, rows, &headers)?;

    Ok(ConvertStats {
        rows,
        cols,
        source_bytes,
        output_bytes,
        distinct_strings: arena.len(),
        millis: 0,
        peak_block_bytes: peak_bytes,
    })
}

/// Index just past the final complete record in `data`, quote-aware.
fn last_record_end(data: &[u8]) -> usize {
    let mut in_quotes = false;
    let mut last = 0usize;
    for (i, &b) in data.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => last = i + 1,
            _ => {}
        }
    }
    last
}

/// One parsed sub-chunk, held in source order.
///
/// Cells are stored flat (row-major) rather than per-column because a worker
/// cannot know the final column count — that is only settled once every chunk
/// has been seen. `row_widths` lets the serial merge replay the exact
/// widen/pad decisions the old single-threaded loop made.
#[derive(Default)]
struct ChunkOut {
    /// One tag byte per cell, row-major.
    tags: Vec<u8>,
    /// One payload per cell: `f64` bits for Number/Bool, chunk-local string
    /// index for Text, 0 for Empty.
    payloads: Vec<u64>,
    /// Field count of each record, in order.
    row_widths: Vec<u32>,
    /// Chunk-local strings, in first-appearance order.
    strings: Vec<String>,
}

impl ChunkOut {
    /// Bytes this chunk occupies, for the peak-memory accounting.
    fn heap_bytes(&self) -> usize {
        self.tags.capacity()
            + self.payloads.capacity() * 8
            + self.row_widths.capacity() * 4
            + self.strings.iter().map(|s| s.len() + 24).sum::<usize>()
    }
}

/// Parse one sub-chunk with no shared state. Pure function of its bytes, which
/// is what makes the parallel pass safe and the result deterministic.
fn parse_chunk_cells(data: &[u8], delim: u8) -> ChunkOut {
    use std::collections::HashMap;
    let mut out = ChunkOut::default();
    // Chunk-local interner. First-appearance order is what the merge replays
    // into the global arena, so it must be an ordered Vec plus an index.
    let mut local: HashMap<Box<str>, u32> = HashMap::new();

    let mut pos = 0usize;
    while pos < data.len() {
        let end = record_end(data, pos);
        let line = &data[pos..end];
        pos = skip_newline(data, end);
        if line.is_empty() {
            continue;
        }

        let fields = split_record(line, delim);
        out.row_widths.push(fields.len() as u32);

        for f in &fields {
            let t = trim_ascii(f);
            if t.is_empty() {
                out.tags.push(ValueTag::Empty as u8);
                out.payloads.push(0);
            } else if looks_numeric(t) {
                match std::str::from_utf8(t)
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                {
                    Some(v) => {
                        out.tags.push(ValueTag::Number as u8);
                        out.payloads.push(v.to_bits());
                    }
                    None => {
                        let s = String::from_utf8_lossy(t);
                        let id = match local.get(s.as_ref()) {
                            Some(&i) => i,
                            None => {
                                let i = out.strings.len() as u32;
                                local.insert(s.as_ref().into(), i);
                                out.strings.push(s.into_owned());
                                i
                            }
                        };
                        out.tags.push(ValueTag::Text as u8);
                        out.payloads.push(id as u64);
                    }
                }
            } else if t == b"true" || t == b"TRUE" || t == b"True" {
                out.tags.push(ValueTag::Bool as u8);
                out.payloads.push(1f64.to_bits());
            } else if t == b"false" || t == b"FALSE" || t == b"False" {
                out.tags.push(ValueTag::Bool as u8);
                out.payloads.push(0f64.to_bits());
            } else {
                let s = String::from_utf8_lossy(t);
                let id = match local.get(s.as_ref()) {
                    Some(&i) => i,
                    None => {
                        let i = out.strings.len() as u32;
                        local.insert(s.as_ref().into(), i);
                        out.strings.push(s.into_owned());
                        i
                    }
                };
                out.tags.push(ValueTag::Text as u8);
                out.payloads.push(id as u64);
            }
        }
    }
    out
}

/// Split a block across cores, parse in parallel, then merge in source order.
///
/// The merge is deliberately serial: it owns the spill writers, the global
/// arena, and the running row count, all of which are order-dependent. It is
/// pure buffered `write_all` of already-decoded bytes, so it is far cheaper
/// than the parse it follows.
#[allow(clippy::too_many_arguments)]
fn parse_block_parallel(
    data: &[u8],
    delim: u8,
    has_headers: bool,
    first_record: &mut bool,
    headers: &mut Vec<String>,
    arena: &mut StringArena,
    spills: &mut Vec<Spill>,
    rows: &mut u64,
    scratch: &Path,
    peak_bytes: &mut usize,
) -> Result<(), ConvertError> {
    let mut body = data;

    // The header record is consumed on this thread before any chunking, so
    // workers only ever see data rows.
    if *first_record {
        // Leading blank records are not the header — the serial parser skipped
        // empty lines before considering the first record.
        loop {
            let end = record_end(body, 0);
            if end != 0 {
                break;
            }
            let next = skip_newline(body, end);
            if next == 0 {
                break;
            }
            body = &body[next..];
            if body.is_empty() {
                return Ok(());
            }
        }
        if has_headers {
            let end = record_end(body, 0);
            *headers = split_record(&body[..end], delim)
                .iter()
                .map(|f| String::from_utf8_lossy(f).trim().to_string())
                .collect();
            let next = skip_newline(body, end);
            body = &body[next..];
        }
        *first_record = false;
    }

    if body.is_empty() {
        return Ok(());
    }

    // Exact, quote-aware boundaries — never a windowed heuristic.
    let n_chunks = rayon::current_num_threads().max(1);
    let bounds = chunk_bounds(body, n_chunks);

    // `par_iter().collect()` on an indexed parallel iterator preserves order,
    // so `parsed[i]` is always chunk `i` of the source.
    let parsed: Vec<ChunkOut> = bounds
        .par_iter()
        .map(|&(s, e)| parse_chunk_cells(&body[s..e], delim))
        .collect();

    *peak_bytes = (*peak_bytes).max(BLOCK + parsed.iter().map(|c| c.heap_bytes()).sum::<usize>());

    for chunk in &parsed {
        // Replay this chunk's local strings into the global arena in
        // first-appearance order. Because chunks are merged in source order,
        // the resulting global ids match a single-threaded scan exactly.
        let remap: Vec<u32> = chunk.strings.iter().map(|s| arena.intern(s).0).collect();

        let mut cell = 0usize;
        for &w in &chunk.row_widths {
            let w = w as usize;

            // Widen if this row has more columns than any seen so far.
            while spills.len() < w {
                let idx = spills.len();
                let mut s = Spill::new(scratch, idx)?;
                // Back-fill the rows that came before this column existed.
                for _ in 0..*rows {
                    s.push_empty()?;
                }
                spills.push(s);
            }

            for i in 0..w {
                let tag = chunk.tags[cell + i];
                let p = chunk.payloads[cell + i];
                if tag == ValueTag::Number as u8 {
                    spills[i].push_number(f64::from_bits(p))?;
                } else if tag == ValueTag::Bool as u8 {
                    spills[i].push_bool(f64::from_bits(p) != 0.0)?;
                } else if tag == ValueTag::Text as u8 {
                    spills[i].push_text(remap[p as usize])?;
                } else {
                    spills[i].push_empty()?;
                }
            }
            cell += w;

            // Narrower rows: pad the remaining columns.
            for s in spills.iter_mut().skip(w) {
                s.push_empty()?;
            }
            *rows += 1;
        }
    }
    Ok(())
}

/// Stitch spill files and the arena into the final `.ferrix` layout.
fn assemble(
    dest: &Path,
    spills: &[FinishedSpill],
    arena: &StringArena,
    rows: u64,
    _headers: &[String],
) -> Result<u64, ConvertError> {
    let mut out = BufWriter::with_capacity(8 << 20, File::create(dest)?);

    // Reserve the header; offsets are patched in at the end.
    out.write_all(&[0u8; HEADER_BYTES])?;
    let mut off = HEADER_BYTES as u64;

    // Column table placeholder.
    let col_table_off = off;
    let table_bytes = (spills.len() * COL_DESC_BYTES) as u64;
    for _ in 0..spills.len() {
        out.write_all(&[0u8; COL_DESC_BYTES])?;
    }
    off += table_bytes;
    off = pad_writer(&mut out, off)?;

    // String arena bytes, then spans.
    let arena_data_off = off;
    let (bytes, spans) = arena.raw_parts();
    out.write_all(bytes)?;
    off += bytes.len() as u64;
    off = pad_writer(&mut out, off)?;

    let arena_spans_off = off;
    for (s, l) in spans {
        out.write_all(&s.to_le_bytes())?;
        out.write_all(&l.to_le_bytes())?;
    }
    off += (spans.len() * 8) as u64;
    off = pad_writer(&mut out, off)?;

    // Column data, recording each section's offset.
    let mut descs = Vec::with_capacity(spills.len());
    for s in spills {
        let tags_off = off;
        off += copy_file(&mut out, &s.tags_path)?;
        off = pad_writer(&mut out, off)?;

        let nums_off = match &s.nums_path {
            Some(p) => {
                let o = off;
                off += copy_file(&mut out, p)?;
                off = pad_writer(&mut out, off)?;
                o
            }
            None => ABSENT,
        };

        let strs_off = match &s.strs_path {
            Some(p) => {
                let o = off;
                off += copy_file(&mut out, p)?;
                off = pad_writer(&mut out, off)?;
                o
            }
            None => ABSENT,
        };

        descs.push(ColumnDesc {
            tags_off,
            nums_off,
            strs_off,
            len: s.len,
        });
    }

    out.flush()?;
    let mut f = out.into_inner().map_err(|e| e.into_error())?;

    // Patch the header and column table now that offsets are known.
    f.seek(SeekFrom::Start(0))?;
    Header {
        version: crate::format::VERSION,
        rows,
        cols: spills.len() as u32,
        col_table_off,
        arena_data_off,
        arena_data_len: bytes.len() as u64,
        arena_spans_off,
        arena_spans: spans.len() as u64,
    }
    .write_to(&mut f)?;

    f.seek(SeekFrom::Start(col_table_off))?;
    for d in &descs {
        d.write_to(&mut f)?;
    }
    f.flush()?;

    Ok(f.metadata()?.len())
}

fn pad_writer<W: Write>(w: &mut W, written: u64) -> Result<u64, ConvertError> {
    let target = align8(written);
    let pad = (target - written) as usize;
    if pad > 0 {
        w.write_all(&[0u8; 8][..pad])?;
    }
    Ok(target)
}

/// Stream a spill file into the output without loading it.
fn copy_file<W: Write>(out: &mut W, path: &Path) -> Result<u64, ConvertError> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        total += n as u64;
    }
    Ok(total)
}

fn record_end(data: &[u8], from: usize) -> usize {
    let mut i = from;
    let mut in_quotes = false;
    while i < data.len() {
        match data[i] {
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => {
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

#[inline]
fn trim_ascii(s: &[u8]) -> &[u8] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && s[a].is_ascii_whitespace() {
        a += 1;
    }
    while b > a && s[b - 1].is_ascii_whitespace() {
        b -= 1;
    }
    &s[a..b]
}

#[inline]
fn looks_numeric(s: &[u8]) -> bool {
    !s.is_empty() && matches!(s[0], b'0'..=b'9' | b'-' | b'+' | b'.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("ferrix_conv_{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_csv(name: &str, content: &str) -> PathBuf {
        let p = temp_dir().join(name);
        let mut f = File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn cache_path_replaces_extension() {
        assert_eq!(
            cache_path_for(Path::new("/data/big.csv")),
            PathBuf::from("/data/big.ferrix")
        );
    }

    #[test]
    fn last_record_end_is_quote_aware() {
        // The newline inside quotes must not be treated as a record boundary.
        let d = b"a,\"x\ny\",b\nnext,row,here\npartial";
        let end = last_record_end(d);
        assert_eq!(&d[end..], b"partial");
    }

    #[test]
    fn converts_and_reports_stats() {
        let src = write_csv(
            "basic.csv",
            "id,name,val\n1,alice,10.5\n2,bob,20\n3,carol,\n",
        );
        let dst = cache_path_for(&src);
        let stats = convert_csv(&src, &dst, b',', true, |_, _| {}).unwrap();
        assert_eq!(stats.rows, 3);
        assert_eq!(stats.cols, 3);
        assert!(dst.exists());
        assert!(stats.output_bytes > 0);
        // alice/bob/carol
        assert_eq!(stats.distinct_strings, 3);
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn output_has_valid_header() {
        let src = write_csv("hdr.csv", "a,b\n1,2\n3,4\n");
        let dst = cache_path_for(&src);
        convert_csv(&src, &dst, b',', true, |_, _| {}).unwrap();

        let data = std::fs::read(&dst).unwrap();
        let h = Header::parse(&data).unwrap();
        assert_eq!(h.rows, 2);
        assert_eq!(h.cols, 2);
        assert_eq!(h.version, crate::format::VERSION);
        // Every declared section must lie inside the file.
        assert!(h.col_table_off < data.len() as u64);
        assert!(h.arena_spans_off <= data.len() as u64);
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn sections_are_eight_byte_aligned() {
        // Direct f64 reads out of the mapping require this.
        let src = write_csv("align.csv", "a,b,c\n1,x,2.5\n2,yy,3.5\n");
        let dst = cache_path_for(&src);
        convert_csv(&src, &dst, b',', true, |_, _| {}).unwrap();
        let data = std::fs::read(&dst).unwrap();
        let h = Header::parse(&data).unwrap();

        for i in 0..h.cols as usize {
            let off = h.col_table_off as usize + i * COL_DESC_BYTES;
            let d = ColumnDesc::parse(&data[off..]).unwrap();
            if d.has_numbers() {
                assert_eq!(d.nums_off % 8, 0, "column {i} numeric section misaligned");
            }
            if d.has_strings() {
                assert_eq!(d.strs_off % 8, 0, "column {i} string section misaligned");
            }
        }
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn text_only_column_has_no_numeric_section() {
        let src = write_csv("textonly.csv", "n,t\n1,alpha\n2,beta\n");
        let dst = cache_path_for(&src);
        convert_csv(&src, &dst, b',', true, |_, _| {}).unwrap();
        let data = std::fs::read(&dst).unwrap();
        let h = Header::parse(&data).unwrap();

        let d0 = ColumnDesc::parse(&data[h.col_table_off as usize..]).unwrap();
        let d1 = ColumnDesc::parse(&data[h.col_table_off as usize + COL_DESC_BYTES..]).unwrap();
        assert!(
            d0.has_numbers(),
            "numeric column should have a nums section"
        );
        assert!(!d0.has_strings());
        assert!(d1.has_strings(), "text column should have a strs section");
        assert!(
            !d1.has_numbers(),
            "text-only column must not allocate an f64 array"
        );
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn handles_ragged_and_crlf() {
        let src = write_csv("ragged.csv", "a,b,c\r\n1\r\n2,3\r\n4,5,6\r\n");
        let dst = cache_path_for(&src);
        let stats = convert_csv(&src, &dst, b',', true, |_, _| {}).unwrap();
        assert_eq!(stats.rows, 3);
        assert_eq!(stats.cols, 3, "widest row defines the column count");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn progress_reaches_completion() {
        let mut content = String::from("n\n");
        for i in 0..5000 {
            content.push_str(&format!("{i}\n"));
        }
        let src = write_csv("progress.csv", &content);
        let dst = cache_path_for(&src);
        let mut last = (0u64, 0u64);
        convert_csv(&src, &dst, b',', true, |done, total| last = (done, total)).unwrap();
        assert_eq!(last.0, last.1, "progress must end at 100%");
        assert!(last.1 > 0);
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn empty_source_is_rejected() {
        let src = write_csv("empty.csv", "");
        let dst = cache_path_for(&src);
        assert!(matches!(
            convert_csv(&src, &dst, b',', true, |_, _| {}),
            Err(ConvertError::Empty)
        ));
        assert!(!dst.exists(), "failed conversion must not leave a file");
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn scratch_dir_is_cleaned_up() {
        let src = write_csv("scratch.csv", "a,b\n1,2\n3,4\n");
        let dst = cache_path_for(&src);
        convert_csv(&src, &dst, b',', true, |_, _| {}).unwrap();
        let scratch = dst.with_extension("ferrix-tmp");
        assert!(
            !scratch.exists(),
            "spill directory must be removed after conversion"
        );
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn cache_freshness_tracks_source() {
        let src = write_csv("fresh.csv", "a\n1\n2\n");
        let dst = cache_path_for(&src);
        assert!(!cache_is_fresh(&src, &dst), "no cache yet");
        convert_csv(&src, &dst, b',', true, |_, _| {}).unwrap();
        assert!(cache_is_fresh(&src, &dst), "cache should be fresh");

        // A truncated cache must never be considered usable.
        File::create(&dst).unwrap().write_all(b"garbage").unwrap();
        assert!(
            !cache_is_fresh(&src, &dst),
            "corrupt cache must be rejected"
        );
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    // --- parallel-conversion invariants ---------------------------------

    /// The serial reference implementation, kept in the test module so the
    /// parallel converter can be diffed against it byte for byte. This is a
    /// faithful copy of the pre-parallel `parse_block` loop.
    fn convert_serial(src: &Path, dst: &Path, delim: u8, has_headers: bool) {
        let scratch = dst.with_extension("ferrix-serial-tmp");
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();

        let data = std::fs::read(src).unwrap();
        let mut arena = StringArena::new();
        let mut spills: Vec<Spill> = Vec::new();
        let mut rows: u64 = 0;
        let mut first_record = true;

        let mut pos = 0usize;
        while pos < data.len() {
            let end = record_end(&data, pos);
            let line = &data[pos..end];
            pos = skip_newline(&data, end);
            if line.is_empty() {
                continue;
            }
            let fields = split_record(line, delim);
            if first_record && has_headers {
                first_record = false;
                continue;
            }
            first_record = false;

            while spills.len() < fields.len() {
                let idx = spills.len();
                let mut s = Spill::new(&scratch, idx).unwrap();
                for _ in 0..rows {
                    s.push_empty().unwrap();
                }
                spills.push(s);
            }
            for (i, f) in fields.iter().enumerate() {
                let t = trim_ascii(f);
                if t.is_empty() {
                    spills[i].push_empty().unwrap();
                } else if looks_numeric(t) {
                    match std::str::from_utf8(t)
                        .ok()
                        .and_then(|s| s.parse::<f64>().ok())
                    {
                        Some(v) => spills[i].push_number(v).unwrap(),
                        None => {
                            let id = arena.intern(&String::from_utf8_lossy(t));
                            spills[i].push_text(id.0).unwrap();
                        }
                    }
                } else if t == b"true" || t == b"TRUE" || t == b"True" {
                    spills[i].push_bool(true).unwrap();
                } else if t == b"false" || t == b"FALSE" || t == b"False" {
                    spills[i].push_bool(false).unwrap();
                } else {
                    let id = arena.intern(&String::from_utf8_lossy(t));
                    spills[i].push_text(id.0).unwrap();
                }
            }
            for s in spills.iter_mut().skip(fields.len()) {
                s.push_empty().unwrap();
            }
            rows += 1;
        }
        for s in &mut spills {
            while s.len < rows {
                s.push_empty().unwrap();
            }
        }
        let finished: Vec<FinishedSpill> =
            spills.into_iter().map(|s| s.finish().unwrap()).collect();
        assemble(dst, &finished, &arena, rows, &[]).unwrap();
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// The core guarantee: parallel output must be byte-identical to serial.
    fn assert_matches_serial(name: &str, content: &str) {
        let src = write_csv(name, content);
        let par = src.with_extension("par.ferrix");
        let ser = src.with_extension("ser.ferrix");

        convert_csv(&src, &par, b',', true, |_, _| {}).unwrap();
        convert_serial(&src, &ser, b',', true);

        let a = std::fs::read(&par).unwrap();
        let b = std::fs::read(&ser).unwrap();
        assert_eq!(
            a, b,
            "parallel output differs from serial for fixture `{name}`"
        );

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&par);
        let _ = std::fs::remove_file(&ser);
    }

    #[test]
    fn parallel_matches_serial_basic() {
        let mut c = String::from("id,name,val,flag\n");
        for i in 0..20_000 {
            c.push_str(&format!(
                "{i},{},{}.{},{}\n",
                ["alpha", "beta", "gamma", "delta"][i % 4],
                i % 977,
                i % 100,
                if i % 2 == 0 { "true" } else { "false" }
            ));
        }
        assert_matches_serial("par_basic.csv", &c);
    }

    #[test]
    fn parallel_matches_serial_with_embedded_newlines() {
        // The case the old windowed heuristic corrupted: quoted fields
        // containing newlines, straddling chunk boundaries.
        let mut c = String::from("id,note,n\n");
        for i in 0..20_000 {
            c.push_str(&format!(
                "{i},\"line one\nline two, with comma\ncell {i}\",{i}\n"
            ));
        }
        assert_matches_serial("par_newlines.csv", &c);
    }

    #[test]
    fn parallel_matches_serial_ragged_and_crlf() {
        let mut c = String::from("a,b,c\r\n");
        for i in 0..20_000 {
            match i % 4 {
                0 => c.push_str(&format!("{i}\r\n")),
                1 => c.push_str(&format!("{i},x{i}\r\n")),
                2 => c.push_str(&format!("{i},y{i},{i}.5\r\n")),
                _ => c.push_str(&format!("{i},,{i}\r\n")),
            }
        }
        assert_matches_serial("par_ragged.csv", &c);
    }

    #[test]
    fn parallel_matches_serial_high_cardinality_strings() {
        // Every row introduces new strings, stressing the local->global
        // arena replay ordering across many chunks.
        let mut c = String::from("k,v\n");
        for i in 0..30_000 {
            c.push_str(&format!("key-{i},val-{}\n", i * 7 % 30_000));
        }
        assert_matches_serial("par_strings.csv", &c);
    }

    #[test]
    fn parallel_matches_serial_column_widens_late() {
        // A column that only appears near the end must back-fill exactly as
        // the serial converter did.
        let mut c = String::from("a\n");
        for i in 0..20_000 {
            c.push_str(&format!("{i}\n"));
        }
        for i in 0..100 {
            c.push_str(&format!("{i},late{i},9.5\n"));
        }
        assert_matches_serial("par_widen.csv", &c);
    }

    #[test]
    fn conversion_is_deterministic_across_runs() {
        // Byte-identical run to run, or the arena merge is racing.
        let mut c = String::from("id,cat,n\n");
        for i in 0..40_000 {
            c.push_str(&format!("{i},cat-{},{i}.25\n", i % 500));
        }
        let src = write_csv("par_determinism.csv", &c);

        let mut hashes = Vec::new();
        for run in 0..3 {
            let dst = src.with_extension(format!("run{run}.ferrix"));
            convert_csv(&src, &dst, b',', true, |_, _| {}).unwrap();
            hashes.push(std::fs::read(&dst).unwrap());
            let _ = std::fs::remove_file(&dst);
        }
        assert_eq!(hashes[0], hashes[1], "run 0 and 1 differ");
        assert_eq!(hashes[1], hashes[2], "run 1 and 2 differ");
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn peak_buffer_stays_bounded_as_input_grows() {
        // The property that lets a 10GB file convert on a 9GB machine: peak
        // must not scale with file size.
        let mut peaks = Vec::new();
        for &n in &[20_000usize, 200_000] {
            let mut c = String::from("a,b,c,d\n");
            for i in 0..n {
                c.push_str(&format!("{i},text-{},{i}.5,{}\n", i % 64, i % 2 == 0));
            }
            let src = write_csv(&format!("par_bound_{n}.csv"), &c);
            let dst = src.with_extension("bound.ferrix");
            let st = convert_csv(&src, &dst, b',', true, |_, _| {}).unwrap();
            peaks.push(st.peak_block_bytes);
            let _ = std::fs::remove_file(&src);
            let _ = std::fs::remove_file(&dst);
        }
        // 10x the rows must not meaningfully move peak, and it must stay well
        // under the 256MB budget.
        assert!(
            peaks[1] < 256 << 20,
            "peak {} exceeds 256MB budget",
            peaks[1]
        );
        assert!(
            peaks[1] <= peaks[0] * 2,
            "peak grew with input size: {peaks:?}"
        );
    }

}
