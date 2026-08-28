//! String interning arena.
//!
//! Spreadsheet text columns are overwhelmingly low-cardinality (statuses,
//! categories, country codes). Storing a `String` per cell at 10M rows means
//! 10M heap allocations and ~24 bytes of `String` header each before any
//! characters. Instead we intern into one contiguous byte buffer and hand out
//! a 4-byte `StrId`, so a text column costs 4 bytes per cell plus one copy of
//! each distinct string.

use std::collections::HashMap;

/// Handle to an interned string. `u32` caps us at 4B distinct strings, which
/// is far beyond any realistic sheet, and keeps `Value` at 16 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct StrId(pub u32);

/// Append-only interner. Strings are never removed during a session; a
/// compaction pass on save reclaims space from deleted cells.
#[derive(Debug, Default)]
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
    pub fn resolve(&self, id: StrId) -> Option<&str> {
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
}
