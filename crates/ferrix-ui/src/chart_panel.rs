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
use ferrix_core::scene::{to_svg, Anchor, Primitive, Rgba, Scene, Viewport};
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChartKind {
    Line,
    Bar,
    Histogram,
    Scatter,
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

        // First numeric column in the selection carries the values; a second
        // column, when present, supplies x for scatter or labels for bar.
        let value_col = c1.min(c0 + 1).max(c0);
        let has_second = c1 > c0;

        let values: Vec<Option<f64>> = (r0 as usize..last_row)
            .map(|r| match view.get(CellRef::new(r as u32, value_col)) {
                Value::Number(n) if n.is_finite() => Some(n),
                Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
                _ => None,
            })
            .collect();

        let scene = match kind {
            ChartKind::Line => Self::build_line(&values, r0 as usize),
            ChartKind::Histogram => Self::build_histogram(&values),
            ChartKind::Bar => {
                let labels: Vec<String> = if has_second {
                    (r0 as usize..last_row)
                        .map(|r| view.display(CellRef::new(r as u32, c0)))
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
                Self::build_bar(&labels, &values)
            }
            ChartKind::Scatter => {
                if !has_second {
                    self.status = "Scatter needs two numeric columns: x then y".to_string();
                    self.scene = None;
                    return;
                }
                let xs: Vec<Option<f64>> = (r0 as usize..last_row)
                    .map(|r| match view.get(CellRef::new(r as u32, c0)) {
                        Value::Number(n) if n.is_finite() => Some(n),
                        _ => None,
                    })
                    .collect();
                Self::build_scatter(&xs, &values)
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
            }
            None => {
                self.status = "No numeric data in the selected range".to_string();
                self.scene = None;
            }
        }
    }

    fn build_line(values: &[Option<f64>], row_offset: usize) -> Option<Scene> {
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

        let mut s = Scene::new(x, y).with_axis_labels("row", "value");
        s.push(Primitive::Polyline {
            points,
            color: Rgba::rgb(0x4a, 0x9e, 0xff),
            width: 1.4,
        });
        Some(s)
    }

    fn build_histogram(values: &[Option<f64>]) -> Option<Scene> {
        let bins = histogram(values, 40, None);
        if bins.is_empty() {
            return None;
        }
        let max = bins.iter().map(|b| b.count).max().unwrap_or(1) as f64;
        let mut s = Scene::new(
            Bounds::new(bins[0].lo, bins[bins.len() - 1].hi),
            Bounds::new(0.0, max),
        )
        .with_axis_labels("value", "count");
        for b in &bins {
            s.push(Primitive::Rect {
                x0: b.lo,
                y0: 0.0,
                x1: b.hi,
                y1: b.count as f64,
                fill: Rgba::rgb(0x4a, 0x9e, 0xff),
                stroke: None,
            });
        }
        Some(s)
    }

    fn build_bar(labels: &[String], values: &[Option<f64>]) -> Option<Scene> {
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

        let mut s = Scene::new(Bounds::new(-0.6, shown as f64 - 0.4), y)
            .with_axis_labels("category", "sum");
        for (i, c) in cats.iter().take(shown).enumerate() {
            s.push(Primitive::Rect {
                x0: i as f64 - 0.38,
                y0: 0.0,
                x1: i as f64 + 0.38,
                y1: c.value,
                fill: Rgba::rgb(0xff, 0x9f, 0x40),
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

    fn build_scatter(xs: &[Option<f64>], ys: &[Option<f64>]) -> Option<Scene> {
        let (cells, xb, yb) = density_grid(xs, ys, 100, 70);
        if cells.is_empty() {
            return None;
        }
        let max = cells.iter().map(|c| c.count).max().unwrap_or(1) as f64;
        let (xw, yh) = (xb.span() / 100.0, yb.span() / 70.0);
        let mut s = Scene::new(xb, yb).with_axis_labels("x", "y");
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

    let vp = Viewport::new(
        (plot.left(), plot.top(), plot.width(), plot.height()),
        scene.x,
        scene.y,
    );

    // --- gridlines and axis labels ---
    let y_ticks = ferrix_core::scene::nice_ticks(scene.y, 5);
    let x_ticks = ferrix_core::scene::nice_ticks(scene.x, 6);
    let y_step = y_ticks.windows(2).next().map_or(1.0, |w| w[1] - w[0]);
    let x_step = x_ticks.windows(2).next().map_or(1.0, |w| w[1] - w[0]);

    for t in &y_ticks {
        let y = vp.map_y(*t);
        painter.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(1.0_f32, chrome.grid),
        );
        painter.text(
            Pos2::new(plot.left() - 6.0, y),
            egui::Align2::RIGHT_CENTER,
            ferrix_core::scene::format_tick(*t, y_step),
            egui::FontId::proportional(10.0),
            chrome.label,
        );
    }
    for t in &x_ticks {
        let x = vp.map_x(*t);
        painter.text(
            Pos2::new(x, plot.bottom() + 4.0),
            egui::Align2::CENTER_TOP,
            ferrix_core::scene::format_tick(*t, x_step),
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
    fn exported_svg_is_always_light() {
        // The documented decision, asserted rather than implied: a chart that
        // leaves the app lands in a light document.
        assert!(!SVG_FOLLOWS_APP_THEME);
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
}
