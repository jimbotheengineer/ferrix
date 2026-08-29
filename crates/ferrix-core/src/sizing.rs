//! Row heights, column widths, hiding, and outline grouping.
//!
//! # Storage
//!
//! Everything here is keyed by **column index or by RANGE**, never per row and
//! never per cell — the same discipline `merge.rs` follows, for the same
//! reason. Hiding rows 1..200_000_000 is ONE entry. A per-row height vector
//! over a 200M-row sheet would be 800MB before the user has typed anything,
//! which is exactly the allocation the scale invariant forbids.
//!
//! Column state is stored per column (column counts are small — thousands at
//! most), row state as inclusive spans in a `BTreeMap` keyed by the span's
//! first row.
//!
//! # Height 0 means hidden
//!
//! Matching Excel, a row height of exactly zero *is* the hidden state rather
//! than a separate flag. There is then no way for the two to disagree, and no
//! second question ("is it hidden, or is it just zero-high?") for a caller to
//! get wrong. [`RowSizes::hide`] sets the height to zero and
//! [`RowSizes::is_hidden`] reads it back.
//!
//! # Resolution
//!
//! Hidden ROWS do not resolve themselves. They are folded into
//! [`HiddenRows`], a prefix-summed index that the UI's single `RowResolver`
//! composes as one more stage. Painting never asks "is this row hidden?" while
//! walking rows — it asks the resolver for the Nth visible row and gets an
//! answer in O(log spans). A second, independent hidden-row lookup consulted
//! by the painter is precisely the bug the one-resolver rule exists to
//! prevent.

use std::collections::BTreeMap;

/// Deepest outline nesting, matching Excel.
pub const MAX_OUTLINE_LEVEL: u8 = 8;

/// Why an outline group was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutlineError {
    /// The group would nest deeper than [`MAX_OUTLINE_LEVEL`].
    TooDeep,
    /// The range is empty or inverted.
    Degenerate,
    /// The range partially overlaps an existing group. Outlines must nest:
    /// a group is either wholly inside another or wholly outside it. A
    /// straddling group has no well-defined level, and collapsing it would
    /// hide half of a sibling.
    Straddles,
}

impl std::fmt::Display for OutlineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutlineError::TooDeep => {
                write!(f, "outline groups nest at most {MAX_OUTLINE_LEVEL} deep")
            }
            OutlineError::Degenerate => write!(f, "that range has no rows"),
            OutlineError::Straddles => {
                write!(f, "groups must nest — that range crosses another group")
            }
        }
    }
}

impl std::error::Error for OutlineError {}

// ------------------------------------------------------------- hidden rows --

/// A prefix-summed index over hidden row spans.
///
/// Built once per frame from whatever currently hides rows (explicit
/// zero-height spans, plus collapsed outline groups), then composed into the
/// UI's row resolver as a single stage. Costs O(spans), not O(rows): a sheet
/// with 200M rows and three hidden blocks builds three entries.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HiddenRows {
    /// Disjoint, ascending, inclusive spans.
    spans: Vec<(u32, u32)>,
    /// `prefix[i]` is how many rows the spans before `i` hide, so the count of
    /// hidden rows below any point is one binary search rather than a scan.
    prefix: Vec<u64>,
    total: u64,
}

impl HiddenRows {
    /// Build from spans in any order, with overlaps and adjacencies merged.
    pub fn from_spans(spans: impl IntoIterator<Item = (u32, u32)>) -> Self {
        let mut v: Vec<(u32, u32)> = spans.into_iter().filter(|(a, b)| a <= b).collect();
        v.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(v.len());
        for (a, b) in v {
            match merged.last_mut() {
                // Touching spans are merged too (`b + 1 == a`), so the index
                // never holds two entries where one would do and `nth_visible`
                // cannot land in a phantom gap between them.
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        let mut prefix = Vec::with_capacity(merged.len());
        let mut acc = 0u64;
        for &(a, b) in &merged {
            prefix.push(acc);
            acc += (b - a) as u64 + 1;
        }
        Self {
            spans: merged,
            prefix,
            total: acc,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Total hidden rows.
    pub fn count(&self) -> u64 {
        self.total
    }

    /// The disjoint spans, ascending.
    pub fn spans(&self) -> &[(u32, u32)] {
        &self.spans
    }

    /// Index of the span containing `row`, if any.
    fn span_of(&self, row: u32) -> Option<usize> {
        match self.spans.binary_search_by(|&(a, b)| {
            if row < a {
                std::cmp::Ordering::Greater
            } else if row > b {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        }) {
            Ok(i) => Some(i),
            Err(_) => None,
        }
    }

    pub fn is_hidden(&self, row: u32) -> bool {
        self.span_of(row).is_some()
    }

    /// How many rows strictly below `row` are hidden.
    pub fn hidden_before(&self, row: u32) -> u64 {
        // Number of spans that start below `row`.
        let i = self.spans.partition_point(|&(a, _)| a < row);
        if i == 0 {
            return 0;
        }
        let (a, b) = self.spans[i - 1];
        let before = self.prefix[i - 1];
        if row <= b {
            // Inside the span: only the part below `row` counts.
            before + (row - a) as u64
        } else {
            before + (b - a) as u64 + 1
        }
    }

    /// The `idx`-th row that is NOT hidden, counting from 0.
    ///
    /// This is the read path the paint loop takes for every visible row, so it
    /// is a walk over spans bounded by how many hidden blocks lie below the
    /// viewport — never a walk over rows.
    pub fn nth_visible(&self, idx: usize) -> usize {
        let idx = idx as u64;
        for (i, &(a, _)) in self.spans.iter().enumerate() {
            // Visible rows strictly below this span's start.
            let vis_before = a as u64 - self.prefix[i];
            if idx < vis_before {
                return (idx + self.prefix[i]) as usize;
            }
        }
        (idx + self.total) as usize
    }

    /// Visible index of `row`, or `None` when it is hidden.
    pub fn visible_index(&self, row: u32) -> Option<usize> {
        if self.is_hidden(row) {
            return None;
        }
        Some((row as u64 - self.hidden_before(row)) as usize)
    }

    /// How many of `total` rows survive hiding.
    pub fn visible_count(&self, total: usize) -> usize {
        let hidden_within = self.hidden_before(total.min(u32::MAX as usize) as u32);
        total.saturating_sub(hidden_within as usize)
    }
}

// -------------------------------------------------------------- row sizing --

/// Row heights, stored as inclusive spans.
///
/// A height of `0.0` means hidden (see the module docs). Rows with no entry
/// use the sheet default and cost nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowSizes {
    /// Keyed by first row; value is `(last row, height)`. Spans never overlap.
    spans: BTreeMap<u32, (u32, f32)>,
}

impl RowSizes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Bytes held on the heap, for the resource guard.
    pub fn heap_bytes(&self) -> usize {
        self.spans.len() * (std::mem::size_of::<u32>() * 2 + std::mem::size_of::<f32>())
    }

    /// Every span, ascending: `(first, last, height)`.
    pub fn spans(&self) -> impl Iterator<Item = (u32, u32, f32)> + '_ {
        self.spans.iter().map(|(&a, &(b, h))| (a, b, h))
    }

    /// Set the height of an inclusive row range.
    ///
    /// Overlapping spans are split rather than merged away, so setting one row
    /// inside a previously sized block leaves the rest of the block alone.
    pub fn set_range(&mut self, first: u32, last: u32, height: f32) {
        if first > last || !height.is_finite() || height < 0.0 {
            return;
        }
        self.carve(first, last);
        self.spans.insert(first, (last, height));
        self.coalesce_around(first);
    }

    pub fn set(&mut self, row: u32, height: f32) {
        self.set_range(row, row, height);
    }

    /// Remove any explicit height, returning the range to the default.
    pub fn clear_range(&mut self, first: u32, last: u32) {
        if first > last {
            return;
        }
        self.carve(first, last);
    }

    /// Punch `first..=last` out of the stored spans, keeping the parts of any
    /// straddling span that lie outside it.
    fn carve(&mut self, first: u32, last: u32) {
        let touched: Vec<(u32, u32, f32)> = self
            .spans
            .range(..=last)
            .filter(|(_, &(b, _))| b >= first)
            .map(|(&a, &(b, h))| (a, b, h))
            .collect();
        for (a, b, h) in touched {
            self.spans.remove(&a);
            if a < first {
                self.spans.insert(a, (first - 1, h));
            }
            if b > last {
                self.spans.insert(last + 1, (b, h));
            }
        }
    }

    /// Merge the span at `key` with equal-height neighbours, so repeated
    /// single-row edits over a block collapse back to one entry instead of
    /// growing the map without bound.
    fn coalesce_around(&mut self, key: u32) {
        let Some(&(mut last, h)) = self.spans.get(&key) else {
            return;
        };
        let mut first = key;
        // Forward.
        while let Some((&na, &(nb, nh))) = self.spans.range(last.saturating_add(1)..).next() {
            if na == last + 1 && nh.to_bits() == h.to_bits() {
                self.spans.remove(&na);
                last = nb;
            } else {
                break;
            }
        }
        // Backward.
        while let Some((&pa, &(pb, ph))) = self.spans.range(..first).next_back() {
            if pb + 1 == first && ph.to_bits() == h.to_bits() {
                self.spans.remove(&pa);
                first = pa;
            } else {
                break;
            }
        }
        self.spans.remove(&key);
        self.spans.insert(first, (last, h));
    }

    /// Explicit height of a row, or `None` when it uses the default.
    pub fn height_of(&self, row: u32) -> Option<f32> {
        self.spans
            .range(..=row)
            .next_back()
            .filter(|(_, &(b, _))| b >= row)
            .map(|(_, &(_, h))| h)
    }

    /// Height a row paints at, given the sheet default.
    pub fn height_or(&self, row: u32, default: f32) -> f32 {
        self.height_of(row).unwrap_or(default)
    }

    /// Excel's rule: a zero height IS the hidden state.
    pub fn is_hidden(&self, row: u32) -> bool {
        self.height_of(row) == Some(0.0)
    }

    /// Hide an inclusive range by setting its height to zero.
    pub fn hide(&mut self, first: u32, last: u32) {
        self.set_range(first, last, 0.0);
    }

    /// Unhide a range: zero-height spans inside it are removed, and any
    /// explicit non-zero height is left untouched.
    pub fn unhide(&mut self, first: u32, last: u32) {
        let hidden: Vec<(u32, u32)> = self
            .spans
            .iter()
            .filter(|(_, &(_, h))| h == 0.0)
            .map(|(&a, &(b, _))| (a, b))
            .filter(|&(a, b)| a <= last && b >= first)
            .collect();
        for (a, b) in hidden {
            self.clear_range(a.max(first), b.min(last));
        }
    }

    /// The hidden spans, for folding into [`HiddenRows`].
    pub fn hidden_spans(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.spans
            .iter()
            .filter(|(_, &(_, h))| h == 0.0)
            .map(|(&a, &(b, _))| (a, b))
    }
}

// ----------------------------------------------------------- column sizing --

/// Column widths and hidden columns.
///
/// Per column rather than per range: column counts are small (thousands at
/// most), and the UI already carries a dense width vector, so a map keyed by
/// column is both simpler and no larger in practice.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColSizes {
    widths: BTreeMap<u32, f32>,
    hidden: BTreeMap<u32, ()>,
}

impl ColSizes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.widths.is_empty() && self.hidden.is_empty()
    }

    pub fn heap_bytes(&self) -> usize {
        (self.widths.len() + self.hidden.len()) * (std::mem::size_of::<u32>() * 2)
    }

    pub fn set_width(&mut self, col: u32, w: f32) {
        if w.is_finite() && w > 0.0 {
            self.widths.insert(col, w);
        }
    }

    pub fn width_of(&self, col: u32) -> Option<f32> {
        self.widths.get(&col).copied()
    }

    pub fn clear_width(&mut self, col: u32) {
        self.widths.remove(&col);
    }

    pub fn widths(&self) -> impl Iterator<Item = (u32, f32)> + '_ {
        self.widths.iter().map(|(&c, &w)| (c, w))
    }

    /// Hidden columns keep their width, so unhiding restores the size the
    /// user chose rather than snapping back to the default.
    pub fn hide(&mut self, col: u32) {
        self.hidden.insert(col, ());
    }

    pub fn unhide(&mut self, col: u32) {
        self.hidden.remove(&col);
    }

    pub fn toggle_hidden(&mut self, col: u32) {
        if self.is_hidden(col) {
            self.unhide(col);
        } else {
            self.hide(col);
        }
    }

    pub fn is_hidden(&self, col: u32) -> bool {
        self.hidden.contains_key(&col)
    }

    pub fn hidden_cols(&self) -> impl Iterator<Item = u32> + '_ {
        self.hidden.keys().copied()
    }

    pub fn hidden_count(&self) -> usize {
        self.hidden.len()
    }

    /// Unhide every column — the "unhide all" the context menu offers when the
    /// user cannot select a column they cannot see.
    pub fn unhide_all(&mut self) {
        self.hidden.clear();
    }
}

// --------------------------------------------------------------- outlining --

/// One outline group over an inclusive range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutlineGroup {
    pub first: u32,
    pub last: u32,
    /// 1-based nesting depth, at most [`MAX_OUTLINE_LEVEL`].
    pub level: u8,
    pub collapsed: bool,
}

impl OutlineGroup {
    pub fn contains(&self, idx: u32) -> bool {
        idx >= self.first && idx <= self.last
    }

    /// Whether `other` lies wholly inside this group.
    fn encloses(&self, other: &OutlineGroup) -> bool {
        self.first <= other.first && self.last >= other.last
    }
}

/// The outline groups of one axis, stored as RANGES.
///
/// Grouping rows 0..200_000_000 is one entry, and collapsing it hides that
/// whole span through [`HiddenRows`] without materializing a row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outline {
    /// Ascending by `first`, then by descending span so an enclosing group
    /// always precedes the groups it contains.
    groups: Vec<OutlineGroup>,
}

impl Outline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn len(&self) -> usize {
        self.groups.len()
    }

    pub fn heap_bytes(&self) -> usize {
        self.groups.capacity() * std::mem::size_of::<OutlineGroup>()
    }

    pub fn groups(&self) -> &[OutlineGroup] {
        &self.groups
    }

    /// Add a group over `first..=last`.
    ///
    /// The level is derived from how many existing groups enclose the range,
    /// so nesting is a consequence of the ranges rather than a number the
    /// caller has to keep consistent.
    pub fn group(&mut self, first: u32, last: u32) -> Result<u8, OutlineError> {
        if first > last {
            return Err(OutlineError::Degenerate);
        }
        let candidate = OutlineGroup {
            first,
            last,
            level: 1,
            collapsed: false,
        };
        let mut depth = 0u8;
        for g in &self.groups {
            if g.encloses(&candidate) {
                depth += 1;
            } else if candidate.encloses(g) {
                // Fine: the new group wraps an existing one.
            } else if g.first <= last && g.last >= first {
                // Partial overlap — no well-defined nesting.
                return Err(OutlineError::Straddles);
            }
        }
        let level = depth + 1;
        if level > MAX_OUTLINE_LEVEL {
            return Err(OutlineError::TooDeep);
        }
        // Re-level anything this group now encloses, so wrapping an existing
        // group pushes it (and its children) one level deeper rather than
        // leaving two groups claiming the same depth.
        for g in &mut self.groups {
            if candidate.encloses(g) {
                g.level = g.level.saturating_add(1);
            }
        }
        if self.groups.iter().any(|g| g.level > MAX_OUTLINE_LEVEL) {
            // Undo the re-level: the wrap would push a child past the limit.
            for g in &mut self.groups {
                if candidate.encloses(g) {
                    g.level -= 1;
                }
            }
            return Err(OutlineError::TooDeep);
        }
        self.groups.push(OutlineGroup { level, ..candidate });
        self.sort();
        Ok(level)
    }

    fn sort(&mut self) {
        self.groups
            .sort_by(|a, b| a.first.cmp(&b.first).then(b.last.cmp(&a.last)));
    }

    /// Remove the innermost group containing `idx`.
    pub fn ungroup_at(&mut self, idx: u32) -> Option<OutlineGroup> {
        let pos = self
            .groups
            .iter()
            .enumerate()
            .filter(|(_, g)| g.contains(idx))
            .max_by_key(|(_, g)| g.level)
            .map(|(i, _)| i)?;
        let removed = self.groups.remove(pos);
        for g in &mut self.groups {
            if removed.encloses(g) && g.level > 1 {
                g.level -= 1;
            }
        }
        Some(removed)
    }

    /// Deepest level covering `idx`, or 0 when nothing groups it. This is what
    /// the gutter draws its indentation from.
    pub fn level_at(&self, idx: u32) -> u8 {
        self.groups
            .iter()
            .filter(|g| g.contains(idx))
            .map(|g| g.level)
            .max()
            .unwrap_or(0)
    }

    /// The deepest level any group reaches.
    pub fn max_level(&self) -> u8 {
        self.groups.iter().map(|g| g.level).max().unwrap_or(0)
    }

    /// Innermost group starting exactly at `idx` — the one whose toggle button
    /// the gutter paints on that row.
    pub fn group_starting_at(&self, idx: u32) -> Option<&OutlineGroup> {
        self.groups
            .iter()
            .filter(|g| g.first == idx)
            .max_by_key(|g| g.level)
    }

    /// Collapse or expand the innermost group starting at `idx`.
    /// Returns the new collapsed state.
    pub fn toggle_at(&mut self, idx: u32) -> Option<bool> {
        let pos = self
            .groups
            .iter()
            .enumerate()
            .filter(|(_, g)| g.first == idx)
            .max_by_key(|(_, g)| g.level)
            .map(|(i, _)| i)?;
        self.groups[pos].collapsed = !self.groups[pos].collapsed;
        Some(self.groups[pos].collapsed)
    }

    pub fn set_collapsed(&mut self, first: u32, collapsed: bool) -> Option<bool> {
        let g = self.groups.iter_mut().find(|g| g.first == first)?;
        g.collapsed = collapsed;
        Some(collapsed)
    }

    /// Collapse every group at `level` or deeper — the "show levels 1..n"
    /// buttons at the top of the gutter.
    pub fn collapse_to_level(&mut self, level: u8) {
        for g in &mut self.groups {
            g.collapsed = g.level > level;
        }
    }

    /// Spans hidden because a group is collapsed.
    ///
    /// The group's FIRST index stays visible — it is the summary row that
    /// carries the expand button, and hiding it would leave the user no way
    /// to get the rows back.
    pub fn collapsed_spans(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.groups
            .iter()
            .filter(|g| g.collapsed && g.last > g.first)
            .map(|g| (g.first + 1, g.last))
    }
}

// ----------------------------------------------------------- sheet sizing ---

/// All sizing state for one sheet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SheetSizing {
    pub rows: RowSizes,
    pub cols: ColSizes,
    pub row_outline: Outline,
    pub col_outline: Outline,
}

impl SheetSizing {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
            && self.cols.is_empty()
            && self.row_outline.is_empty()
            && self.col_outline.is_empty()
    }

    pub fn heap_bytes(&self) -> usize {
        self.rows.heap_bytes()
            + self.cols.heap_bytes()
            + self.row_outline.heap_bytes()
            + self.col_outline.heap_bytes()
    }

    /// Everything that hides a row right now, as ONE index.
    ///
    /// Explicit zero-height spans and collapsed outline groups are folded
    /// together here, so the resolver composes a single stage and there is no
    /// way for two hiding mechanisms to disagree about a row.
    pub fn hidden_rows(&self) -> HiddenRows {
        HiddenRows::from_spans(
            self.rows
                .hidden_spans()
                .chain(self.row_outline.collapsed_spans()),
        )
    }

    /// Columns hidden explicitly or by a collapsed column group.
    pub fn hidden_col_set(&self) -> std::collections::BTreeSet<u32> {
        let mut s: std::collections::BTreeSet<u32> = self.cols.hidden_cols().collect();
        for (a, b) in self.col_outline.collapsed_spans() {
            for c in a..=b {
                s.insert(c);
            }
        }
        s
    }
}

// ------------------------------------------------------------------ tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn height_zero_is_hidden() {
        let mut r = RowSizes::new();
        r.set(4, 0.0);
        assert!(r.is_hidden(4), "Excel treats a zero height as hidden");
        assert!(!r.is_hidden(3));
        r.set(4, 22.0);
        assert!(!r.is_hidden(4), "a non-zero height must un-hide the row");
    }

    #[test]
    fn hide_then_unhide_restores_default() {
        let mut r = RowSizes::new();
        r.hide(10, 20);
        assert!(r.is_hidden(15));
        r.unhide(10, 20);
        assert!(!r.is_hidden(15));
        assert_eq!(r.height_of(15), None);
    }

    #[test]
    fn spans_are_range_keyed_not_per_row() {
        let mut r = RowSizes::new();
        r.hide(0, 199_999_999);
        assert_eq!(r.len(), 1, "200M hidden rows must cost ONE span");
        assert!(r.heap_bytes() < 128, "heap {} too big", r.heap_bytes());
        assert!(r.is_hidden(123_456_789));
    }

    #[test]
    fn setting_inside_a_span_splits_it() {
        let mut r = RowSizes::new();
        r.set_range(0, 100, 30.0);
        r.set(50, 0.0);
        assert_eq!(r.height_of(49), Some(30.0));
        assert!(r.is_hidden(50));
        assert_eq!(r.height_of(51), Some(30.0));
    }

    #[test]
    fn equal_neighbours_coalesce() {
        let mut r = RowSizes::new();
        for row in 0..64 {
            r.set(row, 40.0);
        }
        assert_eq!(
            r.len(),
            1,
            "64 equal single-row heights must collapse to one span, not 64"
        );
        assert_eq!(r.height_of(63), Some(40.0));
    }

    // --- hidden row index ---

    #[test]
    fn hidden_index_maps_visible_to_underlying() {
        // Hide rows 2,3,4 and 7.
        let h = HiddenRows::from_spans([(2, 4), (7, 7)]);
        assert_eq!(h.count(), 4);
        // Visible: 0,1,5,6,8,9,...
        let got: Vec<usize> = (0..6).map(|i| h.nth_visible(i)).collect();
        assert_eq!(got, vec![0, 1, 5, 6, 8, 9]);
    }

    #[test]
    fn visible_index_is_the_exact_inverse() {
        let h = HiddenRows::from_spans([(2, 4), (7, 7), (11, 13)]);
        for i in 0..40 {
            let row = h.nth_visible(i);
            assert_eq!(
                h.visible_index(row as u32),
                Some(i),
                "nth_visible({i}) = {row} did not invert"
            );
        }
        for hidden in [2, 3, 4, 7, 11, 12, 13] {
            assert_eq!(
                h.visible_index(hidden),
                None,
                "row {hidden} is hidden and must not resolve"
            );
        }
    }

    #[test]
    fn hidden_before_counts_correctly_inside_a_span() {
        let h = HiddenRows::from_spans([(10, 19)]);
        assert_eq!(h.hidden_before(10), 0);
        assert_eq!(h.hidden_before(15), 5);
        assert_eq!(h.hidden_before(20), 10);
        assert_eq!(h.hidden_before(100), 10);
    }

    #[test]
    fn overlapping_and_touching_spans_merge() {
        let h = HiddenRows::from_spans([(5, 9), (3, 6), (10, 12)]);
        assert_eq!(h.spans(), &[(3, 12)]);
        assert_eq!(h.count(), 10);
    }

    #[test]
    fn empty_index_is_identity() {
        let h = HiddenRows::default();
        assert!(h.is_empty());
        for i in 0..10 {
            assert_eq!(h.nth_visible(i), i);
            assert_eq!(h.visible_index(i as u32), Some(i));
        }
    }

    #[test]
    fn hiding_a_huge_span_is_constant_work() {
        // The invariant that matters: 200M hidden rows resolve without
        // touching a row.
        let h = HiddenRows::from_spans([(1, 200_000_000)]);
        assert_eq!(h.nth_visible(0), 0);
        assert_eq!(
            h.nth_visible(1),
            200_000_001,
            "past a 200M hidden span the next visible row is the one after it"
        );
        assert_eq!(h.count(), 200_000_000);
    }

    // --- outline ---

    #[test]
    fn nesting_derives_levels_from_containment() {
        let mut o = Outline::new();
        assert_eq!(o.group(0, 99).unwrap(), 1);
        assert_eq!(o.group(10, 49).unwrap(), 2);
        assert_eq!(o.group(20, 29).unwrap(), 3);
        assert_eq!(o.level_at(25), 3);
        assert_eq!(o.level_at(15), 2);
        assert_eq!(o.level_at(60), 1);
        assert_eq!(o.level_at(200), 0);
    }

    #[test]
    fn eight_levels_allowed_ninth_refused() {
        let mut o = Outline::new();
        for i in 0..MAX_OUTLINE_LEVEL as u32 {
            let level = o
                .group(i, 1000 - i)
                .unwrap_or_else(|e| panic!("level {} refused: {e}", i + 1));
            assert_eq!(level, i as u8 + 1);
        }
        assert_eq!(o.max_level(), MAX_OUTLINE_LEVEL);
        assert_eq!(
            o.group(9, 900),
            Err(OutlineError::TooDeep),
            "a 9th level must be refused"
        );
    }

    #[test]
    fn straddling_groups_are_refused() {
        let mut o = Outline::new();
        o.group(10, 20).unwrap();
        assert_eq!(o.group(15, 25), Err(OutlineError::Straddles));
        assert_eq!(o.group(5, 15), Err(OutlineError::Straddles));
        // Disjoint and nested are both fine.
        assert!(o.group(30, 40).is_ok());
        assert!(o.group(12, 18).is_ok());
    }

    #[test]
    fn wrapping_an_existing_group_pushes_it_deeper() {
        let mut o = Outline::new();
        o.group(10, 20).unwrap();
        assert_eq!(o.level_at(15), 1);
        o.group(0, 99).unwrap();
        assert_eq!(o.level_at(15), 2, "the inner group must be re-levelled");
        assert_eq!(o.level_at(50), 1);
    }

    #[test]
    fn collapsing_hides_all_but_the_summary_row() {
        let mut o = Outline::new();
        o.group(10, 20).unwrap();
        assert_eq!(o.collapsed_spans().count(), 0);
        assert_eq!(o.toggle_at(10), Some(true));
        let spans: Vec<_> = o.collapsed_spans().collect();
        assert_eq!(
            spans,
            vec![(11, 20)],
            "row 10 carries the expand button and must stay visible"
        );
        assert_eq!(o.toggle_at(10), Some(false));
        assert_eq!(o.collapsed_spans().count(), 0);
    }

    #[test]
    fn ungrouping_relevels_children() {
        let mut o = Outline::new();
        o.group(0, 99).unwrap();
        o.group(10, 20).unwrap();
        assert_eq!(o.level_at(15), 2);
        o.ungroup_at(50).unwrap(); // innermost at 50 is the outer group
        assert_eq!(o.level_at(15), 1, "the child must move up a level");
        assert_eq!(o.len(), 1);
    }

    #[test]
    fn ungroup_removes_the_innermost() {
        let mut o = Outline::new();
        o.group(0, 99).unwrap();
        o.group(10, 20).unwrap();
        let removed = o.ungroup_at(15).unwrap();
        assert_eq!((removed.first, removed.last), (10, 20));
        assert_eq!(o.level_at(15), 1);
    }

    #[test]
    fn collapse_to_level_folds_deeper_groups() {
        let mut o = Outline::new();
        o.group(0, 99).unwrap();
        o.group(10, 49).unwrap();
        o.group(20, 29).unwrap();
        o.collapse_to_level(1);
        let collapsed = o.groups().iter().filter(|g| g.collapsed).count();
        assert_eq!(collapsed, 2, "levels 2 and 3 fold, level 1 stays open");
    }

    // --- composition ---

    #[test]
    fn sizing_folds_zero_heights_and_collapses_into_one_index() {
        let mut s = SheetSizing::new();
        s.rows.hide(3, 3);
        s.row_outline.group(10, 20).unwrap();
        s.row_outline.toggle_at(10);
        let h = s.hidden_rows();
        assert!(h.is_hidden(3), "explicit hide must be in the index");
        assert!(h.is_hidden(15), "collapsed group must be in the SAME index");
        assert!(!h.is_hidden(10), "the summary row stays visible");
        assert_eq!(h.count(), 1 + 10);
    }

    #[test]
    fn column_hide_keeps_width_for_unhide() {
        let mut c = ColSizes::new();
        c.set_width(2, 250.0);
        c.hide(2);
        assert!(c.is_hidden(2));
        assert_eq!(
            c.width_of(2),
            Some(250.0),
            "unhiding must restore the chosen width, not the default"
        );
        c.unhide(2);
        assert!(!c.is_hidden(2));
        assert_eq!(c.width_of(2), Some(250.0));
    }

    #[test]
    fn collapsed_column_group_hides_columns() {
        let mut s = SheetSizing::new();
        s.col_outline.group(2, 5).unwrap();
        s.col_outline.toggle_at(2);
        let hidden = s.hidden_col_set();
        assert!(!hidden.contains(&2), "summary column stays");
        assert_eq!(hidden.iter().copied().collect::<Vec<_>>(), vec![3, 4, 5]);
    }
}
