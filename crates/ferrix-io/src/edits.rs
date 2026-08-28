//! The `.fxedits` sidecar: persisted cell edits.
//!
//! ## Why a sidecar rather than rewriting the file
//!
//! The base dataset may be a 12 GB memory-mapped `.ferrix` cache. Rewriting it
//! to save three edited cells would cost minutes and require free disk equal to
//! the dataset. Instead the sparse `EditOverlay` — which is already the only
//! place edits live — is serialized on its own, next to the base:
//!
//! ```text
//!   sales.csv           the original
//!   sales.ferrix        columnar cache (12 GB, mmap'd)
//!   sales.ferrix.fxedits  edits only (kilobytes)
//! ```
//!
//! Saving is therefore **O(edits)**, not O(rows): a handful of edits over 200M
//! rows writes a handful of kilobytes.
//!
//! ## Staleness
//!
//! A sidecar is only meaningful against the exact base it was edited over. If
//! the base changes, cell (5, 2) may now hold something entirely different and
//! silently reapplying edits would corrupt the user's data. So the sidecar
//! records a fingerprint of the base — length, mtime, and its declared row and
//! column counts — and refuses to load when that fingerprint no longer matches.
//! Refusing loudly is the whole point: silent misapplication is the failure
//! mode that loses data.
//!
//! ## Layout
//!
//! ```text
//!   [magic      ] 8 bytes  "FXEDIT01"
//!   [version    ] u32
//!   [fingerprint] base_len u64, base_mtime u64, base_rows u64, base_cols u32
//!   [counts     ] cell_count u64, arena_len u64, arena_spans u64
//!   [arena data ] interned bytes for edited text
//!   [arena spans] (offset, len) pairs, u32 each
//!   [cells      ] cell_count records
//! ```
//!
//! Each cell record is:
//!
//! ```text
//!   row u32, col u32, kind u8
//!     kind 0 = literal empty
//!     kind 1 = literal number   -> f64
//!     kind 2 = literal bool     -> u8
//!     kind 3 = literal text     -> u32 arena id
//!     kind 4 = literal error    -> u8 error code
//!     kind 5 = formula          -> u32 src len, src bytes, then a nested
//!                                  value record for the cached result
//! ```
//!
//! Formulas store their **source text**, not just the cached result, so they
//! can be re-evaluated against the base on load.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use ferrix_core::{CellInput, CellRef, EditOverlay, ErrorKind, StrId, StringArena, Value};

pub const EDIT_MAGIC: &[u8; 8] = b"FXEDIT01";
pub const EDIT_VERSION: u32 = 1;

/// Identity of the base a sidecar was written against.
///
/// Cheap to compute (a `stat` plus counts already in memory) and enough to
/// catch the realistic failure: the source file being regenerated or edited
/// outside Ferrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaseFingerprint {
    pub len: u64,
    pub mtime: u64,
    pub rows: u64,
    pub cols: u32,
}

impl BaseFingerprint {
    /// Fingerprint a base file. `rows`/`cols` come from the loaded dataset
    /// rather than the file so a truncated-but-same-size file is still caught.
    pub fn of(path: &Path, rows: u64, cols: u32) -> io::Result<Self> {
        let md = std::fs::metadata(path)?;
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(Self {
            len: md.len(),
            mtime,
            rows,
            cols,
        })
    }
}

#[derive(Debug)]
pub enum EditError {
    Io(io::Error),
    BadMagic,
    BadVersion(u32),
    Truncated,
    /// The sidecar was written against a different base.
    StaleBase {
        expected: BaseFingerprint,
        found: BaseFingerprint,
    },
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::Io(e) => write!(f, "{e}"),
            EditError::BadMagic => write!(f, "not a Ferrix edits file"),
            EditError::BadVersion(v) => write!(f, "unsupported edits version {v}"),
            EditError::Truncated => write!(f, "edits file is truncated"),
            EditError::StaleBase { expected, found } => write!(
                f,
                "edits were saved against a different version of this file \
                 (saved: {} bytes/{} rows, now: {} bytes/{} rows)",
                found.len, found.rows, expected.len, expected.rows
            ),
        }
    }
}

impl std::error::Error for EditError {}

impl From<io::Error> for EditError {
    fn from(e: io::Error) -> Self {
        EditError::Io(e)
    }
}

/// Where the sidecar for a given base lives.
pub fn edits_path_for(base: &Path) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(".fxedits");
    PathBuf::from(s)
}

// --- writing ---

fn put_u32<W: Write>(w: &mut W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_u64<W: Write>(w: &mut W, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_value<W: Write>(w: &mut W, v: &Value) -> io::Result<()> {
    match v {
        Value::Empty => w.write_all(&[0]),
        Value::Number(n) => {
            w.write_all(&[1])?;
            w.write_all(&n.to_le_bytes())
        }
        Value::Bool(b) => {
            w.write_all(&[2])?;
            w.write_all(&[*b as u8])
        }
        Value::Text(id) => {
            w.write_all(&[3])?;
            put_u32(w, id.0)
        }
        Value::Error(e) => {
            w.write_all(&[4])?;
            w.write_all(&[e.to_code()])
        }
    }
}

/// Write the overlay to `path` atomically.
///
/// Writes to a temporary file and renames, so a crash mid-save cannot leave a
/// half-written sidecar that would be rejected (or worse, partially applied)
/// on the next open.
pub fn save_edits(
    path: &Path,
    overlay: &EditOverlay,
    fingerprint: BaseFingerprint,
) -> Result<u64, EditError> {
    let tmp = path.with_extension("fxedits.tmp");
    {
        let f = File::create(&tmp)?;
        let mut w = BufWriter::new(f);

        w.write_all(EDIT_MAGIC)?;
        put_u32(&mut w, EDIT_VERSION)?;
        put_u64(&mut w, fingerprint.len)?;
        put_u64(&mut w, fingerprint.mtime)?;
        put_u64(&mut w, fingerprint.rows)?;
        put_u32(&mut w, fingerprint.cols)?;

        let (arena_bytes, arena_spans) = overlay.arena().raw_parts();
        put_u64(&mut w, overlay.len() as u64)?;
        put_u64(&mut w, arena_bytes.len() as u64)?;
        put_u64(&mut w, arena_spans.len() as u64)?;

        w.write_all(arena_bytes)?;
        for (off, len) in arena_spans {
            put_u32(&mut w, *off)?;
            put_u32(&mut w, *len)?;
        }

        // Sorted so saves are byte-reproducible; HashMap order is not stable
        // and a churning file defeats diffing and backup dedup.
        let mut cells: Vec<(&CellRef, &CellInput)> = overlay.edited_cells().collect();
        cells.sort_by_key(|(c, _)| (c.row, c.col));

        for (cell, input) in cells {
            put_u32(&mut w, cell.row)?;
            put_u32(&mut w, cell.col)?;
            match input {
                CellInput::Literal(v) => write_value(&mut w, v)?,
                CellInput::Formula { src, cached } => {
                    w.write_all(&[5])?;
                    let bytes = src.as_bytes();
                    put_u32(&mut w, bytes.len() as u32)?;
                    w.write_all(bytes)?;
                    write_value(&mut w, cached)?;
                }
            }
        }
        w.flush()?;
    }
    let size = std::fs::metadata(&tmp)?.len();
    // Windows will not rename onto an existing file.
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    Ok(size)
}

// --- reading ---

struct Cursor<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], EditError> {
        if self.p + n > self.d.len() {
            return Err(EditError::Truncated);
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, EditError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, EditError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, EditError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f64(&mut self) -> Result<f64, EditError> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn value(&mut self) -> Result<Value, EditError> {
        Ok(match self.u8()? {
            0 => Value::Empty,
            1 => Value::Number(self.f64()?),
            2 => Value::Bool(self.u8()? != 0),
            3 => Value::Text(StrId(self.u32()?)),
            4 => Value::Error(ErrorKind::from_code(self.u8()?)),
            _ => return Err(EditError::Truncated),
        })
    }
}

/// Load a sidecar, verifying it belongs to this base.
///
/// Returns `Ok(None)` when no sidecar exists — the common case for a file that
/// has never been edited, and not an error.
pub fn load_edits(
    path: &Path,
    expected: BaseFingerprint,
) -> Result<Option<EditOverlay>, EditError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    let mut c = Cursor { d: &buf, p: 0 };

    if c.take(8)? != EDIT_MAGIC {
        return Err(EditError::BadMagic);
    }
    let version = c.u32()?;
    if version != EDIT_VERSION {
        return Err(EditError::BadVersion(version));
    }

    let found = BaseFingerprint {
        len: c.u64()?,
        mtime: c.u64()?,
        rows: c.u64()?,
        cols: c.u32()?,
    };
    if found != expected {
        return Err(EditError::StaleBase { expected, found });
    }

    let cell_count = c.u64()? as usize;
    let arena_len = c.u64()? as usize;
    let span_count = c.u64()? as usize;

    let arena_bytes = c.take(arena_len)?.to_vec();
    let mut spans = Vec::with_capacity(span_count);
    for _ in 0..span_count {
        let off = c.u32()?;
        let len = c.u32()?;
        spans.push((off, len));
    }
    let arena = StringArena::from_raw_parts(arena_bytes, spans);

    let mut cells = HashMap::with_capacity(cell_count);
    for _ in 0..cell_count {
        let row = c.u32()?;
        let col = c.u32()?;
        let kind = c.u8()?;
        let input = if kind == 5 {
            let n = c.u32()? as usize;
            let src = String::from_utf8_lossy(c.take(n)?).into_owned();
            let cached = c.value()?;
            CellInput::Formula { src, cached }
        } else {
            // Re-read the tag byte as part of the value record.
            c.p -= 1;
            CellInput::Literal(c.value()?)
        };
        cells.insert(CellRef::new(row, col), input);
    }

    Ok(Some(EditOverlay::from_parts(cells, arena)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let d = std::env::temp_dir().join("ferrix_edit_tests");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fp() -> BaseFingerprint {
        BaseFingerprint {
            len: 1234,
            mtime: 99,
            rows: 1000,
            cols: 8,
        }
    }

    #[test]
    fn sidecar_path_appends_not_replaces() {
        // .with_extension() would turn sales.ferrix into sales.fxedits and
        // collide across different bases; we must append.
        let p = edits_path_for(Path::new("data/sales.ferrix"));
        assert!(p.to_string_lossy().ends_with("sales.ferrix.fxedits"));
    }

    #[test]
    fn missing_sidecar_is_not_an_error() {
        let p = scratch().join("nonexistent.fxedits");
        let _ = std::fs::remove_file(&p);
        assert!(load_edits(&p, fp()).unwrap().is_none());
    }

    #[test]
    fn roundtrip_every_value_kind() {
        let mut ov = EditOverlay::new();
        let id = ov.intern("hello world");
        ov.set(CellRef::new(0, 0), CellInput::Literal(Value::Number(42.5)));
        ov.set(CellRef::new(1, 0), CellInput::Literal(Value::Bool(true)));
        ov.set(CellRef::new(2, 0), CellInput::Literal(Value::Text(id)));
        ov.set(
            CellRef::new(3, 0),
            CellInput::Literal(Value::Error(ErrorKind::DivZero)),
        );
        ov.set(CellRef::new(4, 0), CellInput::Literal(Value::Empty));

        let p = scratch().join("round.fxedits");
        save_edits(&p, &ov, fp()).unwrap();
        let back = load_edits(&p, fp()).unwrap().unwrap();

        assert_eq!(back.len(), 5);
        assert_eq!(back.value(CellRef::new(0, 0)), Some(Value::Number(42.5)));
        assert_eq!(back.value(CellRef::new(1, 0)), Some(Value::Bool(true)));
        assert_eq!(
            back.value(CellRef::new(3, 0)),
            Some(Value::Error(ErrorKind::DivZero))
        );
        assert_eq!(back.value(CellRef::new(4, 0)), Some(Value::Empty));
        // Text must resolve through the restored arena.
        match back.value(CellRef::new(2, 0)) {
            Some(Value::Text(i)) => assert_eq!(back.resolve(i), Some("hello world")),
            other => panic!("expected text, got {other:?}"),
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn formula_source_survives_roundtrip() {
        let mut ov = EditOverlay::new();
        ov.set(
            CellRef::new(7, 2),
            CellInput::Formula {
                src: "=SUM(A1:A1000)".into(),
                cached: Value::Number(500500.0),
            },
        );
        let p = scratch().join("formula.fxedits");
        save_edits(&p, &ov, fp()).unwrap();
        let back = load_edits(&p, fp()).unwrap().unwrap();

        let cell = back.get(CellRef::new(7, 2)).unwrap();
        assert_eq!(cell.formula_src(), Some("=SUM(A1:A1000)"));
        assert_eq!(cell.value(), Value::Number(500500.0));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn stale_base_is_rejected_not_silently_applied() {
        // The data-loss case: base changed under us. Applying edits by
        // position would write into unrelated cells.
        let mut ov = EditOverlay::new();
        ov.set(CellRef::new(0, 0), CellInput::Literal(Value::Number(1.0)));
        let p = scratch().join("stale.fxedits");
        save_edits(&p, &ov, fp()).unwrap();

        let changed = BaseFingerprint {
            len: 999_999,
            ..fp()
        };
        match load_edits(&p, changed) {
            Err(EditError::StaleBase { .. }) => {}
            other => panic!("stale base must be rejected, got {other:?}"),
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn garbage_is_rejected() {
        let p = scratch().join("garbage.fxedits");
        std::fs::write(&p, b"this is not an edits file at all").unwrap();
        assert!(matches!(load_edits(&p, fp()), Err(EditError::BadMagic)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn truncated_file_is_rejected() {
        let mut ov = EditOverlay::new();
        ov.set(CellRef::new(0, 0), CellInput::Literal(Value::Number(1.0)));
        let p = scratch().join("trunc.fxedits");
        save_edits(&p, &ov, fp()).unwrap();

        let mut bytes = std::fs::read(&p).unwrap();
        bytes.truncate(bytes.len() - 3);
        std::fs::write(&p, &bytes).unwrap();

        assert!(matches!(load_edits(&p, fp()), Err(EditError::Truncated)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn saving_is_proportional_to_edits_not_sheet_size() {
        // The scale claim: 100 edits scattered over a 200M-row sheet must
        // produce a small file quickly, never touching the base.
        let mut ov = EditOverlay::new();
        for i in 0..100u32 {
            ov.set(
                CellRef::new(i * 2_000_000, 3),
                CellInput::Literal(Value::Number(i as f64)),
            );
        }
        let p = scratch().join("sparse.fxedits");
        let big = BaseFingerprint {
            len: 12_000_000_000,
            mtime: 1,
            rows: 200_000_000,
            cols: 8,
        };
        let t = std::time::Instant::now();
        let size = save_edits(&p, &ov, big).unwrap();
        let ms = t.elapsed().as_millis();

        assert!(size < 8_000, "100 edits wrote {size} bytes");
        assert!(ms < 200, "saving 100 edits took {ms}ms");

        let back = load_edits(&p, big).unwrap().unwrap();
        assert_eq!(back.len(), 100);
        // Deep row indices must survive.
        assert_eq!(
            back.value(CellRef::new(99 * 2_000_000, 3)),
            Some(Value::Number(99.0))
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_is_byte_reproducible() {
        // HashMap iteration order varies per run; saves must not.
        let mut ov = EditOverlay::new();
        for i in 0..50u32 {
            ov.set(
                CellRef::new(i, i % 5),
                CellInput::Literal(Value::Number(i as f64)),
            );
        }
        let a = scratch().join("repro_a.fxedits");
        let b = scratch().join("repro_b.fxedits");
        save_edits(&a, &ov, fp()).unwrap();
        save_edits(&b, &ov, fp()).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }
}
