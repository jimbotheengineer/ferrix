//! The `.fxnotes` sidecar: persisted cell comments.
//!
//! ## Why comments get their own file
//!
//! Same reasoning as `.fxfmt`, and for the same reason it is a *separate* file
//! from `.fxedits`: a comment is a statement about a cell that stays true when
//! the base is regenerated. "Check this with finance" on B7 survives the CSV
//! being re-exported with a million more rows. Tying comments to the edit
//! sidecar's base fingerprint would throw the user's annotations away every
//! time their data refreshed, which is precisely the workflow annotations
//! exist to serve.
//!
//! Comments are also not edits. They can exist on a cell that was never typed
//! in, and an edit can exist on a cell with no comment, so neither store is a
//! subset of the other and packing them together would mean writing one
//! whenever the other changed.
//!
//! ## Size
//!
//! O(comments). Three comments on a 200M-row sheet write a few hundred bytes.
//!
//! ## Layout, and why it is read sequentially rather than seeked
//!
//! ```text
//!   [magic  ] 8 bytes  "FXNOTE01"
//!   [version] u32
//!   [count  ] u32       number of comment records
//!   [records] count of:
//!               row     u32
//!               col     u32
//!               author  u32 byte length, then that many UTF-8 bytes
//!               text    u32 byte length, then that many UTF-8 bytes
//! ```
//!
//! A comment is **inherently variable-length** — it is user prose — so the
//! fixed-width, seek-addressable record layout the `.fxfmt` reader could have
//! used is not available here: every record's offset depends on the length of
//! every record before it, and no amount of layout cleverness changes that
//! short of a separate offset index.
//!
//! Building that index was considered and rejected. It would buy random access
//! to comment number N, which nothing wants: every consumer of this file loads
//! **all** comments at once, because the whole point of the store is that
//! there are few of them. An index would add a second structure to keep
//! consistent, in exchange for an access pattern with no caller. So the file
//! is a straight length-prefixed stream, read front to back into a
//! [`CommentMap`] in one pass, exactly as `format_sidecar.rs` reads its own
//! variable-length parts.
//!
//! Every string is length-prefixed and the record count is written up front,
//! so a truncated file is *detected* — [`CommentSidecarError::Truncated`] —
//! rather than silently yielding a shorter set of comments than the user saved.
//!
//! Records are written in (row, column) order, which is the order
//! [`CommentMap::iter`] yields, so saving the same map twice produces
//! byte-identical files.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use ferrix_core::{CellRef, Comment, CommentMap};

pub const NOTE_MAGIC: &[u8; 8] = b"FXNOTE01";
pub const NOTE_VERSION: u32 = 1;

#[derive(Debug)]
pub enum CommentSidecarError {
    Io(io::Error),
    BadMagic,
    BadVersion(u32),
    Truncated,
}

impl std::fmt::Display for CommentSidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommentSidecarError::Io(e) => write!(f, "{e}"),
            CommentSidecarError::BadMagic => write!(f, "not a Ferrix comments file"),
            CommentSidecarError::BadVersion(v) => write!(f, "unsupported comments version {v}"),
            CommentSidecarError::Truncated => write!(f, "comments file is truncated"),
        }
    }
}

impl std::error::Error for CommentSidecarError {}

impl From<io::Error> for CommentSidecarError {
    fn from(e: io::Error) -> Self {
        CommentSidecarError::Io(e)
    }
}

/// Sidecar path for a base file: `sales.ferrix` -> `sales.ferrix.fxnotes`.
///
/// Appends rather than substituting the extension, for the reason `edits.rs`
/// spells out: `Path::with_extension` would map `sales.ferrix` onto
/// `sales.fxnotes` and collide with a different file's sidecar.
pub fn comments_path_for(base: &Path) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(".fxnotes");
    PathBuf::from(s)
}

fn put_u32<W: Write>(w: &mut W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_str<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    put_u32(w, s.len() as u32)?;
    w.write_all(s.as_bytes())
}

/// Write the comment map to `path` atomically, returning the byte size.
///
/// An empty map DELETES the sidecar rather than writing a zero-record file.
/// Otherwise removing the last comment would leave a file that reloads as
/// "no comments" — the same outcome, but with a stale artefact on disk that
/// outlives the data it described.
pub fn save_comments(path: &Path, comments: &CommentMap) -> Result<u64, CommentSidecarError> {
    if comments.is_empty() {
        let _ = std::fs::remove_file(path);
        return Ok(0);
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let f = File::create(&tmp)?;
        let mut w = BufWriter::new(f);
        w.write_all(NOTE_MAGIC)?;
        put_u32(&mut w, NOTE_VERSION)?;
        put_u32(&mut w, comments.len() as u32)?;
        // `iter()` is (row, col) ordered, so two saves of one map are
        // byte-identical and backup dedup keeps working.
        for (cell, c) in comments.iter() {
            put_u32(&mut w, cell.row)?;
            put_u32(&mut w, cell.col)?;
            put_str(&mut w, &c.author)?;
            put_str(&mut w, &c.text)?;
        }
        w.flush()?;
        // fsync before the rename, for the reason edits.rs documents: without
        // it a power loss can leave a correctly-named empty file.
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
    fn take(&mut self, n: usize) -> Result<&'a [u8], CommentSidecarError> {
        if self.p.saturating_add(n) > self.d.len() {
            return Err(CommentSidecarError::Truncated);
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, CommentSidecarError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, CommentSidecarError> {
        let n = self.u32()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
}

/// Load a comment sidecar. `Ok(None)` means there simply isn't one.
pub fn load_comments(path: &Path) -> Result<Option<CommentMap>, CommentSidecarError> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;

    let mut c = Cursor { d: &buf, p: 0 };
    if c.take(8)? != NOTE_MAGIC {
        return Err(CommentSidecarError::BadMagic);
    }
    let v = c.u32()?;
    if v != NOTE_VERSION {
        return Err(CommentSidecarError::BadVersion(v));
    }
    let count = c.u32()? as usize;
    let mut map = CommentMap::new();
    for _ in 0..count {
        let row = c.u32()?;
        let col = c.u32()?;
        let author = c.string()?;
        let text = c.string()?;
        map.set(CellRef::new(row, col), Comment { author, text });
    }
    Ok(Some(map))
}

#[cfg(test)]
mod tests;
