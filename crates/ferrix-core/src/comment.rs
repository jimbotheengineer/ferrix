//! Cell comments (notes): short author-attributed text attached to a cell.
//!
//! A comment is a remark *about* a cell — "check this with finance", "revised
//! after the audit". It is not data: it never participates in a formula, never
//! changes a value, and never shows up in an export of the numbers. It is the
//! reason a number is what it is, parked next to the number.
//!
//! # Storage
//!
//! Sparse, never a per-cell field. A 200M-row sheet with three commented cells
//! costs three entries; a per-cell `Option<Comment>` would cost 200M slots to
//! describe the same three, and 200M x 24 bytes is 4.8 GB of nothing.
//!
//! The shape is deliberately the same as [`crate::merge::MergeMap`]: a
//! [`BTreeMap`] keyed by row, holding the (column, comment) pairs on that row.
//! A comment covers exactly one cell rather than a rectangle, so lookup needs
//! no range query at all — one `BTreeMap` probe lands on the row, and the row
//! carries a handful of columns at most.
//!
//! # The paint path
//!
//! The marker triangle is decided per visible cell, per frame: ~1,500 lookups
//! every frame at 60fps. Two properties keep that free:
//!
//! 1. [`CommentMap::is_empty`] is a field read. A sheet with no comments — the
//!    overwhelming majority — does **zero** map probes per frame, because the
//!    caller short-circuits on it before touching the map.
//! 2. [`CommentMap::row_comments`] answers for a whole row at once, so a
//!    caller hoists it out of its column loop and pays one probe per visible
//!    ROW rather than one per visible CELL.
//!
//! [`CommentMap::probes`] counts map probes so a test can assert both of those
//! rather than take them on faith.
//!
//! # Display coordinates, and why
//!
//! Comments are keyed by **display** position, exactly like
//! [`crate::EditOverlay`], and are relocated by the same code that relocates
//! the overlay when a column is reordered.
//!
//! The alternative — keying by the underlying data coordinate — was rejected
//! because the two stores would then disagree. The overlay holds "the user
//! typed 42 here" at a display coordinate; a comment saying "42 is provisional"
//! must sit on the same cell. If a reorder relocated one and not the other,
//! the comment would silently end up beside a different number, which is the
//! worst possible failure for an annotation: plausible, wrong, and invisible.
//! So both live in display space and both move together. `comment.rs` and the
//! overlay relocation in `workbook.rs` are two halves of one invariant; a test
//! (`comment_follows_its_cell_through_a_column_reorder`) pins it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::CellRef;

/// Longest comment text Ferrix will store on one cell.
///
/// Excel's own cell limit is 32,767 characters and its note writer reserves
/// some of that for an author prefix. Capping here means a paste of a whole
/// document into a note is refused at the door rather than silently truncated
/// on export.
pub const MAX_COMMENT_CHARS: usize = 32_000;

/// One cell's note.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Comment {
    /// Who wrote it. Empty is allowed and means "unattributed"; xlsx round-trip
    /// substitutes a default author, because `xl/comments1.xml` requires one.
    pub author: String,
    pub text: String,
}

impl Comment {
    pub fn new(author: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            author: author.into(),
            text: text.into(),
        }
    }

    /// Heap cost of this comment's strings.
    pub fn heap_bytes(&self) -> usize {
        self.author.capacity() + self.text.capacity()
    }

    /// Text clamped to [`MAX_COMMENT_CHARS`], on a character boundary.
    ///
    /// Applied at insert time so the in-memory store can never hold something
    /// the exporter would have to reject or mangle.
    fn clamp(text: &str) -> String {
        match text.char_indices().nth(MAX_COMMENT_CHARS) {
            None => text.to_string(),
            Some((byte, _)) => text[..byte].to_string(),
        }
    }
}

/// Every comment on one sheet, sparse and keyed by display position.
#[derive(Debug, Default)]
pub struct CommentMap {
    /// Comments keyed by row; each row holds `(col, comment)` sorted by column.
    by_row: BTreeMap<u32, Vec<(u32, Comment)>>,
    len: usize,
    /// Map probes made since construction. Instrumentation for the paint-path
    /// cost tests — an atomic so the map stays `Sync` for off-thread export
    /// snapshots, and `Relaxed` because nothing orders against it.
    probes: AtomicU64,
}

impl Clone for CommentMap {
    fn clone(&self) -> Self {
        Self {
            by_row: self.by_row.clone(),
            len: self.len,
            probes: AtomicU64::new(self.probes.load(Ordering::Relaxed)),
        }
    }
}

impl PartialEq for CommentMap {
    /// Probe count is instrumentation, not content: two maps holding the same
    /// comments are equal however often either has been read.
    fn eq(&self, other: &Self) -> bool {
        self.by_row == other.by_row
    }
}

impl Eq for CommentMap {}

impl CommentMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the sheet has any comments at all.
    ///
    /// A plain field read, NOT a map probe. This is the short-circuit that
    /// makes an uncommented sheet cost nothing on the paint path.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Map probes made so far. Used by tests to assert the paint path's cost.
    #[inline]
    pub fn probes(&self) -> u64 {
        self.probes.load(Ordering::Relaxed)
    }

    /// Reset the probe counter, so a test can measure one frame in isolation.
    pub fn reset_probes(&self) {
        self.probes.store(0, Ordering::Relaxed);
    }

    /// Bytes held on the heap, for the resource guard.
    pub fn heap_bytes(&self) -> usize {
        self.by_row
            .values()
            .map(|v| {
                v.capacity() * (std::mem::size_of::<u32>() + std::mem::size_of::<Comment>())
                    + v.iter().map(|(_, c)| c.heap_bytes()).sum::<usize>()
            })
            .sum::<usize>()
            + self.by_row.len()
                * (std::mem::size_of::<u32>() + std::mem::size_of::<Vec<(u32, Comment)>>())
    }

    /// Attach (or replace) a comment, returning whatever was there before.
    ///
    /// Returning the previous value is what makes edit-and-undo possible
    /// without a second lookup, the same contract `EditOverlay::set` has.
    pub fn set(&mut self, cell: CellRef, comment: Comment) -> Option<Comment> {
        let comment = Comment {
            author: comment.author,
            text: Comment::clamp(&comment.text),
        };
        let row = self.by_row.entry(cell.row).or_default();
        match row.binary_search_by_key(&cell.col, |(c, _)| *c) {
            Ok(i) => Some(std::mem::replace(&mut row[i].1, comment)),
            Err(i) => {
                row.insert(i, (cell.col, comment));
                self.len += 1;
                None
            }
        }
    }

    /// Remove a cell's comment, returning it.
    pub fn remove(&mut self, cell: CellRef) -> Option<Comment> {
        let row = self.by_row.get_mut(&cell.row)?;
        let i = row.binary_search_by_key(&cell.col, |(c, _)| *c).ok()?;
        let (_, prev) = row.remove(i);
        if row.is_empty() {
            self.by_row.remove(&cell.row);
        }
        self.len -= 1;
        Some(prev)
    }

    /// Restore a previous state exactly — the undo primitive.
    pub fn restore(&mut self, cell: CellRef, prev: Option<Comment>) {
        match prev {
            Some(c) => {
                self.set(cell, c);
            }
            None => {
                self.remove(cell);
            }
        }
    }

    /// The comments on one row, sorted by column, or `None` for a row with
    /// none.
    ///
    /// The paint-path entry point: a caller hoists this out of its column loop
    /// and pays one probe per visible row instead of one per visible cell.
    #[inline]
    pub fn row_comments(&self, row: u32) -> Option<&[(u32, Comment)]> {
        if self.len == 0 {
            return None;
        }
        self.probes.fetch_add(1, Ordering::Relaxed);
        self.by_row.get(&row).map(|v| v.as_slice())
    }

    /// One cell's comment.
    #[inline]
    pub fn get(&self, cell: CellRef) -> Option<&Comment> {
        let row = self.row_comments(cell.row)?;
        let i = row.binary_search_by_key(&cell.col, |(c, _)| *c).ok()?;
        Some(&row[i].1)
    }

    #[inline]
    pub fn contains(&self, cell: CellRef) -> bool {
        self.get(cell).is_some()
    }

    /// Every comment, in (row, col) order.
    ///
    /// Ordered rather than arbitrary so persistence is byte-reproducible: two
    /// saves of the same map produce the same file, which is what lets backup
    /// dedup and diffing work.
    pub fn iter(&self) -> impl Iterator<Item = (CellRef, &Comment)> {
        self.by_row
            .iter()
            .flat_map(|(r, v)| v.iter().map(move |(c, cm)| (CellRef::new(*r, *c), cm)))
    }

    /// Move every comment in `from_col` to `to_col`, in one pass.
    ///
    /// Used by the column reorder path. Takes the whole permutation at once
    /// rather than one column at a time, because applying moves one by one can
    /// collide: moving A->B before B->C would clobber B's comments.
    ///
    /// Cost is O(comments), never O(rows) or O(columns) — the whole reason the
    /// store is sparse.
    pub fn remap_columns(&mut self, map: &std::collections::HashMap<u32, u32>) {
        if map.is_empty() || self.len == 0 {
            return;
        }
        let moved: Vec<(CellRef, Comment)> = self
            .iter()
            .filter(|(cell, _)| map.contains_key(&cell.col))
            .map(|(cell, c)| (cell, c.clone()))
            .collect();
        if moved.is_empty() {
            return;
        }
        // Two phases so a permutation cannot overwrite a cell it is about to
        // read: every source is vacated before any destination is written.
        for (cell, _) in &moved {
            self.remove(*cell);
        }
        for (cell, comment) in moved {
            let dest = CellRef::new(cell.row, map[&cell.col]);
            self.set(dest, comment);
        }
    }

    /// Rebuild from saved parts. Used by the sidecar loader.
    pub fn from_iter_cells<I: IntoIterator<Item = (CellRef, Comment)>>(items: I) -> Self {
        let mut m = Self::new();
        for (cell, c) in items {
            m.set(cell, c);
        }
        m
    }
}

#[cfg(test)]
mod tests;
