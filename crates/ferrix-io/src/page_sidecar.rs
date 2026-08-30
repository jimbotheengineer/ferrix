//! The `.fxpage` sidecar: persisted page setup and print area (#37 follow-up).
//!
//! ## Why page setup gets its own file
//!
//! Same reasoning as the other `*_sidecar.rs` modules. Page setup — paper,
//! orientation, margins, scaling, repeat rows/cols, gridlines, headings, the
//! three-part header and footer, page order, the manual page breaks, and the
//! print area — is a statement about how the sheet lands on PAPER, not about
//! its contents. It stays true after the base file is regenerated with a
//! million more rows, so it is keyed to the file rather than tied to the
//! `.fxedits` base fingerprint, which would throw the layout away on every
//! data refresh. Before this file, `page_setup` and `print_area` lived only on
//! `FerrixApp` and were lost on close/reopen.
//!
//! ## Size
//!
//! O(manual breaks + header/footer text), never O(rows) and never O(cells).
//! A sheet with no manual breaks and empty header/footer writes a fixed-size
//! record. Manual breaks are a short sorted list, the same shape they have in
//! memory, so this stays cheap on a 200M-row sheet.
//!
//! ## Layout
//!
//! ```text
//!   [magic      ] 8 bytes  "FXPAGE01"
//!   [version    ] u32
//!   [paper      ] u8   PaperSize discriminant
//!   [orientation] u8   Orientation discriminant
//!   [order      ] u8   PageOrder discriminant
//!   [flags      ] u8   bit0 gridlines, bit1 headings
//!   [margins    ] 6 x f32  left,right,top,bottom,header,footer
//!   [scaling    ] u8 kind (0=Percent,1=FitTo) then:
//!                   Percent: u16
//!                   FitTo:   u8 has_wide, u16 wide, u8 has_tall, u16 tall
//!   [repeat_rows] u8 present, then (u32,u32) when present
//!   [repeat_cols] u8 present, then (u32,u32) when present
//!   [header     ] three length-prefixed utf8 strings (left,center,right)
//!   [footer     ] three length-prefixed utf8 strings (left,center,right)
//!   [row_breaks ] u32 count, then that many u32
//!   [col_breaks ] u32 count, then that many u32
//!   [print_area ] u8 present, then (first_row,last_row,first_col,last_col) u32
//! ```
//!
//! Every list is length-prefixed and written in the order the in-memory value
//! already holds (breaks are kept sorted), so saving the same state twice
//! produces identical bytes.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use ferrix_core::page::{
    HeaderFooter, Margins, Orientation, PageOrder, PageSetup, PaperSize, Scaling,
};
use ferrix_core::TableRange;

pub const PAGE_MAGIC: &[u8; 8] = b"FXPAGE01";
pub const PAGE_VERSION: u32 = 1;

/// A cap on how much header/footer text and how many manual breaks a single
/// sidecar record may declare. Well past any real sheet, but small enough that
/// a crafted count can never reserve enough to abort the process. See the
/// per-record grow note on the read side.
const MAX_RECORDS: usize = 64 * 1024 * 1024;

/// Everything persisted for one sheet's page setup.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct PageState {
    pub setup: PageSetup,
    pub print_area: Option<TableRange>,
}

#[derive(Debug)]
pub enum PageSidecarError {
    Io(io::Error),
    BadMagic,
    BadVersion(u32),
    Truncated,
    BadEnum,
    BadUtf8,
}

impl std::fmt::Display for PageSidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageSidecarError::Io(e) => write!(f, "{e}"),
            PageSidecarError::BadMagic => write!(f, "not a Ferrix page setup file"),
            PageSidecarError::BadVersion(v) => write!(f, "unsupported page setup version {v}"),
            PageSidecarError::Truncated => write!(f, "page setup file is truncated"),
            PageSidecarError::BadEnum => write!(f, "page setup file has an out-of-range value"),
            PageSidecarError::BadUtf8 => write!(f, "page setup header/footer text is not UTF-8"),
        }
    }
}

impl std::error::Error for PageSidecarError {}

impl From<io::Error> for PageSidecarError {
    fn from(e: io::Error) -> Self {
        PageSidecarError::Io(e)
    }
}

/// Sidecar path for a base file: `sales.csv` -> `sales.csv.fxpage`.
pub fn page_path_for(base: &Path) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(".fxpage");
    PathBuf::from(s)
}

// ==================================================================== write ==

fn put_u32<W: Write>(w: &mut W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_u16<W: Write>(w: &mut W, v: u16) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_f32<W: Write>(w: &mut W, v: f32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_str<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    put_u32(w, s.len() as u32)?;
    w.write_all(s.as_bytes())
}

fn put_header_footer<W: Write>(w: &mut W, hf: &HeaderFooter) -> io::Result<()> {
    put_str(w, &hf.left)?;
    put_str(w, &hf.center)?;
    put_str(w, &hf.right)
}

fn paper_disc(p: PaperSize) -> u8 {
    match p {
        PaperSize::Letter => 0,
        PaperSize::Legal => 1,
        PaperSize::Tabloid => 2,
        PaperSize::A3 => 3,
        PaperSize::A4 => 4,
        PaperSize::A5 => 5,
    }
}

/// Write the sidecar atomically, returning its size in bytes.
pub fn save_page(path: &Path, state: &PageState) -> Result<u64, PageSidecarError> {
    let tmp = path.with_extension("fxpage.tmp");
    {
        let f = File::create(&tmp)?;
        let mut w = BufWriter::new(f);
        let s = &state.setup;

        w.write_all(PAGE_MAGIC)?;
        put_u32(&mut w, PAGE_VERSION)?;

        w.write_all(&[paper_disc(s.paper)])?;
        w.write_all(&[match s.orientation {
            Orientation::Portrait => 0u8,
            Orientation::Landscape => 1u8,
        }])?;
        w.write_all(&[match s.order {
            PageOrder::DownThenOver => 0u8,
            PageOrder::OverThenDown => 1u8,
        }])?;
        let flags = (u8::from(s.gridlines)) | (u8::from(s.headings) << 1);
        w.write_all(&[flags])?;

        let m = &s.margins;
        for v in [m.left, m.right, m.top, m.bottom, m.header, m.footer] {
            put_f32(&mut w, v)?;
        }

        match s.scaling {
            Scaling::Percent(p) => {
                w.write_all(&[0u8])?;
                put_u16(&mut w, p)?;
            }
            Scaling::FitTo { wide, tall } => {
                w.write_all(&[1u8])?;
                w.write_all(&[u8::from(wide.is_some())])?;
                put_u16(&mut w, wide.unwrap_or(0))?;
                w.write_all(&[u8::from(tall.is_some())])?;
                put_u16(&mut w, tall.unwrap_or(0))?;
            }
        }

        for repeat in [s.repeat_rows, s.repeat_cols] {
            match repeat {
                Some((a, b)) => {
                    w.write_all(&[1u8])?;
                    put_u32(&mut w, a)?;
                    put_u32(&mut w, b)?;
                }
                None => w.write_all(&[0u8])?,
            }
        }

        put_header_footer(&mut w, &s.header)?;
        put_header_footer(&mut w, &s.footer)?;

        put_u32(&mut w, s.row_breaks.len() as u32)?;
        for &r in &s.row_breaks {
            put_u32(&mut w, r)?;
        }
        put_u32(&mut w, s.col_breaks.len() as u32)?;
        for &c in &s.col_breaks {
            put_u32(&mut w, c)?;
        }

        match state.print_area {
            Some(r) => {
                w.write_all(&[1u8])?;
                put_u32(&mut w, r.first_row)?;
                put_u32(&mut w, r.last_row)?;
                put_u32(&mut w, r.first_col)?;
                put_u32(&mut w, r.last_col)?;
            }
            None => w.write_all(&[0u8])?,
        }

        w.flush()?;
    }
    let size = std::fs::metadata(&tmp)?.len();
    // Windows will not rename onto an existing file.
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    Ok(size)
}

// ===================================================================== read ==

struct Cursor<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PageSidecarError> {
        // `checked_add`, not `self.p + n` — same reason as the other sidecar
        // cursors (issue #57): a near-`usize::MAX` length wraps the sum small,
        // passes this check, and slice-panics on the range that follows.
        let end = self.p.checked_add(n).ok_or(PageSidecarError::Truncated)?;
        if end > self.d.len() {
            return Err(PageSidecarError::Truncated);
        }
        let s = &self.d[self.p..end];
        self.p = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, PageSidecarError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u16(&mut self) -> Result<u16, PageSidecarError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn f32(&mut self) -> Result<f32, PageSidecarError> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u8(&mut self) -> Result<u8, PageSidecarError> {
        Ok(self.take(1)?[0])
    }

    fn bool(&mut self) -> Result<bool, PageSidecarError> {
        Ok(self.u8()? != 0)
    }

    fn string(&mut self) -> Result<String, PageSidecarError> {
        let n = self.u32()? as usize;
        // A crafted length can claim far more bytes than the file holds. `take`
        // bounds it against the real length before any allocation, so a
        // 0xFFFFFFFF length is refused as Truncated rather than reserving 4GB.
        let bytes = self.take(n)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| PageSidecarError::BadUtf8)
    }

    fn header_footer(&mut self) -> Result<HeaderFooter, PageSidecarError> {
        Ok(HeaderFooter {
            left: self.string()?,
            center: self.string()?,
            right: self.string()?,
        })
    }

    /// Read a length-prefixed `u32` list, growing per record.
    ///
    /// NOT `Vec::with_capacity(n)`: `n` is read from the file, and a crafted
    /// 0xFFFFFFFF count times 4 bytes reserves 16GB BEFORE the read loop can
    /// fail — an allocation abort, uncatchable under `panic = "unwind"`, that
    /// takes unsaved edits with it. Same pattern as the other sidecars.
    fn u32_list(&mut self) -> Result<Vec<u32>, PageSidecarError> {
        let n = self.u32()? as usize;
        if n > MAX_RECORDS {
            return Err(PageSidecarError::Truncated);
        }
        let mut v = Vec::new();
        for _ in 0..n {
            v.push(self.u32()?);
        }
        Ok(v)
    }
}

fn paper_from(disc: u8) -> Result<PaperSize, PageSidecarError> {
    Ok(match disc {
        0 => PaperSize::Letter,
        1 => PaperSize::Legal,
        2 => PaperSize::Tabloid,
        3 => PaperSize::A3,
        4 => PaperSize::A4,
        5 => PaperSize::A5,
        _ => return Err(PageSidecarError::BadEnum),
    })
}

/// Load a sidecar, or `None` when the file does not exist.
pub fn load_page(path: &Path) -> Result<Option<PageState>, PageSidecarError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    let mut c = Cursor { d: &buf, p: 0 };

    if c.take(8)? != PAGE_MAGIC {
        return Err(PageSidecarError::BadMagic);
    }
    let version = c.u32()?;
    if version != PAGE_VERSION {
        return Err(PageSidecarError::BadVersion(version));
    }

    let paper = paper_from(c.u8()?)?;
    let orientation = match c.u8()? {
        0 => Orientation::Portrait,
        1 => Orientation::Landscape,
        _ => return Err(PageSidecarError::BadEnum),
    };
    let order = match c.u8()? {
        0 => PageOrder::DownThenOver,
        1 => PageOrder::OverThenDown,
        _ => return Err(PageSidecarError::BadEnum),
    };
    let flags = c.u8()?;
    let gridlines = flags & 1 != 0;
    let headings = flags & 2 != 0;

    let margins = Margins {
        left: c.f32()?,
        right: c.f32()?,
        top: c.f32()?,
        bottom: c.f32()?,
        header: c.f32()?,
        footer: c.f32()?,
    };

    let scaling = match c.u8()? {
        0 => Scaling::Percent(c.u16()?),
        1 => {
            let has_wide = c.bool()?;
            let wide_v = c.u16()?;
            let has_tall = c.bool()?;
            let tall_v = c.u16()?;
            Scaling::FitTo {
                wide: has_wide.then_some(wide_v),
                tall: has_tall.then_some(tall_v),
            }
        }
        _ => return Err(PageSidecarError::BadEnum),
    };

    let mut read_repeat = || -> Result<Option<(u32, u32)>, PageSidecarError> {
        if c.bool()? {
            let a = c.u32()?;
            let b = c.u32()?;
            Ok(Some((a, b)))
        } else {
            Ok(None)
        }
    };
    let repeat_rows = read_repeat()?;
    let repeat_cols = read_repeat()?;

    let header = c.header_footer()?;
    let footer = c.header_footer()?;

    let row_breaks = c.u32_list()?;
    let col_breaks = c.u32_list()?;

    let print_area = if c.bool()? {
        let first_row = c.u32()?;
        let last_row = c.u32()?;
        let first_col = c.u32()?;
        let last_col = c.u32()?;
        // Normalise through the constructor, so a corrupt reversed range comes
        // back well-formed rather than with last < first.
        Some(TableRange::new(first_row, first_col, last_row, last_col))
    } else {
        None
    };

    Ok(Some(PageState {
        setup: PageSetup {
            paper,
            orientation,
            margins,
            scaling,
            repeat_rows,
            repeat_cols,
            gridlines,
            headings,
            header,
            footer,
            order,
            row_breaks,
            col_breaks,
        },
        print_area,
    }))
}

#[cfg(test)]
#[path = "page_sidecar/tests.rs"]
mod tests;
