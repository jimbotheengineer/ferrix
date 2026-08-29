//! Sparklines: a tiny chart drawn INSIDE a cell (issue #36).
//!
//! ## Why this is not a chart object
//!
//! A chart is an object: it owns a canvas, a series cache, an axis, and it
//! exists whether or not anyone is looking at it. One chart per row over a
//! 200M-row column is 200M objects, which is not a slow feature, it is an
//! impossible one.
//!
//! A sparkline here is instead a **rule**, exactly like a conditional format:
//! a small [`SparkGroup`] describing "for every row in this destination range,
//! plot that row's cells from these source columns". The picture only exists
//! for the duration of one paint call, for the rows that are on screen. A
//! group over 200M rows and a group over 20 are the same handful of bytes, and
//! painting a screenful costs the same in both.
//!
//! `one_group_covers_a_200m_row_column` and `heap_bytes_does_not_grow_with_rows`
//! pin that.
//!
//! ## Source range PER ROW
//!
//! A group stores a span of SOURCE COLUMNS, not a source range per row. Row
//! `r`'s series is `src_first_col..=src_last_col` read from row `r` — the same
//! relative shape for every row, which is why the storage is O(1). This is
//! also exactly what Excel's `<x14:sparklineGroup>` degenerates to when its
//! per-cell `<xm:f>` entries march down a column in lockstep, which is what
//! makes the xlsx round trip in `ferrix_io::sparkline_xlsx` possible at all.
//!
//! ## Geometry is shared with `chart.rs`, not re-derived
//!
//! Extents come from [`Bounds`] and reduction comes from [`decimate_min_max`],
//! the same two pieces the full charts use. A sparkline is ~60 pixels wide, so
//! decimation matters MORE here than on a big canvas: a 5,000-cell source row
//! reduced by sampling would drop the spike the user is scanning for, and
//! min/max decimation does not. Nothing in this module computes a minimum, a
//! maximum or a bucket boundary of its own.

use crate::chart::{decimate_min_max, Bounds, DataPoint};
use crate::table::TableRange;
use crate::CellRef;

/// The three sparkline types issue #36 asks for, and the three Excel has.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SparkKind {
    /// A polyline through the values. The default, and Excel's.
    #[default]
    Line,
    /// One bar per value, from the baseline.
    Column,
    /// One equal-height bar per value, up for positive and down for negative.
    /// Magnitude is deliberately discarded — the question is only "did this
    /// period win or lose", and drawing magnitude would make it a Column.
    WinLoss,
}

impl SparkKind {
    pub const ALL: [SparkKind; 3] = [SparkKind::Line, SparkKind::Column, SparkKind::WinLoss];

    pub fn label(self) -> &'static str {
        match self {
            SparkKind::Line => "line",
            SparkKind::Column => "column",
            SparkKind::WinLoss => "win/loss",
        }
    }
}

/// One sparkline group: the whole configuration for any number of rows.
///
/// `target` is where the pictures are DRAWN; the source columns are read from
/// the target cell's own row. Both are plain integers, so a group is 24 bytes
/// however many rows it covers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SparkGroup {
    pub kind: SparkKind,
    /// Destination cells. Usually one column tall enough to cover the data.
    pub target: TableRange,
    /// First source column, read from the destination cell's row.
    pub src_first_col: u32,
    /// Last source column, inclusive.
    pub src_last_col: u32,
}

impl SparkGroup {
    pub fn new(kind: SparkKind, target: TableRange, src_first_col: u32, src_last_col: u32) -> Self {
        Self {
            kind,
            target,
            src_first_col: src_first_col.min(src_last_col),
            src_last_col: src_first_col.max(src_last_col),
        }
    }

    /// How many source cells one row contributes. Bounded by the source span,
    /// never by the row count.
    #[inline]
    pub fn source_len(&self) -> usize {
        (self.src_last_col - self.src_first_col) as usize + 1
    }

    /// The source cells for one row, as a half-open column range.
    ///
    /// Returns `None` for a row the group does not cover, so a caller cannot
    /// accidentally read a source row for a cell outside the target.
    #[inline]
    pub fn source_cols(&self, row: u32) -> Option<std::ops::RangeInclusive<u32>> {
        (row >= self.target.first_row && row <= self.target.last_row)
            .then_some(self.src_first_col..=self.src_last_col)
    }

    /// The source range for one row as an explicit rectangle, for export.
    pub fn source_range(&self, row: u32) -> TableRange {
        TableRange::new(row, self.src_first_col, row, self.src_last_col)
    }

    /// Does the source span overlap the destination?
    ///
    /// A sparkline whose source includes its own cell is a cycle in the
    /// obvious reading and, more practically, draws a picture of itself. The
    /// UI refuses one rather than painting nonsense.
    pub fn self_referential(&self) -> bool {
        self.target.first_col <= self.src_last_col && self.target.last_col >= self.src_first_col
    }
}

/// Every sparkline group on a sheet.
///
/// A `Vec` rather than a map keyed by cell, for the same reason
/// `SheetFormat::ranges` is: the key is a RECTANGLE, and there are a handful of
/// them. Later entries win, matching the conditional-formatting precedence
/// rule so a user who has learned one has learned both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SparklineMap {
    groups: Vec<SparkGroup>,
}

impl SparklineMap {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// How many GROUPS are configured. Never a count of cells: this is the
    /// number the scale test asserts stays at 1 after covering 200M rows.
    #[inline]
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Bytes of heap this store owns. A function of group count only.
    pub fn heap_bytes(&self) -> usize {
        self.groups.capacity() * std::mem::size_of::<SparkGroup>()
    }

    pub fn iter(&self) -> impl Iterator<Item = &SparkGroup> {
        self.groups.iter()
    }

    /// Add a group, replacing any existing group with the same target so
    /// re-applying a different type to the same cells swaps it rather than
    /// stacking an invisible one underneath.
    pub fn add(&mut self, group: SparkGroup) {
        self.groups.retain(|g| g.target != group.target);
        self.groups.push(group);
    }

    /// The group that draws in `cell`, if any. Later entries win.
    ///
    /// A linear scan over the GROUP list, which is the handful of rectangles
    /// the user configured — not over rows, and not over cells.
    #[inline]
    pub fn group_at(&self, cell: CellRef) -> Option<&SparkGroup> {
        self.groups.iter().rev().find(|g| g.target.contains(cell))
    }

    /// Does any group draw in this row? Hoisted out of the paint loop's column
    /// loop so an unsparklined sheet costs one `is_empty` per row.
    #[inline]
    pub fn covers_row(&self, row: u32) -> bool {
        self.groups
            .iter()
            .any(|g| row >= g.target.first_row && row <= g.target.last_row)
    }

    /// Remove every group drawing inside `range`. Returns how many went.
    pub fn clear_in(&mut self, range: TableRange) -> usize {
        let before = self.groups.len();
        self.groups.retain(|g| {
            !(g.target.first_row <= range.last_row
                && g.target.last_row >= range.first_row
                && g.target.first_col <= range.last_col
                && g.target.last_col >= range.first_col)
        });
        before - self.groups.len()
    }
}

// ================================================================ geometry ==

/// One bar of a column or win/loss sparkline, in normalised cell space.
///
/// `x0`/`x1` are 0..=1 across the cell's width; `lo`/`hi` are 0..=1 up from
/// its bottom. Normalised rather than in pixels so this module stays free of
/// any UI dependency and the same geometry is testable without a screen.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SparkBar {
    pub x0: f64,
    pub x1: f64,
    pub lo: f64,
    pub hi: f64,
    /// Below the baseline. Drawn in the negative colour.
    pub negative: bool,
}

/// A sparkline reduced to drawable geometry, in normalised cell space.
#[derive(Clone, PartialEq, Debug)]
pub enum SparkShape {
    /// Polyline through the points, x left-to-right, y up from the bottom.
    Line(Vec<DataPoint>),
    /// Bars, left to right.
    Bars(Vec<SparkBar>),
}

impl SparkShape {
    /// How many primitives this shape draws. The number a paint-count test
    /// compares against, so "the picture got simpler" is visible rather than
    /// hidden inside a total.
    pub fn primitive_count(&self) -> usize {
        match self {
            // A polyline of n points is n-1 segments; a single point draws a
            // dot so it is never invisible.
            SparkShape::Line(p) => p.len().max(1) - 1 + usize::from(p.len() == 1),
            SparkShape::Bars(b) => b.len(),
        }
    }
}

/// Widest sparkline we ever ask for, in "buckets".
///
/// A sparkline is tens of pixels wide, so there is no point emitting more
/// points than that. This is what bounds the work for a row whose source span
/// is thousands of columns wide.
pub const MAX_BUCKETS: usize = 64;

/// Reduce one row's source values to drawable geometry.
///
/// Returns `None` when there is nothing to draw — an empty source, or one
/// holding no numbers at all. That is the acceptance criterion "an empty or
/// non-numeric source draws nothing rather than erroring": the absence of a
/// picture is the correct rendering of an absence of data, and an error marker
/// in the cell would be a lie about a sheet that is merely still being filled
/// in.
///
/// `width_px` sizes the reduction, so a narrow column asks for fewer points.
pub fn sparkline_shape(
    kind: SparkKind,
    values: &[Option<f64>],
    width_px: f32,
) -> Option<SparkShape> {
    if values.is_empty() {
        return None;
    }
    // Nothing numeric: draw nothing. Checked before any geometry so a text
    // column costs one scan of a viewport-sized slice and no allocation.
    if !values.iter().any(|v| v.is_some_and(f64::is_finite)) {
        return None;
    }

    // Buckets track the CANVAS, exactly as they do for a full chart. One
    // bucket per ~2px is the point past which min/max pairs stop being
    // distinguishable to the eye.
    let buckets = ((width_px.max(2.0) / 2.0) as usize).clamp(2, MAX_BUCKETS);
    let series = decimate_min_max(values, buckets);
    if series.points.is_empty() {
        return None;
    }

    // THE extent, from `chart::Bounds` — not recomputed here. `padded` is what
    // keeps a row of identical values from dividing by zero and instead draws
    // it as the flat line it is.
    let mut b = Bounds::unbounded();
    for p in &series.points {
        b.include(p.y);
    }
    if b.is_empty() {
        return None;
    }

    match kind {
        SparkKind::Line => Some(SparkShape::Line(line_points(&series.points, b))),
        SparkKind::Column => Some(SparkShape::Bars(column_bars(&series.points, b))),
        SparkKind::WinLoss => Some(SparkShape::Bars(win_loss_bars(&series.points))),
    }
}

/// Normalise x by the SOURCE INDEX span and y by the data bounds.
///
/// x uses the points' own first/last index rather than `0..n`, so a series
/// whose only numbers sit in the middle of the row still fills the cell
/// instead of being squeezed into a sliver.
fn line_points(points: &[DataPoint], b: Bounds) -> Vec<DataPoint> {
    let b = b.padded();
    let span = b.span();
    let (x_lo, x_hi) = x_extent(points);
    let x_span = (x_hi - x_lo).max(1.0);
    points
        .iter()
        .map(|p| {
            DataPoint::new(
                if points.len() == 1 {
                    0.5
                } else {
                    (p.x - x_lo) / x_span
                },
                ((p.y - b.min) / span).clamp(0.0, 1.0),
            )
        })
        .collect()
}

fn x_extent(points: &[DataPoint]) -> (f64, f64) {
    let lo = points.first().map_or(0.0, |p| p.x);
    let hi = points.last().map_or(0.0, |p| p.x);
    (lo, hi)
}

/// The baseline a column sparkline grows from.
///
/// Zero when the data straddles it, so positive and negative bars point in
/// opposite directions the way a reader expects. Otherwise the nearer bound,
/// so a column of values between 900 and 1000 shows the variation rather than
/// ten indistinguishable full-height bars — which is the entire point of the
/// picture.
fn baseline(b: Bounds) -> f64 {
    if b.min <= 0.0 && b.max >= 0.0 {
        0.0
    } else if b.min > 0.0 {
        b.min
    } else {
        b.max
    }
}

fn column_bars(points: &[DataPoint], b: Bounds) -> Vec<SparkBar> {
    let base = baseline(b);
    // The drawn range must contain the baseline, or a bar has nowhere to
    // start. `padded` then keeps a constant series visible.
    let mut ext = b;
    ext.include(base);
    let ext = ext.padded();
    let span = ext.span();
    let base_n = ((base - ext.min) / span).clamp(0.0, 1.0);

    let slots = points.len().max(1);
    let w = 1.0 / slots as f64;
    points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let y = ((p.y - ext.min) / span).clamp(0.0, 1.0);
            SparkBar {
                x0: i as f64 * w + w * 0.15,
                x1: (i + 1) as f64 * w - w * 0.15,
                lo: y.min(base_n),
                hi: y.max(base_n),
                negative: p.y < base,
            }
        })
        .collect()
}

/// Win/loss: equal-height bars, up for positive and down for negative.
///
/// Zeroes are drawn as nothing rather than as a flat bar: "no change" and "no
/// data" look the same on a win/loss chart in Excel too, and inventing a
/// visible mark for zero would make a flat month look like a win.
fn win_loss_bars(points: &[DataPoint]) -> Vec<SparkBar> {
    let slots = points.len().max(1);
    let w = 1.0 / slots as f64;
    points
        .iter()
        .enumerate()
        .filter(|(_, p)| p.y != 0.0 && p.y.is_finite())
        .map(|(i, p)| {
            let neg = p.y < 0.0;
            SparkBar {
                x0: i as f64 * w + w * 0.15,
                x1: (i + 1) as f64 * w - w * 0.15,
                lo: if neg { 0.1 } else { 0.5 },
                hi: if neg { 0.5 } else { 0.9 },
                negative: neg,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nums(v: &[f64]) -> Vec<Option<f64>> {
        v.iter().map(|x| Some(*x)).collect()
    }

    fn group(kind: SparkKind, rows: (u32, u32)) -> SparkGroup {
        SparkGroup::new(kind, TableRange::new(rows.0, 5, rows.1, 5), 0, 4)
    }

    // ---- storage discipline: per range, never per cell ----

    #[test]
    fn one_group_covers_a_200m_row_column() {
        let mut m = SparklineMap::new();
        m.add(group(SparkKind::Line, (0, 199_999_999)));
        assert_eq!(m.len(), 1, "a 200M-row group must be ONE entry");
        assert!(m.group_at(CellRef::new(0, 5)).is_some());
        assert!(m.group_at(CellRef::new(199_999_999, 5)).is_some());
        assert!(
            m.group_at(CellRef::new(0, 4)).is_none(),
            "a cell outside the target column is not sparklined"
        );
    }

    #[test]
    fn heap_bytes_does_not_grow_with_rows() {
        let mut small = SparklineMap::new();
        small.add(group(SparkKind::Column, (0, 9)));
        let mut huge = SparklineMap::new();
        huge.add(group(SparkKind::Column, (0, 199_999_999)));
        assert_eq!(
            small.heap_bytes(),
            huge.heap_bytes(),
            "storage must be a function of group count, not row count"
        );
    }

    #[test]
    fn re_adding_the_same_target_replaces_rather_than_stacks() {
        let mut m = SparklineMap::new();
        m.add(group(SparkKind::Line, (0, 9)));
        m.add(group(SparkKind::Column, (0, 9)));
        assert_eq!(m.len(), 1);
        assert_eq!(
            m.group_at(CellRef::new(3, 5)).unwrap().kind,
            SparkKind::Column
        );
    }

    #[test]
    fn clear_removes_only_overlapping_groups() {
        let mut m = SparklineMap::new();
        m.add(SparkGroup::new(
            SparkKind::Line,
            TableRange::new(0, 5, 9, 5),
            0,
            4,
        ));
        m.add(SparkGroup::new(
            SparkKind::Line,
            TableRange::new(0, 7, 9, 7),
            0,
            4,
        ));
        assert_eq!(m.clear_in(TableRange::new(0, 7, 0, 7)), 1);
        assert_eq!(m.len(), 1);
        assert!(
            m.group_at(CellRef::new(0, 5)).is_some(),
            "the untouched group survives"
        );
    }

    #[test]
    fn a_source_overlapping_the_target_is_flagged() {
        let g = SparkGroup::new(SparkKind::Line, TableRange::new(0, 2, 9, 2), 0, 4);
        assert!(
            g.self_referential(),
            "target column 2 is inside source 0..=4"
        );
        assert!(!group(SparkKind::Line, (0, 9)).self_referential());
    }

    #[test]
    fn source_cols_are_only_offered_for_covered_rows() {
        let g = group(SparkKind::Line, (10, 20));
        assert!(g.source_cols(9).is_none());
        assert_eq!(g.source_cols(15).unwrap(), 0..=4);
        assert!(g.source_cols(21).is_none());
        assert_eq!(g.source_len(), 5);
    }

    // ---- geometry: draws nothing rather than erroring ----

    #[test]
    fn an_empty_source_draws_nothing() {
        assert!(sparkline_shape(SparkKind::Line, &[], 60.0).is_none());
        for k in SparkKind::ALL {
            assert!(
                sparkline_shape(k, &[None, None, None], 60.0).is_none(),
                "{k:?}: an all-empty source must draw nothing"
            );
        }
    }

    #[test]
    fn a_non_numeric_source_draws_nothing() {
        // What `numeric_column` produces for text cells: all gaps.
        let vals: Vec<Option<f64>> = vec![None; 8];
        for k in SparkKind::ALL {
            assert!(
                sparkline_shape(k, &vals, 60.0).is_none(),
                "{k:?}: text must draw nothing, not an error marker"
            );
        }
    }

    #[test]
    fn a_partly_numeric_source_still_draws_its_numbers() {
        let vals = vec![None, Some(1.0), None, Some(3.0)];
        let s = sparkline_shape(SparkKind::Line, &vals, 60.0).expect("two numbers are drawable");
        match s {
            SparkShape::Line(p) => assert_eq!(p.len(), 2, "gaps are skipped, not zeroed"),
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn geometry_stays_inside_the_cell() {
        for k in SparkKind::ALL {
            let s = sparkline_shape(k, &nums(&[-5.0, 3.0, 0.0, 9.0, -2.0]), 60.0).unwrap();
            match s {
                SparkShape::Line(pts) => {
                    for p in pts {
                        assert!(
                            (0.0..=1.0).contains(&p.x) && (0.0..=1.0).contains(&p.y),
                            "{k:?}: point {p:?} escapes the cell"
                        );
                    }
                }
                SparkShape::Bars(bars) => {
                    for b in bars {
                        assert!(
                            b.x0 >= 0.0 && b.x1 <= 1.0 && b.lo >= 0.0 && b.hi <= 1.0,
                            "{k:?}: bar {b:?} escapes the cell"
                        );
                        assert!(b.hi >= b.lo, "{k:?}: inverted bar {b:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_flat_series_draws_a_flat_line_rather_than_dividing_by_zero() {
        let s = sparkline_shape(SparkKind::Line, &nums(&[7.0; 6]), 60.0).unwrap();
        match s {
            SparkShape::Line(p) => {
                assert!(
                    p.iter().all(|q| q.y.is_finite()),
                    "constant data must not produce NaN"
                );
                let first = p[0].y;
                assert!(
                    p.iter().all(|q| (q.y - first).abs() < 1e-9),
                    "flat data is a flat line"
                );
            }
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn win_loss_ignores_magnitude_but_not_sign() {
        let s = sparkline_shape(SparkKind::WinLoss, &nums(&[1.0, -1000.0, 5000.0]), 60.0).unwrap();
        let SparkShape::Bars(bars) = s else {
            panic!("win/loss draws bars")
        };
        assert_eq!(bars.len(), 3);
        let h: Vec<f64> = bars.iter().map(|b| b.hi - b.lo).collect();
        assert!(
            (h[0] - h[1]).abs() < 1e-9 && (h[1] - h[2]).abs() < 1e-9,
            "every win/loss bar is the same height, got {h:?}"
        );
        assert_eq!(
            bars.iter().map(|b| b.negative).collect::<Vec<_>>(),
            vec![false, true, false],
            "sign is the ONLY thing win/loss encodes"
        );
    }

    #[test]
    fn win_loss_draws_nothing_for_a_zero() {
        let s = sparkline_shape(SparkKind::WinLoss, &nums(&[1.0, 0.0, -1.0]), 60.0).unwrap();
        let SparkShape::Bars(bars) = s else {
            panic!("win/loss draws bars")
        };
        assert_eq!(bars.len(), 2, "a zero is no bar, not a flat one: {bars:?}");
    }

    #[test]
    fn column_bars_straddle_zero_when_the_data_does() {
        let s = sparkline_shape(SparkKind::Column, &nums(&[-4.0, 4.0]), 60.0).unwrap();
        let SparkShape::Bars(bars) = s else {
            panic!("column draws bars")
        };
        assert!(bars[0].negative && !bars[1].negative);
        // They meet at the baseline rather than both growing from the floor.
        assert!(
            (bars[0].hi - bars[1].lo).abs() < 1e-9,
            "bars must share a baseline: {bars:?}"
        );
    }

    #[test]
    fn column_bars_of_a_narrow_positive_range_still_vary() {
        // The reason the baseline is not always zero: 900..1000 drawn from
        // zero is ten identical full bars, which shows nothing.
        let s = sparkline_shape(SparkKind::Column, &nums(&[900.0, 950.0, 1000.0]), 60.0).unwrap();
        let SparkShape::Bars(bars) = s else {
            panic!("column draws bars")
        };
        let heights: Vec<f64> = bars.iter().map(|b| b.hi - b.lo).collect();
        assert!(
            heights[2] - heights[0] > 0.3,
            "a narrow positive range must still show variation, got {heights:?}"
        );
    }

    // ---- the scale claim: cost tracks the canvas, not the source ----

    #[test]
    fn a_huge_source_row_is_reduced_to_the_cell_width() {
        let wide: Vec<Option<f64>> = (0..100_000).map(|i| Some(i as f64)).collect();
        for k in SparkKind::ALL {
            let s = sparkline_shape(k, &wide, 60.0).unwrap();
            let n = match &s {
                SparkShape::Line(p) => p.len(),
                SparkShape::Bars(b) => b.len(),
            };
            assert!(
                n <= MAX_BUCKETS * 2,
                "{k:?}: 100k source cells produced {n} primitives — reduction must track the cell"
            );
        }
    }

    #[test]
    fn decimation_keeps_a_spike_a_narrow_cell_would_otherwise_lose() {
        // The property inherited from `chart::decimate_min_max`, asserted
        // through the sparkline path so a future rewrite that samples instead
        // fails here rather than silently lying in every cell.
        let mut wide: Vec<Option<f64>> = vec![Some(1.0); 20_000];
        wide[13_337] = Some(999.0);
        let s = sparkline_shape(SparkKind::Line, &wide, 60.0).unwrap();
        let SparkShape::Line(p) = s else {
            panic!("line")
        };
        // The spike is the maximum, so after normalisation it is the only
        // point at the top of the cell.
        let top = p.iter().map(|q| q.y).fold(f64::NEG_INFINITY, f64::max);
        assert!(
            (top - 1.0).abs() < 1e-9,
            "the spike must survive reduction and reach the top of the cell"
        );
    }
}
