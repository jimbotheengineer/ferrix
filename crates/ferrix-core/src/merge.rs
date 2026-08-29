//! Merged cell regions.
//!
//! A merge is a rectangle that displays as one cell: the top-left ("anchor")
//! holds the value, and every other cell in the rectangle reads as empty and
//! refuses direct edits. Excel files use them constantly for headers and
//! labels, and Ferrix previously dropped them on import — a merged title row
//! came back as a value in the first column and blanks beside it, which looks
//! like data loss even though the bytes survived.
//!
//! # Storage
//!
//! Merges are stored as a sparse list of rectangles, never as a per-cell flag.
//! A 200M-row sheet with three merged header cells costs three rectangles. A
//! per-cell representation would cost 200M entries to describe the same three.
//!
//! # Lookup
//!
//! Painting asks "is this cell inside a merge?" for every visible cell, every
//! frame. A linear scan over every merge would make a sheet with many merges
//! quadratic in the wrong place. Regions are indexed in a [`BTreeMap`] keyed by
//! first row, and lookup range-queries from `row - tallest` to `row`, so the
//! scan is bounded by the number of merges that could possibly reach the row
//! rather than by the total.

use std::collections::BTreeMap;

use crate::table::TableRange;
use crate::CellRef;

/// Why a merge was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MergeError {
    /// The rectangle is a single cell; merging it would mean nothing.
    Degenerate,
    /// The rectangle overlaps an existing merge.
    ///
    /// Excel silently absorbs the older merge here. Ferrix refuses instead:
    /// absorbing means a click that used to select one region now selects a
    /// different one, with no way for the user to see it happened.
    Overlaps,
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeError::Degenerate => write!(f, "a merge needs more than one cell"),
            MergeError::Overlaps => write!(f, "that range overlaps a merged region"),
        }
    }
}

impl std::error::Error for MergeError {}

/// The merged regions of one sheet.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct MergeMap {
    /// Regions keyed by first row. Several merges may start on one row, hence
    /// the vector.
    by_row: BTreeMap<u32, Vec<TableRange>>,
    /// Rows spanned by the tallest region, which bounds the lookup window.
    tallest: u32,
    len: usize,
}

impl MergeMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Bytes held on the heap, for the resource guard.
    pub fn heap_bytes(&self) -> usize {
        self.by_row
            .values()
            .map(|v| v.capacity() * std::mem::size_of::<TableRange>())
            .sum::<usize>()
            + self.by_row.len()
                * (std::mem::size_of::<u32>() + std::mem::size_of::<Vec<TableRange>>())
    }

    /// Every region, in no particular order.
    pub fn regions(&self) -> impl Iterator<Item = &TableRange> {
        self.by_row.values().flatten()
    }

    /// Add a region.
    ///
    /// Rejects single cells and anything overlapping an existing merge.
    pub fn merge(&mut self, range: TableRange) -> Result<(), MergeError> {
        if range.first_row == range.last_row && range.first_col == range.last_col {
            return Err(MergeError::Degenerate);
        }
        if self.overlaps(range) {
            return Err(MergeError::Overlaps);
        }
        let height = range.last_row - range.first_row + 1;
        self.tallest = self.tallest.max(height);
        self.by_row.entry(range.first_row).or_default().push(range);
        self.len += 1;
        Ok(())
    }

    /// Remove the region containing `cell`, returning it.
    pub fn unmerge_at(&mut self, cell: CellRef) -> Option<TableRange> {
        let found = self.region_at(cell).copied()?;
        if let Some(v) = self.by_row.get_mut(&found.first_row) {
            v.retain(|r| *r != found);
            if v.is_empty() {
                self.by_row.remove(&found.first_row);
            }
        }
        self.len -= 1;
        // `tallest` is deliberately NOT recomputed: it only widens the lookup
        // window, so a stale-high value costs a slightly wider scan and never
        // a wrong answer. Recomputing would be O(n) on every unmerge.
        Some(found)
    }

    /// Drop every region intersecting `range`. Used by "unmerge" over a
    /// selection that covers several merges.
    pub fn unmerge_range(&mut self, range: TableRange) -> usize {
        let victims: Vec<TableRange> = self
            .regions()
            .filter(|r| intersects(**r, range))
            .copied()
            .collect();
        for v in &victims {
            if let Some(list) = self.by_row.get_mut(&v.first_row) {
                list.retain(|r| r != v);
                if list.is_empty() {
                    self.by_row.remove(&v.first_row);
                }
            }
            self.len -= 1;
        }
        victims.len()
    }

    /// The region containing `cell`, if any.
    pub fn region_at(&self, cell: CellRef) -> Option<&TableRange> {
        if self.by_row.is_empty() {
            return None;
        }
        let lo = cell.row.saturating_sub(self.tallest.saturating_sub(1));
        for (_, list) in self.by_row.range(lo..=cell.row) {
            for r in list {
                if contains(*r, cell) {
                    return Some(r);
                }
            }
        }
        None
    }

    /// Is this cell a merge's top-left?
    pub fn is_anchor(&self, cell: CellRef) -> bool {
        self.region_at(cell)
            .is_some_and(|r| r.first_row == cell.row && r.first_col == cell.col)
    }

    /// Is this cell covered by a merge but NOT its anchor?
    ///
    /// These are the cells that read as empty and refuse edits. Keeping this a
    /// single predicate means callers cannot accidentally treat the anchor as
    /// covered, which would blank the very cell holding the value.
    pub fn is_covered(&self, cell: CellRef) -> bool {
        self.region_at(cell)
            .is_some_and(|r| !(r.first_row == cell.row && r.first_col == cell.col))
    }

    /// The cell that actually holds `cell`'s value: its merge anchor, or the
    /// cell itself.
    pub fn resolve(&self, cell: CellRef) -> CellRef {
        match self.region_at(cell) {
            Some(r) => CellRef::new(r.first_row, r.first_col),
            None => cell,
        }
    }

    fn overlaps(&self, range: TableRange) -> bool {
        // A candidate can only overlap regions that start at or above its last
        // row, and no higher than the tallest existing region reaches.
        let lo = range
            .first_row
            .saturating_sub(self.tallest.saturating_sub(1));
        self.by_row
            .range(lo..=range.last_row)
            .flat_map(|(_, v)| v)
            .any(|r| intersects(*r, range))
    }
}

fn contains(r: TableRange, c: CellRef) -> bool {
    c.row >= r.first_row && c.row <= r.last_row && c.col >= r.first_col && c.col <= r.last_col
}

fn intersects(a: TableRange, b: TableRange) -> bool {
    a.first_row <= b.last_row
        && b.first_row <= a.last_row
        && a.first_col <= b.last_col
        && b.first_col <= a.last_col
}

#[cfg(test)]
mod tests;
