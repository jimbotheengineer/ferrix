//! The chart panel: turn a selected range into a chart.
//!
//! This is the UI over `ferrix_core::chart` (aggregation) and
//! `ferrix_core::scene` (geometry). The division of labour matters:
//!
//! - `chart` reduces N rows to a bounded number of points
//! - `scene` describes those points as geometry in data coordinates
//! - this module maps that geometry onto an egui painter
//!
//! Because the scene is toolkit-agnostic, the exact same geometry goes to the
//! SVG writer on export. What you see is what you get.

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use ferrix_core::annotation::{Annotation, Annotations};
use ferrix_core::chart::{
    decimate_min_max, density_grid, group_by, histogram, Aggregate, Bounds, DataPoint,
};
use ferrix_core::scene::{
    to_svg, Anchor, LegendEntry, Primitive, Rgba, Scale, ScaleHint, Scene, Viewport,
};
use ferrix_core::{CellRef, Selection, Value};

use crate::sheet_view::SheetView;
use crate::theme::{ChartChrome, Theme};

/// Ceiling on how many rows a chart will pull from the sheet, whatever the
/// machine's memory.
///
/// Aggregation is bounded by the canvas, but *reading* the column still costs
/// one `get` per row. At 200M rows that is seconds — too slow to do on every
/// frame, and too slow to do synchronously on the UI thread at all. Charting
/// the first slice keeps the panel responsive and is honest about it: the
/// status line always says how many rows were used.
///
/// This is a TIME limit, not a memory one. A machine with 128 GB free still
/// should not spend ten seconds of UI thread reading cells, so the memory
/// budget can only make this number smaller, never larger.
pub const MAX_CHART_ROWS: usize = 2_000_000;

/// Rows this chart may actually read: the responsiveness ceiling above, capped
/// again by what measured memory will hold at [`cost::CHART_ROW`] each.
///
/// On a healthy machine the memory term is far larger and
/// [`MAX_CHART_ROWS`] binds. On a machine under pressure the memory term
/// binds instead, and the panel says so rather than being OOM-killed
/// mid-render.
pub fn chart_row_budget() -> usize {
    let by_memory =
        ferrix_core::Budget::sample().max_units_usize(ferrix_core::budget::cost::CHART_ROW);
    by_memory.min(MAX_CHART_ROWS)
}

/// Target number of decimation buckets. Two points per bucket (min and max),
/// so ~1,600 points across a typical canvas — more than the pixels can
/// resolve, which is the point.
const BUCKETS: usize = 800;

/// Series paint colours. Named so the legend swatch and the geometry that
/// draws the series are guaranteed the same colour — a legend that hard-coded
/// its swatch could drift from the line/bars it labels.
const SERIES_BLUE: Rgba = Rgba::rgb(0x4a, 0x9e, 0xff);
const SERIES_ORANGE: Rgba = Rgba::rgb(0xff, 0x9f, 0x40);

/// The full series palette, in assignment order: first series blue, second
/// orange, then green, purple, red. Chosen to read on both the app's dark
/// chrome and the always-light SVG export.
pub const SERIES_COLORS: [Rgba; 5] = [
    SERIES_BLUE,
    SERIES_ORANGE,
    Rgba::rgb(0x53, 0xb8, 0x7a),
    Rgba::rgb(0xb1, 0x83, 0xe8),
    Rgba::rgb(0xe8, 0x6a, 0x6a),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChartKind {
    Line,
    Bar,
    Histogram,
    Scatter,
}

/// A complete set of custom chart text, applied in one call: what the agent
/// bridge and tests use. The window's label fields edit the same overrides
/// one at a time.
#[derive(Clone, Debug, Default)]
pub struct ChartLabels {
    pub title: String,
    pub x_axis: String,
    pub y_axis: String,
    pub series: String,
}

impl ChartPanel {
    /// Replace the chart's generated text with the user's own words —
    /// "H by G" becomes "Profit by Region". Empty strings clear back to the
    /// derived defaults. The overrides are stored on the panel, so they
    /// survive rebuilds (kind switches, column re-aims) and travel into the
    /// SVG export; they are NOT lost when the data changes.
    pub fn set_custom_labels(&mut self, labels: ChartLabels) {
        let opt = |s: String| {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        };
        self.title_override = opt(labels.title);
        self.x_label_override = opt(labels.x_axis);
        self.y_label_override = opt(labels.y_axis);
        self.series_override = opt(labels.series);
        self.apply_overrides_in_place();
    }

    /// Lay the stored overrides over the current scene. Called after every
    /// build and after every override edit, so the scene on screen and the
    /// scene exported are always the same words.
    pub fn apply_overrides_in_place(&mut self) {
        let Some(s) = self.scene.as_mut() else {
            return;
        };
        if let Some(t) = &self.title_override {
            s.title = Some(t.clone());
        }
        if let Some(x) = &self.x_label_override {
            s.x_label = Some(x.clone());
        }
        if let Some(y) = &self.y_label_override {
            s.y_label = Some(y.clone());
        }
        if let Some(name) = &self.series_override {
            if let Some(first) = s.legend.first_mut() {
                first.label = name.clone();
            }
        }
    }
}

impl ChartKind {
    pub fn label(self) -> &'static str {
        match self {
            ChartKind::Line => "Line",
            ChartKind::Bar => "Bar",
            ChartKind::Histogram => "Histogram",
            ChartKind::Scatter => "Scatter",
        }
    }

    pub const ALL: [ChartKind; 4] = [
        ChartKind::Line,
        ChartKind::Bar,
        ChartKind::Histogram,
        ChartKind::Scatter,
    ];
}

/// Everything the chart panel owns.
pub struct ChartPanel {
    pub open: bool,
    pub kind: ChartKind,
    /// Range the chart was built from, so it can be rebuilt on demand.
    pub source: Option<Selection>,
    /// The built scene. `None` until a range is charted.
    pub scene: Option<Scene>,
    pub annotations: Annotations,
    /// Rows actually read, and rows the range contained — these differ when
    /// the range is larger than MAX_CHART_ROWS, and the difference is shown.
    pub rows_used: usize,
    pub rows_available: usize,
    /// True when the row cap came from memory pressure rather than the
    /// standing responsiveness ceiling — a different thing to tell the user.
    pub capped_by_memory: bool,
    pub build_ms: f32,
    pub status: String,
    /// Index of the annotation being edited, if any.
    pub editing_note: Option<usize>,
    pub note_buffer: String,
    /// True when the next canvas click should place an annotation.
    pub placing_note: bool,
    /// User toggle: plot the value (y) axis on a log scale. Useful for
    /// heavy-tailed data — counts, prices — where a linear axis crushes the
    /// small values against the baseline. X stays linear (it is a row number or
    /// a category index, not a magnitude).
    pub y_log: bool,
    /// Explicit column choices from the chart window's column pickers (issue
    /// #106). `None` means "derive from the selection" (the old behaviour):
    /// `y_col` is the values column, `x_col` the category/x column. Both are
    /// absolute sheet column indices, so they can be non-adjacent (e.g. chart
    /// `region` against `revenue` while skipping the columns between them).
    pub y_col: Option<u32>,
    pub x_col: Option<u32>,
    /// Additional Y (values) columns beyond `y_col` — each is its own series
    /// with its own colour and legend entry. Line and bar charts draw them
    /// all; histogram and scatter use only the primary. Added and removed in
    /// the chart window's series row.
    pub extra_y_cols: Vec<u32>,
    /// User text overrides for the chart's words. `None` = the derived
    /// default ("H by G"); `Some` = exactly what the user typed ("Profit by
    /// Region"). They survive rebuilds and re-aims, travel into the SVG
    /// export, and clear back to the defaults when emptied.
    pub title_override: Option<String>,
    pub x_label_override: Option<String>,
    pub y_label_override: Option<String>,
    /// Renames the FIRST series in the legend ("sum of H" → "Profit").
    pub series_override: Option<String>,
}

impl Default for ChartPanel {
    fn default() -> Self {
        Self {
            open: false,
            kind: ChartKind::Line,
            source: None,
            scene: None,
            annotations: Annotations::new(),
            rows_used: 0,
            rows_available: 0,
            capped_by_memory: false,
            build_ms: 0.0,
            status: String::new(),
            editing_note: None,
            note_buffer: String::new(),
            placing_note: false,
            y_log: false,
            y_col: None,
            x_col: None,
            extra_y_cols: Vec::new(),
            title_override: None,
            x_label_override: None,
            y_label_override: None,
            series_override: None,
        }
    }
}

impl ChartPanel {
    /// Build a chart from `sel` over `view`.
    ///
    /// Reads at most `MAX_CHART_ROWS` rows, aggregates according to `kind`,
    /// and produces a `Scene` in data coordinates.
    pub fn build(&mut self, view: &SheetView<'_>, sel: Selection, kind: ChartKind) {
        let t0 = std::time::Instant::now();
        let (tl, br) = sel.bounds();
        let (r0, c0, r1, c1) = (tl.row, tl.col, br.row, br.col);

        let total_rows = (r1 as usize).saturating_sub(r0 as usize) + 1;
        let budget = chart_row_budget();
        let capped = total_rows.min(budget);
        // Record WHY the chart was capped, so the panel can distinguish "this
        // is as much as we plot for responsiveness" from "your machine is out
        // of memory". Conflating the two is how a user concludes the app is
        // broken when it is actually protecting them.
        self.capped_by_memory = capped < total_rows && budget < MAX_CHART_ROWS;
        let last_row = r0 as usize + capped;

        self.rows_available = total_rows;
        self.rows_used = capped;
        self.source = Some(sel);
        self.kind = kind;

        // Which columns carry x and y. By default (the selection-derived case)
        // the values come from the second column of the range and the labels/x
        // from the first — the classic "select two columns" convention. The
        // chart window's column pickers override either one with an explicit,
        // possibly NON-ADJACENT sheet column, so you can chart e.g. `region`
        // against `revenue` without them being side by side.
        let default_value_col = c1.min(c0 + 1).max(c0);
        let value_col = self.y_col.unwrap_or(default_value_col);
        let label_col = self.x_col.unwrap_or(c0);
        // A second series exists when the label column genuinely differs from
        // the value column (adjacent selection, or an explicit x pick).
        let has_second = label_col != value_col;

        // Real column headers (falling back to the spreadsheet letter) so the
        // axes, title and legend name the user's own columns rather than the
        // placeholders "row"/"value"/"x"/"y" the panel used to draw.
        let value_header = view.header_or_letter(value_col as usize);
        let first_header = view.header_or_letter(label_col as usize);

        // The full series list: the primary Y column plus every extra the
        // user added in the window's series row. Line and bar draw them all;
        // histogram and scatter are single-series by nature and say so.
        let mut series_cols: Vec<u32> = vec![value_col];
        for &c in &self.extra_y_cols {
            if !series_cols.contains(&c) {
                series_cols.push(c);
            }
        }
        let multi = series_cols.len() > 1 && matches!(kind, ChartKind::Line | ChartKind::Bar);

        let values: Vec<Option<f64>> = (r0 as usize..last_row)
            .map(|r| match view.get(CellRef::new(r as u32, value_col)) {
                Value::Number(n) if n.is_finite() => Some(n),
                Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
                _ => None,
            })
            .collect();

        // Per-axis scale hint threaded into every builder. The y (value) axis
        // follows the user's log toggle; x stays linear because it is a row
        // number, a category index, or — for scatter — handed its own hint by
        // the builder. Builders that log a non-positive value axis have their
        // bounds sanitised inside `with_scale`.
        let scale = ScaleHint::new(
            Scale::Linear,
            if self.y_log {
                Scale::Log
            } else {
                Scale::Linear
            },
        );

        let scene = if multi {
            // Read every series column once; names come from the headers so
            // the legend names real columns.
            let read_col = |c: u32| -> Vec<Option<f64>> {
                (r0 as usize..last_row)
                    .map(|r| match view.get(CellRef::new(r as u32, c)) {
                        Value::Number(n) if n.is_finite() => Some(n),
                        Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
                        _ => None,
                    })
                    .collect()
            };
            let series: Vec<(String, Vec<Option<f64>>)> = series_cols
                .iter()
                .map(|&c| (view.header_or_letter(c as usize), read_col(c)))
                .collect();
            match kind {
                ChartKind::Line => Self::build_multi_line(&series, r0 as usize, scale),
                ChartKind::Bar => {
                    if !has_second {
                        self.status = "Bar charts need two columns: labels then values".to_string();
                        self.scene = None;
                        return;
                    }
                    let labels: Vec<String> = (r0 as usize..last_row)
                        .map(|r| view.display(CellRef::new(r as u32, label_col)))
                        .collect();
                    Self::build_multi_bar(&labels, &series, &first_header, scale)
                }
                // Unreachable: `multi` is only true for Line | Bar.
                _ => None,
            }
        } else {
            match kind {
                ChartKind::Line => Self::build_line(&values, r0 as usize, &value_header, scale),
                ChartKind::Histogram => Self::build_histogram(&values, &value_header, scale),
                ChartKind::Bar => {
                    let labels: Vec<String> = if has_second {
                        (r0 as usize..last_row)
                            .map(|r| view.display(CellRef::new(r as u32, label_col)))
                            .collect()
                    } else {
                        // Without a label column every row is its own bar, which
                        // is a line chart with extra steps. Say so instead of
                        // drawing something misleading.
                        Vec::new()
                    };
                    if labels.is_empty() {
                        self.status = "Bar charts need two columns: labels then values".to_string();
                        self.scene = None;
                        return;
                    }
                    Self::build_bar(&labels, &values, &first_header, &value_header, scale)
                }
                ChartKind::Scatter => {
                    if !has_second {
                        self.status = "Scatter needs two numeric columns: x then y".to_string();
                        self.scene = None;
                        return;
                    }
                    let xs: Vec<Option<f64>> = (r0 as usize..last_row)
                        .map(|r| match view.get(CellRef::new(r as u32, label_col)) {
                            Value::Number(n) if n.is_finite() => Some(n),
                            _ => None,
                        })
                        .collect();
                    Self::build_scatter(&xs, &values, &first_header, &value_header, scale)
                }
            }
        };

        self.build_ms = t0.elapsed().as_secs_f32() * 1000.0;
        match scene {
            Some(s) => {
                let truncated = if capped < total_rows {
                    // Distinguish the two reasons a chart is short. "We only
                    // ever plot 2M rows so the panel stays responsive" and
                    // "your machine is low on memory right now" call for
                    // completely different reactions from the user, and
                    // reporting them with the same words is dishonest.
                    let why = if self.capped_by_memory {
                        " (limited by available memory)"
                    } else {
                        ""
                    };
                    format!(
                        " · first {} of {} rows{}",
                        fmt_count(capped),
                        fmt_count(total_rows),
                        why
                    )
                } else {
                    String::new()
                };
                self.status = format!(
                    "{} · {} rows · {} primitives · {:.0} ms{}",
                    kind.label(),
                    fmt_count(capped),
                    s.len(),
                    self.build_ms,
                    truncated
                );
                self.scene = Some(s);
                // The user's words go back on top after every rebuild, so a
                // kind switch or re-aim never silently reverts the text.
                self.apply_overrides_in_place();
            }
            None => {
                self.status = "No numeric data in the selected range".to_string();
                self.scene = None;
            }
        }
    }

    fn build_line(
        values: &[Option<f64>],
        row_offset: usize,
        value_header: &str,
        scale: ScaleHint,
    ) -> Option<Scene> {
        let series = decimate_min_max(values, BUCKETS);
        if series.points.is_empty() {
            return None;
        }
        let mut y = Bounds::unbounded();
        for p in &series.points {
            y.include(p.y);
        }
        // X is the absolute row number, so the axis reads as sheet rows.
        let points: Vec<DataPoint> = series
            .points
            .iter()
            .map(|p| DataPoint::new(p.x + row_offset as f64 + 1.0, p.y))
            .collect();
        let x = Bounds::new(
            row_offset as f64 + 1.0,
            row_offset as f64 + values.len() as f64,
        );

        // The x axis is genuinely the sheet row number, not a data column, so
        // it stays "row"; the y axis and title name the value column the user
        // selected. The legend labels the single series with that same header.
        let mut s = Scene::new(x, y)
            .with_axis_labels("row", value_header)
            .with_title(value_header.to_string())
            .with_legend(vec![LegendEntry::new(
                value_header.to_string(),
                SERIES_BLUE,
            )])
            .with_scale(scale);
        s.push(Primitive::Polyline {
            points,
            color: SERIES_BLUE,
            width: 1.4,
        });
        Some(s)
    }

    fn build_histogram(
        values: &[Option<f64>],
        value_header: &str,
        scale: ScaleHint,
    ) -> Option<Scene> {
        let bins = histogram(values, 40, None);
        if bins.is_empty() {
            return None;
        }
        let max = bins.iter().map(|b| b.count).max().unwrap_or(1) as f64;
        // The x axis of a histogram is the value column's own range; y is the
        // count of rows falling in each bin, which is not a source column, so
        // it stays "count". Title names the distribution being shown.
        let mut s = Scene::new(
            Bounds::new(bins[0].lo, bins[bins.len() - 1].hi),
            Bounds::new(0.0, max),
        )
        .with_axis_labels(value_header, "count")
        .with_title(format!("Distribution of {value_header}"))
        .with_legend(vec![LegendEntry::new(
            value_header.to_string(),
            SERIES_BLUE,
        )])
        .with_scale(scale);
        for b in &bins {
            s.push(Primitive::Rect {
                x0: b.lo,
                y0: 0.0,
                x1: b.hi,
                y1: b.count as f64,
                fill: SERIES_BLUE,
                stroke: None,
            });
        }
        Some(s)
    }

    fn build_bar(
        labels: &[String],
        values: &[Option<f64>],
        label_header: &str,
        value_header: &str,
        scale: ScaleHint,
    ) -> Option<Scene> {
        let cats = group_by(labels, values, Aggregate::Sum);
        if cats.is_empty() {
            return None;
        }
        // Too many categories is an unreadable forest of bars; cap and say so
        // in the axis label rather than drawing 10,000 of them.
        let shown = cats.len().min(40);
        let max = cats
            .iter()
            .take(shown)
            .map(|c| c.value)
            .fold(f64::NEG_INFINITY, f64::max);
        let min = cats
            .iter()
            .take(shown)
            .map(|c| c.value)
            .fold(f64::INFINITY, f64::min);
        let y = Bounds::new(min.min(0.0), max.max(0.0));

        // X axis names the category column; y axis says both what was measured
        // and that it is a sum of it, so the reader is not left guessing which
        // aggregate produced the bar heights.
        let y_label = format!("sum of {value_header}");
        // A bar chart's value axis includes zero and can go negative (sums),
        // where a log scale is meaningless. Honour a log-y request only when
        // every bar sits strictly above zero; otherwise keep the axis linear.
        let bar_scale = if scale.y == Scale::Log && y.min > 0.0 {
            scale
        } else {
            ScaleHint::new(scale.x, Scale::Linear)
        };
        let mut s = Scene::new(Bounds::new(-0.6, shown as f64 - 0.4), y)
            .with_axis_labels(label_header, y_label.clone())
            .with_title(format!("{value_header} by {label_header}"))
            .with_legend(vec![LegendEntry::new(y_label, SERIES_ORANGE)])
            .with_scale(bar_scale)
            .with_categorical_x();
        for (i, c) in cats.iter().take(shown).enumerate() {
            s.push(Primitive::Rect {
                x0: i as f64 - 0.38,
                y0: 0.0,
                x1: i as f64 + 0.38,
                y1: c.value,
                fill: SERIES_ORANGE,
                stroke: None,
            });
            s.push(Primitive::Text {
                at: DataPoint::new(i as f64, y.min.min(0.0)),
                text: c.label.clone(),
                size_px: 10.0,
                color: Rgba::rgb(0xa0, 0xa8, 0xb4),
                anchor: Anchor::Middle,
                offset_px: (0.0, 14.0),
            });
        }
        Some(s)
    }

    /// Multi-series line: one polyline per Y column over the shared row axis,
    /// each in its own palette colour with its own legend entry.
    fn build_multi_line(
        series: &[(String, Vec<Option<f64>>)],
        row_offset: usize,
        scale: ScaleHint,
    ) -> Option<Scene> {
        let mut y = Bounds::unbounded();
        let mut lines: Vec<(usize, Vec<DataPoint>)> = Vec::new();
        let mut x_len = 0usize;
        for (idx, (_, values)) in series.iter().enumerate() {
            let dec = decimate_min_max(values, BUCKETS);
            if dec.points.is_empty() {
                continue;
            }
            for p in &dec.points {
                y.include(p.y);
            }
            x_len = x_len.max(values.len());
            lines.push((
                idx,
                dec.points
                    .iter()
                    .map(|p| DataPoint::new(p.x + row_offset as f64 + 1.0, p.y))
                    .collect(),
            ));
        }
        if lines.is_empty() {
            return None;
        }
        let x = Bounds::new(row_offset as f64 + 1.0, row_offset as f64 + x_len as f64);
        let title = series
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let legend = series
            .iter()
            .enumerate()
            .map(|(i, (n, _))| LegendEntry::new(n.clone(), SERIES_COLORS[i % SERIES_COLORS.len()]))
            .collect();
        let mut s = Scene::new(x, y)
            .with_axis_labels("row", "value")
            .with_title(title)
            .with_legend(legend)
            .with_scale(scale);
        for (idx, points) in lines {
            s.push(Primitive::Polyline {
                points,
                color: SERIES_COLORS[idx % SERIES_COLORS.len()],
                width: 1.4,
            });
        }
        Some(s)
    }

    /// Multi-series bar: grouped bars — for each category, one bar per Y
    /// column, side by side, each series in its own colour.
    fn build_multi_bar(
        labels: &[String],
        series: &[(String, Vec<Option<f64>>)],
        label_header: &str,
        scale: ScaleHint,
    ) -> Option<Scene> {
        // Aggregate each series over the same label column, then align the
        // categories on the FIRST series' order so groups line up.
        let grouped: Vec<Vec<ferrix_core::chart::Category>> = series
            .iter()
            .map(|(_, values)| group_by(labels, values, Aggregate::Sum))
            .collect();
        let first = grouped.first()?;
        if first.is_empty() {
            return None;
        }
        let shown = first.len().min(40);
        let cats: Vec<&str> = first.iter().take(shown).map(|c| c.label.as_str()).collect();

        let mut y = Bounds::new(0.0, 0.0);
        let mut heights: Vec<Vec<f64>> = Vec::new(); // [series][category]
        for g in &grouped {
            let by_label: std::collections::HashMap<&str, f64> =
                g.iter().map(|c| (c.label.as_str(), c.value)).collect();
            let hs: Vec<f64> = cats
                .iter()
                .map(|l| by_label.get(l).copied().unwrap_or(0.0))
                .collect();
            for &h in &hs {
                y.include(h);
            }
            heights.push(hs);
        }
        y.include(0.0);

        let n = series.len() as f64;
        // Log y on grouped sums has the same zero problem as single-series
        // bars; keep linear unless everything is strictly positive.
        let bar_scale = if scale.y == Scale::Log && y.min > 0.0 {
            scale
        } else {
            ScaleHint::new(scale.x, Scale::Linear)
        };
        let title = format!(
            "{} by {label_header}",
            series
                .iter()
                .map(|(nm, _)| nm.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let legend = series
            .iter()
            .enumerate()
            .map(|(i, (nm, _))| {
                LegendEntry::new(nm.clone(), SERIES_COLORS[i % SERIES_COLORS.len()])
            })
            .collect();
        let mut s = Scene::new(Bounds::new(-0.6, shown as f64 - 0.4), y)
            .with_axis_labels(label_header, "value")
            .with_title(title)
            .with_legend(legend)
            .with_scale(bar_scale)
            .with_categorical_x();
        // Group geometry: the group spans ±0.38 around the category index;
        // each series gets an equal slice.
        let group_w = 0.76;
        let slice = group_w / n;
        for (si, hs) in heights.iter().enumerate() {
            let color = SERIES_COLORS[si % SERIES_COLORS.len()];
            for (ci, &h) in hs.iter().enumerate() {
                let x0 = ci as f64 - group_w / 2.0 + si as f64 * slice;
                s.push(Primitive::Rect {
                    x0: x0 + slice * 0.06,
                    y0: 0.0,
                    x1: x0 + slice * 0.94,
                    y1: h,
                    fill: color,
                    stroke: None,
                });
            }
        }
        for (ci, label) in cats.iter().enumerate() {
            s.push(Primitive::Text {
                at: DataPoint::new(ci as f64, y.min.min(0.0)),
                text: (*label).to_string(),
                size_px: 10.0,
                color: Rgba::rgb(0xa0, 0xa8, 0xb4),
                anchor: Anchor::Middle,
                offset_px: (0.0, 14.0),
            });
        }
        Some(s)
    }

    fn build_scatter(
        xs: &[Option<f64>],
        ys: &[Option<f64>],
        x_header: &str,
        y_header: &str,
        scale: ScaleHint,
    ) -> Option<Scene> {
        let (cells, xb, yb) = density_grid(xs, ys, 100, 70);
        if cells.is_empty() {
            return None;
        }
        let max = cells.iter().map(|c| c.count).max().unwrap_or(1) as f64;
        let (xw, yh) = (xb.span() / 100.0, yb.span() / 70.0);
        // The density cells are binned uniformly in *linear* data space, so a
        // log axis would misplace them (a cell's linear-space corners are not
        // its log-space corners). Scatter therefore ignores a log-y request and
        // keeps a linear mapping; the hint is still accepted for a uniform
        // builder signature. A future log-scatter would re-bin in log space.
        let _ = scale;
        // Both axes are real value columns here, so both carry their header.
        let mut s = Scene::new(xb, yb)
            .with_axis_labels(x_header, y_header)
            .with_title(format!("{y_header} vs {x_header}"))
            .with_legend(vec![LegendEntry::new(
                format!("{y_header} vs {x_header}"),
                SERIES_BLUE,
            )]);
        for c in &cells {
            // sqrt keeps sparse cells visible; a linear ramp makes everything
            // but the densest cell invisible.
            let t = (c.count as f64 / max).sqrt();
            let alpha = (50.0 + t * 205.0) as u8;
            s.push(Primitive::Rect {
                x0: xb.min + c.x_bin as f64 * xw,
                y0: yb.min + c.y_bin as f64 * yh,
                x1: xb.min + (c.x_bin + 1) as f64 * xw,
                y1: yb.min + (c.y_bin + 1) as f64 * yh,
                fill: Rgba(0x4a, 0x9e, 0xff, alpha),
                stroke: None,
            });
        }
        Some(s)
    }

    /// Export the current scene, annotations included, as SVG.
    ///
    /// See [`SVG_FOLLOWS_APP_THEME`]: the exported file is ALWAYS light,
    /// whichever theme the app is in.
    pub fn to_svg(&self, width: f32, height: f32) -> Option<String> {
        let scene = self.scene.as_ref()?;
        // Annotations are drawn into a clone so the on-screen scene is not
        // mutated by exporting it — otherwise every export would duplicate them.
        let mut out = scene.clone();
        self.annotations.draw_into(&mut out);
        Some(to_svg(&out, width, height))
    }
}

fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// **Decision: an exported SVG is always light, and does not follow the app
/// theme.**
///
/// The on-screen chart is chrome — it belongs to the window it is sitting in,
/// so it follows the toggle like everything else. An exported SVG is an
/// artefact that leaves the app: it gets pasted into a document, a slide, a
/// README, a printed report. Those are overwhelmingly light, and a chart
/// carrying a near-black background into one is both jarring and, on paper, a
/// solid rectangle of ink. Exporting dark-on-dark also loses the chart
/// entirely the moment someone drops it on a white page.
///
/// The scene's own DATA colours (the series blue, the bar orange) are chosen
/// in `ferrix_core::scene` to read on a light background and are already
/// theme-independent, so nothing has to be recoloured on the way out — the
/// SVG writer's own `#ffffff` backdrop and grey gridlines are simply left
/// alone. `to_svg` therefore takes no theme, and this constant exists so the
/// choice is greppable and testable rather than implied by an absence.
///
/// If a dark export is ever wanted it should be an explicit export OPTION,
/// not a side effect of what the window happened to look like at the time.
#[allow(dead_code)] // documents a decision; referenced from doc comments
pub const SVG_FOLLOWS_APP_THEME: bool = false;

fn to_color(c: Rgba) -> Color32 {
    Color32::from_rgba_unmultiplied(c.0, c.1, c.2, c.3)
}

/// Paint a scene onto an egui painter.
///
/// This is the second consumer of `Scene`, alongside the SVG writer. Both walk
/// the same primitives, which is what keeps the exported file matching the
/// screen.
pub fn paint_scene(
    ui: &mut egui::Ui,
    scene: &Scene,
    annotations: &Annotations,
    rect: Rect,
    theme: Theme,
) -> (Viewport, egui::Response) {
    // The chart's chrome — backdrop, gridlines, tick labels — follows the app
    // theme. The scene's data colours do not: they are chosen once to read on
    // either backdrop, which is also what lets the SVG export stay light. See
    // `SVG_FOLLOWS_APP_THEME`.
    let chrome = ChartChrome::from(theme);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, chrome.bg);

    // Margins for axis labels; the plot area is what data maps into.
    let (ml, mr, mt, mb) = (58.0, 12.0, 10.0, 30.0);
    let plot = Rect::from_min_size(
        Pos2::new(rect.left() + ml, rect.top() + mt),
        Vec2::new(
            (rect.width() - ml - mr).max(1.0),
            (rect.height() - mt - mb).max(1.0),
        ),
    );

    let vp = Viewport::for_scene(
        (plot.left(), plot.top(), plot.width(), plot.height()),
        scene,
    );

    // --- gridlines and axis labels ---
    // Same source as the SVG writer: `axis_ticks` picks linear or log ticks and
    // formats them, then `elide_overlapping` drops labels that would collide at
    // this pixel width. Screen and export therefore agree on both.
    let (y_ticks, y_labels) = ferrix_core::scene::axis_ticks(scene.y, scene.scale.y, 5);
    let (x_ticks, x_labels) = ferrix_core::scene::axis_ticks(scene.x, scene.scale.x, 6);
    let x_centers: Vec<f32> = x_ticks.iter().map(|t| vp.map_x(*t)).collect();
    let y_centers: Vec<f32> = y_ticks.iter().map(|t| vp.map_y(*t)).collect();
    let x_keep = ferrix_core::scene::elide_overlapping(&x_labels, &x_centers, 10.0);
    let y_keep = ferrix_core::scene::elide_overlapping(&y_labels, &y_centers, 10.0);

    for (i, t) in y_ticks.iter().enumerate() {
        if !y_keep[i] {
            continue;
        }
        let y = vp.map_y(*t);
        painter.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(1.0_f32, chrome.grid),
        );
        painter.text(
            Pos2::new(plot.left() - 6.0, y),
            egui::Align2::RIGHT_CENTER,
            y_labels[i].clone(),
            egui::FontId::proportional(10.0),
            chrome.label,
        );
    }
    for (i, t) in x_ticks.iter().enumerate() {
        // Categorical scenes draw their own labels; numeric ticks under them
        // are the "0 2 4 under Central East West" bug.
        if scene.x_categorical {
            break;
        }
        if !x_keep[i] {
            continue;
        }
        let x = vp.map_x(*t);
        painter.text(
            Pos2::new(x, plot.bottom() + 4.0),
            egui::Align2::CENTER_TOP,
            x_labels[i].clone(),
            egui::FontId::proportional(10.0),
            chrome.label,
        );
    }

    // --- the scene, then annotations on top ---
    let mut with_notes = scene.clone();
    annotations.draw_into(&mut with_notes);

    for p in &with_notes.primitives {
        match p {
            Primitive::Polyline {
                points,
                color,
                width,
            } => {
                if points.len() < 2 {
                    continue;
                }
                let pts: Vec<Pos2> = points
                    .iter()
                    .map(|d| {
                        let (x, y) = vp.map(*d);
                        Pos2::new(x, y)
                    })
                    .collect();
                painter.add(egui::Shape::line(
                    pts,
                    Stroke::new(*width, to_color(*color)),
                ));
            }
            Primitive::Rect {
                x0,
                y0,
                x1,
                y1,
                fill,
                stroke,
            } => {
                let (px0, py0) = vp.map(DataPoint::new(*x0, *y0));
                let (px1, py1) = vp.map(DataPoint::new(*x1, *y1));
                let r = Rect::from_two_pos(Pos2::new(px0, py0), Pos2::new(px1, py1));
                painter.rect_filled(r, 0.0, to_color(*fill));
                if let Some((c, w)) = stroke {
                    painter.rect_stroke(r, 0.0, Stroke::new(*w, to_color(*c)));
                }
            }
            Primitive::Circle {
                center,
                radius_px,
                fill,
            } => {
                let (x, y) = vp.map(*center);
                painter.circle_filled(Pos2::new(x, y), *radius_px, to_color(*fill));
            }
            Primitive::Text {
                at,
                text,
                size_px,
                color,
                anchor,
                offset_px,
            } => {
                let (x, y) = vp.map(*at);
                let align = match anchor {
                    Anchor::Start => egui::Align2::LEFT_CENTER,
                    Anchor::Middle => egui::Align2::CENTER_CENTER,
                    Anchor::End => egui::Align2::RIGHT_CENTER,
                };
                painter.text(
                    Pos2::new(x + offset_px.0, y + offset_px.1),
                    align,
                    text,
                    egui::FontId::proportional(*size_px),
                    to_color(*color),
                );
            }
        }
    }

    // Axis titles.
    if let Some(l) = &scene.x_label {
        painter.text(
            Pos2::new(plot.center().x, rect.bottom() - 2.0),
            egui::Align2::CENTER_BOTTOM,
            l,
            egui::FontId::proportional(11.0),
            chrome.label,
        );
    }
    if let Some(l) = &scene.y_label {
        // Draw the y-axis title rotated up the left margin, mirroring the SVG
        // export so the on-screen chart and the exported file name the same
        // axis the same way.
        let galley =
            painter.layout_no_wrap(l.clone(), egui::FontId::proportional(11.0), chrome.label);
        let center = Pos2::new(rect.left() + 11.0, plot.center().y);
        let mut shape = egui::epaint::TextShape::new(
            center - Vec2::new(galley.size().x / 2.0, galley.size().y / 2.0),
            galley,
            chrome.label,
        );
        shape.angle = -std::f32::consts::FRAC_PI_2;
        painter.add(shape);
    }
    // Chart title, centred along the top.
    if let Some(t) = &scene.title {
        painter.text(
            Pos2::new(plot.center().x, rect.top() + 1.0),
            egui::Align2::CENTER_TOP,
            t,
            egui::FontId::proportional(13.0),
            chrome.label,
        );
    }
    // Series legend, top-right of the plot, matching the SVG layout: a colour
    // swatch beside each series' label.
    if !scene.legend.is_empty() {
        let (sw, row_h, pad) = (12.0f32, 18.0f32, 8.0f32);
        let right = plot.right() - pad;
        let top = plot.top() + pad;
        for (i, e) in scene.legend.iter().enumerate() {
            let y = top + i as f32 * row_h;
            let swatch = Rect::from_min_size(Pos2::new(right - sw, y), Vec2::splat(sw));
            painter.rect_filled(swatch, 0.0, to_color(e.color));
            painter.text(
                Pos2::new(right - sw - 5.0, y + sw / 2.0),
                egui::Align2::RIGHT_CENTER,
                &e.label,
                egui::FontId::proportional(11.0),
                chrome.label,
            );
        }
    }

    let resp = ui.interact(plot, ui.id().with("chart_canvas"), Sense::click());
    (vp, resp)
}

/// Convenience for placing a note at a clicked position.
pub fn note_at(vp: &Viewport, pos: Pos2, text: &str) -> Annotation {
    Annotation::point(
        DataPoint::new(vp.unmap_x(pos.x), vp.unmap_y(pos.y)),
        text.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped_for_reading() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(200_000_000), "200,000,000");
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn exported_svg_is_always_light() {
        // The documented decision, asserted rather than implied: a chart that
        // leaves the app lands in a light document.
        // Reading through a binding keeps this a real assertion rather than
        // a const-folded one clippy objects to.
        let follows = SVG_FOLLOWS_APP_THEME;
        assert!(!follows, "exported SVG is always light; see the const");
        let mut p = ChartPanel::default();
        p.scene = Some({
            let mut s = ferrix_core::scene::Scene::new(
                ferrix_core::chart::Bounds::new(0.0, 10.0),
                ferrix_core::chart::Bounds::new(0.0, 10.0),
            );
            s.push(Primitive::Polyline {
                points: vec![DataPoint::new(0.0, 0.0), DataPoint::new(10.0, 10.0)],
                color: Rgba::rgb(0x4a, 0x9e, 0xff),
                width: 1.0,
            });
            s
        });
        // `to_svg` takes no theme at all, so there is no way for the app's
        // palette to reach the file — and the backdrop is white either way.
        let svg = p.to_svg(400.0, 300.0).expect("scene present");
        assert!(svg.contains(r##"fill="#ffffff""##), "export went dark");
    }

    #[test]
    fn chart_chrome_differs_between_themes() {
        // The on-screen chart, unlike the export, does follow the toggle.
        let d = ChartChrome::from(Theme::dark());
        let l = ChartChrome::from(Theme::light());
        assert_ne!(d.bg, l.bg);
        assert_ne!(d.grid, l.grid);
        assert_ne!(d.label, l.label);
    }

    #[test]
    fn chart_kinds_all_have_labels() {
        for k in ChartKind::ALL {
            assert!(!k.label().is_empty());
        }
    }

    // --- real-column-header regression tests -------------------------------
    //
    // The bug (#83) these pin: the chart panel drew placeholder axis labels
    // "row"/"value"/"x"/"y" and had no legend or title, so a user could not
    // tell which column mapped to which axis. Every assertion below checks for
    // the *specific header string* in the built SVG and, where a placeholder
    // used to sit, that the placeholder is gone — a "non-empty SVG" assertion
    // would pass against the old placeholder output and prove nothing.

    fn svg_of(scene: &Scene) -> String {
        to_svg(scene, 800.0, 400.0)
    }

    #[test]
    fn line_chart_labels_axis_and_title_with_the_value_header() {
        let s = ChartPanel::build_line(
            &[Some(1.0), Some(2.0), Some(3.0)],
            0,
            "revenue",
            ScaleHint::default(),
        )
        .expect("line scene");
        assert_eq!(
            s.y_label.as_deref(),
            Some("revenue"),
            "y axis = value header"
        );
        assert_eq!(s.title.as_deref(), Some("revenue"), "title = value header");
        assert_eq!(s.legend.len(), 1);
        assert_eq!(s.legend[0].label, "revenue");
        assert_eq!(s.legend[0].color, SERIES_BLUE, "legend swatch matches line");

        let svg = svg_of(&s);
        assert!(svg.contains("revenue"), "header text missing from SVG");
        // The old placeholder must not be what the y axis / title render.
        assert!(
            !svg.contains(">value<"),
            "placeholder 'value' label regressed into the SVG"
        );
    }

    #[test]
    fn histogram_labels_x_axis_with_the_value_header() {
        let s = ChartPanel::build_histogram(
            &[Some(1.0), Some(2.0), Some(9.0)],
            "latency_ms",
            ScaleHint::default(),
        )
        .expect("histogram scene");
        assert_eq!(s.x_label.as_deref(), Some("latency_ms"));
        assert_eq!(s.title.as_deref(), Some("Distribution of latency_ms"));

        let svg = svg_of(&s);
        assert!(
            svg.contains("latency_ms"),
            "header missing from histogram SVG"
        );
    }

    #[test]
    fn bar_chart_labels_both_axes_with_real_headers() {
        let labels = vec!["west".to_string(), "east".to_string(), "west".to_string()];
        let s = ChartPanel::build_bar(
            &labels,
            &[Some(1.0), Some(2.0), Some(3.0)],
            "region",
            "sales",
            ScaleHint::default(),
        )
        .expect("bar scene");
        assert_eq!(s.x_label.as_deref(), Some("region"), "x = label header");
        assert_eq!(s.y_label.as_deref(), Some("sum of sales"));
        assert_eq!(s.legend[0].color, SERIES_ORANGE, "legend matches bars");

        let svg = svg_of(&s);
        assert!(svg.contains("region"), "label header missing");
        assert!(svg.contains("sum of sales"), "value header missing");
        assert!(
            !svg.contains(">category<"),
            "placeholder 'category' regressed"
        );
    }

    #[test]
    fn scatter_labels_both_axes_with_real_headers() {
        let xs = vec![Some(1.0), Some(2.0), Some(3.0)];
        let ys = vec![Some(4.0), Some(5.0), Some(6.0)];
        let s = ChartPanel::build_scatter(&xs, &ys, "height", "weight", ScaleHint::default())
            .expect("scatter scene");
        assert_eq!(s.x_label.as_deref(), Some("height"));
        assert_eq!(s.y_label.as_deref(), Some("weight"));

        let svg = svg_of(&s);
        assert!(
            svg.contains("height") && svg.contains("weight"),
            "headers missing"
        );
        // Neither bare placeholder may render as an axis label.
        assert!(
            !svg.contains(">x<") && !svg.contains(">y<"),
            "placeholder axis label regressed"
        );
    }

    #[test]
    fn custom_labels_replace_generated_chart_text_and_reach_svg() {
        let labels = vec!["west".to_string(), "east".to_string(), "west".to_string()];
        let scene = ChartPanel::build_bar(
            &labels,
            &[Some(1.0), Some(2.0), Some(3.0)],
            "G",
            "H",
            ScaleHint::default(),
        )
        .expect("bar scene");
        let mut panel = ChartPanel {
            scene: Some(scene),
            ..Default::default()
        };

        panel.set_custom_labels(ChartLabels {
            title: "Profit by region".into(),
            x_axis: "Region".into(),
            y_axis: "Profit ($)".into(),
            series: "Profit".into(),
        });

        let scene = panel.scene.as_ref().expect("scene remains available");
        assert_eq!(scene.title.as_deref(), Some("Profit by region"));
        assert_eq!(scene.x_label.as_deref(), Some("Region"));
        assert_eq!(scene.y_label.as_deref(), Some("Profit ($)"));
        assert_eq!(scene.legend[0].label, "Profit");

        let svg = panel.to_svg(800.0, 400.0).expect("custom chart exports");
        assert!(svg.contains("Profit by region"));
        assert!(svg.contains("Profit ($)"));
        assert!(!svg.contains("H by G"), "generated title survived override");
        assert!(
            !svg.contains("sum of H"),
            "generated legend survived override"
        );
    }

    /// Wire-through-production guard (per roadmap comment #46): drive the exact
    /// `ChartPanel::build` entry point `app.rs` calls, over a real `SheetView`
    /// carrying named headers, and assert the header reaches the exported SVG.
    /// A refactor that stops threading headers from the selection into the
    /// builders — even if the builders' own unit tests still pass — fails here.
    #[test]
    fn build_through_the_panel_puts_real_headers_in_the_exported_svg() {
        use crate::sheet_view::BaseData;
        use crate::workbook::Workbook;
        use ferrix_core::Sheet;

        let mut sheet = Sheet::new("data");
        sheet.set_headers(vec!["quarter".into(), "revenue".into()]);
        for r in 0..6u32 {
            sheet.set(CellRef::new(r, 0), Value::Number(r as f64));
            sheet.set(CellRef::new(r, 1), Value::Number((r as f64) * 10.0 + 1.0));
        }
        let wb = Workbook::new(BaseData::Memory(sheet));

        let mut panel = ChartPanel::default();
        // Chart the "revenue" column (col 1) as a line, exactly as open_chart
        // would after the user selects that column.
        let sel = Selection::new(CellRef::new(0, 1), CellRef::new(5, 1));
        {
            let view = wb.view();
            panel.build(&view, sel, ChartKind::Line);
        }

        let svg = panel
            .to_svg(800.0, 400.0)
            .expect("panel built a scene to export");
        assert!(
            svg.contains("revenue"),
            "the real column header did not survive the production build+export path;\nSVG was: {svg}"
        );
        assert!(
            !svg.contains(">value<"),
            "the placeholder 'value' label reached the production SVG"
        );
    }
}
