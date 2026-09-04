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

/// How an axis maps data values onto the plot.
///
/// A `Linear` axis divides its range uniformly; a `Log` axis divides it
/// uniformly in log10 space, so each factor of ten occupies the same screen
/// distance. Log is the right default for data that spans many orders of
/// magnitude (populations, prices, error counts) where a linear axis crushes
/// everything below the largest value into the baseline.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Scale {
    #[default]
    Linear,
    Log,
}

impl Scale {
    /// The smallest positive value a log axis will map. A log axis cannot
    /// represent zero or negatives, and values at or below this floor are
    /// clamped to it so a stray non-positive datum does not produce `-inf`.
    const LOG_FLOOR: f64 = 1e-300;

    /// Project a data value into the scale's linear-fraction space.
    ///
    /// For `Linear` this is the identity; for `Log` it is `log10(v)`, with
    /// non-positive values clamped up to [`Scale::LOG_FLOOR`] so the mapping
    /// stays finite. The result is *not* a screen coordinate — callers combine
    /// it with the projected bounds to get a 0..1 fraction across the axis.
    #[inline]
    fn project(self, v: f64) -> f64 {
        match self {
            Scale::Linear => v,
            Scale::Log => v.max(Self::LOG_FLOOR).log10(),
        }
    }

    /// The inverse of [`Scale::project`]: turn a projected coordinate back into
    /// a data value. `Linear` is the identity; `Log` raises ten to the power.
    #[inline]
    fn unproject(self, p: f64) -> f64 {
        match self {
            Scale::Linear => p,
            Scale::Log => 10f64.powf(p),
        }
    }
}

/// A pair of per-axis scale hints, carried on a [`Scene`] so every consumer
/// (the egui painter and the SVG writer) maps and ticks the axes identically.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScaleHint {
    pub x: Scale,
    pub y: Scale,
}

impl ScaleHint {
    pub fn new(x: Scale, y: Scale) -> Self {
        Self { x, y }
    }
}

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

/// A named series and the colour it is drawn in, for the legend.
///
/// The colour is carried as an [`Rgba`] so the legend swatch matches the exact
/// paint the series' primitives use — the same `Scene` feeds both the egui
/// painter and the SVG writer, so a legend that hard-coded a colour string
/// could drift from the geometry it describes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LegendEntry {
    pub label: String,
    pub color: Rgba,
}

impl LegendEntry {
    pub fn new(label: impl Into<String>, color: Rgba) -> Self {
        Self {
            label: label.into(),
            color,
        }
    }
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
    /// Series legend. One entry per drawn series; rendered only when the panel
    /// asks for it (a single-series chart usually communicates the series via
    /// the y-axis label and title instead, so an empty legend draws nothing).
    pub legend: Vec<LegendEntry>,
    /// Per-axis scale hint. Defaults to linear on both axes; a builder sets
    /// this when the data warrants a log axis, and every consumer maps and
    /// ticks the axes from it so the screen and the SVG export agree.
    pub scale: ScaleHint,
    /// True when the x axis is a category index, not a magnitude: the scene
    /// draws its own category labels (bar charts), so consumers must NOT
    /// print numeric x ticks — "0 2 4" under "Central East West" is two axes
    /// fighting over one edge.
    pub x_categorical: bool,
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
            legend: Vec::new(),
            scale: ScaleHint::default(),
            x_categorical: false,
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

    /// Attach a series legend. Replaces any existing entries.
    pub fn with_legend(mut self, entries: Vec<LegendEntry>) -> Self {
        self.legend = entries;
        self
    }

    /// Mark the x axis as categorical: the scene carries its own category
    /// labels, so consumers suppress numeric x ticks.
    pub fn with_categorical_x(mut self) -> Self {
        self.x_categorical = true;
        self
    }

    /// Set the per-axis scale hint (linear or log per axis).
    ///
    /// A log axis cannot represent zero or negative values, so any axis marked
    /// `Log` whose lower bound is non-positive is raised to the first power of
    /// ten at or below its upper bound (or `1.0` as a last resort). This keeps
    /// the mapping finite without silently hiding that the data was clamped —
    /// the axis simply starts at the first decade it can show.
    pub fn with_scale(mut self, scale: ScaleHint) -> Self {
        if scale.x == Scale::Log {
            self.x = sanitize_log_bounds(self.x);
        }
        if scale.y == Scale::Log {
            self.y = sanitize_log_bounds(self.y);
        }
        self.scale = scale;
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
    /// Per-axis scale. A log axis maps in log10 space; the default is linear.
    pub scale: ScaleHint,
}

impl Viewport {
    pub fn new(rect: (f32, f32, f32, f32), x: Bounds, y: Bounds) -> Self {
        Self {
            rect,
            x,
            y,
            scale: ScaleHint::default(),
        }
    }

    /// A viewport that maps `scene`'s data using the scene's own scale hint, so
    /// hit-testing and export share exactly one mapping.
    pub fn for_scene(rect: (f32, f32, f32, f32), scene: &Scene) -> Self {
        Self {
            rect,
            x: scene.x,
            y: scene.y,
            scale: scene.scale,
        }
    }

    /// Set the per-axis scale hint, returning the modified viewport.
    pub fn with_scale(mut self, scale: ScaleHint) -> Self {
        self.scale = scale;
        self
    }

    /// The 0..1 fraction of `v` across `bounds` under `scale`, guarding a
    /// degenerate (zero-span) range by centring.
    #[inline]
    fn fraction(scale: Scale, bounds: Bounds, v: f64) -> Option<f32> {
        let lo = scale.project(bounds.min);
        let hi = scale.project(bounds.max);
        let span = hi - lo;
        if span.abs() < f64::EPSILON {
            return None;
        }
        Some(((scale.project(v) - lo) / span) as f32)
    }

    /// The data value at 0..1 fraction `f` across `bounds` under `scale`.
    #[inline]
    fn unfraction(scale: Scale, bounds: Bounds, f: f32) -> f64 {
        let lo = scale.project(bounds.min);
        let hi = scale.project(bounds.max);
        scale.unproject(lo + f as f64 * (hi - lo))
    }

    /// Data x to device x.
    #[inline]
    pub fn map_x(&self, x: f64) -> f32 {
        let (left, _, w, _) = self.rect;
        match Self::fraction(self.scale.x, self.x, x) {
            Some(f) => left + f * w,
            None => left + w / 2.0,
        }
    }

    /// Data y to device y. Y is flipped: data grows upward, screens grow down.
    #[inline]
    pub fn map_y(&self, y: f64) -> f32 {
        let (_, top, _, h) = self.rect;
        match Self::fraction(self.scale.y, self.y, y) {
            Some(f) => top + h - f * h,
            None => top + h / 2.0,
        }
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
        Self::unfraction(self.scale.x, self.x, (px - left) / w)
    }

    #[inline]
    pub fn unmap_y(&self, py: f32) -> f64 {
        let (_, top, _, h) = self.rect;
        if h.abs() < f32::EPSILON {
            return self.y.min;
        }
        Self::unfraction(self.scale.y, self.y, (top + h - py) / h)
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

/// Clamp a range to something a log axis can display.
///
/// A log axis cannot show zero or negatives. If the lower bound is
/// non-positive, raise it to a decade at or below the upper bound (`1.0` when
/// even the upper bound is non-positive) so `data_to_screen` stays finite. The
/// caller has already decided the axis is logarithmic; this only makes the
/// range representable.
fn sanitize_log_bounds(b: Bounds) -> Bounds {
    if b.is_empty() {
        return Bounds::new(1.0, 10.0);
    }
    let max = if b.max > 0.0 { b.max } else { 1.0 };
    let min = if b.min > 0.0 {
        b.min
    } else {
        // First power of ten at or below `max`, but never above it.
        let decade = 10f64.powf(max.log10().floor());
        decade.min(max).max(f64::MIN_POSITIVE)
    };
    if min >= max {
        // Degenerate after clamping: give it one decade of room.
        Bounds::new(max / 10.0, max)
    } else {
        Bounds::new(min, max)
    }
}

/// Choose log-axis tick positions: one major per power of ten across the range,
/// plus the 2..9 minor ticks inside each decade that fall within `bounds`.
///
/// The result is sorted ascending and always includes at least the decades
/// bracketing the data. Minors let a viewer read intermediate values (a point
/// at 3×10^4 sits clearly between the 10^4 and 10^5 majors) without cluttering
/// the axis with arbitrary fractions.
///
/// `bounds` must be positive; callers reach this only for a log axis, whose
/// bounds are sanitised by [`sanitize_log_bounds`]. Non-positive input yields
/// an empty vec rather than `NaN` ticks.
pub fn log_ticks(bounds: Bounds) -> Vec<f64> {
    if bounds.is_empty() || bounds.min <= 0.0 || bounds.max <= 0.0 {
        return Vec::new();
    }
    let lo_exp = bounds.min.log10().floor() as i32;
    let hi_exp = bounds.max.log10().ceil() as i32;
    // Guard against a pathological range spanning enormous magnitudes.
    let max_decades = 320;
    let mut ticks = Vec::new();
    let mut exp = lo_exp;
    let mut decades = 0;
    while exp <= hi_exp && decades <= max_decades {
        let decade = 10f64.powi(exp);
        for m in 1..=9 {
            let v = m as f64 * decade;
            // Keep ticks within the data range, with a small tolerance so a
            // major sitting exactly on the bound is not lost to rounding.
            if v >= bounds.min * (1.0 - 1e-9) && v <= bounds.max * (1.0 + 1e-9) {
                ticks.push(v);
            }
        }
        exp += 1;
        decades += 1;
    }
    ticks
}

/// Format a log-axis tick. Majors (powers of ten) read as their plain value via
/// [`format_tick`]; minors get the same compact formatting so `2000` shows as
/// `2k`, keeping the label set consistent with the linear axis.
pub fn format_log_tick(v: f64) -> String {
    if v <= 0.0 {
        return "0".to_string();
    }
    let decade = 10f64.powf(v.log10().floor());
    format_tick(v, decade)
}

/// Approximate the rendered width, in device pixels, of a tick label at the
/// axis font size. A proportional sans-serif digit is roughly 0.6em wide; this
/// is deliberately a cheap estimate — the elide pass only needs to know when
/// labels *collide*, not their exact metrics.
fn approx_label_width(text: &str, font_px: f32) -> f32 {
    text.chars().count() as f32 * font_px * 0.6
}

/// Decide which of a set of horizontally-placed tick labels to keep so that no
/// two kept labels overlap.
///
/// `centers` are the device positions of each label's anchor (parallel to
/// `labels`); `font_px` is the label font size. The pass keeps the first and
/// last tick always, then repeatedly drops every other interior label until no
/// two survivors' half-width boxes (plus a small gap) intersect. Returns a
/// boolean keep-mask parallel to the inputs.
///
/// This exists because `nice_ticks` spacing is chosen in *data* space and says
/// nothing about pixel width: at a narrow viewport, or with wide labels like
/// `1.5M`, adjacent labels can still collide. Dropping every other label (a
/// power-of-two thinning) keeps the surviving ticks evenly spaced.
pub fn elide_overlapping(labels: &[String], centers: &[f32], font_px: f32) -> Vec<bool> {
    let n = labels.len();
    let mut keep = vec![true; n];
    if n <= 2 {
        return keep;
    }
    let gap = font_px * 0.35; // minimum whitespace between two labels
    let half: Vec<f32> = labels
        .iter()
        .map(|l| approx_label_width(l, font_px) / 2.0)
        .collect();

    // Does any pair of currently-kept, adjacent-in-keep-order labels overlap?
    let overlaps = |keep: &[bool]| -> bool {
        let mut prev: Option<usize> = None;
        for (i, &k) in keep.iter().enumerate() {
            if !k {
                continue;
            }
            if let Some(p) = prev {
                let dist = (centers[i] - centers[p]).abs();
                if dist < half[i] + half[p] + gap {
                    return true;
                }
            }
            prev = Some(i);
        }
        false
    };

    // Thin interior labels by powers of two until nothing overlaps. `stride`
    // doubles each round: keep indices 0, stride, 2*stride, ... and the last;
    // this preserves even spacing and never drops the first or last tick.
    let last = n - 1;
    let mut stride = 1usize;
    while overlaps(&keep) {
        stride *= 2;
        if stride >= n {
            // Cannot thin further without dropping first/last; keep only the
            // endpoints, which by construction do not overlap unless the
            // viewport is narrower than a single label.
            for k in keep.iter_mut() {
                *k = false;
            }
            keep[0] = true;
            keep[last] = true;
            break;
        }
        for (i, k) in keep.iter_mut().enumerate() {
            *k = i == 0 || i == last || i % stride == 0;
        }
    }
    keep
}

/// Produce the tick values and their formatted labels for one axis, dispatching
/// on the axis scale. This is the single place both the SVG writer and the egui
/// painter derive ticks from, so screen and export never disagree on which
/// values are marked or how they read.
///
/// `target` is the desired number of divisions for a linear axis; it is ignored
/// for a log axis, whose ticks are decade-driven.
pub fn axis_ticks(bounds: Bounds, scale: Scale, target: usize) -> (Vec<f64>, Vec<String>) {
    match scale {
        Scale::Log => {
            let ticks = log_ticks(bounds);
            let labels = ticks.iter().map(|t| format_log_tick(*t)).collect();
            (ticks, labels)
        }
        Scale::Linear => {
            let ticks = nice_ticks(bounds, target);
            let step = ticks.windows(2).next().map_or(1.0, |w| w[1] - w[0]);
            let labels = ticks.iter().map(|t| format_tick(*t, step)).collect();
            (ticks, labels)
        }
    }
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
    let vp = Viewport::for_scene(plot, scene);

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
    // Tick values and labels come from the axis scale: a log axis emits decade
    // majors + 2..9 minors and formats each against its own decade; a linear
    // axis keeps the nice 1/2/5 steps. The overlap-elide pass then drops
    // labels that would collide at this pixel size, never the first or last.
    let (x_ticks, x_labels) = axis_ticks(scene.x, scene.scale.x, 6);
    let (y_ticks, y_labels) = axis_ticks(scene.y, scene.scale.y, 5);

    let x_centers: Vec<f32> = x_ticks.iter().map(|t| vp.map_x(*t)).collect();
    let y_centers: Vec<f32> = y_ticks.iter().map(|t| vp.map_y(*t)).collect();
    let x_keep = elide_overlapping(&x_labels, &x_centers, 11.0);
    let y_keep = elide_overlapping(&y_labels, &y_centers, 11.0);

    let _ = write!(s, r##"<g stroke="#d0d0d0" stroke-width="1">"##);
    for (i, t) in y_ticks.iter().enumerate() {
        if !y_keep[i] {
            continue;
        }
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
    for (i, t) in y_ticks.iter().enumerate() {
        if !y_keep[i] {
            continue;
        }
        let _ = write!(
            s,
            r#"<text x="{:.2}" y="{:.2}" text-anchor="end">{}</text>"#,
            plot.0 - 6.0,
            vp.map_y(*t) + 4.0,
            escape(&y_labels[i])
        );
    }
    for (i, t) in x_ticks.iter().enumerate() {
        // A categorical x axis draws its own labels from the scene; numeric
        // ticks under them would be a second, meaningless axis.
        if scene.x_categorical {
            break;
        }
        if !x_keep[i] {
            continue;
        }
        let _ = write!(
            s,
            r#"<text x="{:.2}" y="{:.2}" text-anchor="middle">{}</text>"#,
            vp.map_x(*t),
            plot.1 + plot.3 + 16.0,
            escape(&x_labels[i])
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

    // Series legend, top-right inside the plot area. One row per series: a
    // colour swatch matching the series' own paint, then its label. Drawn last
    // so it sits above the gridlines and geometry.
    if !scene.legend.is_empty() {
        let sw = 12.0f32; // swatch size
        let row_h = 18.0f32;
        let pad = 8.0f32;
        // Right-align the block just inside the plot's right edge.
        let right = plot.0 + plot.2 - pad;
        let top = plot.1 + pad;
        for (i, e) in scene.legend.iter().enumerate() {
            let y = top + i as f32 * row_h;
            // Label sits to the LEFT of the swatch, right-anchored, so a long
            // series name grows away from the edge rather than off it.
            let _ = write!(
                s,
                r#"<rect x="{:.2}" y="{:.2}" width="{sw:.2}" height="{sw:.2}" fill="{}" fill-opacity="{}"/>"#,
                right - sw,
                y,
                e.color.svg_hex(),
                e.color.opacity()
            );
            let _ = write!(
                s,
                r##"<text x="{:.2}" y="{:.2}" font-family="sans-serif" font-size="11" text-anchor="end" fill="#404040">{}</text>"##,
                right - sw - 5.0,
                y + sw - 2.0,
                escape(&e.label)
            );
        }
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

    // --- log scale -----------------------------------------------------------

    #[test]
    fn log_axis_round_trips_through_unmap() {
        // A log viewport must map and unmap consistently, just like linear.
        let vp = Viewport::new(
            (10.0, 20.0, 200.0, 100.0),
            Bounds::new(1.0, 1000.0),
            Bounds::new(1.0, 1_000_000.0),
        )
        .with_scale(ScaleHint::new(Scale::Log, Scale::Log));
        for x in [1.0, 3.0, 10.0, 100.0, 999.0] {
            let back = vp.unmap_x(vp.map_x(x));
            let rel = (back - x).abs() / x;
            assert!(rel < 1e-4, "log x {x} -> {back} (rel {rel})");
        }
        for y in [1.0, 42.0, 1000.0, 1_000_000.0] {
            let back = vp.unmap_y(vp.map_y(y));
            let rel = (back - y).abs() / y;
            assert!(rel < 1e-4, "log y {y} -> {back} (rel {rel})");
        }
    }

    #[test]
    fn log_axis_spaces_decades_evenly() {
        // Each factor of ten occupies the same screen distance — that is the
        // whole point of a log axis. A linear axis over 1..1000 would put 100
        // almost at the far right; a log axis puts it two thirds across.
        let vp = Viewport::new(
            (0.0, 0.0, 300.0, 100.0),
            Bounds::new(1.0, 1000.0),
            Bounds::new(1.0, 1000.0),
        )
        .with_scale(ScaleHint::new(Scale::Log, Scale::Linear));
        let x1 = vp.map_x(1.0);
        let x10 = vp.map_x(10.0);
        let x100 = vp.map_x(100.0);
        let x1000 = vp.map_x(1000.0);
        let d1 = x10 - x1;
        let d2 = x100 - x10;
        let d3 = x1000 - x100;
        assert!((d1 - d2).abs() < 0.5, "decades uneven: {d1} vs {d2}");
        assert!((d2 - d3).abs() < 0.5, "decades uneven: {d2} vs {d3}");
        // And 100 sits at exactly two-thirds across three decades.
        assert!((x100 - 200.0).abs() < 0.5, "100 at {x100}, expected ~200");
    }

    #[test]
    fn log_ticks_are_decade_majors_plus_minors() {
        // 1..1000 spans three decades. Majors are 1, 10, 100, 1000; minors are
        // 2..9 inside each decade. So 1,2,..,9,10,20,..,90,100,...,1000.
        let t = log_ticks(Bounds::new(1.0, 1000.0));
        for major in [1.0, 10.0, 100.0, 1000.0] {
            assert!(t.contains(&major), "missing decade major {major}: {t:?}");
        }
        for minor in [2.0, 5.0, 20.0, 50.0, 300.0] {
            assert!(t.contains(&minor), "missing minor {minor}: {t:?}");
        }
        // Strictly ascending, all positive.
        for w in t.windows(2) {
            assert!(w[1] > w[0], "not ascending: {t:?}");
            assert!(w[0] > 0.0);
        }
    }

    #[test]
    fn log_ticks_reject_non_positive_bounds() {
        // A log axis cannot show zero or negatives; ticks bail rather than
        // emitting NaN.
        assert!(log_ticks(Bounds::new(0.0, 100.0)).is_empty());
        assert!(log_ticks(Bounds::new(-5.0, 5.0)).is_empty());
        assert!(log_ticks(Bounds::unbounded()).is_empty());
    }

    #[test]
    fn with_scale_lifts_a_non_positive_log_axis_into_range() {
        // A y range of 0..1000 marked log cannot start at 0. `with_scale`
        // raises the lower bound to a displayable decade without panicking or
        // producing an infinite mapping.
        let s = Scene::new(Bounds::new(0.0, 1.0), Bounds::new(0.0, 1000.0))
            .with_scale(ScaleHint::new(Scale::Linear, Scale::Log));
        assert!(s.y.min > 0.0, "log y still starts at {}", s.y.min);
        assert!(s.y.min <= s.y.max);
        let vp = Viewport::for_scene((0.0, 0.0, 100.0, 100.0), &s);
        assert!(vp.map_y(1000.0).is_finite());
        assert!(vp.map_y(s.y.min).is_finite());
    }

    #[test]
    fn log_tick_labels_read_as_plain_values() {
        assert_eq!(format_log_tick(1.0), "1");
        assert_eq!(format_log_tick(100.0), "100");
        // Below the 1e4 k-threshold labels read in full; at and above it they
        // take the same compact suffix `format_tick` gives a linear axis.
        assert_eq!(format_log_tick(2000.0), "2000");
        assert_eq!(format_log_tick(20000.0), "20k");
        assert_eq!(format_log_tick(1_000_000.0), "1M");
    }

    #[test]
    fn log_axis_ticks_flow_through_axis_ticks() {
        // The dispatch both backends share must pick log ticks for a log axis
        // and linear ticks for a linear one.
        let (lt, ll) = axis_ticks(Bounds::new(1.0, 100.0), Scale::Log, 6);
        assert!(lt.contains(&1.0) && lt.contains(&10.0) && lt.contains(&100.0));
        assert_eq!(ll.len(), lt.len());
        let (nt, _) = axis_ticks(Bounds::new(0.0, 100.0), Scale::Linear, 5);
        assert!(nt.contains(&0.0), "linear axis keeps its clean zero");
    }

    // --- overlap elide -------------------------------------------------------

    #[test]
    fn elide_keeps_everything_when_labels_fit() {
        // Widely spaced short labels: nothing to drop.
        let labels: Vec<String> = ["0", "25", "50", "75", "100"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let centers = vec![0.0, 100.0, 200.0, 300.0, 400.0];
        let keep = elide_overlapping(&labels, &centers, 11.0);
        assert!(keep.iter().all(|&k| k), "dropped a label that fit");
    }

    #[test]
    fn elide_drops_every_other_label_when_crowded() {
        // Ten labels crammed into a narrow span must thin out, but the first
        // and last must survive.
        let labels: Vec<String> = (0..10).map(|i| format!("{}", i * 10)).collect();
        let centers: Vec<f32> = (0..10).map(|i| i as f32 * 8.0).collect();
        let keep = elide_overlapping(&labels, &centers, 11.0);
        assert!(keep[0], "dropped the first tick");
        assert!(keep[9], "dropped the last tick");
        let kept = keep.iter().filter(|&&k| k).count();
        assert!(kept < 10, "nothing was elided despite crowding");
        // Surviving labels must not overlap: check consecutive kept centers.
        let half = 11.0 * 0.6; // rough single-char half width upper bound
        let mut prev: Option<f32> = None;
        for (i, &k) in keep.iter().enumerate() {
            if !k {
                continue;
            }
            if let Some(p) = prev {
                assert!(centers[i] - p >= half, "kept labels still overlap");
            }
            prev = Some(centers[i]);
        }
    }

    #[test]
    fn elide_never_drops_first_or_last_even_when_two_labels_touch() {
        // Two labels only: both must always survive, whatever the spacing.
        let labels = vec!["1000000".to_string(), "2000000".to_string()];
        let centers = vec![100.0, 101.0];
        let keep = elide_overlapping(&labels, &centers, 11.0);
        assert_eq!(keep, vec![true, true]);
    }

    #[test]
    fn log_scene_svg_uses_log_labels_and_stays_well_formed() {
        // End-to-end: a log-y scene renders with decade labels and one root.
        let mut s = Scene::new(Bounds::new(0.0, 10.0), Bounds::new(1.0, 100_000.0))
            .with_scale(ScaleHint::new(Scale::Linear, Scale::Log));
        s.push(Primitive::Polyline {
            points: vec![
                DataPoint::new(0.0, 1.0),
                DataPoint::new(5.0, 1000.0),
                DataPoint::new(10.0, 100_000.0),
            ],
            color: Rgba::rgb(0x1f, 0x77, 0xb4),
            width: 1.5,
        });
        let svg = to_svg(&s, 400.0, 300.0);
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert_eq!(svg.matches("<svg").count(), 1);
        // Decade labels present on the log y axis.
        assert!(svg.contains(">100k<"), "missing 100k decade label");
        assert!(svg.contains(">10<"), "missing 10 decade label");
    }
}
