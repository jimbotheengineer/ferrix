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
    write_atomic(path, ".tmp", overlay, fingerprint)
}

/// The shared atomic writer behind both the official sidecar and the autosave.
///
/// Follows the pattern established in `export.rs`: serialize into a temp file
/// that is a *sibling* of the destination (so the rename stays on one
/// filesystem and is therefore atomic), flush and fsync it so the bytes are
/// durable before the rename, then rename into place.
///
/// The consequence that matters: a crash at any instant leaves either the
/// complete previous file or the complete new one on disk. Never a prefix of
/// the new one. A truncated recovery file is worse than none at all, because
/// the user believes they are protected right up until they try to use it.
///
/// The temp suffix is a parameter rather than derived, because
/// `Path::with_extension` would mangle `sales.ferrix.fxedits.autosave` into
/// `sales.ferrix.fxedits.tmp` — colliding with the real sidecar's temp file.
/// Suffixes are appended here, never substituted.
fn write_atomic(
    path: &Path,
    tmp_suffix: &str,
    overlay: &EditOverlay,
    fingerprint: BaseFingerprint,
) -> Result<u64, EditError> {
    let tmp = sibling_with_suffix(path, tmp_suffix);
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
        // fsync before the rename. Without this the rename can land in the
        // directory while the file's contents are still only in the page
        // cache, and a power loss leaves a correctly-named empty file — the
        // truncation this whole dance exists to prevent.
        w.get_ref().sync_all()?;
    }
    let size = std::fs::metadata(&tmp)?.len();
    // Windows will not rename onto an existing file.
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    Ok(size)
}

/// Append a suffix to a path, never substituting its extension.
fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

// --- autosave ---

/// Suffix appended to the sidecar path to get its autosave companion.
pub const AUTOSAVE_SUFFIX: &str = ".autosave";

/// Default autosave cadence in seconds.
pub const DEFAULT_AUTOSAVE_SECS: u64 = 30;

/// Where the autosave for a given sidecar lives: `<base>.fxedits.autosave`.
///
/// Deliberately a separate file from the sidecar. Autosave is speculative —
/// the user has not asked for it and may not want it — so it must never be
/// able to overwrite the thing they *did* ask for. Recovery promotes the
/// autosave into the overlay only after the user says so.
pub fn autosave_path_for_sidecar(sidecar: &Path) -> PathBuf {
    sibling_with_suffix(sidecar, AUTOSAVE_SUFFIX)
}

/// Write the overlay to the autosave file, atomically.
///
/// Cost is O(edits), identical to `save_edits`: 100 edits over a 200M-row
/// sheet write a few kilobytes regardless of row count.
pub fn write_autosave(
    sidecar: &Path,
    overlay: &EditOverlay,
    fingerprint: BaseFingerprint,
) -> Result<u64, EditError> {
    let path = autosave_path_for_sidecar(sidecar);
    write_atomic(&path, ".tmp", overlay, fingerprint)
}

/// Delete the autosave, if present. Missing is success: the caller's intent is
/// "there must be no autosave after this", and there isn't.
pub fn discard_autosave(sidecar: &Path) -> io::Result<()> {
    let path = autosave_path_for_sidecar(sidecar);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// An autosave that is newer than the sidecar it sits beside — i.e. work the
/// user would otherwise have lost.
#[derive(Debug, Clone)]
pub struct RecoveryCandidate {
    pub autosave: PathBuf,
    /// How long ago the autosave was written, for the prompt's "HH:MM ago".
    pub age: std::time::Duration,
}

impl RecoveryCandidate {
    /// The age rendered as `HH:MM`, which is what the prompt shows.
    pub fn age_hhmm(&self) -> String {
        let total = self.age.as_secs();
        format!("{:02}:{:02}", total / 3600, (total % 3600) / 60)
    }
}

fn mtime_of(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Decide whether there is recoverable work next to `sidecar`.
///
/// Returns `Some` only when an autosave exists AND is strictly newer than the
/// official sidecar. If the sidecar is newer, the user has saved since the
/// autosave was written and the autosave holds nothing they do not already
/// have — prompting there would be noise, and worse, would invite them to
/// overwrite a good save with stale edits.
///
/// A missing sidecar with a present autosave *is* a candidate: that is exactly
/// the crash-before-first-save case, the one where everything typed is only in
/// the autosave.
///
/// This deliberately does not parse the autosave. It is a `stat` of two files,
/// so it stays instant on a huge overlay; the contents are read only if the
/// user chooses to recover.
pub fn find_recovery(sidecar: &Path) -> Option<RecoveryCandidate> {
    let auto = autosave_path_for_sidecar(sidecar);
    let auto_mtime = mtime_of(&auto)?;
    if let Some(side_mtime) = mtime_of(sidecar) {
        if auto_mtime <= side_mtime {
            return None;
        }
    }
    let age = std::time::SystemTime::now()
        .duration_since(auto_mtime)
        // A clock skew that puts the file in the future is not worth failing
        // over; report it as brand new.
        .unwrap_or_default();
    Some(RecoveryCandidate {
        autosave: auto,
        age,
    })
}

/// Load the overlay out of an autosave, with the same staleness check the
/// sidecar gets. Recovering edits onto a base that has changed underneath
/// would misapply them by position, which is the data-loss mode the
/// fingerprint exists to stop — autosave gets no exemption from it.
pub fn load_autosave(
    sidecar: &Path,
    expected: BaseFingerprint,
) -> Result<Option<EditOverlay>, EditError> {
    load_edits(&autosave_path_for_sidecar(sidecar), expected)
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

    // --- autosave ---

    /// A scratch sidecar path unique to this test, so the autosave tests can
    /// run in parallel without fighting over the same files.
    fn side(name: &str) -> PathBuf {
        let p = scratch().join(format!("{name}.fxedits"));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(autosave_path_for_sidecar(&p));
        p
    }

    fn overlay_of(vals: &[(u32, f64)]) -> EditOverlay {
        let mut ov = EditOverlay::new();
        for (row, v) in vals {
            ov.set(CellRef::new(*row, 0), CellInput::Literal(Value::Number(*v)));
        }
        ov
    }

    #[test]
    fn autosave_path_sits_beside_the_sidecar_and_is_not_it() {
        let s = edits_path_for(Path::new("data/sales.ferrix"));
        let a = autosave_path_for_sidecar(&s);
        assert!(a
            .to_string_lossy()
            .ends_with("sales.ferrix.fxedits.autosave"));
        assert_ne!(a, s, "autosave must never be the official sidecar");
    }

    #[test]
    fn autosave_roundtrips_without_touching_the_sidecar() {
        let s = side("auto_roundtrip");
        let ov = overlay_of(&[(0, 1.0), (5, 2.0)]);
        write_autosave(&s, &ov, fp()).unwrap();

        // The official sidecar must not have been created as a side effect.
        assert!(
            !s.exists(),
            "autosave wrote the official sidecar; it must only ever write its own file"
        );
        let back = load_autosave(&s, fp()).unwrap().unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back.value(CellRef::new(5, 0)), Some(Value::Number(2.0)));
    }

    #[test]
    fn recovery_offered_when_autosave_is_newer_than_the_sidecar() {
        let s = side("recover_newer");
        // Sidecar first, then a later autosave: the crash-after-save case.
        save_edits(&s, &overlay_of(&[(0, 1.0)]), fp()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_autosave(&s, &overlay_of(&[(0, 1.0), (1, 2.0)]), fp()).unwrap();

        let c = find_recovery(&s).expect("newer autosave must be offered for recovery");
        assert!(c.autosave.exists());
    }

    #[test]
    fn recovery_not_offered_when_the_sidecar_is_newer() {
        // The user saved after the autosave. The autosave holds nothing they
        // do not already have, so prompting would invite them to overwrite a
        // good save with stale edits.
        let s = side("recover_older");
        write_autosave(&s, &overlay_of(&[(0, 1.0)]), fp()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        save_edits(&s, &overlay_of(&[(0, 1.0), (1, 2.0)]), fp()).unwrap();

        assert!(
            find_recovery(&s).is_none(),
            "a sidecar newer than the autosave must not prompt"
        );
    }

    #[test]
    fn recovery_offered_when_there_is_no_sidecar_at_all() {
        // Crash before the first ever save: everything typed lives only in
        // the autosave, so this is the case that matters most.
        let s = side("recover_no_sidecar");
        write_autosave(&s, &overlay_of(&[(3, 7.0)]), fp()).unwrap();
        assert!(!s.exists());
        assert!(
            find_recovery(&s).is_some(),
            "an autosave with no sidecar is pure unrecovered work"
        );
    }

    #[test]
    fn no_autosave_means_no_recovery() {
        let s = side("recover_none");
        save_edits(&s, &overlay_of(&[(0, 1.0)]), fp()).unwrap();
        assert!(find_recovery(&s).is_none());
    }

    #[test]
    fn discard_removes_the_autosave_and_leaves_the_sidecar_intact() {
        let s = side("discard");
        save_edits(&s, &overlay_of(&[(0, 42.0)]), fp()).unwrap();
        let sidecar_bytes = std::fs::read(&s).unwrap();
        write_autosave(&s, &overlay_of(&[(0, 42.0), (1, 99.0)]), fp()).unwrap();

        discard_autosave(&s).unwrap();

        assert!(
            !autosave_path_for_sidecar(&s).exists(),
            "discard must delete"
        );
        assert_eq!(
            std::fs::read(&s).unwrap(),
            sidecar_bytes,
            "discarding an autosave must not touch the official sidecar"
        );
        assert!(find_recovery(&s).is_none());
    }

    #[test]
    fn discarding_a_missing_autosave_is_not_an_error() {
        let s = side("discard_missing");
        discard_autosave(&s).expect("removing a nonexistent autosave must succeed");
    }

    #[test]
    fn autosave_stale_base_is_rejected_like_the_sidecar() {
        // Recovery must not misapply edits by position onto changed data.
        let s = side("auto_stale");
        write_autosave(&s, &overlay_of(&[(0, 1.0)]), fp()).unwrap();
        let changed = BaseFingerprint { rows: 7, ..fp() };
        assert!(matches!(
            load_autosave(&s, changed),
            Err(EditError::StaleBase { .. })
        ));
    }

    #[test]
    fn an_autosave_over_an_existing_one_is_never_observed_truncated() {
        // The core atomicity claim. A reader racing repeated autosaves must
        // only ever see a complete, loadable file — never a prefix of the one
        // being written. Without the temp-file + rename this fails, because a
        // reader catches File::create's zero-length truncation.
        let s = side("auto_atomic");
        // First autosave, so there is always an existing file to overwrite.
        write_autosave(&s, &overlay_of(&[(0, 0.0)]), fp()).unwrap();
        let auto = autosave_path_for_sidecar(&s);

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_stop = stop.clone();
        let reader_path = auto.clone();
        // The reader loads the file as fast as it can while writes churn.
        let reader = std::thread::spawn(move || {
            let mut reads = 0usize;
            let mut bad = Vec::new();
            while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
                match load_edits(&reader_path, fp()) {
                    // A complete file, or momentarily absent mid-rename.
                    Ok(_) => reads += 1,
                    // Windows can deny access during the rename; that is a
                    // sharing violation, not a truncated file.
                    Err(EditError::Io(_)) => {}
                    // Truncated / bad magic means a partial file was visible,
                    // which is precisely the failure this test exists to catch.
                    Err(e) => bad.push(format!("{e}")),
                }
            }
            (reads, bad)
        });

        // A sizeable overlay, so a non-atomic write would have a wide window
        // in which a reader could catch it half-written.
        let big: Vec<(u32, f64)> = (0..4000).map(|i| (i, i as f64)).collect();
        let ov = overlay_of(&big);
        for _ in 0..40 {
            write_autosave(&s, &ov, fp()).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let (reads, bad) = reader.join().unwrap();

        assert!(
            bad.is_empty(),
            "reader observed {} truncated/corrupt autosave(s): {:?}",
            bad.len(),
            &bad[..bad.len().min(5)]
        );
        assert!(
            reads > 0,
            "reader never managed a read; test proved nothing"
        );
    }

    #[test]
    fn autosave_cost_tracks_edits_not_rows() {
        // The scale invariant: 100 edits over 200M rows is kilobytes and
        // milliseconds. A row-proportional implementation would blow both.
        let s = side("auto_scale");
        let mut ov = EditOverlay::new();
        for i in 0..100u32 {
            ov.set(
                CellRef::new(i * 2_000_000, 3),
                CellInput::Literal(Value::Number(i as f64)),
            );
        }
        let big = BaseFingerprint {
            len: 12_000_000_000,
            mtime: 1,
            rows: 200_000_000,
            cols: 8,
        };
        let t = std::time::Instant::now();
        let size = write_autosave(&s, &ov, big).unwrap();
        let ms = t.elapsed().as_millis();
        assert!(size < 8_000, "100 autosaved edits wrote {size} bytes");
        assert!(ms < 500, "autosaving 100 edits took {ms}ms");
        assert_eq!(load_autosave(&s, big).unwrap().unwrap().len(), 100);
    }

    #[test]
    fn autosave_temp_file_does_not_collide_with_the_sidecar_temp() {
        // with_extension() would map both sales.fxedits and
        // sales.fxedits.autosave onto the same .tmp path, so two writers
        // would corrupt each other. Suffixes are appended, never replaced.
        let s = Path::new("data/sales.ferrix.fxedits");
        let a = autosave_path_for_sidecar(s);
        assert_ne!(
            sibling_with_suffix(s, ".tmp"),
            sibling_with_suffix(&a, ".tmp")
        );
    }

    #[test]
    fn age_renders_as_hours_and_minutes() {
        let c = |secs| RecoveryCandidate {
            autosave: PathBuf::from("x"),
            age: std::time::Duration::from_secs(secs),
        };
        assert_eq!(c(0).age_hhmm(), "00:00");
        assert_eq!(c(90).age_hhmm(), "00:01");
        assert_eq!(c(3600 * 2 + 60 * 5).age_hhmm(), "02:05");
    }
}
