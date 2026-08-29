//! Bake the edit overlay back into the `.ferrix` cache — "Compact".
//!
//! ## Why this exists
//!
//! Edits live in a sparse `.fxedits` sidecar that is *reapplied on every
//! open*. That is exactly right for the first few hundred edits: saving stays
//! O(edits) rather than O(rows), so three edits over a 200M-row file write a
//! few kilobytes instead of rewriting 12 GB.
//!
//! It stops being right once the edits accumulate. Every open pays to parse
//! and replay them, and — worse — the sidecar becomes a single point of
//! failure: lose it and the user's work is gone, because the cache underneath
//! never changed. Compact is the escape hatch. It rewrites the cache with the
//! overlay's values already in it, and then retires the sidecar.
//!
//! ## The memory rule
//!
//! Compact rewrites what may be a 12 GB file, so it obeys the same discipline
//! as [`crate::convert`]: **peak memory is one column stripe**, never the
//! sheet. Concretely, the resident set is
//!
//! * three 1 MiB spill writers for the column currently being rewritten,
//! * the string arena (bounded by the *distinct* strings, not the rows),
//! * the overlay's edits grouped by column (bounded by the edit count).
//!
//! The source cache is read through the existing `mmap`, so its pages are
//! page-cache, not heap, and the kernel evicts them behind us. Nothing here
//! scales with row count. A cell is read, translated, and written; it is never
//! collected.
//!
//! ## The atomicity rule
//!
//! This function rewrites the user's data file. Every intermediate state must
//! be one a crash can survive:
//!
//! 1. Write the new cache to `<cache>.compacting`, a sibling so the later
//!    rename stays on one filesystem.
//! 2. `fsync` it (inside [`crate::convert::assemble`]).
//! 3. `rename` it over the cache. On both Unix and Windows this is atomic:
//!    a reader sees the old file or the new one, never a prefix.
//! 4. **Only then** delete the sidecar and its autosave.
//!
//! Crash before (3) and the original cache and sidecar are both untouched —
//! the leftover `.compacting` file is ignored by everything, since the loader
//! only ever opens `<name>.ferrix`. Crash between (3) and (4) and the cache is
//! the new one while the sidecar still exists; the sidecar's fingerprint no
//! longer matches the rewritten base, so it is *rejected* rather than applied
//! twice. Double-application is the failure this ordering is chosen to
//! prevent, and rejection is loud.
//!
//! ## Formulas
//!
//! A columnar cache stores values, not expressions. So a formula cell's
//! *cached result* is baked into the new cache, and the formula itself is
//! carried forward in a fresh, much smaller sidecar written against the new
//! base's fingerprint. That keeps `=SUM(A1:A10)` a formula after a compact
//! instead of silently freezing it into a number. A workbook whose overlay is
//! all literals — the ordinary case — ends with no sidecar at all.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ferrix_core::{CellInput, CellRef, EditOverlay, StringArena, Value};

use crate::convert::{assemble, ConvertError, Spill};
use crate::edits::{self, BaseFingerprint};
use crate::format::FormatError;
use crate::mapped::MappedSheet;

/// Rows between cancellation polls.
///
/// At the ~100M cell/s this loop runs at, 64K rows is well under a
/// millisecond of work — a Cancel button that responds within a frame. A
/// token polled once per column would be decorative on a 200M-row file.
const CANCEL_POLL_ROWS: u64 = 1 << 16;

/// Bytes of buffered writer per column stripe: tags + nums + strs, 1 MiB each.
/// This is the *whole* row-dependent allocation, and it does not depend on the
/// row count.
pub const STRIPE_BUF_BYTES: usize = 3 << 20;

#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("format error: {0}")]
    Format(#[from] FormatError),
    #[error("compact cancelled")]
    Cancelled,
    #[error("could not write the compacted cache: {0}")]
    Write(String),
    #[error("could not carry formulas forward: {0}")]
    Sidecar(String),
}

impl From<ConvertError> for CompactError {
    fn from(e: ConvertError) -> Self {
        match e {
            ConvertError::Io(e) => CompactError::Io(e),
            ConvertError::Format(e) => CompactError::Format(e),
            ConvertError::Cancelled => CompactError::Cancelled,
            other => CompactError::Write(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CompactStats {
    pub rows: u64,
    pub cols: usize,
    /// Literal edits baked into the cache.
    pub edits_baked: usize,
    /// Formula cells carried forward into the new sidecar.
    pub formulas_kept: usize,
    pub output_bytes: u64,
    pub distinct_strings: usize,
    pub millis: u128,
    /// Peak *row-independent* buffer: one column stripe's writers.
    pub peak_stripe_bytes: usize,
    /// Heap held by the new string arena at the end. Bounded by distinct
    /// strings, not rows — reported so the claim can be checked, not assumed.
    pub arena_bytes: usize,
    /// Heap held by the grouped edit map. Bounded by the edit count.
    pub edits_bytes: usize,
}

impl CompactStats {
    /// Everything compact held resident that was not page cache.
    pub fn peak_heap_bytes(&self) -> usize {
        self.peak_stripe_bytes + self.arena_bytes + self.edits_bytes
    }
}

/// The scratch cache a compact builds before committing it.
///
/// A sibling of the destination, so the final `rename` stays on one
/// filesystem — a cross-device rename degrades to copy+delete and stops being
/// atomic, which would defeat the entire point.
pub fn temp_path_for(cache: &Path) -> PathBuf {
    let mut s = cache.as_os_str().to_os_string();
    s.push(".compacting");
    PathBuf::from(s)
}

/// A resolved overlay value, detached from the overlay's arena.
///
/// Text is owned here rather than kept as a `StrId` because the new cache
/// gets its own arena and the ids will not match. Cost is O(edited text),
/// not O(rows).
#[derive(Debug, Clone, PartialEq)]
enum Baked {
    Empty,
    Number(f64),
    Bool(bool),
    Text(String),
    Error(u8),
}

fn bake(v: Value, overlay: &EditOverlay) -> Baked {
    match v {
        Value::Empty => Baked::Empty,
        Value::Number(n) => Baked::Number(n),
        Value::Bool(b) => Baked::Bool(b),
        Value::Text(id) => Baked::Text(overlay.resolve(id).unwrap_or_default().to_string()),
        Value::Error(e) => Baked::Error(e.to_code()),
    }
}

/// What a completed compact leaves the caller holding.
#[derive(Debug)]
pub struct CompactOutcome {
    pub stats: CompactStats,
    /// The overlay the workbook should keep: empty when everything was baked,
    /// or the formula cells alone when there were formulas. Either way it is
    /// exactly what the (possibly absent) new sidecar contains.
    pub residual: EditOverlay,
    /// The sidecar path that now exists, or `None` when the sidecar was
    /// retired entirely — the ordinary outcome.
    pub sidecar: Option<PathBuf>,
}

/// Compact `cache` in place, applying `overlay`, then retire the sidecar.
///
/// `progress` is called with `(columns_done, columns_total)`.
///
/// On success the cache has been replaced atomically and the sidecar (and its
/// autosave) deleted. On *any* error — including cancellation — the original
/// cache and sidecar are untouched and the scratch file is removed.
pub fn compact_cache<F, C>(
    cache: &Path,
    overlay: &EditOverlay,
    mut progress: F,
    mut should_cancel: C,
) -> Result<CompactOutcome, CompactError>
where
    F: FnMut(u64, u64),
    C: FnMut() -> bool,
{
    let start = std::time::Instant::now();
    let tmp = temp_path_for(cache);
    // A leftover scratch file from a previous crashed attempt is meaningless;
    // it is not the cache and nothing reads it.
    let _ = std::fs::remove_file(&tmp);

    let result = write_compacted(cache, &tmp, overlay, &mut progress, &mut should_cancel);
    let mut stats = match result {
        Ok(s) => s,
        Err(e) => {
            // Nothing has been renamed or deleted yet, so removing the
            // scratch file restores the world exactly as it was.
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };

    // --- commit ---
    //
    // The new cache is complete and fsync'd. This rename is the instant the
    // change becomes real; before it the original is authoritative, after it
    // the new one is. There is no in-between state on disk.
    if let Err(e) = std::fs::rename(&tmp, cache) {
        let _ = std::fs::remove_file(&tmp);
        return Err(CompactError::Io(e));
    }

    // Only now is it safe to drop the sidecar: its contents are in the file
    // that is on disk under the real name.
    let sidecar_path = edits::edits_path_for(cache);
    let _ = edits::discard_autosave(&sidecar_path);
    match std::fs::remove_file(&sidecar_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CompactError::Io(e)),
    }

    // Carry formulas forward, fingerprinted against the file that now exists.
    let residual = formula_only(overlay);
    stats.formulas_kept = residual.len();
    let sidecar = if residual.is_empty() {
        None
    } else {
        let fp = BaseFingerprint::of(cache, stats.rows, stats.cols as u32)
            .map_err(|e| CompactError::Sidecar(e.to_string()))?;
        edits::save_edits(&sidecar_path, &residual, fp)
            .map_err(|e| CompactError::Sidecar(e.to_string()))?;
        Some(sidecar_path)
    };

    stats.millis = start.elapsed().as_millis();
    Ok(CompactOutcome {
        stats,
        residual,
        sidecar,
    })
}

/// The fingerprint a caller should hold after compacting.
///
/// Compact rewrites the base, so every fingerprint taken against the old one
/// is now stale by construction. A UI that kept the old fingerprint would
/// write a sidecar the next open refuses — the edits would look lost. This is
/// how the caller re-anchors.
pub fn fingerprint_after(cache: &Path, rows: u64, cols: u32) -> std::io::Result<BaseFingerprint> {
    BaseFingerprint::of(cache, rows, cols)
}

/// Every formula cell, lifted into a standalone overlay with its own arena.
fn formula_only(overlay: &EditOverlay) -> EditOverlay {
    let mut out = EditOverlay::new();
    let mut cells: Vec<(&CellRef, &CellInput)> = overlay
        .edited_cells()
        .filter(|(_, i)| i.is_formula())
        .collect();
    // Deterministic order so the arena ids — and therefore the bytes of the
    // sidecar — are reproducible.
    cells.sort_by_key(|(c, _)| (c.row, c.col));
    for (cell, input) in cells {
        let CellInput::Formula { src, cached } = input else {
            continue;
        };
        // Re-intern any text result into the new overlay's arena.
        let cached = match cached {
            Value::Text(id) => {
                let s = overlay.resolve(*id).unwrap_or_default().to_string();
                Value::Text(out.intern(&s))
            }
            other => *other,
        };
        out.set(
            *cell,
            CellInput::Formula {
                src: src.clone(),
                cached,
            },
        );
    }
    out
}

/// Stream the compacted cache into `dest`. Does not touch `cache` or the
/// sidecar; the caller commits.
fn write_compacted<F, C>(
    cache: &Path,
    dest: &Path,
    overlay: &EditOverlay,
    progress: &mut F,
    should_cancel: &mut C,
) -> Result<CompactStats, CompactError>
where
    F: FnMut(u64, u64),
    C: FnMut() -> bool,
{
    let src = MappedSheet::open(cache)?;
    let base_rows = src.row_count() as u64;
    let base_cols = src.col_count();

    // Editing past the end of the sheet extends it, so the compacted file must
    // be at least as large as the extent the user actually sees.
    let (ov_rows, ov_cols) = overlay.extent();
    let rows = base_rows.max(ov_rows as u64);
    let cols = base_cols.max(ov_cols);

    // Group the edits by column, resolving text out of the overlay's arena.
    // O(edits) in both time and space — this is the only structure that grows
    // with user input, and 100 edits over 200M rows is a few kilobytes.
    let mut by_col: Vec<HashMap<u32, Baked>> = vec![HashMap::new(); cols.max(1)];
    let mut edits_baked = 0usize;
    for (cell, input) in overlay.edited_cells() {
        let ci = cell.col as usize;
        if ci >= cols || cell.row as u64 >= rows {
            continue;
        }
        by_col[ci].insert(cell.row, bake(input.value(), overlay));
        edits_baked += 1;
    }
    let edits_bytes: usize = by_col
        .iter()
        .map(|m| {
            m.capacity() * (4 + std::mem::size_of::<Baked>() + 16)
                + m.values()
                    .map(|v| match v {
                        Baked::Text(s) => s.len(),
                        _ => 0,
                    })
                    .sum::<usize>()
        })
        .sum();

    // Spill files live in a scratch directory beside the destination, exactly
    // as the converter does, and are removed either way.
    let scratch = temp_scratch(dest);
    std::fs::create_dir_all(&scratch)?;

    let mut arena = StringArena::new();
    // Base string ids are remapped lazily into the new arena. `u32::MAX` is
    // "not yet seen"; a column of one repeated string interns once.
    let mut remap: Vec<u32> = Vec::new();

    let run = (|| -> Result<(u64, usize), CompactError> {
        let mut finished = Vec::with_capacity(cols);
        for ci in 0..cols {
            if should_cancel() {
                return Err(CompactError::Cancelled);
            }
            let mut spill = Spill::new(&scratch, ci)?;
            let edits = &by_col[ci];
            let col_is_base = ci < base_cols;

            for row in 0..rows {
                if row % CANCEL_POLL_ROWS == 0 && should_cancel() {
                    return Err(CompactError::Cancelled);
                }
                // The overlay wins, exactly as it does when reading the live
                // sheet — that equivalence is what makes the compacted file
                // show what the user was looking at.
                if let Some(v) = edits.get(&(row as u32)) {
                    match v {
                        Baked::Empty => spill.push_empty()?,
                        Baked::Number(n) => spill.push_number(*n)?,
                        Baked::Bool(b) => spill.push_bool(*b)?,
                        Baked::Text(s) => {
                            let id = arena.intern(s).0;
                            spill.push_text(id)?
                        }
                        Baked::Error(c) => spill.push_error(*c)?,
                    }
                    continue;
                }
                if !col_is_base || row >= base_rows {
                    spill.push_empty()?;
                    continue;
                }
                match src.get(CellRef::new(row as u32, ci as u32)) {
                    Value::Empty => spill.push_empty()?,
                    Value::Number(n) => spill.push_number(n)?,
                    Value::Bool(b) => spill.push_bool(b)?,
                    Value::Error(e) => spill.push_error(e.to_code())?,
                    Value::Text(id) => {
                        let i = id.0 as usize;
                        if i >= remap.len() {
                            remap.resize(i + 1, u32::MAX);
                        }
                        if remap[i] == u32::MAX {
                            remap[i] = arena.intern(src.resolve(id)).0;
                        }
                        spill.push_text(remap[i])?;
                    }
                }
            }
            finished.push(spill.finish()?);
            progress(ci as u64 + 1, cols as u64);
        }
        let bytes = assemble(dest, &finished, &arena, rows, &[])?;
        Ok((bytes, arena.len()))
    })();

    // Spills are gigabytes on a large file; remove them whatever happened.
    let _ = std::fs::remove_dir_all(&scratch);
    let (output_bytes, distinct_strings) = run?;

    Ok(CompactStats {
        rows,
        cols,
        edits_baked,
        formulas_kept: 0,
        output_bytes,
        distinct_strings,
        millis: 0,
        peak_stripe_bytes: STRIPE_BUF_BYTES,
        arena_bytes: arena.heap_bytes(),
        edits_bytes,
    })
}

fn temp_scratch(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push("-spill");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_core::{ErrorKind, Value};
    use std::io::Write;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ferrix_compact_{}_{}_{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Build a `.ferrix` cache from CSV text, returning its path.
    fn make_cache(d: &Path, csv: &str) -> PathBuf {
        let src = d.join("data.csv");
        std::fs::File::create(&src)
            .unwrap()
            .write_all(csv.as_bytes())
            .unwrap();
        let cache = crate::cache_path_for(&src);
        crate::convert_csv(&src, &cache, b',', true, |_, _| {}).unwrap();
        cache
    }

    fn grid(cache: &Path) -> Vec<Vec<String>> {
        let m = MappedSheet::open(cache).unwrap();
        (0..m.row_count())
            .map(|r| {
                (0..m.col_count())
                    .map(|c| m.display(CellRef::new(r as u32, c as u32)))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn bakes_edits_and_retires_the_sidecar() {
        let d = dir("basic");
        let cache = make_cache(&d, "a,b,c\n1,alpha,10\n2,beta,20\n3,gamma,30\n4,delta,40\n");
        let before = grid(&cache);

        let mut ov = EditOverlay::new();
        let s = ov.intern("EDITED");
        ov.set(CellRef::new(1, 1), CellInput::Literal(Value::Text(s)));
        ov.set(CellRef::new(2, 2), CellInput::Literal(Value::Number(999.0)));

        // A sidecar exists before the compact and must be gone after.
        let sidecar = edits::edits_path_for(&cache);
        let fp = BaseFingerprint::of(&cache, 4, 3).unwrap();
        edits::save_edits(&sidecar, &ov, fp).unwrap();
        assert!(sidecar.exists());

        let out = compact_cache(&cache, &ov, |_, _| {}, || false).unwrap();
        assert_eq!(out.stats.edits_baked, 2);
        assert!(out.sidecar.is_none());
        assert!(!sidecar.exists(), "sidecar must be retired");

        let after = grid(&cache);
        assert_eq!(after.len(), before.len(), "row count preserved");
        for r in 0..before.len() {
            for c in 0..before[r].len() {
                let expect = match (r, c) {
                    (1, 1) => "EDITED".to_string(),
                    (2, 2) => "999".to_string(),
                    _ => before[r][c].clone(),
                };
                assert_eq!(after[r][c], expect, "cell ({r},{c})");
            }
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn every_unedited_cell_is_identical_and_rows_keep_their_order() {
        // The trap check_order.rs exists for: a total would pass even if rows
        // were reordered. This compares row by row, in place.
        let d = dir("order");
        let mut csv = String::from("id,name,val\n");
        for i in 0..500 {
            csv.push_str(&format!("{i},name{},{}\n", i % 7, i * 3));
        }
        let cache = make_cache(&d, &csv);
        let before = grid(&cache);

        let mut ov = EditOverlay::new();
        for i in (0..500).step_by(50) {
            ov.set(
                CellRef::new(i as u32, 2),
                CellInput::Literal(Value::Number(-(i as f64))),
            );
        }
        compact_cache(&cache, &ov, |_, _| {}, || false).unwrap();
        let after = grid(&cache);

        assert_eq!(after.len(), 500);
        for (r, row) in before.iter().enumerate() {
            // Column 0 is the row's identity: if a row moved, this catches it
            // at the exact index.
            assert_eq!(after[r][0], row[0], "row {r} identity moved");
            assert_eq!(after[r][1], row[1], "row {r} col 1 changed");
            let expect = if r % 50 == 0 {
                format!("-{r}")
            } else {
                row[2].clone()
            };
            let expect = if r == 0 { "0".to_string() } else { expect };
            assert_eq!(after[r][2], expect, "row {r} col 2");
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn cancellation_leaves_the_original_cache_and_sidecar_intact() {
        let d = dir("cancel");
        let mut csv = String::from("a,b\n");
        for i in 0..5000 {
            csv.push_str(&format!("{i},v{i}\n"));
        }
        let cache = make_cache(&d, &csv);
        let original = std::fs::read(&cache).unwrap();

        let mut ov = EditOverlay::new();
        ov.set(CellRef::new(0, 0), CellInput::Literal(Value::Number(42.0)));
        let sidecar = edits::edits_path_for(&cache);
        let fp = BaseFingerprint::of(&cache, 5000, 2).unwrap();
        edits::save_edits(&sidecar, &ov, fp).unwrap();
        let sidecar_bytes = std::fs::read(&sidecar).unwrap();

        // Cancel after the first column, i.e. mid-write.
        let seen = std::sync::atomic::AtomicU64::new(0);
        let err = compact_cache(
            &cache,
            &ov,
            |done, _| seen.store(done, std::sync::atomic::Ordering::Relaxed),
            || seen.load(std::sync::atomic::Ordering::Relaxed) >= 1,
        )
        .unwrap_err();
        assert!(matches!(err, CompactError::Cancelled), "got {err:?}");

        assert_eq!(
            std::fs::read(&cache).unwrap(),
            original,
            "cache must be byte-identical after a cancelled compact"
        );
        assert_eq!(std::fs::read(&sidecar).unwrap(), sidecar_bytes);
        assert!(
            !temp_path_for(&cache).exists(),
            "scratch file must not survive"
        );
        // And both are still openable.
        let m = MappedSheet::open(&cache).unwrap();
        assert_eq!(m.row_count(), 5000);
        assert_eq!(m.display(CellRef::new(4999, 1)), "v4999");
        assert!(edits::load_edits(&sidecar, fp).unwrap().is_some());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_crash_before_the_rename_changes_nothing() {
        // Simulates the crash window directly: build the scratch file, then
        // stop without committing. This is the state a power cut leaves.
        let d = dir("crash");
        let cache = make_cache(&d, "a,b\n1,x\n2,y\n");
        let original = std::fs::read(&cache).unwrap();
        let mut ov = EditOverlay::new();
        ov.set(CellRef::new(0, 0), CellInput::Literal(Value::Number(7.0)));

        let tmp = temp_path_for(&cache);
        write_compacted(&cache, &tmp, &ov, &mut |_, _| {}, &mut || false).unwrap();
        assert!(tmp.exists(), "scratch cache should have been written");

        // The crash: no rename, no deletion.
        assert_eq!(std::fs::read(&cache).unwrap(), original);
        let m = MappedSheet::open(&cache).unwrap();
        assert_eq!(m.display(CellRef::new(0, 0)), "1", "original untouched");
        // And the loader never looks at `.compacting`, so the stale scratch is
        // inert. Prove it cannot be mistaken for the cache.
        assert_ne!(tmp, cache);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_compacted_cache_is_not_stale_against_its_source() {
        let d = dir("fresh");
        let cache = make_cache(&d, "a,b\n1,x\n2,y\n");
        let src = d.join("data.csv");
        assert!(crate::cache_is_fresh(&src, &cache));

        let mut ov = EditOverlay::new();
        ov.set(CellRef::new(1, 0), CellInput::Literal(Value::Number(88.0)));
        compact_cache(&cache, &ov, |_, _| {}, || false).unwrap();

        assert!(
            crate::cache_is_fresh(&src, &cache),
            "a compacted cache must not look stale on the next open"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn formulas_are_carried_forward_against_the_new_fingerprint() {
        let d = dir("formula");
        let cache = make_cache(&d, "a,b\n1,10\n2,20\n");
        let mut ov = EditOverlay::new();
        ov.set(
            CellRef::new(0, 1),
            CellInput::Formula {
                src: "=A1+1".to_string(),
                cached: Value::Number(2.0),
            },
        );
        ov.set(CellRef::new(1, 0), CellInput::Literal(Value::Number(5.0)));

        let out = compact_cache(&cache, &ov, |_, _| {}, || false).unwrap();
        assert_eq!(out.stats.formulas_kept, 1);
        assert_eq!(out.stats.edits_baked, 2, "the cached result is baked too");
        let side = out.sidecar.expect("formulas need a sidecar");
        assert!(side.exists());

        // The value is in the cache...
        let m = MappedSheet::open(&cache).unwrap();
        assert_eq!(m.display(CellRef::new(0, 1)), "2");
        assert_eq!(m.display(CellRef::new(1, 0)), "5");
        // ...and the carried-forward sidecar loads against the NEW base.
        let fp = fingerprint_after(&cache, m.row_count() as u64, m.col_count() as u32).unwrap();
        let back = edits::load_edits(&side, fp).unwrap().expect("loads");
        assert_eq!(back.len(), 1);
        assert_eq!(
            back.get(CellRef::new(0, 1)).unwrap().formula_src(),
            Some("=A1+1")
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn edits_past_the_end_extend_the_sheet() {
        let d = dir("extend");
        let cache = make_cache(&d, "a,b\n1,x\n");
        let mut ov = EditOverlay::new();
        ov.set(CellRef::new(4, 3), CellInput::Literal(Value::Number(9.0)));
        compact_cache(&cache, &ov, |_, _| {}, || false).unwrap();

        let m = MappedSheet::open(&cache).unwrap();
        assert_eq!(m.row_count(), 5);
        assert_eq!(m.col_count(), 4);
        assert_eq!(m.display(CellRef::new(4, 3)), "9");
        assert_eq!(m.display(CellRef::new(0, 0)), "1", "old data still there");
        assert_eq!(m.display(CellRef::new(3, 2)), "", "new space is empty");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn error_values_survive_the_round_trip() {
        let d = dir("errors");
        let cache = make_cache(&d, "a,b\n1,2\n");
        let mut ov = EditOverlay::new();
        ov.set(
            CellRef::new(0, 0),
            CellInput::Literal(Value::Error(ErrorKind::DivZero)),
        );
        compact_cache(&cache, &ov, |_, _| {}, || false).unwrap();
        let m = MappedSheet::open(&cache).unwrap();
        assert_eq!(m.get(CellRef::new(0, 0)), Value::Error(ErrorKind::DivZero));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn compacting_twice_is_a_no_op_the_second_time() {
        let d = dir("idempotent");
        let cache = make_cache(&d, "a,b\n1,x\n2,y\n3,z\n");
        let mut ov = EditOverlay::new();
        ov.set(CellRef::new(1, 0), CellInput::Literal(Value::Number(77.0)));
        compact_cache(&cache, &ov, |_, _| {}, || false).unwrap();
        let once = std::fs::read(&cache).unwrap();

        let empty = EditOverlay::new();
        let out = compact_cache(&cache, &empty, |_, _| {}, || false).unwrap();
        assert_eq!(out.stats.edits_baked, 0);
        assert_eq!(
            std::fs::read(&cache).unwrap(),
            once,
            "compacting an unedited cache must reproduce it byte for byte"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn peak_buffers_do_not_scale_with_rows() {
        // The memory claim, asserted rather than described: ten times the rows
        // must not move the row-independent buffers at all.
        let d = dir("mem");
        let small = {
            let mut csv = String::from("a,b\n");
            for i in 0..200 {
                csv.push_str(&format!("{i},t{}\n", i % 5));
            }
            make_cache(&d, &csv)
        };
        let s1 = compact_cache(&small, &EditOverlay::new(), |_, _| {}, || false)
            .unwrap()
            .stats;

        let d2 = dir("mem2");
        let big = {
            let mut csv = String::from("a,b\n");
            for i in 0..20_000 {
                csv.push_str(&format!("{i},t{}\n", i % 5));
            }
            make_cache(&d2, &csv)
        };
        let s2 = compact_cache(&big, &EditOverlay::new(), |_, _| {}, || false)
            .unwrap()
            .stats;

        assert_eq!(s2.rows, 20_000);
        assert_eq!(s1.peak_stripe_bytes, s2.peak_stripe_bytes);
        assert_eq!(
            s1.arena_bytes, s2.arena_bytes,
            "arena tracks distinct strings, not rows"
        );
        assert_eq!(s1.edits_bytes, 0);
        std::fs::remove_dir_all(&d).ok();
        std::fs::remove_dir_all(&d2).ok();
    }

    #[test]
    fn temp_paths_are_siblings_and_never_the_cache() {
        let p = Path::new("/data/sales.ferrix");
        let t = temp_path_for(p);
        assert_eq!(t.parent(), p.parent(), "rename must stay on one filesystem");
        assert_ne!(t, p);
        assert!(t.to_string_lossy().ends_with(".compacting"));
    }
}
