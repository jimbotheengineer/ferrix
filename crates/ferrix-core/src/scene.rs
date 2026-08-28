//! Vector chart scenes: geometry that can be drawn anywhere, at any size.
//!
//! ## Why a scene rather than a canvas
//!
//! The requirement is charts that resize freely. That rules out rasterising
//! to a bitmap: enlarging a raster chart blurs it, and shrinking one destroys
//! the text. So a chart is described as **geometry in data coordinates** —
//! polylines, rectangles, text with anchors — and converted to device
//! coordinates only at draw time.
//!
//! One scene therefore feeds two very different backends without knowing
//! either exists:
//!
//! - the egui painter, redrawing at whatever zoom the user picks
//! - an SVG writer, emitting `<polyline>`/`<rect>`/`<text>` at any size
//!
//! Both consume the same [`Scene`], so what you see is what you export.
//!
//! ## Tick selection
//!
//! Axis ticks land on "nice" numbers — 1, 2, 5 times a power of ten — rather
//! than on arithmetic divisions of the data range. Dividing 0..97 into five
//! equal parts gives ticks at 19.4, 38.8, 58.2, which is technically correct
//! and useless to read. [`nice_ticks`] instead produces 0, 25, 50, 75, 100.

use std::fmt::Write as _;

use crate::chart::{Bounds, DataPoint};

/// Where text sits relative to its anchor point.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Anchor {
    Start,
    Middle,
    End,
}

impl Anchor {
    fn svg(self) -> &'static str {
        match self {
            Anchor::Start => "start",
            Anchor::Middle => "middle",
            Anchor::End => "end",
        }
    }
}

/// An RGBA colour, kept independent of any UI toolkit so `ferrix-core` does
/// not depend on egui.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self(r, g, b, 255)
    }

    fn svg_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    fn opacity(self) -> f32 {
        self.3 as f32 / 255.0
    }
}

/// One drawable primitive, in **data coordinates**.
#[derive(Clone, PartialEq, Debug)]
pub enum Primitive {
    /// Connected line segments. Used for line charts and axis rules.
    Polyline {
        points: Vec<DataPoint>,
        color: Rgba,
        width: f32,
    },
    /// An axis-aligned rectangle from `(x0,y0)` to `(x1,y1)`.
    Rect {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        fill: Rgba,
        stroke: Option<(Rgba, f32)>,
    },
    Circle {
        center: DataPoint,
        /// Radius in *device* pixels: a marker should not grow when zooming.
        radius_px: f32,
        fill: Rgba,
    },
    Text {
        at: DataPoint,
        text: String,
        size_px: f32,
        color: Rgba,
        anchor: Anchor,
        /// Offset in device pixels, for nudging labels clear of the geometry
        /// they annotate without disturbing their data anchor.
        offset_px: (f32, f32),
    },
}

/// A complete chart: primitives plus the data ranges they live in.
#[derive(Clone, Debug)]
pub struct Scene {
    pub primitives: Vec<Primitive>,
    pub x: Bounds,
    pub y: Bounds,
    pub title: Option<String>,
    pub x_label: Option<String>,
    pub y_label: Option<String>,
}

impl Scene {
    pub fn new(x: Bounds, y: Bounds) -> Self {
        Self {
            primitives: Vec::new(),
            x: x.padded(),
            y: y.padded(),
            title: None,
            x_label: None,
            y_label: None,
        }
    }

    pub fn push(&mut self, p: Primitive) {
        self.primitives.push(p);
    }

    pub fn with_title(mut self, t: impl Into<String>) -> Self {
        self.title = Some(t.into());
        self
    }

    pub fn with_axis_labels(mut self, x: impl Into<String>, y: impl Into<String>) -> Self {
        self.x_label = Some(x.into());
        self.y_label = Some(y.into());
        self
    }

    /// Number of primitives — the quantity that must stay bounded no matter
    /// how many rows were aggregated.
    pub fn len(&self) -> usize {
        self.primitives.len()
    }

    pub fn is_empty(&self) -> bool {
        self.primitives.is_empty()
    }
}

/// Maps data coordinates to device pixels.
///
/// Held separately from the scene so the same geometry can be drawn at any
/// size: only the viewport changes.
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    /// Plot area in device pixels: (left, top, width, height).
    pub rect: (f32, f32, f32, f32),
    pub x: Bounds,
    pub y: Bounds,
}

impl Viewport {
    pub fn new(rect: (f32, f32, f32, f32), x: Bounds, y: Bounds) -> Self {
        Self { rect, x, y }
    }

    /// Data x to device x.
    #[inline]
    pub fn map_x(&self, x: f64) -> f32 {
        let (left, _, w, _) = self.rect;
        let span = self.x.span();
        if span.abs() < f64::EPSILON {
            return left + w / 2.0;
        }
        left + (((x - self.x.min) / span) as f32) * w
    }

    /// Data y to device y. Y is flipped: data grows upward, screens grow down.
    #[inline]
    pub fn map_y(&self, y: f64) -> f32 {
        let (_, top, _, h) = self.rect;
        let span = self.y.span();
        if span.abs() < f64::EPSILON {
            return top + h / 2.0;
        }
        top + h - (((y - self.y.min) / span) as f32) * h
    }

    #[inline]
    pub fn map(&self, p: DataPoint) -> (f32, f32) {
        (self.map_x(p.x), self.map_y(p.y))
    }

    /// Device x back to data x — for hit-testing and cursor readouts.
    #[inline]
    pub fn unmap_x(&self, px: f32) -> f64 {
        let (left, _, w, _) = self.rect;
        if w.abs() < f32::EPSILON {
            return self.x.min;
        }
        self.x.min + ((px - left) / w) as f64 * self.x.span()
    }

    #[inline]
    pub fn unmap_y(&self, py: f32) -> f64 {
        let (_, top, _, h) = self.rect;
        if h.abs() < f32::EPSILON {
            return self.y.min;
        }
        self.y.min + ((top + h - py) / h) as f64 * self.y.span()
    }
}

/// Choose axis tick positions on human-readable values.
///
/// Returns ticks at multiples of 1, 2, or 5 times a power of ten, covering
/// `bounds` with roughly `target` divisions. The count is approximate on
/// purpose: readable steps matter more than an exact number of them.
pub fn nice_ticks(bounds: Bounds, target: usize) -> Vec<f64> {
    if bounds.is_empty() || target == 0 {
        return Vec::new();
    }
    let span = bounds.span();
    if span.abs() < f64::EPSILON {
        return vec![bounds.min];
    }

    let raw = span / target as f64;
    let magnitude = 10f64.powf(raw.abs().log10().floor());
    let normalized = raw / magnitude;
    // Pick the nicest step at or above the raw interval.
    let step = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    } * magnitude;

    let first = (bounds.min / step).ceil() * step;
    let mut ticks = Vec::new();
    let mut v = first;
    // Guard against a pathological step producing an unbounded loop.
    let max_ticks = target * 4 + 10;
    while v <= bounds.max + step * 1e-9 && ticks.len() < max_ticks {
        // Snap values that are within rounding error of zero, so an axis
        // shows "0" rather than "-0.0000000001".
        ticks.push(if v.abs() < step * 1e-9 { 0.0 } else { v });
        v += step;
    }
    ticks
}

/// Format a tick value compactly, without trailing noise.
pub fn format_tick(v: f64, step: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    let abs = v.abs();
    // Large magnitudes get SI-ish suffixes; a chart axis reading
    // "1200000000" is unreadable.
    if abs >= 1e9 {
        return trim_zeros(v / 1e9, 1) + "B";
    }
    if abs >= 1e6 {
        return trim_zeros(v / 1e6, 1) + "M";
    }
    if abs >= 1e4 {
        return trim_zeros(v / 1e3, 1) + "k";
    }
    // Decimal places follow the step size, so a 0.25 step shows 2 places and
    // a 25 step shows none.
    let decimals = if step >= 1.0 {
        0
    } else {
        (-step.log10().floor()) as usize
    };
    format!("{v:.decimals$}")
}

fn trim_zeros(v: f64, max_decimals: usize) -> String {
    let s = format!("{v:.max_decimals$}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// Render a scene to standalone SVG at the given pixel size.
///
/// The output is resolution independent: the same scene emitted at 400x300 or
/// 4000x3000 produces identical geometry with different coordinates, and any
/// SVG consumer can rescale it further without loss.
pub fn to_svg(scene: &Scene, width: f32, height: f32) -> String {
    // Margins leave room for axis labels and the title.
    let (ml, mr, mt, mb) = (64.0f32, 16.0f32, 32.0f32, 44.0f32);
    let plot = (
        ml,
        mt,
        (width - ml - mr).max(1.0),
        (height - mt - mb).max(1.0),
    );
    let vp = Viewport::new(plot, scene.x, scene.y);

    let mut s = String::with_capacity(4096);
    let _ = write!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">"#
    );
    let _ = write!(
        s,
        r##"<rect width="{width}" height="{height}" fill="#ffffff"/>"##
    );

    // --- axes ---
    let x_ticks = nice_ticks(scene.x, 6);
    let y_ticks = nice_ticks(scene.y, 5);
    let x_step = x_ticks.windows(2).next().map_or(1.0, |w| w[1] - w[0]);
    let y_step = y_ticks.windows(2).next().map_or(1.0, |w| w[1] - w[0]);

    let _ = write!(s, r##"<g stroke="#d0d0d0" stroke-width="1">"##);
    for t in &y_ticks {
        let y = vp.map_y(*t);
        let _ = write!(
            s,
            r#"<line x1="{:.2}" y1="{y:.2}" x2="{:.2}" y2="{y:.2}"/>"#,
            plot.0,
            plot.0 + plot.2
        );
    }
    let _ = write!(s, "</g>");

    let _ = write!(
        s,
        r##"<g font-family="sans-serif" font-size="11" fill="#404040">"##
    );
    for t in &y_ticks {
        let _ = write!(
            s,
            r#"<text x="{:.2}" y="{:.2}" text-anchor="end">{}</text>"#,
            plot.0 - 6.0,
            vp.map_y(*t) + 4.0,
            escape(&format_tick(*t, y_step))
        );
    }
    for t in &x_ticks {
        let _ = write!(
            s,
            r#"<text x="{:.2}" y="{:.2}" text-anchor="middle">{}</text>"#,
            vp.map_x(*t),
            plot.1 + plot.3 + 16.0,
            escape(&format_tick(*t, x_step))
        );
    }
    let _ = write!(s, "</g>");

    // --- primitives ---
    for p in &scene.primitives {
        match p {
            Primitive::Polyline {
                points,
                color,
                width,
            } => {
                if points.is_empty() {
                    continue;
                }
                let mut d = String::with_capacity(points.len() * 16);
                for pt in points.iter() {
                    let (x, y) = vp.map(*pt);
                    let _ = write!(d, "{x:.2},{y:.2} ");
                }
                let _ = write!(
                    s,
                    r#"<polyline points="{}" fill="none" stroke="{}" stroke-opacity="{}" stroke-width="{width}"/>"#,
                    d.trim_end(),
                    color.svg_hex(),
                    color.opacity()
                );
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
                let (x, y) = (px0.min(px1), py0.min(py1));
                let (w, h) = ((px1 - px0).abs(), (py1 - py0).abs());
                let stroke_attr = match stroke {
                    Some((c, sw)) => {
                        format!(r#" stroke="{}" stroke-width="{sw}""#, c.svg_hex())
                    }
                    None => String::new(),
                };
                let _ = write!(
                    s,
                    r#"<rect x="{x:.2}" y="{y:.2}" width="{w:.2}" height="{h:.2}" fill="{}" fill-opacity="{}"{stroke_attr}/>"#,
                    fill.svg_hex(),
                    fill.opacity()
                );
            }
            Primitive::Circle {
                center,
                radius_px,
                fill,
            } => {
                let (x, y) = vp.map(*center);
                let _ = write!(
                    s,
                    r#"<circle cx="{x:.2}" cy="{y:.2}" r="{radius_px}" fill="{}" fill-opacity="{}"/>"#,
                    fill.svg_hex(),
                    fill.opacity()
                );
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
                let _ = write!(
                    s,
                    r#"<text x="{:.2}" y="{:.2}" font-family="sans-serif" font-size="{size_px}" fill="{}" text-anchor="{}">{}</text>"#,
                    x + offset_px.0,
                    y + offset_px.1,
                    color.svg_hex(),
                    anchor.svg(),
                    escape(text)
                );
            }
        }
    }

    // --- chrome ---
    if let Some(t) = &scene.title {
        let _ = write!(
            s,
            r##"<text x="{:.2}" y="20" font-family="sans-serif" font-size="14" font-weight="bold" text-anchor="middle" fill="#202020">{}</text>"##,
            width / 2.0,
            escape(t)
        );
    }
    if let Some(l) = &scene.x_label {
        let _ = write!(
            s,
            r##"<text x="{:.2}" y="{:.2}" font-family="sans-serif" font-size="12" text-anchor="middle" fill="#404040">{}</text>"##,
            width / 2.0,
            height - 8.0,
            escape(l)
        );
    }
    if let Some(l) = &scene.y_label {
        // Rotated so a long label does not consume horizontal space.
        let _ = write!(
            s,
            r##"<text transform="translate(14,{:.2}) rotate(-90)" font-family="sans-serif" font-size="12" text-anchor="middle" fill="#404040">{}</text>"##,
            plot.1 + plot.3 / 2.0,
            escape(l)
        );
    }

    let _ = write!(s, "</svg>");
    s
}

/// Escape the five XML metacharacters. Without this a cell containing `&` or
/// `<` produces malformed SVG that no viewer will open.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::{decimate_min_max, Bounds};

    fn scene_with_line() -> Scene {
        let mut s = Scene::new(Bounds::new(0.0, 10.0), Bounds::new(0.0, 100.0));
        s.push(Primitive::Polyline {
            points: vec![
                DataPoint::new(0.0, 0.0),
                DataPoint::new(5.0, 50.0),
                DataPoint::new(10.0, 100.0),
            ],
            color: Rgba::rgb(0x1f, 0x77, 0xb4),
            width: 1.5,
        });
        s
    }

    #[test]
    fn viewport_maps_corners_correctly() {
        let vp = Viewport::new(
            (0.0, 0.0, 100.0, 50.0),
            Bounds::new(0.0, 10.0),
            Bounds::new(0.0, 100.0),
        );
        assert_eq!(vp.map_x(0.0), 0.0);
        assert_eq!(vp.map_x(10.0), 100.0);
        assert_eq!(vp.map_x(5.0), 50.0);
        // Y is flipped: data max sits at the TOP of the rect.
        assert_eq!(vp.map_y(100.0), 0.0);
        assert_eq!(vp.map_y(0.0), 50.0);
    }

    #[test]
    fn viewport_round_trips_through_unmap() {
        let vp = Viewport::new(
            (10.0, 20.0, 200.0, 100.0),
            Bounds::new(-5.0, 15.0),
            Bounds::new(0.0, 1000.0),
        );
        for x in [-5.0, 0.0, 7.5, 15.0] {
            let back = vp.unmap_x(vp.map_x(x));
            assert!((back - x).abs() < 1e-6, "x {x} -> {back}");
        }
        for y in [0.0, 250.0, 1000.0] {
            let back = vp.unmap_y(vp.map_y(y));
            assert!((back - y).abs() < 1e-3, "y {y} -> {back}");
        }
    }

    #[test]
    fn viewport_survives_a_degenerate_range() {
        // Constant data: every point maps to the middle rather than dividing
        // by zero or producing NaN.
        let vp = Viewport::new(
            (0.0, 0.0, 100.0, 50.0),
            Bounds::new(5.0, 5.0),
            Bounds::new(7.0, 7.0),
        );
        assert_eq!(vp.map_x(5.0), 50.0);
        assert_eq!(vp.map_y(7.0), 25.0);
        assert!(vp.map_x(5.0).is_finite());
    }

    #[test]
    fn ticks_land_on_round_numbers() {
        // The whole point: 0..97 must not produce ticks at 19.4, 38.8, ...
        // A raw step of 19.4 rounds up to the nice step 20.
        let t = nice_ticks(Bounds::new(0.0, 97.0), 5);
        assert!(t.contains(&0.0));
        assert_eq!(t, vec![0.0, 20.0, 40.0, 60.0, 80.0], "got {t:?}");

        // Every tick must be a whole multiple of a 1/2/5 x 10^n step.
        for v in &t {
            assert!((v / 20.0).fract().abs() < 1e-9, "{v} is not a round tick");
        }
    }

    #[test]
    fn ticks_work_across_magnitudes() {
        for (lo, hi) in [
            (0.0, 1.0),
            (0.0, 1e6),
            (-50.0, 50.0),
            (0.001, 0.01),
            (1e9, 2e9),
        ] {
            let t = nice_ticks(Bounds::new(lo, hi), 5);
            assert!(!t.is_empty(), "no ticks for {lo}..{hi}");
            assert!(
                t.len() < 40,
                "runaway tick count for {lo}..{hi}: {}",
                t.len()
            );
            for v in &t {
                assert!(
                    *v >= lo - (hi - lo) && *v <= hi + (hi - lo),
                    "tick {v} far outside {lo}..{hi}"
                );
            }
        }
    }

    #[test]
    fn ticks_include_a_clean_zero() {
        let t = nice_ticks(Bounds::new(-10.0, 10.0), 4);
        let zero = t.iter().find(|v| v.abs() < 1e-9).expect("zero tick");
        // Must be exactly 0.0, not -0.0 or 1e-17, so it formats as "0".
        assert_eq!(*zero, 0.0);
        assert_eq!(format_tick(*zero, 5.0), "0");
    }

    #[test]
    fn empty_bounds_produce_no_ticks() {
        assert!(nice_ticks(Bounds::unbounded(), 5).is_empty());
        assert_eq!(nice_ticks(Bounds::new(3.0, 3.0), 5), vec![3.0]);
    }

    #[test]
    fn tick_labels_are_readable() {
        assert_eq!(format_tick(0.0, 1.0), "0");
        assert_eq!(format_tick(25.0, 25.0), "25");
        assert_eq!(format_tick(1_500_000.0, 500_000.0), "1.5M");
        assert_eq!(format_tick(2_000_000_000.0, 1e9), "2B");
        assert_eq!(format_tick(50_000.0, 10_000.0), "50k");
        assert_eq!(format_tick(0.25, 0.25), "0.2");
    }

    #[test]
    fn svg_is_well_formed_and_scales() {
        let s = scene_with_line()
            .with_title("Test")
            .with_axis_labels("x", "y");
        let small = to_svg(&s, 400.0, 300.0);
        let large = to_svg(&s, 4000.0, 3000.0);

        for svg in [&small, &large] {
            assert!(svg.starts_with("<svg"));
            assert!(svg.ends_with("</svg>"));
            assert!(svg.contains("<polyline"));
            assert_eq!(svg.matches("<svg").count(), 1, "exactly one root element");
        }
        // Same geometry, different coordinates — this is what "resizable"
        // means: no rasterisation at any size.
        assert!(small.contains(r#"width="400""#));
        assert!(large.contains(r#"width="4000""#));
        assert_eq!(
            small.matches("<polyline").count(),
            large.matches("<polyline").count()
        );
    }

    #[test]
    fn svg_escapes_xml_metacharacters() {
        // A category label containing & or < must not produce broken SVG.
        let mut s = Scene::new(Bounds::new(0.0, 1.0), Bounds::new(0.0, 1.0));
        s.push(Primitive::Text {
            at: DataPoint::new(0.5, 0.5),
            text: "R&D <\"tag\">".to_string(),
            size_px: 12.0,
            color: Rgba::rgb(0, 0, 0),
            anchor: Anchor::Middle,
            offset_px: (0.0, 0.0),
        });
        let svg = to_svg(&s, 200.0, 100.0);
        assert!(svg.contains("R&amp;D"));
        assert!(svg.contains("&lt;&quot;tag&quot;&gt;"));
        assert!(
            !svg.contains("R&D <\"tag\">"),
            "raw metacharacters leaked into the SVG"
        );
    }

    #[test]
    fn scene_from_decimated_data_stays_small() {
        // End-to-end with the aggregation layer: a million rows must produce
        // one polyline with a bounded point count, not a million primitives.
        let data: Vec<Option<f64>> = (0..1_000_000)
            .map(|i| Some((i as f64 / 1000.0).sin() * 100.0))
            .collect();
        let series = decimate_min_max(&data, 400);

        let mut y = Bounds::unbounded();
        for p in &series.points {
            y.include(p.y);
        }
        let mut scene = Scene::new(Bounds::new(0.0, 1_000_000.0), y);
        scene.push(Primitive::Polyline {
            points: series.points.clone(),
            color: Rgba::rgb(0x1f, 0x77, 0xb4),
            width: 1.0,
        });

        assert_eq!(scene.len(), 1, "one polyline regardless of row count");
        assert!(
            series.points.len() <= 800,
            "400 buckets -> at most 800 points, got {}",
            series.points.len()
        );

        let svg = to_svg(&scene, 800.0, 400.0);
        assert!(svg.contains("<polyline"));
        // Sanity: the SVG stays a sensible size rather than megabytes.
        assert!(svg.len() < 100_000, "svg is {} bytes", svg.len());
    }

    #[test]
    fn empty_scene_still_renders() {
        let s = Scene::new(Bounds::new(0.0, 1.0), Bounds::new(0.0, 1.0));
        let svg = to_svg(&s, 100.0, 100.0);
        assert!(svg.starts_with("<svg"));
        assert!(svg.ends_with("</svg>"));
    }
}
