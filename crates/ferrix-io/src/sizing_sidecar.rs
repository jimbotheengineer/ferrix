//! The `.fxsize` sidecar: persisted row heights, column widths, hidden spans
//! and outline groups (issue #29).
//!
//! ## Why sizing gets its own file
//!
//! Same reasoning as `format_sidecar.rs`, and deliberately the same shape.
//! Sizing is a statement about the SHAPE of a sheet — "column 3 is 240px
//! wide", "rows 100..200 are a collapsed group" — not about its contents. It
//! stays true and useful after the base file is regenerated with a million
//! more rows, so it is not guarded by the base fingerprint that `.fxedits`
//! refuses to load without. Tying it to one would throw away the user's
//! layout every time their data refreshed.
//!
//! ## Size
//!
//! O(spans + columns), never O(rows) and never O(cells) — the same property
//! the in-memory [`SheetSizing`] has. Hiding a 200M-row range writes twelve
//! bytes. That is the whole reason the model is range-keyed.
//!
//! ## Layout
//!
//! ```text
//!   [magic     ] 8 bytes  "FXSIZ001"
//!   [version   ] u32
//!   [counts    ] row spans u32, col widths u32, hidden cols u32,
//!                row groups u32, col groups u32
//!   [row spans ] per span: first u32, last u32, height f32
//!   [col widths] per column: col u32, width f32
//!   [hidden cols] per column: col u32
//!   [row groups] per group: first u32, last u32, level u8, collapsed u8
//!   [col groups] per group: first u32, last u32, level u8, collapsed u8
//! ```
//!
//! Every list is length-prefixed and written in key order, so saving twice
//! produces identical bytes — the same reproducibility the other sidecars get
//! by sorting.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use ferrix_core::sizing::{ColSizes, Outline, OutlineGroup, RowSizes, SheetSizing};

pub const SIZE_MAGIC: &[u8; 8] = b"FXSIZ001";
pub const SIZE_VERSION: u32 = 1;

#[derive(Debug)]
pub enum SizeSidecarError {
    Io(io::Error),
    BadMagic,
    BadVersion(u32),
    Truncated,
}

impl std::fmt::Display for SizeSidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SizeSidecarError::Io(e) => write!(f, "{e}"),
            SizeSidecarError::BadMagic => write!(f, "not a Ferrix sizing file"),
            SizeSidecarError::BadVersion(v) => write!(f, "unsupported sizing version {v}"),
            SizeSidecarError::Truncated => write!(f, "sizing file is truncated"),
        }
    }
}

impl std::error::Error for SizeSidecarError {}

impl From<io::Error> for SizeSidecarError {
    fn from(e: io::Error) -> Self {
        SizeSidecarError::Io(e)
    }
}

/// Sidecar path for a base file: `sales.csv` -> `sales.csv.fxsize`.
pub fn sizing_path_for(base: &Path) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(".fxsize");
    PathBuf::from(s)
}

// ==================================================================== write ==

fn put_u32<W: Write>(w: &mut W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_f32<W: Write>(w: &mut W, v: f32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_group<W: Write>(w: &mut W, g: &OutlineGroup) -> io::Result<()> {
    put_u32(w, g.first)?;
    put_u32(w, g.last)?;
    w.write_all(&[g.level, u8::from(g.collapsed)])
}

/// Write the sidecar atomically, returning its size in bytes.
pub fn save_sizing(path: &Path, s: &SheetSizing) -> Result<u64, SizeSidecarError> {
    let tmp = path.with_extension("fxsize.tmp");
    {
        let f = File::create(&tmp)?;
        let mut w = BufWriter::new(f);

        w.write_all(SIZE_MAGIC)?;
        put_u32(&mut w, SIZE_VERSION)?;

        let row_spans: Vec<(u32, u32, f32)> = s.rows.spans().collect();
        let col_widths: Vec<(u32, f32)> = s.cols.widths().collect();
        let hidden_cols: Vec<u32> = s.cols.hidden_cols().collect();

        put_u32(&mut w, row_spans.len() as u32)?;
        put_u32(&mut w, col_widths.len() as u32)?;
        put_u32(&mut w, hidden_cols.len() as u32)?;
        put_u32(&mut w, s.row_outline.len() as u32)?;
        put_u32(&mut w, s.col_outline.len() as u32)?;

        for (first, last, h) in row_spans {
            put_u32(&mut w, first)?;
            put_u32(&mut w, last)?;
            put_f32(&mut w, h)?;
        }
        for (col, width) in col_widths {
            put_u32(&mut w, col)?;
            put_f32(&mut w, width)?;
        }
        for col in hidden_cols {
            put_u32(&mut w, col)?;
        }
        for g in s.row_outline.groups() {
            put_group(&mut w, g)?;
        }
        for g in s.col_outline.groups() {
            put_group(&mut w, g)?;
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
    fn take(&mut self, n: usize) -> Result<&'a [u8], SizeSidecarError> {
        if self.p + n > self.d.len() {
            return Err(SizeSidecarError::Truncated);
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, SizeSidecarError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn f32(&mut self) -> Result<f32, SizeSidecarError> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u8(&mut self) -> Result<u8, SizeSidecarError> {
        Ok(self.take(1)?[0])
    }
}

/// Load a sidecar, or `None` when the file does not exist.
pub fn load_sizing(path: &Path) -> Result<Option<SheetSizing>, SizeSidecarError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    let mut c = Cursor { d: &buf, p: 0 };

    if c.take(8)? != SIZE_MAGIC {
        return Err(SizeSidecarError::BadMagic);
    }
    let version = c.u32()?;
    if version != SIZE_VERSION {
        return Err(SizeSidecarError::BadVersion(version));
    }

    let n_row_spans = c.u32()? as usize;
    let n_widths = c.u32()? as usize;
    let n_hidden = c.u32()? as usize;
    let n_row_groups = c.u32()? as usize;
    let n_col_groups = c.u32()? as usize;

    let mut rows = RowSizes::new();
    for _ in 0..n_row_spans {
        let first = c.u32()?;
        let last = c.u32()?;
        let h = c.f32()?;
        rows.set_range(first, last, h);
    }
    let mut cols = ColSizes::new();
    for _ in 0..n_widths {
        let col = c.u32()?;
        let w = c.f32()?;
        cols.set_width(col, w);
    }
    for _ in 0..n_hidden {
        cols.hide(c.u32()?);
    }

    // Groups are restored with their STORED level and collapsed state rather
    // than re-derived: `Outline::group` recomputes a level from containment,
    // which is right when the user makes a group and wrong when reloading one,
    // because re-adding an inner group before its parent would give it level 1.
    let mut row_outline = Outline::new();
    let mut col_outline = Outline::new();
    for (outline, n) in [
        (&mut row_outline, n_row_groups),
        (&mut col_outline, n_col_groups),
    ] {
        let mut groups = Vec::with_capacity(n);
        for _ in 0..n {
            let first = c.u32()?;
            let last = c.u32()?;
            let level = c.u8()?;
            let collapsed = c.u8()? != 0;
            groups.push(OutlineGroup {
                first,
                last,
                level,
                collapsed,
            });
        }
        *outline = Outline::from_groups(groups);
    }

    Ok(Some(SheetSizing {
        rows,
        cols,
        row_outline,
        col_outline,
    }))
}

#[cfg(test)]
#[path = "sizing_sidecar/tests.rs"]
mod tests;
