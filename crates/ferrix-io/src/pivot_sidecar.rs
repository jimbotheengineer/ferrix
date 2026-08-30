//! The `.fxpivot` sidecar: persisted pivot-sheet bindings (issue #33 Part B).
//!
//! ## Why pivots get their own file
//!
//! Same reasoning as `.fxnotes` and `.fxfmt`: a pivot binding is workbook state
//! that stays true when the base data is regenerated. "Group Sales by Region,
//! sum Amount" survives the CSV being re-exported with a million more rows —
//! the spec references COLUMNS, not rows. Tying it to the edit sidecar's base
//! fingerprint would throw the definition away on every data refresh, which is
//! exactly the workflow a pivot exists to serve.
//!
//! A pivot binding is also not an edit and not a comment. It lives on a whole
//! SHEET (a pivot sheet is the result of applying a spec to a source sheet),
//! so it cannot share the cell-keyed layout of the other sidecars.
//!
//! ## Why a Ferrix sidecar and not the xlsx pivotCache
//!
//! xlsx has a native `pivotCacheDefinition`/`pivotTableDefinition` pair, but it
//! is a heavy, cache-oriented format built around Excel's own field model and
//! is out of scope for Part B (the builder UI that would populate it is Part C).
//! Part B needs the SPEC to round-trip intact through the workbook-state path
//! the project already uses for non-Excel state, which is the length-prefixed
//! binary sidecar convention every other `.fx*` file follows. This file is that.
//!
//! ## Size
//!
//! O(pivot sheets x spec size). A pivot binding is a source id plus a short list
//! of column indices and aggregate codes, so even a workbook full of pivots over
//! 200M-row sources writes a few hundred bytes. Nothing here is per-row.
//!
//! ## Layout
//!
//! ```text
//!   [magic   ] 8 bytes  "FXPIVOT1"
//!   [version ] u32
//!   [count   ] u32       number of pivot-sheet records
//!   [records ] count of:
//!               sheet_id     u32   the pivot sheet this binding belongs to
//!               source_id    u32   the sheet it pivots
//!               auto_refresh u8    0 = off, 1 = on
//!               n_group      u32   then that many u32 group-by column indices
//!               n_values     u32   then that many (u32 col, u8 agg) pairs
//! ```
//!
//! Records and their inner lists are written in a fixed order, so saving the
//! same set of bindings twice produces byte-identical files (backup dedup keeps
//! working). The count is written up front and every list is length-prefixed, so
//! a truncated file is DETECTED ([`PivotSidecarError::Truncated`]) rather than
//! silently yielding a shorter set of pivots than the user saved.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub const PIVOT_MAGIC: &[u8; 8] = b"FXPIVOT1";
pub const PIVOT_VERSION: u32 = 1;

/// A pivot binding as it lives on disk. Deliberately UI-agnostic: it carries the
/// raw ids, flags and the spec as plain integers so this crate does not depend
/// on the UI's `Workbook`. The UI reconstructs its richer binding from these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PivotRecord {
    /// The pivot sheet this binding defines.
    pub sheet_id: u32,
    /// The sheet it pivots.
    pub source_id: u32,
    /// Whether the pivot recomputes automatically when its source changes.
    pub auto_refresh: bool,
    /// Group-by columns, in order.
    pub group_by: Vec<u32>,
    /// `(value column, aggregate code)` pairs, in order. The aggregate code is
    /// the stable encoding from [`agg_code`] / [`agg_from_code`].
    pub values: Vec<(u32, u8)>,
}

#[derive(Debug)]
pub enum PivotSidecarError {
    Io(io::Error),
    BadMagic,
    BadVersion(u32),
    Truncated,
    /// An aggregate code on disk is outside the range this build understands.
    BadAgg(u8),
}

impl std::fmt::Display for PivotSidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PivotSidecarError::Io(e) => write!(f, "{e}"),
            PivotSidecarError::BadMagic => write!(f, "not a Ferrix pivot file"),
            PivotSidecarError::BadVersion(v) => write!(f, "unsupported pivot version {v}"),
            PivotSidecarError::Truncated => write!(f, "pivot file is truncated"),
            PivotSidecarError::BadAgg(c) => write!(f, "unknown pivot aggregate code {c}"),
        }
    }
}

impl std::error::Error for PivotSidecarError {}

impl From<io::Error> for PivotSidecarError {
    fn from(e: io::Error) -> Self {
        PivotSidecarError::Io(e)
    }
}

/// Sidecar path for a base file: `sales.ferrix` -> `sales.ferrix.fxpivot`.
///
/// Appends rather than substituting the extension, for the reason `edits.rs`
/// spells out: `Path::with_extension` would map `sales.ferrix` onto
/// `sales.fxpivot` and collide with a different file's sidecar.
pub fn pivot_path_for(base: &Path) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(".fxpivot");
    PathBuf::from(s)
}

/// The stable on-disk code for a pivot aggregate.
///
/// A dedicated encoding rather than `as u8` on the enum, so reordering the enum
/// in ferrix-core can never silently repoint an old file's aggregates. Callers
/// map their `PivotAgg` through here; the two functions are inverses and the
/// round-trip test guards that.
pub fn agg_code(name: &str) -> Option<u8> {
    Some(match name {
        "Sum" => 0,
        "Count" => 1,
        "Avg" => 2,
        "Min" => 3,
        "Max" => 4,
        "StdDev" => 5,
        _ => return None,
    })
}

/// Inverse of [`agg_code`]: the aggregate name for an on-disk code.
pub fn agg_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0 => "Sum",
        1 => "Count",
        2 => "Avg",
        3 => "Min",
        4 => "Max",
        5 => "StdDev",
        _ => return None,
    })
}

fn put_u32<W: Write>(w: &mut W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Write the pivot bindings to `path` atomically, returning the byte size.
///
/// An empty list DELETES the sidecar rather than writing a zero-record file, for
/// the reason `comment_sidecar.rs` documents: removing the last pivot would
/// otherwise leave a stale artefact that reloads as "no pivots" — the same
/// outcome, but with a file on disk that outlives the state it described.
pub fn save_pivots(path: &Path, records: &[PivotRecord]) -> Result<u64, PivotSidecarError> {
    if records.is_empty() {
        let _ = std::fs::remove_file(path);
        return Ok(0);
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let f = File::create(&tmp)?;
        let mut w = BufWriter::new(f);
        w.write_all(PIVOT_MAGIC)?;
        put_u32(&mut w, PIVOT_VERSION)?;
        put_u32(&mut w, records.len() as u32)?;
        for r in records {
            put_u32(&mut w, r.sheet_id)?;
            put_u32(&mut w, r.source_id)?;
            w.write_all(&[u8::from(r.auto_refresh)])?;
            put_u32(&mut w, r.group_by.len() as u32)?;
            for &g in &r.group_by {
                put_u32(&mut w, g)?;
            }
            put_u32(&mut w, r.values.len() as u32)?;
            for &(col, agg) in &r.values {
                put_u32(&mut w, col)?;
                w.write_all(&[agg])?;
            }
        }
        w.flush()?;
        // fsync before the rename, for the reason edits.rs documents: without it
        // a power loss can leave a correctly-named empty file.
        w.get_ref().sync_all()?;
    }
    let size = std::fs::metadata(&tmp)?.len();
    // Windows will not rename onto an existing file.
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    Ok(size)
}

struct Cursor<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PivotSidecarError> {
        if self.p.saturating_add(n) > self.d.len() {
            return Err(PivotSidecarError::Truncated);
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, PivotSidecarError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u8(&mut self) -> Result<u8, PivotSidecarError> {
        Ok(self.take(1)?[0])
    }
}

/// Load a pivot sidecar. `Ok(None)` means there simply isn't one.
pub fn load_pivots(path: &Path) -> Result<Option<Vec<PivotRecord>>, PivotSidecarError> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;

    let mut c = Cursor { d: &buf, p: 0 };
    if c.take(8)? != PIVOT_MAGIC {
        return Err(PivotSidecarError::BadMagic);
    }
    let v = c.u32()?;
    if v != PIVOT_VERSION {
        return Err(PivotSidecarError::BadVersion(v));
    }
    let count = c.u32()? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let sheet_id = c.u32()?;
        let source_id = c.u32()?;
        let auto_refresh = c.u8()? != 0;
        let n_group = c.u32()? as usize;
        let mut group_by = Vec::with_capacity(n_group);
        for _ in 0..n_group {
            group_by.push(c.u32()?);
        }
        let n_values = c.u32()? as usize;
        let mut values = Vec::with_capacity(n_values);
        for _ in 0..n_values {
            let col = c.u32()?;
            let agg = c.u8()?;
            // Reject an aggregate code this build does not understand rather
            // than silently dropping it: a spec that quietly lost a column
            // would be worse than a load error the user can act on.
            if agg_name(agg).is_none() {
                return Err(PivotSidecarError::BadAgg(agg));
            }
            values.push((col, agg));
        }
        out.push(PivotRecord {
            sheet_id,
            source_id,
            auto_refresh,
            group_by,
            values,
        });
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests;
