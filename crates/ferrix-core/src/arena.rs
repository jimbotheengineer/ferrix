//! String interning arena.
//!
//! Spreadsheet text columns are overwhelmingly low-cardinality (statuses,
//! categories, country codes). Storing a `String` per cell at 10M rows means
//! 10M heap allocations and ~24 bytes of `String` header each before any
//! characters. Instead we intern into one contiguous byte buffer and hand out
//! a 4-byte `StrId`, so a text column costs 4 bytes per cell plus one copy of
//! each distinct string.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Handle to an interned string. `u32` caps us at 4B distinct strings, which
/// is far beyond any realistic sheet, and keeps `Value` at 16 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct StrId(pub u32);

// --- formula-produced text -------------------------------------------------
//
// ## Why a second interner exists
//
// `Value` is 16 bytes and `Copy`, and `Value::Text` holds a `StrId` — an index
// into an arena, never the bytes. That is what keeps a 10M-row text column at
// 4 bytes per cell.
//
// The formula evaluator, however, only ever holds `&self` on its `CellSource`
// (see `ferrix_formula::eval::CellSource`): there is no `&mut StringArena`
// anywhere on the evaluation path, and there cannot be one without threading a
// sink through every `eval_view` call site — a refactor that would touch every
// consumer of the engine. So a text function such as `UPPER(A1)` has nowhere to
// put its result.
//
// This is that place: one process-wide, append-only, **deduplicating** interner
// for strings produced by formulas. Ids from it carry [`FORMULA_TEXT_TAG`] in
// their high bit, so they can never be confused with a per-sheet arena id
// (which would need 2^31 distinct strings — hundreds of gigabytes — to reach
// that bit), and [`StringArena::resolve`] routes them here automatically. That
// means every existing resolver — `Sheet`, `EditOverlay`, `SheetView`,
// `WorkbookSource` — resolves formula text with no change at all.
//
// ## Scale
//
// Dedup is the point. A million-row column of `=UPPER(region)` over three
// distinct regions stores THREE strings, not a million: retained memory is
// O(distinct results), not O(rows). Storage is also hard-capped
// ([`FORMULA_TEXT_BUDGET`]); past the cap interning fails and the caller
// reports `#VALUE!` rather than growing without bound.
//
// ## The honest cost
//
// Interned strings are leaked (`Box::leak`), so they live for the process, not
// for the workbook: closing a file does not reclaim them. That is the price of
// handing out a `&'static str` from a `&self` method without `unsafe`. It is
// bounded by the budget below and by dedup, but it is not free, and a
// per-source sink would be strictly better if the evaluator ever gains one.

/// High bit of a [`StrId`] marking it as formula-produced rather than a
/// per-sheet arena id.
pub const FORMULA_TEXT_TAG: u32 = 0x8000_0000;

/// Hard ceiling on retained formula-result text. Past this, interning fails
/// and text functions report `#VALUE!` — bounded memory beats a silent leak.
pub const FORMULA_TEXT_BUDGET: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct FormulaText {
    /// Leaked string bodies, indexed by the low bits of the id.
    ids: Vec<&'static str>,
    /// Dedup index. Keys borrow the same leaked bodies, so this costs no
    /// extra string data.
    lookup: HashMap<&'static str, u32>,
    bytes: usize,
}

static FORMULA_TEXT: LazyLock<Mutex<FormulaText>> =
    LazyLock::new(|| Mutex::new(FormulaText::default()));

/// Intern a formula result, returning a tagged [`StrId`].
///
/// Returns `None` only when the process-wide budget is exhausted (or the lock
/// is poisoned), which callers surface as `#VALUE!`.
pub fn intern_formula_text(s: &str) -> Option<StrId> {
    let mut g = FORMULA_TEXT.lock().ok()?;
    if let Some(&idx) = g.lookup.get(s) {
        return Some(StrId(FORMULA_TEXT_TAG | idx));
    }
    if g.bytes + s.len() > FORMULA_TEXT_BUDGET || g.ids.len() as u32 >= FORMULA_TEXT_TAG {
        return None;
    }
    // Leaked so `resolve_formula_text` can hand back a `&'static str` from a
    // shared reference without `unsafe`. Only DISTINCT strings reach here.
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    let idx = g.ids.len() as u32;
    g.ids.push(leaked);
    g.lookup.insert(leaked, idx);
    g.bytes += leaked.len();
    Some(StrId(FORMULA_TEXT_TAG | idx))
}

/// Resolve a tagged id. `None` for untagged ids, so callers can chain.
#[inline]
pub fn resolve_formula_text(id: StrId) -> Option<&'static str> {
    if id.0 & FORMULA_TEXT_TAG == 0 {
        return None;
    }
    let g = FORMULA_TEXT.lock().ok()?;
    g.ids.get((id.0 & !FORMULA_TEXT_TAG) as usize).copied()
}

/// `(distinct strings, bytes retained)` — the handle tests use to prove that
/// interning is bounded by distinct results rather than by cell count.
pub fn formula_text_stats() -> (usize, usize) {
    match FORMULA_TEXT.lock() {
        Ok(g) => (g.ids.len(), g.bytes),
        Err(_) => (0, 0),
    }
}

/// Append-only interner. Strings are never removed during a session; a
/// compaction pass on save reclaims space from deleted cells.
///
/// `Clone` exists so an edit overlay can be snapshotted for a background
/// export without the exporter borrowing live workbook state across a thread
/// boundary. Cloning copies the byte buffer, so callers check the cost
/// against the memory budget first — see `StringArena::heap_bytes`.
#[derive(Clone, Debug, Default)]
pub struct StringArena {
    /// All string bytes back to back.
    buf: Vec<u8>,
    /// (start, len) into `buf`, indexed by `StrId`.
    spans: Vec<(u32, u32)>,
    /// Dedup index: string -> existing id.
    lookup: HashMap<Box<str>, StrId>,
}

impl StringArena {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(bytes: usize, strings: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes),
            spans: Vec::with_capacity(strings),
            lookup: HashMap::with_capacity(strings),
        }
    }

    /// Intern `s`, returning an existing id when the string is already present.
    pub fn intern(&mut self, s: &str) -> StrId {
        if let Some(&id) = self.lookup.get(s) {
            return id;
        }
        let start = self.buf.len() as u32;
        let len = s.len() as u32;
        self.buf.extend_from_slice(s.as_bytes());
        let id = StrId(self.spans.len() as u32);
        self.spans.push((start, len));
        self.lookup.insert(s.into(), id);
        id
    }

    /// Resolve an id back to its string.
    ///
    /// A tagged id (see [`FORMULA_TEXT_TAG`]) belongs to the formula-result
    /// interner, not to this arena. Routing it here — one predictable branch —
    /// is what lets every existing resolver (`Sheet`, `EditOverlay`,
    /// `SheetView`, `WorkbookSource`) display `=UPPER(A1)` without knowing the
    /// second interner exists.
    pub fn resolve(&self, id: StrId) -> Option<&str> {
        if id.0 & FORMULA_TEXT_TAG != 0 {
            return resolve_formula_text(id);
        }
        let &(start, len) = self.spans.get(id.0 as usize)?;
        let bytes = &self.buf[start as usize..(start + len) as usize];
        // SAFETY-free: we only ever push valid UTF-8 from &str in `intern`.
        std::str::from_utf8(bytes).ok()
    }

    /// Resolve, falling back to an empty string for unknown ids.
    #[inline]
    pub fn resolve_or_empty(&self, id: StrId) -> &str {
        self.resolve(id).unwrap_or("")
    }

    /// Number of distinct interned strings.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Total bytes of string data (excluding index overhead).
    pub fn data_bytes(&self) -> usize {
        self.buf.len()
    }

    /// Approximate total heap footprint.
    pub fn heap_bytes(&self) -> usize {
        self.buf.capacity()
            + self.spans.capacity() * std::mem::size_of::<(u32, u32)>()
            // Rough estimate for the HashMap: key box + entry overhead.
            + self.lookup.capacity() * (std::mem::size_of::<(Box<str>, StrId)>() + 16)
            + self.buf.len()
    }

    /// Drop the dedup index, freeing memory once ingest is complete.
    /// Interning after this still works but stops deduplicating.
    pub fn shrink_for_readonly(&mut self) {
        self.lookup.clear();
        self.lookup.shrink_to_fit();
        self.buf.shrink_to_fit();
        self.spans.shrink_to_fit();
    }

    /// Raw contents for serialization: the byte buffer and the span table.
    pub fn raw_parts(&self) -> (&[u8], &[(u32, u32)]) {
        (&self.buf, &self.spans)
    }

    /// Rebuild an arena from previously serialized parts.
    ///
    /// The dedup index is deliberately NOT rebuilt — a 10GB file's arena can
    /// hold millions of strings, and reconstructing a HashMap over them would
    /// cost more memory than the strings themselves. Loaded arenas are for
    /// reading; edits intern into the overlay's own arena instead.
    pub fn from_raw_parts(buf: Vec<u8>, spans: Vec<(u32, u32)>) -> Self {
        Self {
            buf,
            spans,
            lookup: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_dedups() {
        let mut a = StringArena::new();
        let x = a.intern("hello");
        let y = a.intern("world");
        let z = a.intern("hello");
        assert_eq!(x, z);
        assert_ne!(x, y);
        assert_eq!(a.len(), 2);
        assert_eq!(a.resolve(x), Some("hello"));
        assert_eq!(a.resolve(y), Some("world"));
    }

    #[test]
    fn handles_empty_and_unicode() {
        let mut a = StringArena::new();
        let e = a.intern("");
        let u = a.intern("héllo → 世界");
        assert_eq!(a.resolve(e), Some(""));
        assert_eq!(a.resolve(u), Some("héllo → 世界"));
    }

    #[test]
    fn unknown_id_resolves_to_none() {
        let a = StringArena::new();
        assert_eq!(a.resolve(StrId(42)), None);
        assert_eq!(a.resolve_or_empty(StrId(42)), "");
    }

    #[test]
    fn low_cardinality_is_cheap() {
        let mut a = StringArena::new();
        for i in 0..10_000 {
            a.intern(["alpha", "beta", "gamma"][i % 3]);
        }
        assert_eq!(a.len(), 3);
        assert_eq!(a.data_bytes(), "alpha".len() + "beta".len() + "gamma".len());
    }

    #[test]
    fn formula_text_ids_are_tagged_deduped_and_resolve_through_any_arena() {
        // The two properties the text functions depend on:
        //   1. a formula-result id never collides with a sheet-arena id, and
        //   2. any arena resolves one, so every existing display path works.
        let mut a = StringArena::new();
        let sheet_id = a.intern("hello");
        let f1 = intern_formula_text("HELLO").expect("budget");
        let f2 = intern_formula_text("HELLO").expect("budget");

        assert_eq!(f1, f2, "formula text must dedup, not allocate per call");
        assert_ne!(f1, sheet_id);
        assert_eq!(f1.0 & FORMULA_TEXT_TAG, FORMULA_TEXT_TAG);
        assert_eq!(sheet_id.0 & FORMULA_TEXT_TAG, 0);

        // An arena that has never seen "HELLO" still resolves it, which is
        // what lets Sheet/EditOverlay/SheetView display formula results.
        assert_eq!(a.resolve(f1), Some("HELLO"));
        assert_eq!(StringArena::new().resolve(f1), Some("HELLO"));
        // ...and the sheet's own strings are unaffected.
        assert_eq!(a.resolve(sheet_id), Some("hello"));
        assert_eq!(resolve_formula_text(sheet_id), None);
    }
}
