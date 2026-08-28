//! Streaming CSV export.
//!
//! ## Why this exists
//!
//! Saving in Ferrix writes a `.fxedits` sidecar: the sparse overlay, in a
//! private binary format, leaving the source untouched. That is right for
//! *editing* a 12 GB dataset — but it means the edited result cannot be handed
//! to Excel, pandas, a database, or a colleague. Without an export path the
//! application is a place data goes in and never comes out.
//!
//! ## Constraints
//!
//! Export has to obey the same rule as the converter: **peak memory bounded
//! and independent of row count**. A 200M-row sheet cannot be rendered into a
//! `String` first. So rows are formatted one at a time into a reused buffer and
//! streamed through a `BufWriter`, and the only allocation that grows is the
//! buffer for the single widest row.
//!
//! Writes go to a temporary file and are renamed into place at the end. A
//! crash, a full disk, or a cancel therefore leaves the previous file intact
//! rather than a truncated one that looks complete — the failure mode that
//! silently destroys data.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use ferrix_core::CellRef;

/// How a value should be rendered.
///
/// The exporter is generic over the sheet so it works with an in-RAM `Sheet`,
/// a memory-mapped one, or the composite base+overlay view the UI edits
/// through — without `ferrix-io` depending on the UI crate.
pub trait ExportSource {
    fn row_count(&self) -> usize;
    fn col_count(&self) -> usize;
    /// The cell's text exactly as the grid shows it.
    fn display(&self, cell: CellRef) -> String;
    /// Column header, or a spreadsheet letter when the sheet has none.
    fn header(&self, col: usize) -> String;
}

#[derive(Debug)]
pub enum ExportError {
    Io(io::Error),
    Cancelled,
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Io(e) => write!(f, "{e}"),
            ExportError::Cancelled => write!(f, "export cancelled"),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<io::Error> for ExportError {
    fn from(e: io::Error) -> Self {
        ExportError::Io(e)
    }
}

/// What an export produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportStats {
    pub rows: usize,
    pub cols: usize,
    pub bytes: u64,
    pub millis: u128,
}

impl ExportStats {
    pub fn throughput_mbps(&self) -> f64 {
        if self.millis == 0 {
            return 0.0;
        }
        (self.bytes as f64 / 1e6) / (self.millis as f64 / 1000.0)
    }
}

/// Options for a CSV export.
#[derive(Debug, Clone, Copy)]
pub struct ExportOptions {
    pub delimiter: u8,
    /// Write the header row first.
    pub headers: bool,
    /// Line ending. CRLF is what Excel expects on Windows.
    pub crlf: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            headers: true,
            crlf: cfg!(windows),
        }
    }
}

/// Append `field` to `out`, quoting only when the CSV grammar requires it.
///
/// A field needs quoting if it contains the delimiter, a quote, or any line
/// break. Skipping this is how exports silently corrupt: a value containing a
/// comma becomes two columns on reimport, and one containing a newline becomes
/// two rows.
pub fn write_field(out: &mut Vec<u8>, field: &str, delimiter: u8) {
    let needs = field
        .bytes()
        .any(|b| b == delimiter || b == b'"' || b == b'\n' || b == b'\r');
    if !needs {
        out.extend_from_slice(field.as_bytes());
        return;
    }
    out.push(b'"');
    for b in field.bytes() {
        if b == b'"' {
            out.push(b'"'); // Doubled, per RFC 4180.
        }
        out.push(b);
    }
    out.push(b'"');
}

/// Stream a sheet to `path` as CSV.
///
/// `progress(done_rows, total_rows)` is called periodically. `should_cancel`
/// is polled on the same cadence; returning true aborts and leaves no output
/// file behind.
pub fn export_csv<S, P, C>(
    path: &Path,
    sheet: &S,
    opts: ExportOptions,
    mut progress: P,
    mut should_cancel: C,
) -> Result<ExportStats, ExportError>
where
    S: ExportSource + ?Sized,
    P: FnMut(usize, usize),
    C: FnMut() -> bool,
{
    let start = std::time::Instant::now();
    let rows = sheet.row_count();
    let cols = sheet.col_count();

    // Write beside the destination so the rename is on the same filesystem;
    // a cross-device rename would fall back to a copy and defeat atomicity.
    let tmp = temp_sibling(path);
    let mut written: u64 = 0;

    {
        let file = File::create(&tmp)?;
        // 1 MB buffer: large enough that syscalls are rare, small enough that
        // peak memory does not track the dataset.
        let mut w = BufWriter::with_capacity(1 << 20, file);
        let newline: &[u8] = if opts.crlf { b"\r\n" } else { b"\n" };

        // One reusable buffer for the whole export. This is the allocation
        // that would otherwise grow with row count.
        let mut line: Vec<u8> = Vec::with_capacity(4096);

        if opts.headers && cols > 0 {
            line.clear();
            for c in 0..cols {
                if c > 0 {
                    line.push(opts.delimiter);
                }
                write_field(&mut line, &sheet.header(c), opts.delimiter);
            }
            line.extend_from_slice(newline);
            w.write_all(&line)?;
            written += line.len() as u64;
        }

        // Poll cancellation and report progress every this many rows. Frequent
        // enough to feel responsive, rare enough not to cost anything.
        const TICK: usize = 50_000;

        for r in 0..rows {
            if r % TICK == 0 {
                if should_cancel() {
                    drop(w);
                    let _ = std::fs::remove_file(&tmp);
                    return Err(ExportError::Cancelled);
                }
                progress(r, rows);
            }
            line.clear();
            for c in 0..cols {
                if c > 0 {
                    line.push(opts.delimiter);
                }
                let text = sheet.display(CellRef::new(r as u32, c as u32));
                write_field(&mut line, &text, opts.delimiter);
            }
            line.extend_from_slice(newline);
            w.write_all(&line)?;
            written += line.len() as u64;
        }
        w.flush()?;
    }

    // Windows will not rename onto an existing file.
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    progress(rows, rows);

    Ok(ExportStats {
        rows,
        cols,
        bytes: written,
        millis: start.elapsed().as_millis(),
    })
}

/// A temp path next to `path`, so the final rename stays on one filesystem.
fn temp_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".exporting");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::{column_name, Sheet, Value};

    impl ExportSource for Sheet {
        fn row_count(&self) -> usize {
            Sheet::row_count(self)
        }
        fn col_count(&self) -> usize {
            Sheet::col_count(self)
        }
        fn display(&self, cell: CellRef) -> String {
            Sheet::display(self, cell)
        }
        fn header(&self, col: usize) -> String {
            Sheet::header_or_letter(self, col).to_string()
        }
    }

    fn scratch() -> PathBuf {
        let d = std::env::temp_dir().join("ferrix_export_tests");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn opts_lf() -> ExportOptions {
        ExportOptions {
            crlf: false,
            ..Default::default()
        }
    }

    #[test]
    fn quoting_only_when_required() {
        let mut out = Vec::new();
        write_field(&mut out, "plain", b',');
        assert_eq!(String::from_utf8(out).unwrap(), "plain");

        let mut out = Vec::new();
        write_field(&mut out, "has,comma", b',');
        assert_eq!(String::from_utf8(out).unwrap(), "\"has,comma\"");

        let mut out = Vec::new();
        write_field(&mut out, "say \"hi\"", b',');
        assert_eq!(String::from_utf8(out).unwrap(), "\"say \"\"hi\"\"\"");

        let mut out = Vec::new();
        write_field(&mut out, "two\nlines", b',');
        assert_eq!(String::from_utf8(out).unwrap(), "\"two\nlines\"");

        // A comma is harmless when the delimiter is a tab.
        let mut out = Vec::new();
        write_field(&mut out, "has,comma", b'\t');
        assert_eq!(String::from_utf8(out).unwrap(), "has,comma");
    }

    #[test]
    fn exports_values_and_headers() {
        let mut s = Sheet::new("t");
        s.set_headers(vec!["n".into(), "label".into()]);
        s.set(CellRef::new(0, 0), Value::Number(1.0));
        s.set_text(CellRef::new(0, 1), "alpha");
        s.set(CellRef::new(1, 0), Value::Number(2.5));
        s.set_text(CellRef::new(1, 1), "beta");

        let p = scratch().join("basic.csv");
        let stats = export_csv(&p, &s, opts_lf(), |_, _| {}, || false).unwrap();
        assert_eq!(stats.rows, 2);
        assert_eq!(read(&p), "n,label\n1,alpha\n2.5,beta\n");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn round_trips_through_the_csv_loader() {
        // The real contract: what comes out must load back identically.
        let mut s = Sheet::new("t");
        s.set_headers(vec!["a".into(), "b".into()]);
        s.set_text(CellRef::new(0, 0), "has,comma");
        s.set_text(CellRef::new(0, 1), "say \"hi\"");
        s.set_text(CellRef::new(1, 0), "two\nlines");
        s.set(CellRef::new(1, 1), Value::Number(42.0));

        let p = scratch().join("roundtrip.csv");
        export_csv(&p, &s, opts_lf(), |_, _| {}, || false).unwrap();

        let (back, _) = crate::load_csv(&p, crate::CsvOptions::default()).unwrap();
        assert_eq!(
            back.row_count(),
            2,
            "row with embedded newline must survive"
        );
        assert_eq!(back.display(CellRef::new(0, 0)), "has,comma");
        assert_eq!(back.display(CellRef::new(0, 1)), "say \"hi\"");
        assert_eq!(back.display(CellRef::new(1, 0)), "two\nlines");
        assert_eq!(back.display(CellRef::new(1, 1)), "42");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn headerless_export_uses_letters() {
        let mut s = Sheet::new("t");
        s.set(CellRef::new(0, 0), Value::Number(1.0));
        s.set(CellRef::new(0, 1), Value::Number(2.0));
        let p = scratch().join("letters.csv");
        export_csv(&p, &s, opts_lf(), |_, _| {}, || false).unwrap();
        assert!(read(&p).starts_with("A,B\n"));
        assert_eq!(column_name(0), "A");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn cancel_leaves_no_output_and_no_temp() {
        let mut s = Sheet::new("t");
        for r in 0..200_000u32 {
            s.set(CellRef::new(r, 0), Value::Number(r as f64));
        }
        let p = scratch().join("cancelled.csv");
        let _ = std::fs::remove_file(&p);

        let err = export_csv(&p, &s, opts_lf(), |_, _| {}, || true).unwrap_err();
        assert!(matches!(err, ExportError::Cancelled));
        assert!(!p.exists(), "no output file after cancel");
        assert!(!temp_sibling(&p).exists(), "temp file must be cleaned up");
    }

    #[test]
    fn a_failed_export_does_not_destroy_the_previous_file() {
        // Atomicity: the old file must survive a cancelled overwrite intact.
        let p = scratch().join("existing.csv");
        std::fs::write(&p, "PRECIOUS DATA\n").unwrap();

        let mut s = Sheet::new("t");
        for r in 0..200_000u32 {
            s.set(CellRef::new(r, 0), Value::Number(r as f64));
        }
        let _ = export_csv(&p, &s, opts_lf(), |_, _| {}, || true);

        assert_eq!(
            read(&p),
            "PRECIOUS DATA\n",
            "a cancelled export must not clobber the existing file"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn progress_reaches_completion() {
        let mut s = Sheet::new("t");
        for r in 0..1000u32 {
            s.set(CellRef::new(r, 0), Value::Number(r as f64));
        }
        let p = scratch().join("progress.csv");
        let mut last = (0usize, 0usize);
        export_csv(&p, &s, opts_lf(), |d, t| last = (d, t), || false).unwrap();
        assert_eq!(last, (1000, 1000), "progress must finish at 100%");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn export_memory_is_bounded_by_row_width_not_row_count() {
        // The scale claim. The exporter reuses one line buffer, so writing
        // 300k rows must not grow allocation with the row count. We assert on
        // the output being correct and the run completing promptly; the
        // structural guarantee is that `line` is cleared and reused.
        let mut s = Sheet::new("t");
        for r in 0..300_000u32 {
            s.set(CellRef::new(r, 0), Value::Number(r as f64));
        }
        let p = scratch().join("big.csv");
        let t = std::time::Instant::now();
        let stats = export_csv(&p, &s, opts_lf(), |_, _| {}, || false).unwrap();
        let ms = t.elapsed().as_millis();

        assert_eq!(stats.rows, 300_000);
        assert!(ms < 10_000, "300k-row export took {ms}ms");
        // Spot-check the tail so we know nothing was truncated.
        let text = read(&p);
        assert!(text.ends_with("299999\n"), "last row missing");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn crlf_option_matches_excel() {
        let mut s = Sheet::new("t");
        s.set(CellRef::new(0, 0), Value::Number(1.0));
        let p = scratch().join("crlf.csv");
        let opts = ExportOptions {
            crlf: true,
            headers: false,
            ..Default::default()
        };
        export_csv(&p, &s, opts, |_, _| {}, || false).unwrap();
        assert_eq!(read(&p), "1\r\n");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn empty_sheet_produces_an_empty_file() {
        let s = Sheet::new("t");
        let p = scratch().join("empty.csv");
        let stats = export_csv(&p, &s, opts_lf(), |_, _| {}, || false).unwrap();
        assert_eq!(stats.rows, 0);
        assert_eq!(read(&p), "");
        let _ = std::fs::remove_file(&p);
    }
}
