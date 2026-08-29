//! Display order: a permutation of an axis, stored as runs.
//!
//! ## The problem
//!
//! Reordering a column must not move the data. A `.ferrix` base is immutable
//! and often a 12 GB mmap; a 200M-row column is 800 MB of values that the user
//! expects to "move" in the time it takes to release the mouse. So a reorder
//! is a change to *which data index each display position shows*, and the data
//! never moves at all.
//!
//! ## Why runs, and not a `Vec<u32>` permutation
//!
//! The obvious representation is `to_data: Vec<u32>`, indexed by display
//! position. For columns that is perfect — 8 columns is 32 bytes. For rows it
//! is a disaster: 200M rows is **800 MB**, allocated the instant the user drags
//! one row, and `O(rows)` to build. That is exactly the per-row work this
//! feature is supposed to avoid, moved from the data to the index.
//!
//! The observation that fixes it: a permutation produced by a handful of
//! structural edits is not arbitrary. It is a small number of **contiguous
//! ascending runs** of the original order. Identity over 200M rows is ONE run.
//! Moving a row splits and re-splices at most three boundaries, so `k`
//! structural operations leave `O(k)` runs regardless of how many rows the
//! sheet has:
//!
//! ```text
//!   identity, 200M rows      runs: [0..200_000_000)                 16 bytes
//!   move row 5 to 100        runs: [0..5) [6..101) [5..6) [101..)   64 bytes
//! ```
//!
//! Lookup is a binary search over run starts — `O(log k)`, and `k` is the
//! number of edits the user made, never the number of rows. This is what lets
//! rows and columns share one implementation instead of rows needing a
//! separate, weaker feature.
//!
//! ## The bound, and why it is visible
//!
//! Runs accumulate: every non-adjacent structural edit can add up to two. So
//! the growth is bounded by user actions rather than data size, but it is not
//! bounded by *nothing*. [`AxisOrder::MAX_RUNS`] caps it, and an operation that
//! would exceed the cap is **refused with an error the UI shows**, rather than
//! being accepted and quietly making every subsequent lookup slower. A limit
//! the user can see is worth more than one they can only feel.
//!
//! [`AxisOrder::coalesce`] merges runs back together whenever an edit happens
//! to restore adjacency, so undoing a move genuinely gives the memory back and
//! a sheet returned to its original order is one run again.
//!
//! ## Fresh indices
//!
//! Inserting a column cannot shift the data indices of its neighbours — that
//! would mean rewriting the mmap. Instead an insert allocates a **fresh** data
//! index from beyond the base's extent (`next_fresh`) and places it at the
//! requested display position. The base is untouched, the new column reads as
//! empty because nothing has ever been written there, and edits to it land in
//! the sparse overlay like any other edit.
//!
//! Deleting is the mirror image: the run is cut out of the display order and
//! the data is simply no longer addressed. Nothing is erased, which is what
//! makes undo a snapshot of this structure rather than a restore of the data.

/// A maximal block of consecutively-increasing data indices.
///
/// `data..data + len` occupy consecutive display positions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Run {
    data: u32,
    len: u32,
}

/// Why a structural edit was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OrderError {
    /// The operation would push the run count past [`AxisOrder::MAX_RUNS`].
    TooFragmented { runs: usize, limit: usize },
    /// A display position or span fell outside the axis.
    OutOfRange { at: u64, len: u64 },
    /// A move whose destination lies inside the span being moved.
    DestinationInsideSpan,
}

impl std::fmt::Display for OrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderError::TooFragmented { runs, limit } => write!(
                f,
                "too many separate moves to track ({runs} of {limit}) — \
                 sort the sheet or save and reopen it to start from a clean order"
            ),
            OrderError::OutOfRange { at, len } => {
                write!(f, "position {at} is outside the {len} available")
            }
            OrderError::DestinationInsideSpan => {
                write!(f, "cannot move a block into the middle of itself")
            }
        }
    }
}

/// A permutation of one axis (rows or columns), stored as ascending runs.
///
/// Construct with [`AxisOrder::identity`]; it stays a single run — and every
/// lookup stays a trivially predictable `display == data` — until something
/// actually reorders it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AxisOrder {
    /// Runs in display order. Their lengths sum to `len`.
    runs: Vec<Run>,
    /// `starts[i]` is the display position where `runs[i]` begins.
    /// `starts.len() == runs.len() + 1`, with the total in the last slot.
    starts: Vec<u64>,
    /// Next unused data index, handed out by [`AxisOrder::insert_fresh`].
    next_fresh: u32,
}

impl AxisOrder {
    /// Cap on run count. Beyond this, structural edits are refused rather than
    /// silently degrading lookup. 64K runs is ~512 KB and roughly 32,000
    /// individual drag-reorders — far past any hand-driven session, and small
    /// enough that a binary search over it stays in cache.
    pub const MAX_RUNS: usize = 65_536;

    /// The untouched order for an axis of `len` entries.
    ///
    /// This is one run no matter how large `len` is: identity over 200M rows
    /// costs the same 16 bytes as identity over 8 columns.
    pub fn identity(len: u64) -> Self {
        let len32 = u32::try_from(len).unwrap_or(u32::MAX);
        let runs = if len32 == 0 {
            Vec::new()
        } else {
            vec![Run {
                data: 0,
                len: len32,
            }]
        };
        let starts = if len32 == 0 {
            vec![0]
        } else {
            vec![0, len32 as u64]
        };
        Self {
            runs,
            starts,
            next_fresh: len32,
        }
    }

    /// Number of display positions.
    #[inline]
    pub fn len(&self) -> u64 {
        *self.starts.last().unwrap_or(&0)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many runs the order currently needs. Exposed so the UI can show the
    /// user how close they are to [`MAX_RUNS`](Self::MAX_RUNS) instead of
    /// surprising them at the cap.
    #[inline]
    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// True when display position equals data index everywhere.
    ///
    /// The hot paths check this first and skip all mapping work, so a sheet
    /// nobody has reordered pays literally nothing for this feature.
    #[inline]
    pub fn is_identity(&self) -> bool {
        match self.runs.as_slice() {
            [] => true,
            [only] => only.data == 0,
            _ => false,
        }
    }

    /// Approximate heap cost, for the memory readout.
    pub fn heap_bytes(&self) -> usize {
        self.runs.capacity() * std::mem::size_of::<Run>()
            + self.starts.capacity() * std::mem::size_of::<u64>()
    }

    /// Data index shown at a display position, or `None` past the end.
    ///
    /// `O(log runs)` — and `runs` counts the user's structural edits, never
    /// the sheet's size.
    #[inline]
    pub fn data_of(&self, display: u64) -> Option<u32> {
        if display >= self.len() {
            return None;
        }
        // Fast path: an unreordered axis is the identity, and the overwhelming
        // majority of lookups happen on one.
        if self.is_identity() {
            return u32::try_from(display).ok();
        }
        // `starts` is sorted; find the run containing `display`.
        let i = self.starts.partition_point(|&s| s <= display) - 1;
        let run = self.runs[i];
        let within = display - self.starts[i];
        Some(run.data + within as u32)
    }

    /// Display position of a data index, or `None` if it is not shown (it was
    /// deleted, or never existed).
    ///
    /// `O(runs)`: unlike [`data_of`](Self::data_of) the runs are not sorted by
    /// data index, so this cannot binary search. Callers use it for one-off
    /// questions ("where did the column I just moved end up?"), never per
    /// painted cell — the paint loop walks display positions and maps forward.
    pub fn display_of(&self, data: u32) -> Option<u64> {
        if self.is_identity() {
            return (u64::from(data) < self.len()).then(|| u64::from(data));
        }
        for (i, run) in self.runs.iter().enumerate() {
            if data >= run.data && data < run.data + run.len {
                return Some(self.starts[i] + u64::from(data - run.data));
            }
        }
        None
    }

    /// Rebuild `starts` from `runs`.
    fn reindex(&mut self) {
        self.starts.clear();
        self.starts.reserve(self.runs.len() + 1);
        let mut acc = 0u64;
        for r in &self.runs {
            self.starts.push(acc);
            acc += u64::from(r.len);
        }
        self.starts.push(acc);
    }

    /// Merge runs whose data indices are adjacent.
    ///
    /// This is what makes the structure self-healing: undoing a move, or
    /// dragging a column back where it came from, collapses the fragments and
    /// returns the order to a single identity run.
    fn coalesce(&mut self) {
        if self.runs.len() < 2 {
            return;
        }
        let mut merged: Vec<Run> = Vec::with_capacity(self.runs.len());
        for r in self.runs.drain(..) {
            match merged.last_mut() {
                Some(prev) if prev.data + prev.len == r.data => prev.len += r.len,
                _ => merged.push(r),
            }
        }
        self.runs = merged;
    }

    /// Split runs so that a run boundary exists exactly at display position
    /// `at`, and return the index of the run starting there.
    ///
    /// `at == len()` returns `runs.len()`, which is what makes appending work
    /// without a special case.
    fn split_at(&mut self, at: u64) -> usize {
        if at >= self.len() {
            return self.runs.len();
        }
        let i = self.starts.partition_point(|&s| s <= at) - 1;
        let offset = at - self.starts[i];
        if offset == 0 {
            return i;
        }
        // Cut run `i` into [..offset] and [offset..].
        let run = self.runs[i];
        let left = Run {
            data: run.data,
            len: offset as u32,
        };
        let right = Run {
            data: run.data + offset as u32,
            len: run.len - offset as u32,
        };
        self.runs[i] = left;
        self.runs.insert(i + 1, right);
        self.reindex();
        i + 1
    }

    /// Refuse an edit that would fragment the order past the cap.
    fn check_runs(&self, extra: usize) -> Result<(), OrderError> {
        let projected = self.runs.len() + extra;
        if projected > Self::MAX_RUNS {
            return Err(OrderError::TooFragmented {
                runs: projected,
                limit: Self::MAX_RUNS,
            });
        }
        Ok(())
    }

    /// Move `count` entries starting at display position `from` so that they
    /// begin at display position `to` **in the order as it stands before the
    /// move** — the same convention a drag-and-drop drop indicator implies.
    ///
    /// Cost is `O(runs)`, independent of `count`: moving one row and moving
    /// 200M rows are the same amount of work, because neither touches data.
    pub fn move_span(&mut self, from: u64, count: u64, to: u64) -> Result<(), OrderError> {
        if count == 0 || from == to {
            return Ok(());
        }
        let len = self.len();
        if from + count > len {
            return Err(OrderError::OutOfRange { at: from, len });
        }
        if to > len {
            return Err(OrderError::OutOfRange { at: to, len });
        }
        if to > from && to < from + count {
            return Err(OrderError::DestinationInsideSpan);
        }
        // A move splits at up to three boundaries, so bound the growth before
        // committing to any of them.
        self.check_runs(3)?;

        // Establish boundaries at every cut point. Split the LATER positions
        // first so earlier indices stay valid.
        let mut cuts = [from, from + count, to];
        cuts.sort_unstable();
        for &c in cuts.iter().rev() {
            self.split_at(c);
        }
        // Re-resolve after splitting: indices moved.
        let start_i = self.split_at(from);
        let end_i = self.split_at(from + count);
        let moved: Vec<Run> = self.runs.drain(start_i..end_i).collect();
        self.reindex();

        // The destination, expressed in the order with the span removed.
        let dest = if to > from { to - count } else { to };
        let insert_i = self.split_at(dest);
        // splice() keeps the moved runs in their original relative order.
        self.runs.splice(insert_i..insert_i, moved);

        self.coalesce();
        self.reindex();
        Ok(())
    }

    /// Insert `count` brand-new entries at display position `at`, returning the
    /// first fresh data index allocated.
    ///
    /// The fresh indices come from beyond everything the base holds, so no
    /// existing data index shifts and the immutable base is never rewritten.
    /// The new entries read as empty until something is written to them.
    pub fn insert_fresh(&mut self, at: u64, count: u64) -> Result<u32, OrderError> {
        if count == 0 {
            return Ok(self.next_fresh);
        }
        let len = self.len();
        if at > len {
            return Err(OrderError::OutOfRange { at, len });
        }
        self.check_runs(2)?;
        let data = self.next_fresh;
        let count32 =
            u32::try_from(count).map_err(|_| OrderError::OutOfRange { at: count, len })?;
        self.next_fresh = self.next_fresh.saturating_add(count32);

        let i = self.split_at(at);
        self.runs.insert(i, Run { data, len: count32 });
        self.coalesce();
        self.reindex();
        Ok(data)
    }

    /// Remove `count` entries at display position `at`.
    ///
    /// The underlying data is not erased — it simply stops being addressed.
    /// That is deliberate: it makes delete `O(runs)` on a 12 GB mmap, and it
    /// makes undo a snapshot of this structure rather than a restore of the
    /// values.
    pub fn remove(&mut self, at: u64, count: u64) -> Result<(), OrderError> {
        if count == 0 {
            return Ok(());
        }
        let len = self.len();
        if at + count > len {
            return Err(OrderError::OutOfRange { at, len });
        }
        self.check_runs(2)?;
        let start_i = self.split_at(at);
        let end_i = self.split_at(at + count);
        self.runs.drain(start_i..end_i);
        self.coalesce();
        self.reindex();
        Ok(())
    }

    /// Grow the axis to at least `len` display positions, appending identity
    /// entries.
    ///
    /// Editing past the end of a sheet extends it; the order has to follow, or
    /// the newly-reachable rows would have no display position at all.
    pub fn ensure_len(&mut self, len: u64) {
        let have = self.len();
        if len <= have {
            return;
        }
        let want = u32::try_from(len).unwrap_or(u32::MAX);
        // Extend using data indices that have never been handed out, so this
        // can never alias a fresh index from an insert.
        let start = self
            .next_fresh
            .max(want.saturating_sub((len - have) as u32));
        let add = want - have as u32;
        // Prefer plain identity growth when the order has never been touched:
        // that keeps `is_identity()` true and the fast paths live.
        if self.is_identity() {
            match self.runs.first_mut() {
                Some(r) if r.data == 0 => r.len = want,
                _ => self.runs.push(Run { data: 0, len: want }),
            }
            self.next_fresh = self.next_fresh.max(want);
        } else {
            self.runs.push(Run {
                data: start,
                len: add,
            });
            self.next_fresh = self.next_fresh.max(start.saturating_add(add));
        }
        self.coalesce();
        self.reindex();
    }

    /// Map a contiguous display span to the data spans it covers.
    ///
    /// A range that is contiguous on screen need not be contiguous in the data
    /// once the axis has been reordered: display columns A:C might be data
    /// columns 0, 2 and 3. Anything that wants to treat a selection as
    /// rectangles over the data — copy, clear, a columnar SUM — has to ask for
    /// the pieces rather than assume one block.
    ///
    /// Returns `(data_start, count)` pairs in display order. For an
    /// unreordered axis this is always a single pair, so the columnar fast
    /// paths keep their single rectangle.
    pub fn data_spans(&self, at: u64, count: u64) -> Vec<(u32, u32)> {
        if count == 0 {
            return Vec::new();
        }
        if self.is_identity() {
            let start = u32::try_from(at).unwrap_or(u32::MAX);
            let n = u32::try_from(count.min(self.len().saturating_sub(at))).unwrap_or(u32::MAX);
            return if n == 0 { Vec::new() } else { vec![(start, n)] };
        }
        let end = (at + count).min(self.len());
        let mut out = Vec::new();
        let mut pos = at;
        while pos < end {
            let i = self.starts.partition_point(|&s| s <= pos) - 1;
            let run = self.runs[i];
            let within = pos - self.starts[i];
            let take = (u64::from(run.len) - within).min(end - pos);
            out.push((run.data + within as u32, take as u32));
            pos += take;
        }
        out
    }

    /// Build an order with exactly `runs` runs, for testing the run cap.
    ///
    /// Reaching [`MAX_RUNS`](Self::MAX_RUNS) through real moves is quadratic;
    /// the cap's behaviour is what matters, not the path taken to it.
    #[cfg(test)]
    fn fragmented_for_test(runs: usize) -> Self {
        // Descending data indices, so no two adjacent runs can coalesce.
        let mut o = Self {
            runs: (0..runs)
                .map(|i| Run {
                    data: (runs - i) as u32,
                    len: 1,
                })
                .collect(),
            starts: Vec::new(),
            next_fresh: runs as u32 + 1,
        };
        o.reindex();
        o
    }
}

/// What a structural edit did to one axis, as a map over DISPLAY positions.
///
/// [`AxisOrder`] answers "which data does display position N show". This
/// answers the other question every side table needs: "the thing that used to
/// be at display position N — where is it now, if anywhere?".
///
/// Insert and delete are the only two edits with a well-defined answer for a
/// RECTANGLE as well as for a point, which is why merges and format ranges are
/// remapped through this rather than through a general permutation. A move
/// cannot keep a rectangle a rectangle (display columns A:C can become 0, 2, 3),
/// so the move path remaps only per-cell stores and leaves rectangles alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AxisShift {
    /// `count` fresh entries appeared at display position `at`.
    Insert { at: u32, count: u32 },
    /// `count` entries were removed starting at display position `at`.
    Delete { at: u32, count: u32 },
}

impl AxisShift {
    /// Where the entry formerly at display position `old` is now, or `None`
    /// when the edit deleted it.
    ///
    /// This is exactly the [`crate::order`] half of what
    /// `ferrix_formula::remap::AxisMap` wants, so a reference to a deleted row
    /// or column collapses to `#REF!` instead of silently sliding onto its
    /// neighbour.
    #[inline]
    pub fn map(&self, old: u32) -> Option<u32> {
        match *self {
            AxisShift::Insert { at, count } => Some(if old >= at {
                old.saturating_add(count)
            } else {
                old
            }),
            AxisShift::Delete { at, count } => {
                let end = at.saturating_add(count);
                if old < at {
                    Some(old)
                } else if old < end {
                    // The entry itself is gone. Not clamped to a neighbour:
                    // silently reading different data is the failure mode the
                    // whole remap family exists to prevent.
                    None
                } else {
                    Some(old - count)
                }
            }
        }
    }

    /// Where the span `first..=last` ends up, or `None` when the edit removed
    /// every position in it.
    ///
    /// The rectangle rule, and the reason it is here rather than open-coded in
    /// three side tables:
    ///
    /// * an insert **inside** a span grows the span, matching what a user means
    ///   by inserting a row into a merged block or a formatted range;
    /// * a delete that eats only part of a span shrinks it to the survivors;
    /// * a delete that eats all of it returns `None`, and the caller drops the
    ///   entry rather than keeping a rectangle over rows that no longer exist.
    pub fn map_span(&self, first: u32, last: u32) -> Option<(u32, u32)> {
        if first > last {
            return None;
        }
        match *self {
            AxisShift::Insert { at, count } => {
                let f = if first >= at {
                    first.saturating_add(count)
                } else {
                    first
                };
                // `>=` on the start and `>=` on the end together mean an insert
                // strictly inside the span moves only its end, i.e. the span
                // grows by `count`.
                let l = if last >= at {
                    last.saturating_add(count)
                } else {
                    last
                };
                Some((f, l))
            }
            AxisShift::Delete { at, count } => {
                let end = at.saturating_add(count);
                // Lowest surviving position at or after `first`.
                let f = if first < at {
                    first
                } else if first < end {
                    at
                } else {
                    first - count
                };
                // Highest surviving position at or before `last`.
                let l = if last < at {
                    last
                } else if last < end {
                    // Everything from `at` up is gone; fall back to at - 1.
                    at.checked_sub(1)?
                } else {
                    last - count
                };
                (f <= l).then_some((f, l))
            }
        }
    }
}

/// Both axes of one sheet's display order.
///
/// ## Which space is which
///
/// Only the **immutable base** is addressed in data space. Everything the user
/// interacts with — the selection, the edit overlay, formula text, the
/// dependency graph — stays in DISPLAY space, and a reorder permutes those
/// eagerly.
///
/// That split is deliberate. The base is the thing that must never be
/// rewritten: it can be a 12 GB mmap, so it is mapped lazily through the
/// permutation on every read, and a reorder touches none of it. The overlay is
/// sparse by construction — a million-row file with three edits holds three
/// entries — so permuting it on a reorder is O(edits), not O(rows), and in
/// exchange every other subsystem keeps working in the coordinates the user
/// sees. The alternative (overlay in data space) would push the mapping into
/// `commit_edit`, undo, the clipboard, the fill handle, and the depgraph, for
/// no gain.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SheetOrder {
    pub rows: Option<AxisOrder>,
    pub cols: Option<AxisOrder>,
}

impl SheetOrder {
    /// An untouched sheet. Both axes are `None`, meaning pure identity — the
    /// representation costs nothing and every mapping call short-circuits.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when neither axis has been reordered, so reads can skip mapping
    /// entirely. This is the state of every sheet nobody has dragged.
    #[inline]
    pub fn is_identity(&self) -> bool {
        self.rows.as_ref().is_none_or(|o| o.is_identity())
            && self.cols.as_ref().is_none_or(|o| o.is_identity())
    }

    /// Map a display cell to the base data cell it shows.
    ///
    /// `None` means the display position addresses no base data — an inserted
    /// row or column, which correctly reads as empty.
    #[inline]
    pub fn to_data(&self, row: u32, col: u32) -> Option<(u32, u32)> {
        let r = match &self.rows {
            None => row,
            Some(o) => o.data_of(u64::from(row))?,
        };
        let c = match &self.cols {
            None => col,
            Some(o) => o.data_of(u64::from(col))?,
        };
        Some((r, c))
    }

    /// The row axis, materialising identity on first use.
    pub fn rows_mut(&mut self, len: u64) -> &mut AxisOrder {
        self.rows.get_or_insert_with(|| AxisOrder::identity(len))
    }

    /// The column axis, materialising identity on first use.
    pub fn cols_mut(&mut self, len: u64) -> &mut AxisOrder {
        self.cols.get_or_insert_with(|| AxisOrder::identity(len))
    }

    pub fn heap_bytes(&self) -> usize {
        self.rows.as_ref().map_or(0, |o| o.heap_bytes())
            + self.cols.as_ref().map_or(0, |o| o.heap_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expand to an explicit permutation. Test-only — the whole point of the
    /// run representation is that production code never does this.
    fn expand(o: &AxisOrder) -> Vec<u32> {
        (0..o.len()).map(|d| o.data_of(d).unwrap()).collect()
    }

    #[test]
    fn identity_is_one_run_at_any_size() {
        for len in [0u64, 1, 8, 200_000_000] {
            let o = AxisOrder::identity(len);
            assert_eq!(o.len(), len);
            assert!(o.is_identity());
            assert!(o.run_count() <= 1, "identity must never need two runs");
            assert!(
                o.heap_bytes() < 256,
                "identity over {len} cost {} bytes",
                o.heap_bytes()
            );
        }
    }

    #[test]
    fn identity_maps_display_straight_to_data() {
        let o = AxisOrder::identity(200_000_000);
        assert_eq!(o.data_of(0), Some(0));
        assert_eq!(o.data_of(199_999_999), Some(199_999_999));
        assert_eq!(o.data_of(200_000_000), None, "past the end");
        assert_eq!(o.display_of(199_999_999), Some(199_999_999));
    }

    #[test]
    fn moving_a_column_permutes_the_index() {
        // The headline case: 8 columns, drag B (display 1) to position 3.
        let mut o = AxisOrder::identity(8);
        o.move_span(1, 1, 4).unwrap();
        // B lands after what was C and D; everything else shuffles left.
        assert_eq!(expand(&o), vec![0, 2, 3, 1, 4, 5, 6, 7]);
        // The data index for the moved column is unchanged — the DATA did not
        // move, only where it is shown.
        assert_eq!(o.display_of(1), Some(3));
        assert_eq!(o.data_of(3), Some(1));
    }

    #[test]
    fn moving_backwards_works_too() {
        let mut o = AxisOrder::identity(6);
        o.move_span(4, 1, 1).unwrap();
        assert_eq!(expand(&o), vec![0, 4, 1, 2, 3, 5]);
    }

    #[test]
    fn moving_a_multi_column_block_keeps_its_internal_order() {
        let mut o = AxisOrder::identity(8);
        o.move_span(1, 3, 6).unwrap(); // B,C,D -> after F
        assert_eq!(expand(&o), vec![0, 4, 5, 1, 2, 3, 6, 7]);
    }

    #[test]
    fn a_move_and_its_inverse_restore_identity_and_the_memory() {
        // Self-healing: dragging a row back where it came from must collapse
        // the runs again, not leave the order permanently fragmented.
        let mut o = AxisOrder::identity(200_000_000);
        o.move_span(5, 1, 100).unwrap();
        assert!(!o.is_identity());
        assert!(o.run_count() > 1);

        // Row 5 now sits at display 99 (the 100 destination minus the one
        // entry removed from before it). Drag it back.
        assert_eq!(o.data_of(99), Some(5));
        o.move_span(99, 1, 5).unwrap();

        assert!(o.is_identity(), "inverse move did not restore identity");
        assert_eq!(o.run_count(), 1, "runs must coalesce back to one");
        assert_eq!(o.len(), 200_000_000);
        assert_eq!(o.data_of(5), Some(5));
    }

    #[test]
    fn reordering_a_200m_row_axis_does_no_per_row_work() {
        // THE central constraint. A move on a 200M-entry axis must cost a
        // handful of runs and no allocation proportional to the axis.
        let mut o = AxisOrder::identity(200_000_000);
        let before = o.heap_bytes();
        o.move_span(0, 1, 100_000_000).unwrap();
        assert_eq!(o.len(), 200_000_000, "no entries lost");
        assert!(
            o.run_count() <= 4,
            "a single move needed {} runs",
            o.run_count()
        );
        assert!(
            o.heap_bytes() < before + 512,
            "a move over 200M rows allocated {} bytes",
            o.heap_bytes() - before
        );
        // And the mapping is still exact at the deep end.
        assert_eq!(o.data_of(199_999_999), Some(199_999_999));
        // Row 0 was dropped in at display 100_000_000, which is display
        // 99_999_999 once its own removal from the front is accounted for.
        assert_eq!(o.data_of(99_999_999), Some(0));
        assert_eq!(o.display_of(0), Some(99_999_999));
        // Its former neighbours shuffled down by exactly one.
        assert_eq!(o.data_of(0), Some(1));
        assert_eq!(o.data_of(100_000_000), Some(100_000_000));
    }

    #[test]
    fn many_moves_stay_bounded_by_edits_not_by_rows() {
        // 1,000 separate moves over a 200M-row axis. The cost must track the
        // number of EDITS, not the number of rows.
        let mut o = AxisOrder::identity(200_000_000);
        for i in 0..1_000u64 {
            o.move_span(i, 1, 1_000_000 + i * 1_000).unwrap();
        }
        assert!(
            o.run_count() < 4_000,
            "1000 moves produced {} runs",
            o.run_count()
        );
        assert!(
            o.heap_bytes() < 200_000,
            "1000 moves over 200M rows cost {} bytes (a Vec<u32> would be 800MB)",
            o.heap_bytes()
        );
        assert_eq!(o.len(), 200_000_000);
    }

    #[test]
    fn insert_allocates_a_fresh_index_and_shifts_nothing() {
        let mut o = AxisOrder::identity(4);
        let fresh = o.insert_fresh(1, 1).unwrap();
        assert_eq!(fresh, 4, "fresh index comes from past the base extent");
        assert_eq!(expand(&o), vec![0, 4, 1, 2, 3]);
        // Crucially the neighbours kept their DATA indices, so nothing that
        // referenced them has been invalidated and no base bytes moved.
        assert_eq!(o.data_of(2), Some(1));
        assert_eq!(o.len(), 5);
    }

    #[test]
    fn successive_inserts_never_reuse_an_index() {
        let mut o = AxisOrder::identity(3);
        let a = o.insert_fresh(0, 1).unwrap();
        let b = o.insert_fresh(0, 1).unwrap();
        assert_ne!(a, b);
        assert_eq!(expand(&o), vec![b, a, 0, 1, 2]);
    }

    #[test]
    fn remove_cuts_the_display_order_without_erasing_data() {
        let mut o = AxisOrder::identity(5);
        o.remove(1, 2).unwrap();
        assert_eq!(expand(&o), vec![0, 3, 4]);
        assert_eq!(o.len(), 3);
        // The removed data indices are simply unreachable now.
        assert_eq!(o.display_of(1), None);
        assert_eq!(o.display_of(2), None);
    }

    #[test]
    fn removing_a_span_of_a_200m_axis_is_still_cheap() {
        let mut o = AxisOrder::identity(200_000_000);
        o.remove(50_000_000, 1).unwrap();
        assert_eq!(o.len(), 199_999_999);
        assert!(o.run_count() <= 2);
        assert_eq!(o.data_of(50_000_000), Some(50_000_001));
    }

    #[test]
    fn out_of_range_operations_are_refused_not_clamped() {
        let mut o = AxisOrder::identity(4);
        assert!(matches!(
            o.move_span(3, 4, 0),
            Err(OrderError::OutOfRange { .. })
        ));
        assert!(matches!(o.remove(3, 9), Err(OrderError::OutOfRange { .. })));
        assert!(matches!(
            o.insert_fresh(99, 1),
            Err(OrderError::OutOfRange { .. })
        ));
        // ...and a refused operation must leave the order untouched.
        assert_eq!(expand(&o), vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_move_into_its_own_span_is_refused() {
        let mut o = AxisOrder::identity(8);
        assert_eq!(o.move_span(2, 3, 3), Err(OrderError::DestinationInsideSpan));
        assert_eq!(expand(&o), vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn a_no_op_move_is_accepted_and_changes_nothing() {
        let mut o = AxisOrder::identity(5);
        o.move_span(2, 0, 4).unwrap();
        o.move_span(2, 1, 2).unwrap();
        assert!(o.is_identity());
    }

    #[test]
    fn fragmentation_is_refused_with_a_message_not_absorbed() {
        // The documented bound, made visible: at the cap, the next edit is
        // REFUSED rather than accepted and quietly making lookup slower.
        //
        // Driving 32k real moves to reach the cap would be quadratic, so this
        // fabricates an order already at the limit and checks the gate.
        let mut o = AxisOrder::fragmented_for_test(AxisOrder::MAX_RUNS);
        assert_eq!(o.run_count(), AxisOrder::MAX_RUNS);

        let e = o
            .move_span(0, 1, 100)
            .expect_err("at the cap, a move must be refused");
        assert!(matches!(e, OrderError::TooFragmented { .. }));
        // The message has to tell the user what to do about it.
        let msg = e.to_string();
        assert!(msg.contains("too many"), "unhelpful message: {msg}");
        assert!(msg.contains("save and reopen"), "no remedy offered: {msg}");
        // Inserts and deletes are gated by the same cap.
        assert!(matches!(
            o.insert_fresh(0, 1),
            Err(OrderError::TooFragmented { .. })
        ));
        assert!(matches!(
            o.remove(0, 1),
            Err(OrderError::TooFragmented { .. })
        ));
        // A refused edit leaves the order exactly as it was.
        assert_eq!(o.run_count(), AxisOrder::MAX_RUNS);
    }

    #[test]
    fn fragmentation_grows_only_with_edits() {
        // Each non-coalescing move costs a bounded, small number of runs — and
        // that number does not depend on how many rows the axis has.
        for len in [100_000u64, 200_000_000] {
            let mut o = AxisOrder::identity(len);
            for i in 0..200u64 {
                o.move_span(i * 2, 1, len - 1).unwrap();
            }
            assert!(
                o.run_count() <= 200 * 3,
                "200 moves over {len} rows produced {} runs",
                o.run_count()
            );
        }
    }

    #[test]
    fn data_spans_are_one_block_when_unreordered() {
        // The columnar fast path depends on this: an untouched axis must hand
        // back a single rectangle, never a pile of one-column pieces.
        let o = AxisOrder::identity(200_000_000);
        assert_eq!(o.data_spans(0, 200_000_000), vec![(0, 200_000_000)]);
        assert_eq!(o.data_spans(10, 5), vec![(10, 5)]);
        assert_eq!(o.data_spans(5, 0), Vec::new());
    }

    #[test]
    fn data_spans_split_a_reordered_range() {
        // Display A:C after moving B away is data 0, 2, 3 — NOT one block.
        // Anything summing that range has to know.
        let mut o = AxisOrder::identity(8);
        o.move_span(1, 1, 6).unwrap();
        assert_eq!(expand(&o), vec![0, 2, 3, 4, 5, 1, 6, 7]);
        assert_eq!(o.data_spans(0, 3), vec![(0, 1), (2, 2)]);
        // A span that is still contiguous stays one piece.
        assert_eq!(o.data_spans(1, 3), vec![(2, 3)]);
    }

    #[test]
    fn data_spans_cover_exactly_the_requested_count() {
        let mut o = AxisOrder::identity(20);
        o.move_span(3, 2, 15).unwrap();
        o.move_span(0, 1, 9).unwrap();
        for at in 0..o.len() {
            for count in 1..=(o.len() - at) {
                let spans = o.data_spans(at, count);
                let total: u64 = spans.iter().map(|&(_, n)| u64::from(n)).sum();
                assert_eq!(total, count, "spans lost entries at {at}+{count}");
                // And they must name exactly the same data, in order.
                let flat: Vec<u32> = spans.iter().flat_map(|&(d, n)| d..d + n).collect();
                let expect: Vec<u32> = (at..at + count).map(|d| o.data_of(d).unwrap()).collect();
                assert_eq!(flat, expect);
            }
        }
    }

    #[test]
    fn ensure_len_grows_without_disturbing_the_order() {
        let mut o = AxisOrder::identity(3);
        o.ensure_len(6);
        assert_eq!(o.len(), 6);
        assert!(o.is_identity(), "growing an untouched axis stays identity");

        let mut o = AxisOrder::identity(4);
        o.move_span(0, 1, 4).unwrap();
        let before = expand(&o);
        o.ensure_len(6);
        assert_eq!(o.len(), 6);
        assert_eq!(&expand(&o)[..4], &before[..], "existing positions moved");
        // Growth must not alias an index already on screen.
        let all = expand(&o);
        let mut sorted = all.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "duplicate data index after growth");
    }

    #[test]
    fn ensure_len_never_shrinks() {
        let mut o = AxisOrder::identity(10);
        o.ensure_len(4);
        assert_eq!(o.len(), 10);
    }

    #[test]
    fn every_position_maps_to_a_distinct_data_index() {
        // A permutation that repeats an index would show one column twice and
        // silently lose another.
        let mut o = AxisOrder::identity(12);
        o.move_span(2, 3, 9).unwrap();
        o.insert_fresh(0, 2).unwrap();
        o.remove(7, 1).unwrap();
        o.move_span(0, 1, 5).unwrap();
        let all = expand(&o);
        let mut sorted = all.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "{all:?} repeats a data index");
        // And the forward/back mapping must agree everywhere.
        for (d, &data) in all.iter().enumerate() {
            assert_eq!(o.display_of(data), Some(d as u64));
        }
    }

    // --- AxisShift: the display-position map every side table is remapped by ---

    #[test]
    fn an_insert_shifts_everything_at_or_after_it() {
        let s = AxisShift::Insert { at: 2, count: 1 };
        assert_eq!(s.map(0), Some(0));
        assert_eq!(s.map(1), Some(1));
        // The entry that WAS at 2 is now at 3 — the inserted one took its slot.
        assert_eq!(s.map(2), Some(3));
        assert_eq!(s.map(9), Some(10));
    }

    #[test]
    fn a_delete_drops_its_own_span_and_pulls_the_rest_back() {
        let s = AxisShift::Delete { at: 2, count: 2 };
        assert_eq!(s.map(1), Some(1));
        // Deleted: None, never a neighbour. Clamping to 1 or 2 here is exactly
        // the silent-wrong-data bug #REF! exists to make visible.
        assert_eq!(s.map(2), None);
        assert_eq!(s.map(3), None);
        assert_eq!(s.map(4), Some(2));
        assert_eq!(s.map(10), Some(8));
    }

    #[test]
    fn an_insert_inside_a_span_grows_it() {
        // A merge over rows 2..=5 with a row inserted at 3 must still cover
        // the same records, plus the new blank one in the middle.
        let s = AxisShift::Insert { at: 3, count: 1 };
        assert_eq!(s.map_span(2, 5), Some((2, 6)));
        // Inserted before: the whole span slides.
        assert_eq!(s.map_span(4, 6), Some((5, 7)));
        // Inserted after: untouched.
        assert_eq!(
            AxisShift::Insert { at: 9, count: 1 }.map_span(2, 5),
            Some((2, 5))
        );
        // Inserted exactly at the start pushes the span down whole, so the new
        // blank row lands OUTSIDE the merge rather than inside it.
        assert_eq!(
            AxisShift::Insert { at: 2, count: 1 }.map_span(2, 5),
            Some((3, 6))
        );
    }

    #[test]
    fn a_delete_shrinks_a_span_to_its_survivors() {
        // Overlapping the tail.
        assert_eq!(
            AxisShift::Delete { at: 4, count: 2 }.map_span(2, 5),
            Some((2, 3))
        );
        // Overlapping the head.
        assert_eq!(
            AxisShift::Delete { at: 1, count: 2 }.map_span(2, 5),
            Some((1, 3))
        );
        // Strictly inside.
        assert_eq!(
            AxisShift::Delete { at: 3, count: 1 }.map_span(2, 5),
            Some((2, 4))
        );
        // Entirely before / entirely after.
        assert_eq!(
            AxisShift::Delete { at: 8, count: 2 }.map_span(2, 5),
            Some((2, 5))
        );
        assert_eq!(
            AxisShift::Delete { at: 0, count: 1 }.map_span(2, 5),
            Some((1, 4))
        );
    }

    #[test]
    fn a_delete_that_eats_a_whole_span_removes_it() {
        // The caller must DROP the entry, not keep a rectangle over rows that
        // no longer exist — a merge left behind here would paint over live data.
        assert_eq!(AxisShift::Delete { at: 2, count: 4 }.map_span(2, 5), None);
        assert_eq!(AxisShift::Delete { at: 0, count: 9 }.map_span(2, 5), None);
        assert_eq!(AxisShift::Delete { at: 0, count: 1 }.map_span(0, 0), None);
    }

    #[test]
    fn insert_then_its_matching_delete_is_the_identity_map() {
        // Undoing an insert must put every surviving position back exactly, or
        // a side table would drift one column per undo cycle.
        let ins = AxisShift::Insert { at: 3, count: 2 };
        let del = AxisShift::Delete { at: 3, count: 2 };
        for old in 0..20u32 {
            let there = ins.map(old).unwrap();
            assert_eq!(
                del.map(there),
                Some(old),
                "position {old} did not round trip"
            );
        }
    }

    #[test]
    fn a_row_move_over_200m_rows_stays_a_handful_of_runs() {
        // Issue #17 scope item 4. A row MOVE is affordable at this size
        // precisely because it is O(runs): this is the measurement the choice
        // of option (a) over "refuse above a threshold" rests on.
        let mut o = AxisOrder::identity(200_000_000);
        let before = o.heap_bytes();

        o.move_span(5, 1, 150_000_000).unwrap();

        assert_eq!(o.len(), 200_000_000, "no rows lost");
        assert!(
            o.run_count() <= 4,
            "one row move needed {} runs",
            o.run_count()
        );
        assert!(
            o.heap_bytes() < before + 512,
            "a row move over 200M rows allocated {} bytes; a Vec<u32> \
             permutation would be 800MB",
            o.heap_bytes() - before
        );
        // Exact at both ends, so "cheap" has not been bought with wrongness.
        assert_eq!(o.data_of(149_999_999), Some(5));
        assert_eq!(o.data_of(5), Some(6));
        assert_eq!(o.data_of(199_999_999), Some(199_999_999));
    }
}
