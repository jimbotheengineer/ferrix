//! Chart aggregation: drawing more data than there are pixels.
//!
//! ## The problem this solves
//!
//! A chart canvas is on the order of 1,000 pixels wide. A 200M-row column has
//! ~200,000 points per pixel column. Building one primitive per row would
//! allocate gigabytes to draw something the eye cannot resolve, and would take
//! minutes to render a frame.
//!
//! So aggregation happens **in the columnar store, before any geometry
//! exists**. Every function here consumes a typed slice and produces a bounded
//! number of output points — bounded by the canvas, never by the input. This
//! is the same discipline as `sum_rect` and the search scan: one streaming
//! pass over contiguous memory.
//!
//! ## Why min/max decimation rather than sampling
//!
//! The obvious way to shrink 200M points to 1,000 is to sample every
//! 200,000th. That silently deletes outliers: a single catastrophic spike —
//! exactly the thing a human is scanning the chart for — has a 1-in-200,000
//! chance of surviving.
//!
//! Min/max decimation instead emits *both* extremes of each pixel column, so a
//! one-row spike is always drawn. It costs two points per pixel instead of
//! one, and it is the difference between a chart you can trust and one that
//! lies by omission. `decimation_preserves_a_single_spike` pins this.

use crate::Value;

/// A point in data space. Charts are described in data coordinates and mapped
/// to the screen at draw time, which is what makes them resolution
/// independent.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DataPoint {
    pub x: f64,
    pub y: f64,
}

impl DataPoint {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Inclusive numeric bounds.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Bounds {
    pub min: f64,
    pub max: f64,
}

impl Bounds {
    pub fn new(min: f64, max: f64) -> Self {
        Self { min, max }
    }

    /// Widen to include `v`, ignoring non-finite values.
    #[inline]
    pub fn include(&mut self, v: f64) {
        if !v.is_finite() {
            return;
        }
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
        }
    }

    pub fn span(&self) -> f64 {
        self.max - self.min
    }

    pub fn is_empty(&self) -> bool {
        !self.min.is_finite() || !self.max.is_finite() || self.max < self.min
    }

    /// An empty range that any `include` will fill.
    pub fn unbounded() -> Self {
        Self {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// Pad a degenerate range so a chart of constant data still has a visible
    /// axis rather than dividing by zero.
    pub fn padded(&self) -> Self {
        if self.is_empty() {
            return Bounds::new(0.0, 1.0);
        }
        let span = self.span();
        if span.abs() < f64::EPSILON {
            let pad = if self.min.abs() < f64::EPSILON {
                1.0
            } else {
                self.min.abs() * 0.1
            };
            return Bounds::new(self.min - pad, self.max + pad);
        }
        *self
    }
}

/// Result of reducing a series to something drawable.
#[derive(Clone, Debug, Default)]
pub struct Series {
    /// Points to draw, already ordered by x.
    pub points: Vec<DataPoint>,
    /// How many input rows were considered.
    pub source_rows: usize,
    /// True when aggregation reduced the data, so the UI can say so rather
    /// than implying every row is individually visible.
    pub aggregated: bool,
}

/// Min/max decimation: reduce `values` to at most `2 * buckets` points while
/// preserving every local extreme.
///
/// Each bucket contributes its minimum and maximum in x order, so a spike
/// anywhere in the input survives. Returns points whose `x` is the row index.
///
/// `None` entries are gaps (empty or non-numeric cells) and are skipped rather
/// than treated as zero — plotting a missing value as 0 invents data.
pub fn decimate_min_max(values: &[Option<f64>], buckets: usize) -> Series {
    let n = values.len();
    if n == 0 || buckets == 0 {
        return Series::default();
    }
    // Fewer points than buckets: nothing to aggregate, draw them all.
    if n <= buckets {
        let points: Vec<DataPoint> = values
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.map(|y| DataPoint::new(i as f64, y)))
            .collect();
        return Series {
            points,
            source_rows: n,
            aggregated: false,
        };
    }

    let mut points = Vec::with_capacity(buckets * 2);
    // Ceiling division so the last bucket cannot run past the end.
    let per = n.div_ceil(buckets);

    for b in 0..buckets {
        let start = b * per;
        if start >= n {
            break;
        }
        let end = (start + per).min(n);

        let mut lo: Option<(usize, f64)> = None;
        let mut hi: Option<(usize, f64)> = None;
        for (i, v) in values[start..end].iter().enumerate() {
            let Some(y) = *v else { continue };
            if !y.is_finite() {
                continue;
            }
            let idx = start + i;
            match lo {
                Some((_, cur)) if y >= cur => {}
                _ => lo = Some((idx, y)),
            }
            match hi {
                Some((_, cur)) if y <= cur => {}
                _ => hi = Some((idx, y)),
            }
        }

        // Emit in x order so the polyline does not zig-zag backwards.
        match (lo, hi) {
            (Some((li, lv)), Some((hi_i, hv))) => {
                if li == hi_i {
                    points.push(DataPoint::new(li as f64, lv));
                } else if li < hi_i {
                    points.push(DataPoint::new(li as f64, lv));
                    points.push(DataPoint::new(hi_i as f64, hv));
                } else {
                    points.push(DataPoint::new(hi_i as f64, hv));
                    points.push(DataPoint::new(li as f64, lv));
                }
            }
            _ => continue, // Bucket was entirely gaps.
        }
    }

    Series {
        points,
        source_rows: n,
        aggregated: true,
    }
}

/// One bar of a histogram.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Bin {
    pub lo: f64,
    pub hi: f64,
    pub count: u64,
}

/// Bin numeric values into a histogram in one streaming pass.
///
/// The aggregation *is* the chart here: output size is `bins`, whatever the
/// input size.
pub fn histogram(values: &[Option<f64>], bins: usize, range: Option<Bounds>) -> Vec<Bin> {
    if bins == 0 {
        return Vec::new();
    }
    let range = match range {
        Some(r) => r,
        None => {
            let mut b = Bounds::unbounded();
            for v in values.iter().flatten() {
                b.include(*v);
            }
            b
        }
    };
    if range.is_empty() {
        return Vec::new();
    }
    let range = range.padded();
    let span = range.span();

    let mut counts = vec![0u64; bins];
    for v in values.iter().flatten() {
        if !v.is_finite() || *v < range.min || *v > range.max {
            continue;
        }
        // The maximum value would land one past the last bin; clamp it in.
        let t = (*v - range.min) / span;
        let idx = ((t * bins as f64) as usize).min(bins - 1);
        counts[idx] += 1;
    }

    let width = span / bins as f64;
    counts
        .into_iter()
        .enumerate()
        .map(|(i, count)| Bin {
            lo: range.min + width * i as f64,
            hi: range.min + width * (i + 1) as f64,
            count,
        })
        .collect()
}

/// A cell in a 2-D density grid, for scatter plots too dense to draw as points.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DensityCell {
    pub x_bin: usize,
    pub y_bin: usize,
    pub count: u64,
}

/// Bin (x, y) pairs into a density grid.
///
/// Beyond a few thousand markers a scatter plot becomes an unreadable blob and
/// costs one primitive per row. Binning to a grid bounded by the canvas turns
/// it into a heatmap that is both faster and more legible.
pub fn density_grid(
    xs: &[Option<f64>],
    ys: &[Option<f64>],
    x_bins: usize,
    y_bins: usize,
) -> (Vec<DensityCell>, Bounds, Bounds) {
    let mut xb = Bounds::unbounded();
    let mut yb = Bounds::unbounded();
    if x_bins == 0 || y_bins == 0 {
        return (Vec::new(), xb, yb);
    }
    let n = xs.len().min(ys.len());
    for i in 0..n {
        if let (Some(x), Some(y)) = (xs[i], ys[i]) {
            if x.is_finite() && y.is_finite() {
                xb.include(x);
                yb.include(y);
            }
        }
    }
    if xb.is_empty() || yb.is_empty() {
        return (Vec::new(), xb, yb);
    }
    let xb = xb.padded();
    let yb = yb.padded();

    let mut grid = vec![0u64; x_bins * y_bins];
    for i in 0..n {
        let (Some(x), Some(y)) = (xs[i], ys[i]) else {
            continue;
        };
        if !x.is_finite() || !y.is_finite() {
            continue;
        }
        let xi = (((x - xb.min) / xb.span()) * x_bins as f64) as usize;
        let yi = (((y - yb.min) / yb.span()) * y_bins as f64) as usize;
        let xi = xi.min(x_bins - 1);
        let yi = yi.min(y_bins - 1);
        grid[yi * x_bins + xi] += 1;
    }

    // Only occupied cells are emitted, so an empty region costs nothing.
    let cells = grid
        .into_iter()
        .enumerate()
        .filter(|(_, c)| *c > 0)
        .map(|(i, count)| DensityCell {
            x_bin: i % x_bins,
            y_bin: i / x_bins,
            count,
        })
        .collect();
    (cells, xb, yb)
}

/// Group rows by a category and reduce each group to one number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Aggregate {
    Sum,
    Count,
    Mean,
    Min,
    Max,
}

/// One bar of a categorical chart.
#[derive(Clone, Debug, PartialEq)]
pub struct Category {
    pub label: String,
    pub value: f64,
    pub count: u64,
}

/// Group `labels` by value and aggregate the matching `values`.
///
/// Output is bounded by the number of *distinct categories*, which is exactly
/// the property the interned string arena makes cheap: a 200M-row status
/// column has a handful of distinct values.
///
/// Results are sorted by descending value then label, so the chart is stable
/// across runs rather than reflecting hash order.
pub fn group_by(labels: &[String], values: &[Option<f64>], agg: Aggregate) -> Vec<Category> {
    use std::collections::HashMap;

    struct Acc {
        sum: f64,
        count: u64,
        min: f64,
        max: f64,
    }

    let mut groups: HashMap<&str, Acc> = HashMap::new();
    let n = labels.len().min(values.len().max(labels.len()));

    for (i, label) in labels.iter().enumerate().take(n) {
        let label = label.as_str();
        let v = values.get(i).copied().flatten();
        let e = groups.entry(label).or_insert(Acc {
            sum: 0.0,
            count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        });
        e.count += 1;
        if let Some(y) = v {
            if y.is_finite() {
                e.sum += y;
                if y < e.min {
                    e.min = y;
                }
                if y > e.max {
                    e.max = y;
                }
            }
        }
    }

    let mut out: Vec<Category> = groups
        .into_iter()
        .map(|(label, a)| {
            let value = match agg {
                Aggregate::Sum => a.sum,
                Aggregate::Count => a.count as f64,
                Aggregate::Mean => {
                    if a.count == 0 {
                        0.0
                    } else {
                        a.sum / a.count as f64
                    }
                }
                Aggregate::Min => {
                    if a.min.is_finite() {
                        a.min
                    } else {
                        0.0
                    }
                }
                Aggregate::Max => {
                    if a.max.is_finite() {
                        a.max
                    } else {
                        0.0
                    }
                }
            };
            Category {
                label: label.to_string(),
                value,
                count: a.count,
            }
        })
        .collect();

    out.sort_by(|a, b| {
        b.value
            .partial_cmp(&a.value)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.label.cmp(&b.label))
    });
    out
}

/// Extract a numeric column as `Option<f64>`, treating text and errors as gaps.
///
/// Gaps are deliberately not zeros: plotting a missing measurement as 0 would
/// invent a data point that the source never contained.
pub fn numeric_column(values: impl Iterator<Item = Value>) -> Vec<Option<f64>> {
    values
        .map(|v| match v {
            Value::Number(n) if n.is_finite() => Some(n),
            Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nums(v: &[f64]) -> Vec<Option<f64>> {
        v.iter().map(|x| Some(*x)).collect()
    }

    #[test]
    fn small_series_is_not_aggregated() {
        let s = decimate_min_max(&nums(&[1.0, 2.0, 3.0]), 100);
        assert!(
            !s.aggregated,
            "fewer points than buckets needs no reduction"
        );
        assert_eq!(s.points.len(), 3);
        assert_eq!(s.points[0], DataPoint::new(0.0, 1.0));
    }

    #[test]
    fn decimation_bounds_output_by_buckets_not_input() {
        // The core scale claim: output size tracks the canvas, not the data.
        let data: Vec<Option<f64>> = (0..1_000_000).map(|i| Some(i as f64)).collect();
        let s = decimate_min_max(&data, 500);
        assert!(s.aggregated);
        assert!(
            s.points.len() <= 1000,
            "500 buckets must yield <= 1000 points, got {}",
            s.points.len()
        );
        assert_eq!(s.source_rows, 1_000_000);
    }

    #[test]
    fn decimation_preserves_a_single_spike() {
        // Why min/max rather than sampling: one anomalous row in a million
        // must still be drawn. Sampling every Nth would drop it.
        let mut data: Vec<Option<f64>> = (0..1_000_000).map(|_| Some(1.0)).collect();
        data[777_777] = Some(9999.0);

        let s = decimate_min_max(&data, 500);
        let peak = s
            .points
            .iter()
            .map(|p| p.y)
            .fold(f64::NEG_INFINITY, f64::max);
        assert_eq!(peak, 9999.0, "the spike must survive decimation");

        // And it must be at roughly the right x, not relocated.
        let spike = s.points.iter().find(|p| p.y == 9999.0).unwrap();
        assert!(
            (spike.x - 777_777.0).abs() < 2100.0,
            "spike moved to x={}",
            spike.x
        );
    }

    #[test]
    fn decimation_preserves_a_downward_spike_too() {
        let mut data: Vec<Option<f64>> = (0..100_000).map(|_| Some(50.0)).collect();
        data[42_000] = Some(-500.0);
        let s = decimate_min_max(&data, 200);
        let trough = s.points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
        assert_eq!(trough, -500.0, "min must be preserved as well as max");
    }

    #[test]
    fn decimated_points_stay_in_x_order() {
        // A polyline must not zig-zag backwards, or it draws as a mess.
        let data: Vec<Option<f64>> = (0..10_000)
            .map(|i| Some(((i as f64) * 0.01).sin()))
            .collect();
        let s = decimate_min_max(&data, 100);
        for w in s.points.windows(2) {
            assert!(w[0].x <= w[1].x, "out of order: {:?} then {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn gaps_are_skipped_not_zeroed() {
        // Plotting a missing value as 0 invents data the source never had.
        let data = vec![Some(5.0), None, Some(7.0), None];
        let s = decimate_min_max(&data, 100);
        assert_eq!(s.points.len(), 2);
        assert!(s.points.iter().all(|p| p.y != 0.0));
    }

    #[test]
    fn all_gaps_yields_nothing() {
        let data: Vec<Option<f64>> = vec![None; 1000];
        let s = decimate_min_max(&data, 50);
        assert!(s.points.is_empty());
    }

    #[test]
    fn histogram_counts_every_value_once() {
        let data = nums(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]);
        let bins = histogram(&data, 5, None);
        assert_eq!(bins.len(), 5);
        let total: u64 = bins.iter().map(|b| b.count).sum();
        assert_eq!(total, 10, "no value may be lost or double-counted");
    }

    #[test]
    fn histogram_includes_the_maximum_value() {
        // Off-by-one trap: the max maps exactly to bins.len(), one past the
        // end, and must be clamped into the last bin rather than dropped.
        let data = nums(&[0.0, 10.0]);
        let bins = histogram(&data, 4, Some(Bounds::new(0.0, 10.0)));
        let total: u64 = bins.iter().map(|b| b.count).sum();
        assert_eq!(total, 2, "the maximum must land in the last bin");
        assert_eq!(bins[3].count, 1);
    }

    #[test]
    fn histogram_of_constant_data_does_not_divide_by_zero() {
        let data = nums(&[7.0; 100]);
        let bins = histogram(&data, 10, None);
        assert_eq!(bins.len(), 10);
        let total: u64 = bins.iter().map(|b| b.count).sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn histogram_output_is_bounded_by_bin_count() {
        let data: Vec<Option<f64>> = (0..2_000_000).map(|i| Some((i % 1000) as f64)).collect();
        let bins = histogram(&data, 64, None);
        assert_eq!(bins.len(), 64, "output tracks bins, not 2M rows");
        let total: u64 = bins.iter().map(|b| b.count).sum();
        assert_eq!(total, 2_000_000);
    }

    #[test]
    fn density_grid_bins_pairs() {
        let xs = nums(&[0.0, 1.0, 0.0, 1.0]);
        let ys = nums(&[0.0, 1.0, 0.0, 1.0]);
        let (cells, _, _) = density_grid(&xs, &ys, 2, 2);
        let total: u64 = cells.iter().map(|c| c.count).sum();
        assert_eq!(total, 4);
        // Only occupied cells are emitted.
        assert!(cells.len() <= 4);
    }

    #[test]
    fn density_grid_output_is_bounded() {
        let xs: Vec<Option<f64>> = (0..500_000).map(|i| Some((i % 997) as f64)).collect();
        let ys: Vec<Option<f64>> = (0..500_000).map(|i| Some((i % 991) as f64)).collect();
        let (cells, _, _) = density_grid(&xs, &ys, 64, 64);
        assert!(
            cells.len() <= 64 * 64,
            "cells must not exceed the grid, got {}",
            cells.len()
        );
        let total: u64 = cells.iter().map(|c| c.count).sum();
        assert_eq!(total, 500_000, "every point must be counted exactly once");
    }

    #[test]
    fn group_by_aggregates_and_sorts_stably() {
        let labels: Vec<String> = ["north", "south", "north", "east", "south", "north"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let values = nums(&[10.0, 5.0, 20.0, 1.0, 5.0, 30.0]);

        let sums = group_by(&labels, &values, Aggregate::Sum);
        assert_eq!(sums[0].label, "north");
        assert_eq!(sums[0].value, 60.0);
        assert_eq!(sums[0].count, 3);
        assert_eq!(sums[1].label, "south");
        assert_eq!(sums[1].value, 10.0);

        // Sorted descending by value, so the ordering is deterministic
        // regardless of HashMap iteration order.
        for w in sums.windows(2) {
            assert!(w[0].value >= w[1].value);
        }

        let counts = group_by(&labels, &values, Aggregate::Count);
        assert_eq!(counts[0].value, 3.0);

        let means = group_by(&labels, &values, Aggregate::Mean);
        let north = means.iter().find(|c| c.label == "north").unwrap();
        assert_eq!(north.value, 20.0, "60 / 3");
    }

    #[test]
    fn group_by_output_tracks_cardinality_not_rows() {
        // The arena makes this cheap: 200M rows of 4 distinct statuses is 4
        // bars, and the aggregation never allocates per row.
        let labels: Vec<String> = (0..200_000)
            .map(|i| ["a", "b", "c", "d"][i % 4].to_string())
            .collect();
        let values: Vec<Option<f64>> = (0..200_000).map(|i| Some(i as f64)).collect();
        let out = group_by(&labels, &values, Aggregate::Count);
        assert_eq!(out.len(), 4, "four distinct labels, four bars");
        let total: f64 = out.iter().map(|c| c.value).sum();
        assert_eq!(total, 200_000.0);
    }

    #[test]
    fn bounds_pad_degenerate_ranges() {
        let b = Bounds::new(5.0, 5.0).padded();
        assert!(b.span() > 0.0, "constant data still needs a visible axis");
        assert!(b.min < 5.0 && b.max > 5.0);

        let zero = Bounds::new(0.0, 0.0).padded();
        assert!(zero.span() > 0.0);

        let empty = Bounds::unbounded().padded();
        assert_eq!(empty, Bounds::new(0.0, 1.0));
    }

    #[test]
    fn bounds_ignore_non_finite_values() {
        let mut b = Bounds::unbounded();
        b.include(1.0);
        b.include(f64::NAN);
        b.include(f64::INFINITY);
        b.include(3.0);
        assert_eq!(b, Bounds::new(1.0, 3.0), "NaN/inf must not poison bounds");
    }

    #[test]
    fn numeric_column_maps_types_sensibly() {
        use crate::{ErrorKind, StrId};
        let vals = vec![
            Value::Number(1.5),
            Value::Bool(true),
            Value::Bool(false),
            Value::Empty,
            Value::Text(StrId(0)),
            Value::Error(ErrorKind::DivZero),
            Value::Number(f64::NAN),
        ];
        let got = numeric_column(vals.into_iter());
        assert_eq!(
            got,
            vec![
                Some(1.5),
                Some(1.0),
                Some(0.0),
                None,
                None,
                None,
                None // NaN is a gap, not a point
            ]
        );
    }
}
