//! Memory-mapped `.ferrix` reader.
//!
//! This is what makes 10GB+ work. The file is mapped, not read: no parse on
//! open, no heap proportional to the dataset. Reading a cell indexes into the
//! mapping and lets the OS fault in the 4KB page containing it. Scrolling
//! touches a viewport's worth of pages; the page cache keeps hot rows resident
//! and evicts cold ones automatically under memory pressure.
//!
//! Consequently `open()` is O(1) regardless of file size, and a 10GB dataset
//! has a resident set of megabytes rather than gigabytes.

use std::fs::File;
use std::path::Path;

use ferrix_core::{CellRef, ErrorKind, StrId, Value, ValueTag};
use memmap2::Mmap;

use crate::format::{ColumnDesc, FormatError, Header, COL_DESC_BYTES};

/// A memory-mapped columnar dataset.
pub struct MappedSheet {
    mmap: Mmap,
    header: Header,
    columns: Vec<ColumnDesc>,
    /// Arena spans, decoded once at open (small: 8 bytes per distinct string).
    spans: Vec<(u32, u32)>,
    headers: Vec<String>,
    pub name: String,
}

impl MappedSheet {
    /// Map a `.ferrix` file. Validates every section offset up front so a
    /// corrupt file fails here rather than as an out-of-bounds read later.
    pub fn open(path: &Path) -> Result<Self, FormatError> {
        let file = File::open(path)?;
        // SAFETY: the mapping is read-only and we validate all offsets below.
        // A concurrent truncation could still fault; we accept that, matching
        // every other mmap-based reader.
        let mmap = unsafe { Mmap::map(&file)? };
        let len = mmap.len() as u64;

        let header = Header::parse(&mmap)?;

        let check = |off: u64, size: u64| -> Result<(), FormatError> {
            if off == crate::format::ABSENT {
                return Ok(());
            }
            if off.saturating_add(size) > len {
                return Err(FormatError::OutOfBounds { off, len });
            }
            Ok(())
        };

        check(
            header.col_table_off,
            header.cols as u64 * COL_DESC_BYTES as u64,
        )?;
        check(header.arena_data_off, header.arena_data_len)?;
        check(header.arena_spans_off, header.arena_spans * 8)?;

        let mut columns = Vec::with_capacity(header.cols as usize);
        for i in 0..header.cols as usize {
            let off = header.col_table_off as usize + i * COL_DESC_BYTES;
            let d = ColumnDesc::parse(&mmap[off..])?;
            check(d.tags_off, d.len)?;
            check(d.nums_off, d.len * 8)?;
            check(d.strs_off, d.len * 4)?;
            columns.push(d);
        }

        // Decode the span table once: 8 bytes per distinct string, so even a
        // million distinct strings is only 8MB.
        let mut spans = Vec::with_capacity(header.arena_spans as usize);
        for i in 0..header.arena_spans as usize {
            let o = header.arena_spans_off as usize + i * 8;
            let s = u32::from_le_bytes(mmap[o..o + 4].try_into().unwrap());
            let l = u32::from_le_bytes(mmap[o + 4..o + 8].try_into().unwrap());
            spans.push((s, l));
        }

        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Sheet1".into());

        Ok(Self {
            mmap,
            header,
            columns,
            spans,
            headers: Vec::new(),
            name,
        })
    }

    #[inline]
    pub fn row_count(&self) -> usize {
        self.header.rows as usize
    }

    #[inline]
    pub fn col_count(&self) -> usize {
        self.columns.len()
    }

    /// Bytes mapped. This is address space, not resident memory — the whole
    /// point is that only touched pages occupy RAM.
    pub fn mapped_bytes(&self) -> usize {
        self.mmap.len()
    }

    pub fn set_headers(&mut self, h: Vec<String>) {
        self.headers = h;
    }

    pub fn headers(&self) -> &[String] {
        &self.headers
    }

    pub fn header_or_letter(&self, col: usize) -> String {
        self.headers
            .get(col)
            .filter(|h| !h.is_empty())
            .cloned()
            .unwrap_or_else(|| ferrix_core::column_name(col as u32))
    }

    /// Read a cell straight out of the mapping.
    #[inline]
    pub fn get(&self, cell: CellRef) -> Value {
        let Some(desc) = self.columns.get(cell.col as usize) else {
            return Value::Empty;
        };
        let row = cell.row as u64;
        if row >= desc.len {
            return Value::Empty;
        }
        let tag = self.mmap[(desc.tags_off + row) as usize];
        match tag {
            t if t == ValueTag::Number as u8 => Value::Number(self.num_at(desc, row)),
            t if t == ValueTag::Bool as u8 => Value::Bool(self.num_at(desc, row) != 0.0),
            t if t == ValueTag::Text as u8 => Value::Text(StrId(self.str_at(desc, row))),
            t if t == ValueTag::Error as u8 => {
                Value::Error(decode_error(self.num_at(desc, row) as u8))
            }
            _ => Value::Empty,
        }
    }

    #[inline]
    fn num_at(&self, desc: &ColumnDesc, row: u64) -> f64 {
        if !desc.has_numbers() {
            return 0.0;
        }
        let o = (desc.nums_off + row * 8) as usize;
        f64::from_le_bytes(self.mmap[o..o + 8].try_into().unwrap())
    }

    #[inline]
    fn str_at(&self, desc: &ColumnDesc, row: u64) -> u32 {
        if !desc.has_strings() {
            return 0;
        }
        let o = (desc.strs_off + row * 4) as usize;
        u32::from_le_bytes(self.mmap[o..o + 4].try_into().unwrap())
    }

    /// Resolve an interned string out of the mapped arena.
    #[inline]
    pub fn resolve(&self, id: StrId) -> &str {
        let Some(&(start, len)) = self.spans.get(id.0 as usize) else {
            return "";
        };
        let o = (self.header.arena_data_off + start as u64) as usize;
        let end = o + len as usize;
        if end > self.mmap.len() {
            return "";
        }
        std::str::from_utf8(&self.mmap[o..end]).unwrap_or("")
    }

    pub fn display(&self, cell: CellRef) -> String {
        match self.get(cell) {
            Value::Empty => String::new(),
            Value::Number(n) => ferrix_core::format_number(n),
            Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.to_string(),
            Value::Text(id) => self.resolve(id).to_string(),
            Value::Error(e) => e.to_string(),
        }
    }

    /// Sum a rectangle. Reads the f64 array as a typed slice so the loop stays
    /// tight and the kernel can prefetch sequentially — this is why a
    /// full-column aggregate over 200M rows is a streaming disk read rather
    /// than 200M random accesses.
    pub fn sum_rect(&self, start: CellRef, end: CellRef) -> f64 {
        let (r0, r1) = (
            start.row.min(end.row) as u64,
            start.row.max(end.row) as u64 + 1,
        );
        let (c0, c1) = (
            start.col.min(end.col) as usize,
            start.col.max(end.col) as usize + 1,
        );
        let mut acc = 0.0;
        let num_tag = ValueTag::Number as u8;
        for c in c0..c1.min(self.columns.len()) {
            let d = &self.columns[c];
            if !d.has_numbers() {
                continue;
            }
            let hi = r1.min(d.len);
            if r0 >= hi {
                continue;
            }
            let tags = &self.mmap[(d.tags_off + r0) as usize..(d.tags_off + hi) as usize];
            let nums_base = d.nums_off + r0 * 8;
            for (i, &t) in tags.iter().enumerate() {
                if t == num_tag {
                    let o = (nums_base + i as u64 * 8) as usize;
                    acc += f64::from_le_bytes(self.mmap[o..o + 8].try_into().unwrap());
                }
            }
        }
        acc
    }

    pub fn count_rect(&self, start: CellRef, end: CellRef) -> usize {
        let (r0, r1) = (
            start.row.min(end.row) as u64,
            start.row.max(end.row) as u64 + 1,
        );
        let (c0, c1) = (
            start.col.min(end.col) as usize,
            start.col.max(end.col) as usize + 1,
        );
        let num_tag = ValueTag::Number as u8;
        let mut n = 0usize;
        for c in c0..c1.min(self.columns.len()) {
            let d = &self.columns[c];
            let hi = r1.min(d.len);
            if r0 >= hi {
                continue;
            }
            let tags = &self.mmap[(d.tags_off + r0) as usize..(d.tags_off + hi) as usize];
            n += tags.iter().filter(|&&t| t == num_tag).count();
        }
        n
    }

    pub fn min_max_rect(&self, start: CellRef, end: CellRef) -> Option<(f64, f64)> {
        let (r0, r1) = (
            start.row.min(end.row) as u64,
            start.row.max(end.row) as u64 + 1,
        );
        let (c0, c1) = (
            start.col.min(end.col) as usize,
            start.col.max(end.col) as usize + 1,
        );
        let num_tag = ValueTag::Number as u8;
        let mut lo = f64::INFINITY;
        let mut hi_v = f64::NEG_INFINITY;
        let mut seen = false;
        for c in c0..c1.min(self.columns.len()) {
            let d = &self.columns[c];
            if !d.has_numbers() {
                continue;
            }
            let hi = r1.min(d.len);
            if r0 >= hi {
                continue;
            }
            let tags = &self.mmap[(d.tags_off + r0) as usize..(d.tags_off + hi) as usize];
            let nums_base = d.nums_off + r0 * 8;
            for (i, &t) in tags.iter().enumerate() {
                if t == num_tag {
                    let o = (nums_base + i as u64 * 8) as usize;
                    let v = f64::from_le_bytes(self.mmap[o..o + 8].try_into().unwrap());
                    if v < lo {
                        lo = v;
                    }
                    if v > hi_v {
                        hi_v = v;
                    }
                    seen = true;
                }
            }
        }
        seen.then_some((lo, hi_v))
    }
}

#[inline]
const fn decode_error(b: u8) -> ErrorKind {
    match b {
        0 => ErrorKind::DivZero,
        1 => ErrorKind::Value,
        2 => ErrorKind::Ref,
        3 => ErrorKind::Name,
        4 => ErrorKind::Num,
        5 => ErrorKind::NotAvailable,
        6 => ErrorKind::Null,
        _ => ErrorKind::Circular,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{cache_path_for, convert_csv};
    use std::io::Write;

    fn scratch() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ferrix_map_{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Build a mapped sheet from CSV text, returning it plus paths to clean.
    fn mapped(name: &str, csv: &str) -> (MappedSheet, std::path::PathBuf, std::path::PathBuf) {
        let src = scratch().join(name);
        File::create(&src)
            .unwrap()
            .write_all(csv.as_bytes())
            .unwrap();
        let dst = cache_path_for(&src);
        convert_csv(&src, &dst, b',', true, |_, _| {}).unwrap();
        let m = MappedSheet::open(&dst).unwrap();
        (m, src, dst)
    }

    fn cleanup(src: std::path::PathBuf, dst: std::path::PathBuf) {
        let _ = std::fs::remove_file(src);
        let _ = std::fs::remove_file(dst);
    }

    #[test]
    fn roundtrips_values_through_disk() {
        let (m, s, d) = mapped("rt.csv", "n,t,b\n1.5,alpha,true\n2.5,beta,false\n,gamma,\n");
        assert_eq!(m.row_count(), 3);
        assert_eq!(m.col_count(), 3);
        assert_eq!(m.get(CellRef::new(0, 0)), Value::Number(1.5));
        assert_eq!(m.get(CellRef::new(1, 0)), Value::Number(2.5));
        assert_eq!(m.get(CellRef::new(2, 0)), Value::Empty);
        assert_eq!(m.display(CellRef::new(0, 1)), "alpha");
        assert_eq!(m.display(CellRef::new(2, 1)), "gamma");
        assert_eq!(m.get(CellRef::new(0, 2)), Value::Bool(true));
        assert_eq!(m.get(CellRef::new(1, 2)), Value::Bool(false));
        cleanup(s, d);
    }

    #[test]
    fn out_of_range_reads_are_empty_not_panics() {
        let (m, s, d) = mapped("oob.csv", "a\n1\n");
        assert_eq!(m.get(CellRef::new(999, 0)), Value::Empty);
        assert_eq!(m.get(CellRef::new(0, 99)), Value::Empty);
        assert_eq!(m.resolve(StrId(9999)), "");
        cleanup(s, d);
    }

    #[test]
    fn aggregates_match_expected() {
        let mut csv = String::from("v\n");
        for i in 1..=1000 {
            csv.push_str(&format!("{i}\n"));
        }
        let (m, s, d) = mapped("agg.csv", &csv);
        let range = (CellRef::new(0, 0), CellRef::new(999, 0));
        // 1..=1000 sums to 500500
        assert_eq!(m.sum_rect(range.0, range.1), 500_500.0);
        assert_eq!(m.count_rect(range.0, range.1), 1000);
        assert_eq!(m.min_max_rect(range.0, range.1), Some((1.0, 1000.0)));
        cleanup(s, d);
    }

    #[test]
    fn aggregates_skip_text_and_empty() {
        let (m, s, d) = mapped("mixed.csv", "v\n10\nhello\n\n20\n");
        let r = (CellRef::new(0, 0), CellRef::new(3, 0));
        assert_eq!(m.sum_rect(r.0, r.1), 30.0);
        assert_eq!(m.count_rect(r.0, r.1), 2);
        cleanup(s, d);
    }

    #[test]
    fn string_dedup_survives_the_format() {
        let mut csv = String::from("cat\n");
        for i in 0..3000 {
            csv.push_str(["alpha", "beta", "gamma"][i % 3]);
            csv.push('\n');
        }
        let (m, s, d) = mapped("dedup.csv", &csv);
        assert_eq!(m.row_count(), 3000);
        assert_eq!(m.display(CellRef::new(0, 0)), "alpha");
        assert_eq!(m.display(CellRef::new(2999, 0)), "gamma");
        // 3000 rows of 3 distinct strings must not cost 3000 strings on disk.
        assert!(
            m.mapped_bytes() < 30_000,
            "3000 low-cardinality rows took {} bytes",
            m.mapped_bytes()
        );
        cleanup(s, d);
    }

    #[test]
    fn unicode_survives_the_format() {
        let (m, s, d) = mapped("uni.csv", "t\nhéllo → 世界\nplain\n");
        assert_eq!(m.display(CellRef::new(0, 0)), "héllo → 世界");
        assert_eq!(m.display(CellRef::new(1, 0)), "plain");
        cleanup(s, d);
    }

    #[test]
    fn rejects_corrupt_file() {
        let p = scratch().join("corrupt.ferrix");
        File::create(&p)
            .unwrap()
            .write_all(b"definitely not a ferrix file")
            .unwrap();
        assert!(matches!(
            MappedSheet::open(&p),
            Err(FormatError::BadMagic) | Err(FormatError::Truncated)
        ));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn open_does_not_read_the_whole_file() {
        // The scale claim: open is O(1). We assert it by timing a file large
        // enough that a full read would be measurably slow.
        let mut csv = String::from("a,b\n");
        for i in 0..200_000 {
            csv.push_str(&format!("{i},{}\n", i * 2));
        }
        let (m, s, d) = mapped("bigopen.csv", &csv);
        let bytes = m.mapped_bytes();
        drop(m);

        let t = std::time::Instant::now();
        let m2 = MappedSheet::open(&d).unwrap();
        let micros = t.elapsed().as_micros();
        assert_eq!(m2.row_count(), 200_000);
        assert!(
            micros < 50_000,
            "opening a {bytes}-byte mapping took {micros}µs — is it reading the file?"
        );
        cleanup(s, d);
    }
}
