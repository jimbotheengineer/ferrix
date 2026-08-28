//! The `.ferrix` on-disk columnar format.
//!
//! ## Why this exists
//!
//! A 10GB CSV is ~200M rows. Parsed into RAM at ~7.6 bytes/cell that is ~12GB
//! of heap — more than most machines have free, and it must be re-parsed on
//! every open. Instead we convert once to a columnar file laid out exactly as
//! the engine wants it, then `mmap` that file. Dataset size becomes bounded by
//! disk rather than RAM, and the OS page cache keeps hot rows resident for
//! free while evicting cold ones under pressure.
//!
//! ## Layout
//!
//! ```text
//!   [header        ] fixed 64 bytes, magic + version + counts + offsets
//!   [column table  ] one ColumnDesc per column
//!   [string arena  ] all interned bytes, then (offset,len) spans
//!   [column 0 tags ] 1 byte per row
//!   [column 0 nums ] 8 bytes per row  (present only if the column has numbers)
//!   [column 0 strs ] 4 bytes per row  (present only if the column has text)
//!   [column 1 ...  ]
//! ```
//!
//! Every section is 8-byte aligned so `f64` slices can be read directly out of
//! the mapping with no copy and no unaligned loads.
//!
//! Numbers are stored little-endian, matching every platform we target; on a
//! big-endian host the loader would need to byte-swap, which we detect and
//! reject rather than silently mis-read.

use std::io::{self, Write};

pub const MAGIC: &[u8; 8] = b"FERRIX01";
pub const VERSION: u32 = 1;
pub const HEADER_BYTES: usize = 64;

/// Fixed-size file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u32,
    pub rows: u64,
    pub cols: u32,
    /// Byte offset of the column descriptor table.
    pub col_table_off: u64,
    /// Byte offset and length of the string arena's byte buffer.
    pub arena_data_off: u64,
    pub arena_data_len: u64,
    /// Byte offset of the arena's span table (count = arena_spans).
    pub arena_spans_off: u64,
    pub arena_spans: u64,
}

impl Header {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut buf = [0u8; HEADER_BYTES];
        buf[0..8].copy_from_slice(MAGIC);
        buf[8..12].copy_from_slice(&self.version.to_le_bytes());
        buf[12..16].copy_from_slice(&self.cols.to_le_bytes());
        buf[16..24].copy_from_slice(&self.rows.to_le_bytes());
        buf[24..32].copy_from_slice(&self.col_table_off.to_le_bytes());
        buf[32..40].copy_from_slice(&self.arena_data_off.to_le_bytes());
        buf[40..48].copy_from_slice(&self.arena_data_len.to_le_bytes());
        buf[48..56].copy_from_slice(&self.arena_spans_off.to_le_bytes());
        buf[56..64].copy_from_slice(&self.arena_spans.to_le_bytes());
        w.write_all(&buf)
    }

    pub fn parse(data: &[u8]) -> Result<Self, FormatError> {
        if data.len() < HEADER_BYTES {
            return Err(FormatError::Truncated);
        }
        if &data[0..8] != MAGIC {
            return Err(FormatError::BadMagic);
        }
        let u32at = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().unwrap());
        let u64at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
        let version = u32at(8);
        if version != VERSION {
            return Err(FormatError::BadVersion(version));
        }
        Ok(Header {
            version,
            cols: u32at(12),
            rows: u64at(16),
            col_table_off: u64at(24),
            arena_data_off: u64at(32),
            arena_data_len: u64at(40),
            arena_spans_off: u64at(48),
            arena_spans: u64at(56),
        })
    }
}

/// Per-column descriptor. `u64::MAX` in an offset means "section absent",
/// which is how a text-only column avoids paying for an f64 array.
pub const ABSENT: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnDesc {
    pub tags_off: u64,
    pub nums_off: u64,
    pub strs_off: u64,
    /// Length in rows (may be shorter than the sheet for ragged data).
    pub len: u64,
}

pub const COL_DESC_BYTES: usize = 32;

impl ColumnDesc {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut buf = [0u8; COL_DESC_BYTES];
        buf[0..8].copy_from_slice(&self.tags_off.to_le_bytes());
        buf[8..16].copy_from_slice(&self.nums_off.to_le_bytes());
        buf[16..24].copy_from_slice(&self.strs_off.to_le_bytes());
        buf[24..32].copy_from_slice(&self.len.to_le_bytes());
        w.write_all(&buf)
    }

    pub fn parse(data: &[u8]) -> Result<Self, FormatError> {
        if data.len() < COL_DESC_BYTES {
            return Err(FormatError::Truncated);
        }
        let u64at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
        Ok(ColumnDesc {
            tags_off: u64at(0),
            nums_off: u64at(8),
            strs_off: u64at(16),
            len: u64at(24),
        })
    }

    pub fn has_numbers(&self) -> bool {
        self.nums_off != ABSENT
    }

    pub fn has_strings(&self) -> bool {
        self.strs_off != ABSENT
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("not a ferrix file (bad magic)")]
    BadMagic,
    #[error("unsupported ferrix format version {0}")]
    BadVersion(u32),
    #[error("file is truncated or corrupt")]
    Truncated,
    #[error("section offset {off} exceeds file length {len}")]
    OutOfBounds { off: u64, len: u64 },
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

/// Round `n` up to the next multiple of 8 so every section stays aligned for
/// direct `f64` reads out of the mapping.
#[inline]
pub const fn align8(n: u64) -> u64 {
    (n + 7) & !7
}

/// Pad a writer out to the next 8-byte boundary.
pub fn pad_to_align<W: Write>(w: &mut W, written: u64) -> io::Result<u64> {
    let target = align8(written);
    let pad = (target - written) as usize;
    if pad > 0 {
        w.write_all(&[0u8; 8][..pad])?;
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = Header {
            version: VERSION,
            rows: 200_000_000,
            cols: 8,
            col_table_off: 64,
            arena_data_off: 1024,
            arena_data_len: 512,
            arena_spans_off: 2048,
            arena_spans: 42,
        };
        let mut buf = Vec::new();
        h.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), HEADER_BYTES);
        assert_eq!(Header::parse(&buf).unwrap(), h);
    }

    #[test]
    fn header_rejects_garbage() {
        // Full-length input with the wrong magic -> BadMagic.
        let mut wrong_magic = vec![0u8; HEADER_BYTES];
        wrong_magic[0..8].copy_from_slice(b"NOTFRRX!");
        assert!(matches!(
            Header::parse(&wrong_magic),
            Err(FormatError::BadMagic)
        ));
        // Anything shorter than a header -> Truncated, checked first so we
        // never index out of bounds looking for the magic.
        assert!(matches!(Header::parse(b"FER"), Err(FormatError::Truncated)));
        assert!(matches!(Header::parse(&[]), Err(FormatError::Truncated)));
    }

    #[test]
    fn header_rejects_future_version() {
        let mut buf = vec![0u8; HEADER_BYTES];
        buf[0..8].copy_from_slice(MAGIC);
        buf[8..12].copy_from_slice(&999u32.to_le_bytes());
        assert!(matches!(
            Header::parse(&buf),
            Err(FormatError::BadVersion(999))
        ));
    }

    #[test]
    fn header_survives_200m_rows() {
        // u64 row counts: the format must not be the thing that caps us.
        let h = Header {
            version: VERSION,
            rows: u64::MAX / 2,
            cols: 16_384,
            col_table_off: 64,
            arena_data_off: 0,
            arena_data_len: 0,
            arena_spans_off: 0,
            arena_spans: 0,
        };
        let mut buf = Vec::new();
        h.write_to(&mut buf).unwrap();
        assert_eq!(Header::parse(&buf).unwrap().rows, u64::MAX / 2);
    }

    #[test]
    fn column_desc_roundtrip() {
        let d = ColumnDesc {
            tags_off: 100,
            nums_off: 200,
            strs_off: ABSENT,
            len: 12345,
        };
        let mut buf = Vec::new();
        d.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), COL_DESC_BYTES);
        let got = ColumnDesc::parse(&buf).unwrap();
        assert_eq!(got, d);
        assert!(got.has_numbers());
        assert!(!got.has_strings(), "ABSENT must mean no string section");
    }

    #[test]
    fn alignment_math() {
        assert_eq!(align8(0), 0);
        assert_eq!(align8(1), 8);
        assert_eq!(align8(7), 8);
        assert_eq!(align8(8), 8);
        assert_eq!(align8(9), 16);
    }

    #[test]
    fn padding_reaches_alignment() {
        for start in 0..17u64 {
            let mut buf = vec![0u8; start as usize];
            let end = pad_to_align(&mut buf, start).unwrap();
            assert_eq!(end % 8, 0, "start {start} did not align");
            assert_eq!(buf.len() as u64, end);
        }
    }
}
