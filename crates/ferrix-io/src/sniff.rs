//! Import detection: encoding, delimiter, quote character, and header row —
//! all decided from a **bounded prefix** of the file (issue #31).
//!
//! ## Why a prefix, and why that is the headline property
//!
//! The obvious implementation of "auto-detect the delimiter" is to count
//! candidate separators over the file. On a 10GB CSV that is a 40-second stare
//! at a spinner before the user has been shown a single row, and it allocates
//! in proportion to the file. Ferrix's whole scale claim is that peak memory is
//! bounded by the viewport and never by row count, so detection has to obey the
//! same rule as `convert.rs`: hold a fixed window, never the file.
//!
//! So every entry point here reads **at most [`PREFIX_BYTES`]**. That is not a
//! performance nicety layered on afterwards; it is structural. [`sniff_reader`]
//! wraps its reader in `Read::take(PREFIX_BYTES)`, so a detector that tried to
//! consume the whole file would see EOF at 128 KiB rather than quietly working
//! and being slow. `detection_never_reads_past_the_prefix` pins that by handing
//! in a reader which **panics** if asked for byte `PREFIX_BYTES + 1`, and
//! `delimiter_detection_ignores_everything_past_the_prefix` pins it from the
//! other side, with a file whose tail would produce a *different* answer if it
//! were read.
//!
//! ## What "does not parse cleanly" means
//!
//! [`Detection::clean`] is false when loading with the defaults would produce
//! nonsense rather than data: a non-UTF-8 encoding, a delimiter that is not a
//! comma, ragged records in the prefix, a preamble before the real header, or
//! undecodable bytes. That is the signal the UI uses to raise the import
//! wizard instead of silently loading one 900-character-wide column.

use std::io::Read;
use std::path::Path;

use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8};

use crate::csv::CsvOptions;

/// How much of a file detection is allowed to look at, in bytes.
///
/// 128 KiB is ~1000 typical CSV rows: far more than the ~100 lines the
/// heuristics need, and small enough that reaching it on a 10GB file is a
/// single sequential read.
pub const PREFIX_BYTES: usize = 128 * 1024;

/// How many records the preview and the heuristics look at.
pub const PREVIEW_ROWS: usize = 100;

/// Delimiters offered by auto-detection, in preference order for ties.
///
/// Comma first so a file that is genuinely ambiguous resolves to the
/// conventional answer rather than to whichever candidate the iteration order
/// happened to visit.
pub const CANDIDATE_DELIMITERS: [u8; 4] = [b',', b';', b'\t', b'|'];

/// Quote characters auto-detection will consider.
pub const CANDIDATE_QUOTES: [u8; 2] = [b'"', b'\''];

/// What detection concluded about a file.
#[derive(Clone, Debug)]
pub struct Detection {
    /// The encoding the bytes are in. Always a concrete encoding, never
    /// "unknown" — an unreadable guess is worse than a stated one the user can
    /// override.
    pub encoding: &'static Encoding,
    pub delimiter: u8,
    pub quote: u8,
    pub has_headers: bool,
    /// Records to discard before the header row (a title/preamble block).
    pub skip_rows: usize,
    /// How many bytes were actually examined. Never above [`PREFIX_BYTES`].
    pub prefix_bytes: usize,
    /// True when loading with `CsvOptions::default()` would be correct, i.e.
    /// there is nothing for a wizard to fix.
    pub clean: bool,
    /// Human-readable reason the file is not clean, for the wizard's banner.
    pub reason: Option<String>,
}

impl Detection {
    /// The loader options this detection implies.
    pub fn to_options(&self) -> CsvOptions {
        CsvOptions {
            delimiter: self.delimiter,
            has_headers: self.has_headers,
            max_rows: None,
            quote: self.quote,
            skip_rows: self.skip_rows,
            encoding: Some(self.encoding),
        }
    }
}

/// Detect settings for `path`, reading at most [`PREFIX_BYTES`] from it.
///
/// This is O(prefix), not O(file): a 10GB file costs exactly the same as a
/// 200KB one.
pub fn sniff_path(path: &Path) -> std::io::Result<Detection> {
    let file = std::fs::File::open(path)?;
    Ok(sniff_reader(file))
}

/// Detect settings from any reader, consuming at most [`PREFIX_BYTES`].
///
/// The `take` is the bound. It is here rather than at the call sites so that
/// no caller can accidentally opt out of it.
pub fn sniff_reader<R: Read>(reader: R) -> Detection {
    let prefix = read_prefix(reader);
    sniff_bytes(&prefix)
}

/// Read at most [`PREFIX_BYTES`], ignoring any error after the first byte.
///
/// A read error partway through a prefix is not a reason to refuse detection —
/// whatever arrived is still a sample, and the real load will report the error
/// properly if it persists.
fn read_prefix<R: Read>(reader: R) -> Vec<u8> {
    let mut buf = Vec::with_capacity(PREFIX_BYTES.min(64 * 1024));
    let mut limited = reader.take(PREFIX_BYTES as u64);
    let _ = limited.read_to_end(&mut buf);
    buf.truncate(PREFIX_BYTES);
    buf
}

/// Detect settings from an already-read prefix.
pub fn sniff_bytes(prefix: &[u8]) -> Detection {
    let prefix_bytes = prefix.len();
    let encoding = detect_encoding(prefix);
    // Decode FIRST, then cut back to the last complete line.
    //
    // Cutting bytes first is wrong for any encoding whose newline is not a
    // lone 0x0A: in UTF-16LE a newline is `0A 00`, so byte-level truncation
    // at the 0x0A leaves a dangling half code unit and the last field comes
    // back with a replacement character glued to it.
    let (decoded, _, had_errors) = encoding.decode(prefix);
    let text = truncate_text_to_last_newline(&decoded);

    let lines = sample_lines(text);
    let (quote, delimiter, modal_fields, consistent) = detect_delimiter_and_quote(&lines);
    let skip_rows = detect_skip_rows(&lines, delimiter, quote, modal_fields);
    let has_headers = detect_headers(&lines[skip_rows.min(lines.len())..], delimiter, quote);

    let mut reason = None;
    if encoding != UTF_8 {
        reason = Some(format!(
            "text is {}, not UTF-8 — loading it as UTF-8 would mangle accented characters",
            encoding.name()
        ));
    } else if had_errors {
        reason = Some("file contains bytes that are not valid UTF-8".to_string());
    } else if delimiter != b',' {
        reason = Some(format!(
            "fields look separated by {}, not a comma",
            describe_delimiter(delimiter)
        ));
    } else if !consistent {
        reason = Some("rows have inconsistent field counts".to_string());
    } else if skip_rows > 0 {
        reason = Some(format!(
            "the first {} row{} look like a preamble, not data",
            skip_rows,
            if skip_rows == 1 { "" } else { "s" }
        ));
    } else if quote != b'"' {
        reason = Some(format!(
            "quoting looks like {}, not a double quote",
            describe_delimiter(quote)
        ));
    } else if modal_fields <= 1 {
        reason = Some("no delimiter found — the file may not be delimited text".to_string());
    }

    Detection {
        encoding,
        delimiter,
        quote,
        has_headers,
        skip_rows,
        prefix_bytes,
        clean: reason.is_none(),
        reason,
    }
}

fn describe_delimiter(d: u8) -> String {
    match d {
        b',' => "a comma".into(),
        b';' => "a semicolon".into(),
        b'\t' => "a tab".into(),
        b'|' => "a pipe".into(),
        b'\'' => "a single quote".into(),
        b'"' => "a double quote".into(),
        b' ' => "a space".into(),
        c if c.is_ascii_graphic() => format!("'{}'", c as char),
        c => format!("byte 0x{c:02x}"),
    }
}

/// Cut decoded text back to the last newline so the tail is a complete record.
///
/// A prefix stops wherever the byte budget ran out, which is usually mid-row.
/// Keeping that partial row would give it a short field count and drag the
/// modal-agreement score down for no reason. Falls back to the whole string
/// when there is no newline at all — a single-line file is still worth
/// detecting on.
fn truncate_text_to_last_newline(text: &str) -> &str {
    match text.rfind('\n') {
        Some(i) => &text[..=i],
        None => text,
    }
}

/// Identify the encoding, preferring an explicit BOM over any statistic.
///
/// chardetng deliberately does not do BOM sniffing (it is a *guesser* for
/// files with no declaration), so a UTF-16 file with a BOM would otherwise be
/// guessed as some single-byte encoding and decode to interleaved NULs.
pub fn detect_encoding(prefix: &[u8]) -> &'static Encoding {
    if prefix.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return UTF_8;
    }
    if prefix.starts_with(&[0xFF, 0xFE]) {
        return UTF_16LE;
    }
    if prefix.starts_with(&[0xFE, 0xFF]) {
        return UTF_16BE;
    }
    // Valid UTF-8 is never reported as anything else. chardetng is a
    // statistical guesser and will happily call a short ASCII-plus-accents
    // sample windows-1252 even when it decodes cleanly as UTF-8; preferring
    // UTF-8 when it fits keeps the common case exact rather than probable.
    if std::str::from_utf8(prefix).is_ok() {
        return UTF_8;
    }
    let mut det = chardetng::EncodingDetector::new();
    det.feed(prefix, true);
    det.guess(None, true)
}

/// The first [`PREVIEW_ROWS`] *records* of `text`, respecting quoted newlines.
///
/// Quote-awareness matters here for the same reason it does in `chunk_bounds`:
/// a newline inside a quoted field is not a record boundary, and treating it
/// as one makes every downstream field count wrong for that record.
fn sample_lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(PREVIEW_ROWS);
    let mut start = 0usize;
    let mut in_quotes = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            // Double quote only: RFC 4180 is what an unknown file most likely
            // follows, and a wrong guess here costs at most a mis-split sample
            // line, not a mis-parsed load.
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => {
                let mut end = i;
                if end > start && bytes[end - 1] == b'\r' {
                    end -= 1;
                }
                out.push(&text[start..end]);
                start = i + 1;
                if out.len() >= PREVIEW_ROWS {
                    return out;
                }
            }
            _ => {}
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Count fields in one record for a given delimiter and quote character.
fn count_fields(line: &str, delim: u8, quote: u8) -> usize {
    let mut n = 1usize;
    let mut in_quotes = false;
    for &b in line.as_bytes() {
        if b == quote {
            in_quotes = !in_quotes;
        } else if b == delim && !in_quotes {
            n += 1;
        }
    }
    n
}

/// Pick the delimiter and quote character that explain the sample best.
///
/// Scoring: for each candidate pair, take the MODAL field count across the
/// sample and score by how many records agree with it. A real delimiter
/// produces the same field count on nearly every line; a delimiter that merely
/// occurs inside the text does not. Field count 1 scores zero — "every line
/// has one field" is what a wrong delimiter looks like, not a good fit.
///
/// Returns `(quote, delimiter, modal_field_count, consistent)`.
fn detect_delimiter_and_quote(lines: &[&str]) -> (u8, u8, usize, bool) {
    let mut best = (b'"', b',', 1usize, 0f64);
    for &quote in &CANDIDATE_QUOTES {
        for &delim in &CANDIDATE_DELIMITERS {
            let counts: Vec<usize> = lines
                .iter()
                .map(|l| count_fields(l, delim, quote))
                .collect();
            let Some(modal) = modal_value(&counts) else {
                continue;
            };
            if modal <= 1 {
                continue;
            }
            let agree = counts.iter().filter(|&&c| c == modal).count();
            // Agreement dominates; more columns breaks ties, so `a;b;c` is
            // preferred over reading the same line as one comma-free field.
            let score = agree as f64 + (modal as f64) / 1000.0;
            if score > best.3 {
                best = (quote, delim, modal, score);
            }
        }
    }
    let (quote, delim, modal, _) = best;
    let counts: Vec<usize> = lines
        .iter()
        .map(|l| count_fields(l, delim, quote))
        .collect();
    let agree = counts.iter().filter(|&&c| c == modal).count();
    // "Consistent" allows a preamble: leading odd rows are handled by
    // `detect_skip_rows`, so require agreement among the bulk rather than all.
    let consistent = !lines.is_empty() && agree * 10 >= lines.len() * 9;
    (quote, delim, modal, consistent)
}

fn modal_value(counts: &[usize]) -> Option<usize> {
    let mut best = None;
    let mut best_n = 0usize;
    for &c in counts {
        let n = counts.iter().filter(|&&o| o == c).count();
        if n > best_n {
            best_n = n;
            best = Some(c);
        }
    }
    best
}

/// Leading records that do not have the modal field count — a title block,
/// an export banner, a blank line.
///
/// Stops at the first record that fits, so an odd row in the MIDDLE of the
/// file is a raggedness problem rather than something to skip past.
fn detect_skip_rows(lines: &[&str], delim: u8, quote: u8, modal: usize) -> usize {
    if modal <= 1 {
        return 0;
    }
    let mut n = 0usize;
    for line in lines {
        if count_fields(line, delim, quote) == modal {
            break;
        }
        n += 1;
        // Refuse to "skip" the entire sample: at that point the modal count is
        // wrong, not the leading rows.
        if n >= lines.len().saturating_sub(1) {
            return 0;
        }
    }
    n
}

/// Does the first record look like column names rather than data?
///
/// The signal that can actually fail: a header cell is non-numeric where the
/// column below it is numeric. Comparing "is the first row text" alone would
/// call every all-text file headed, which is how a real data row silently
/// becomes column names.
fn detect_headers(lines: &[&str], delim: u8, quote: u8) -> bool {
    if lines.len() < 2 {
        // One record and nothing to compare it to: treat it as a header, which
        // matches the loader's long-standing default and is trivially
        // overridable in the wizard.
        return true;
    }
    let first = split_simple(lines[0], delim, quote);
    if first.iter().any(|f| f.trim().is_empty()) {
        // A blank column name is what a data row looks like, not a header.
        return false;
    }
    let body: Vec<Vec<String>> = lines[1..lines.len().min(PREVIEW_ROWS)]
        .iter()
        .map(|l| split_simple(l, delim, quote))
        .collect();
    if body.is_empty() {
        return true;
    }
    for (i, head) in first.iter().enumerate() {
        if numeric(head) {
            continue;
        }
        // This column is text in row 0. If it is numeric further down, row 0
        // is a name and not a value.
        let below_numeric = body
            .iter()
            .filter_map(|r| r.get(i))
            .filter(|v| !v.trim().is_empty())
            .take(20)
            .any(|v| numeric(v));
        if below_numeric {
            return true;
        }
    }
    // No column changes type. Fall back to "all-text first row over a body
    // that also contains numbers somewhere" — still a real signal, just a
    // weaker one.
    let first_all_text = first.iter().all(|f| !numeric(f));
    let body_has_numbers = body.iter().flatten().any(|v| numeric(v));
    first_all_text && body_has_numbers
}

fn numeric(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && s.parse::<f64>().is_ok()
}

/// Split one record into unescaped field strings.
pub(crate) fn split_simple(line: &str, delim: u8, quote: u8) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut field: Vec<u8> = Vec::new();
    let mut i = 0usize;
    let mut in_quotes = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_quotes {
            if b == quote {
                if i + 1 < bytes.len() && bytes[i + 1] == quote {
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
            out.push(String::from_utf8_lossy(&field).into_owned());
            field.clear();
        } else if b != b'\r' {
            field.push(b);
        }
        i += 1;
    }
    out.push(String::from_utf8_lossy(&field).into_owned());
    out
}

/// The first `max_rows` rows of a file, decoded and split with `opts`.
#[derive(Clone, Debug, Default)]
pub struct Preview {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub cols: usize,
    /// Bytes examined — never above [`PREFIX_BYTES`].
    pub prefix_bytes: usize,
    /// True when the file continues past the prefix, i.e. the preview is a
    /// window and not the whole file.
    pub truncated: bool,
}

/// Build a preview of `path` under `opts`, reading at most [`PREFIX_BYTES`].
///
/// Same bound as detection, for the same reason: the preview updates on every
/// keystroke in the wizard, and it must cost the same on a 10GB file as on a
/// 10KB one.
pub fn preview_path(path: &Path, opts: CsvOptions, max_rows: usize) -> std::io::Result<Preview> {
    let file = std::fs::File::open(path)?;
    Ok(preview_reader(file, opts, max_rows))
}

pub fn preview_reader<R: Read>(reader: R, opts: CsvOptions, max_rows: usize) -> Preview {
    let prefix = read_prefix(reader);
    let truncated = prefix.len() >= PREFIX_BYTES;
    let mut p = preview_bytes(&prefix, opts, max_rows);
    p.truncated = truncated || p.truncated;
    p
}

pub fn preview_bytes(prefix: &[u8], opts: CsvOptions, max_rows: usize) -> Preview {
    let prefix_bytes = prefix.len();
    let enc = opts.encoding.unwrap_or(UTF_8);
    let (decoded, _, _) = enc.decode(prefix);
    let text = truncate_text_to_last_newline(&decoded);
    let lines = sample_lines_n(text, opts.skip_rows + max_rows + 1);

    let mut it = lines.into_iter().skip(opts.skip_rows);
    let headers = if opts.has_headers {
        it.next()
            .map(|l| split_simple(l, opts.delimiter, opts.quote))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let rows: Vec<Vec<String>> = it
        .take(max_rows)
        .map(|l| split_simple(l, opts.delimiter, opts.quote))
        .collect();
    let cols = rows
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .max(headers.len());

    Preview {
        headers,
        rows,
        cols,
        prefix_bytes,
        truncated: false,
    }
}

/// `sample_lines` with a caller-chosen cap.
fn sample_lines_n(text: &str, cap: usize) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_quotes = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => {
                let mut end = i;
                if end > start && bytes[end - 1] == b'\r' {
                    end -= 1;
                }
                out.push(&text[start..end]);
                start = i + 1;
                if out.len() >= cap {
                    return out;
                }
            }
            _ => {}
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Encodings offered in the wizard's override list, most likely first.
///
/// Kept short on purpose: a 200-entry dropdown is not a usable override.
pub const ENCODING_CHOICES: [&str; 9] = [
    "UTF-8",
    "windows-1252",
    "ISO-8859-15",
    "windows-1251",
    "UTF-16LE",
    "UTF-16BE",
    "Shift_JIS",
    "GBK",
    "Big5",
];

/// Resolve a label from [`ENCODING_CHOICES`] to an encoding.
pub fn encoding_for_label(label: &str) -> Option<&'static Encoding> {
    Encoding::for_label(label.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch() -> std::path::PathBuf {
        let d = std::env::temp_dir().join("ferrix_sniff_tests");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = scratch().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    // ---------- the headline criterion ----------

    /// A reader that serves an endless CSV and PANICS the moment it is asked
    /// for a byte past `PREFIX_BYTES`.
    ///
    /// This is the whole point of the issue expressed as a type: if detection
    /// ever reverts to reading the file, this test does not get slower, it
    /// aborts. A timing-only test on a small file would pass against a
    /// detector that read everything.
    struct PoisonedPastPrefix {
        served: usize,
        limit: usize,
    }

    impl Read for PoisonedPastPrefix {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.served >= self.limit {
                panic!(
                    "detection read past the {}-byte prefix (served {})",
                    self.limit, self.served
                );
            }
            // One record per 16 bytes, endlessly. Never returns 0, so this
            // reader looks like a file with no end.
            let row = b"aa;bb;cc;dd;ee\n";
            let n = buf.len().min(row.len());
            buf[..n].copy_from_slice(&row[..n]);
            self.served += n;
            if self.served > self.limit {
                panic!("detection read past the {}-byte prefix", self.limit);
            }
            Ok(n)
        }
    }

    #[test]
    fn detection_never_reads_past_the_prefix() {
        let d = sniff_reader(PoisonedPastPrefix {
            served: 0,
            limit: PREFIX_BYTES,
        });
        // It still produced a real answer from the bounded sample.
        assert_eq!(
            d.delimiter, b';',
            "semicolon file detected as {}",
            d.delimiter
        );
        assert_eq!(
            d.prefix_bytes, PREFIX_BYTES,
            "detection must consume the whole prefix and not one byte more"
        );
    }

    #[test]
    fn preview_never_reads_past_the_prefix() {
        let p = preview_reader(
            PoisonedPastPrefix {
                served: 0,
                limit: PREFIX_BYTES,
            },
            CsvOptions {
                delimiter: b';',
                ..Default::default()
            },
            PREVIEW_ROWS,
        );
        assert_eq!(p.rows.len(), PREVIEW_ROWS, "preview must fill 100 rows");
        assert!(
            p.truncated,
            "a file that continues must be marked truncated"
        );
        assert_eq!(p.cols, 5);
    }

    /// The same property from the file side, with an answer that CHANGES if
    /// the tail is read.
    ///
    /// The first 128 KiB are semicolon-delimited; the remaining ~64 MB are
    /// comma-delimited and outnumber them 500:1. A whole-file detector answers
    /// "comma". A bounded-prefix detector answers "semicolon". Only one of
    /// those assertions can pass, so the test cannot be satisfied by a
    /// detector that ignores the bound.
    #[test]
    fn delimiter_detection_ignores_everything_past_the_prefix() {
        let p = scratch().join("prefix_vs_tail.csv");
        {
            let f = std::fs::File::create(&p).unwrap();
            let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
            let head = b"a;b;c;d\n1;2;3;4\n";
            let mut written = 0usize;
            while written < PREFIX_BYTES * 2 {
                w.write_all(head).unwrap();
                written += head.len();
            }
            let tail = b"1,2,3,4,5,6,7,8\n";
            for _ in 0..(64 * 1024 * 1024 / tail.len()) {
                w.write_all(tail).unwrap();
            }
            w.flush().unwrap();
        }
        let size = std::fs::metadata(&p).unwrap().len();
        assert!(size > 64 * 1024 * 1024, "fixture too small to be a test");

        let t = std::time::Instant::now();
        let d = sniff_path(&p).unwrap();
        let ms = t.elapsed().as_millis();

        assert_eq!(
            d.delimiter, b';',
            "detection used the tail: a {size}-byte file answered \
             {:?} instead of ';'",
            d.delimiter as char
        );
        assert!(
            d.prefix_bytes <= PREFIX_BYTES,
            "examined {} bytes of a {size}-byte file",
            d.prefix_bytes
        );
        // A whole-file read of 64MB cannot happen in this budget even from
        // page cache on a slow disk; this is a corroborating signal, not the
        // primary assertion.
        assert!(ms < 500, "detection of a {size}-byte file took {ms}ms");
        let _ = std::fs::remove_file(&p);
    }

    // ---------- delimiter ----------

    #[test]
    fn detects_each_candidate_delimiter() {
        for (delim, sep) in [(b',', ","), (b';', ";"), (b'\t', "\t"), (b'|', "|")] {
            let mut text = format!("id{sep}name{sep}score\n");
            for i in 0..50 {
                text.push_str(&format!("{i}{sep}row{i}{sep}{}\n", i * 2));
            }
            let d = sniff_bytes(text.as_bytes());
            assert_eq!(
                d.delimiter, delim,
                "{:?}-delimited file detected as {:?}",
                delim as char, d.delimiter as char
            );
            assert!(d.has_headers, "header row missed for {:?}", delim as char);
        }
    }

    #[test]
    fn a_comma_inside_quoted_text_does_not_beat_the_real_semicolon() {
        // Every row has more commas than semicolons, but only the semicolon
        // gives a consistent field count. Counting raw occurrences picks the
        // comma and is wrong.
        let mut text = String::from("name;note\n");
        for i in 0..40 {
            text.push_str(&format!("row{i};\"a, b, c, d, e\"\n"));
        }
        let d = sniff_bytes(text.as_bytes());
        assert_eq!(d.delimiter, b';', "quoted commas beat the real delimiter");
    }

    #[test]
    fn a_single_column_file_is_not_forced_into_a_delimiter() {
        let text = "value\n1\n2\n3\n4\n5\n";
        let d = sniff_bytes(text.as_bytes());
        assert!(!d.clean, "a file with no delimiter must reach the wizard");
        assert_eq!(d.delimiter, b',', "fall back to the conventional default");
    }

    // ---------- quote ----------

    #[test]
    fn detects_a_single_quote_character() {
        let mut text = String::from("a,b\n");
        for i in 0..40 {
            text.push_str(&format!("{i},'has, comma'\n"));
        }
        let d = sniff_bytes(text.as_bytes());
        assert_eq!(d.quote, b'\'', "single-quoted file detected as double");
        assert_eq!(d.delimiter, b',');
        assert!(!d.clean, "a non-standard quote must reach the wizard");
    }

    // ---------- encoding ----------

    #[test]
    fn latin1_accents_are_detected_and_decoded_exactly() {
        // windows-1252 bytes for "café" / "Zürich" / "naïve" / "crêpe".
        // The numeric third column is what makes the header row detectable —
        // an all-text file genuinely has no way to tell names from data, and
        // this test is about the ENCODING, not about that.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(b"ville,pays,n\n");
        for i in 0..40 {
            bytes.extend_from_slice(b"caf\xe9,Z\xfcrich,");
            bytes.extend_from_slice(i.to_string().as_bytes());
            bytes.push(b'\n');
            bytes.extend_from_slice(b"na\xefve,cr\xeape,");
            bytes.extend_from_slice(i.to_string().as_bytes());
            bytes.push(b'\n');
        }
        let d = sniff_bytes(&bytes);
        assert_ne!(
            d.encoding, UTF_8,
            "0xE9 is not valid UTF-8; detection must not claim UTF-8"
        );
        let p = preview_bytes(&bytes, d.to_options(), 10);
        // The exact decoded string, not "loading succeeded".
        assert_eq!(p.headers, vec!["ville", "pays", "n"]);
        assert_eq!(p.rows[0], vec!["café", "Zürich", "0"]);
        assert_eq!(p.rows[1], vec!["naïve", "crêpe", "0"]);
        assert!(!d.clean, "a non-UTF-8 file must reach the wizard");
    }

    #[test]
    fn utf8_stays_utf8_and_is_clean() {
        let text = "ville,pays\ncafé,Zürich\nnaïve,crêpe\nx,1\ny,2\n";
        let d = sniff_bytes(text.as_bytes());
        assert_eq!(d.encoding, UTF_8, "valid UTF-8 must not be re-guessed");
        let p = preview_bytes(text.as_bytes(), d.to_options(), 10);
        assert_eq!(p.rows[0], vec!["café", "Zürich"]);
    }

    #[test]
    fn a_utf8_bom_is_recognised_and_not_shown_as_data() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"id,name\n1,alpha\n2,beta\n");
        let d = sniff_bytes(&bytes);
        assert_eq!(d.encoding, UTF_8);
        let p = preview_bytes(&bytes, d.to_options(), 10);
        assert_eq!(
            p.headers[0], "id",
            "the BOM leaked into the first header cell"
        );
    }

    #[test]
    fn utf16le_is_recognised_from_its_bom() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in "id,name\n1,café\n".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let d = sniff_bytes(&bytes);
        assert_eq!(d.encoding, UTF_16LE);
        let p = preview_bytes(&bytes, d.to_options(), 10);
        assert_eq!(p.headers, vec!["id", "name"]);
        assert_eq!(p.rows[0], vec!["1", "café"]);
    }

    #[test]
    fn an_encoding_override_is_honoured_over_detection() {
        // Bytes that are valid UTF-8 but were really windows-1252: 0xC3 0xA9
        // read as Latin-1 is "Ã©". The user must be able to say so.
        let text = "a\ncafé\n";
        let mut opts = sniff_bytes(text.as_bytes()).to_options();
        assert_eq!(opts.encoding, Some(UTF_8));
        // Pinned rather than detected: this fixture is one all-text column,
        // where "is row 0 a header" is genuinely undecidable. Fixing it here
        // keeps the assertion below about the ENCODING override only.
        opts.has_headers = true;
        opts.encoding = encoding_for_label("windows-1252");
        let p = preview_bytes(text.as_bytes(), opts, 10);
        assert_eq!(
            p.rows[0],
            vec!["cafÃ©"],
            "the override was ignored and detection won"
        );
    }

    #[test]
    fn every_offered_encoding_label_resolves() {
        for label in ENCODING_CHOICES {
            assert!(
                encoding_for_label(label).is_some(),
                "wizard offers {label}, which encoding_rs does not know"
            );
        }
    }

    // ---------- headers and skipping ----------

    #[test]
    fn a_numeric_first_row_is_not_a_header() {
        let mut text = String::new();
        for i in 0..30 {
            text.push_str(&format!("{i},{},{}\n", i * 2, i * 3));
        }
        let d = sniff_bytes(text.as_bytes());
        assert!(
            !d.has_headers,
            "an all-numeric first row became column names"
        );
    }

    #[test]
    fn a_text_first_row_over_numbers_is_a_header() {
        let mut text = String::from("id,label,score\n");
        for i in 0..30 {
            text.push_str(&format!("{i},row{i},{}\n", i * 2));
        }
        assert!(sniff_bytes(text.as_bytes()).has_headers);
    }

    #[test]
    fn a_preamble_is_detected_as_rows_to_skip() {
        let mut text = String::from("# Export from Widgets Inc\n\nGenerated 2026-01-01\n");
        text.push_str("id,label,score\n");
        for i in 0..30 {
            text.push_str(&format!("{i},row{i},{}\n", i * 2));
        }
        let d = sniff_bytes(text.as_bytes());
        assert_eq!(d.skip_rows, 3, "preamble rows were not skipped");
        assert!(d.has_headers, "the header after the preamble was missed");
        assert!(!d.clean, "a preamble must reach the wizard");

        let p = preview_bytes(text.as_bytes(), d.to_options(), 10);
        assert_eq!(p.headers, vec!["id", "label", "score"]);
        assert_eq!(p.rows[0], vec!["0", "row0", "0"]);
    }

    #[test]
    fn header_at_row_n_is_expressible() {
        let text = "junk\nmore junk\nid,name\n1,alpha\n2,beta\n";
        let opts = CsvOptions {
            delimiter: b',',
            has_headers: true,
            skip_rows: 2,
            ..Default::default()
        };
        let p = preview_bytes(text.as_bytes(), opts, 10);
        assert_eq!(p.headers, vec!["id", "name"]);
        assert_eq!(p.rows, vec![vec!["1", "alpha"], vec!["2", "beta"]]);
    }

    #[test]
    fn headers_off_keeps_the_first_row_as_data() {
        let text = "id,name\n1,alpha\n";
        let opts = CsvOptions {
            has_headers: false,
            ..Default::default()
        };
        let p = preview_bytes(text.as_bytes(), opts, 10);
        assert!(p.headers.is_empty());
        assert_eq!(p.rows[0], vec!["id", "name"]);
    }

    // ---------- quoting shape parity with the exporter ----------

    #[test]
    fn preview_survives_embedded_delimiters_and_newlines_in_quotes() {
        // The exact shape `export::round_trips_through_the_csv_loader` covers.
        let text = "a,b\n\"has,comma\",\"say \"\"hi\"\"\"\n\"two\nlines\",42\n";
        let d = sniff_bytes(text.as_bytes());
        let p = preview_bytes(text.as_bytes(), d.to_options(), 10);
        assert_eq!(p.headers, vec!["a", "b"]);
        assert_eq!(
            p.rows.len(),
            2,
            "an embedded newline split a record into two preview rows"
        );
        assert_eq!(p.rows[0], vec!["has,comma", "say \"hi\""]);
        assert_eq!(p.rows[1], vec!["two\nlines", "42"]);
    }

    // ---------- clean files must NOT raise the wizard ----------

    #[test]
    fn a_plain_utf8_comma_file_is_clean() {
        let mut text = String::from("id,name,score\n");
        for i in 0..50 {
            text.push_str(&format!("{i},row{i},{}\n", i * 3));
        }
        let d = sniff_bytes(text.as_bytes());
        assert!(
            d.clean,
            "an ordinary CSV would raise the wizard: {:?}",
            d.reason
        );
        assert_eq!(d.delimiter, b',');
        assert_eq!(d.quote, b'"');
        assert_eq!(d.skip_rows, 0);
        assert!(d.has_headers);
    }

    #[test]
    fn a_semicolon_file_is_not_clean() {
        let mut text = String::from("id;name\n");
        for i in 0..30 {
            text.push_str(&format!("{i};row{i}\n"));
        }
        let d = sniff_bytes(text.as_bytes());
        assert!(!d.clean);
        assert!(d.reason.unwrap().contains("semicolon"));
    }

    #[test]
    fn empty_input_does_not_panic() {
        let d = sniff_bytes(b"");
        assert_eq!(d.delimiter, b',');
        assert!(!d.clean);
    }

    #[test]
    fn sniff_path_reports_the_bytes_it_read() {
        let p = write("small.csv", b"a,b\n1,2\n");
        let d = sniff_path(&p).unwrap();
        assert_eq!(d.prefix_bytes, 8);
        let _ = std::fs::remove_file(&p);
    }
}
