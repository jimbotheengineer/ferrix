//! Chart annotations: user-authored marks anchored to data.
//!
//! ## Why annotations anchor to data, not to pixels
//!
//! An annotation records *why* a number matters — "outage started here",
//! "revised figure". If it were stored at pixel (412, 88) it would drift the
//! moment the chart is resized, the window is rescaled, or the axis range
//! changes, and would end up pointing at the wrong data. Worse, it would
//! point somewhere *plausible but wrong*, which is the failure mode a reader
//! cannot detect.
//!
//! So an annotation stores a position in **data coordinates**. Resizing the
//! chart moves the pixels; the annotation stays on its data point. That is
//! also what lets it survive an SVG export at any size.
//!
//! Annotations live alongside the chart spec, not inside the dataset: they
//! must never modify the user's data.

use crate::chart::DataPoint;
use crate::scene::{Anchor, Primitive, Rgba, Scene, Viewport};

/// What an annotation is attached to.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AnnotationKind {
    /// A note at a single data point, optionally with a leader line.
    Point { at: DataPoint },
    /// A vertical rule at an x value — "deploy happened here".
    VLine { x: f64 },
    /// A horizontal rule at a y value — "SLA threshold".
    HLine { y: f64 },
    /// A shaded x span — "maintenance window".
    XBand { x0: f64, x1: f64 },
    /// A shaded rectangle in data space.
    Region { x0: f64, y0: f64, x1: f64, y1: f64 },
}

/// A user-authored annotation.
#[derive(Clone, PartialEq, Debug)]
pub struct Annotation {
    pub kind: AnnotationKind,
    pub text: String,
    pub color: Rgba,
    /// Label offset in device pixels. Kept in pixels deliberately: this is a
    /// nudge to avoid overlapping ink, not a data position, so it should not
    /// scale with the axis range.
    pub label_offset_px: (f32, f32),
    /// Draw a leader line from the label back to the anchor point.
    pub leader: bool,
}

impl Annotation {
    pub fn point(at: DataPoint, text: impl Into<String>) -> Self {
        Self {
            kind: AnnotationKind::Point { at },
            text: text.into(),
            color: Rgba::rgb(0xd6, 0x28, 0x2c),
            label_offset_px: (8.0, -8.0),
            leader: true,
        }
    }

    pub fn vline(x: f64, text: impl Into<String>) -> Self {
        Self {
            kind: AnnotationKind::VLine { x },
            text: text.into(),
            color: Rgba::rgb(0xd6, 0x28, 0x2c),
            label_offset_px: (4.0, 12.0),
            leader: false,
        }
    }

    pub fn hline(y: f64, text: impl Into<String>) -> Self {
        Self {
            kind: AnnotationKind::HLine { y },
            text: text.into(),
            color: Rgba::rgb(0x2c, 0xa0, 0x2c),
            label_offset_px: (6.0, -6.0),
            leader: false,
        }
    }

    pub fn band(x0: f64, x1: f64, text: impl Into<String>) -> Self {
        Self {
            kind: AnnotationKind::XBand { x0, x1 },
            text: text.into(),
            color: Rgba(0xff, 0xa5, 0x00, 0x40),
            label_offset_px: (4.0, 12.0),
            leader: false,
        }
    }

    pub fn with_color(mut self, c: Rgba) -> Self {
        self.color = c;
        self
    }

    /// The data point this annotation is anchored to, used for hit-testing
    /// and for keeping it attached across resizes.
    pub fn anchor_point(&self) -> DataPoint {
        match self.kind {
            AnnotationKind::Point { at } => at,
            AnnotationKind::VLine { x } => DataPoint::new(x, f64::NAN),
            AnnotationKind::HLine { y } => DataPoint::new(f64::NAN, y),
            AnnotationKind::XBand { x0, x1 } => DataPoint::new((x0 + x1) / 2.0, f64::NAN),
            AnnotationKind::Region { x0, y0, x1, y1 } => {
                DataPoint::new((x0 + x1) / 2.0, (y0 + y1) / 2.0)
            }
        }
    }

    /// Distance in device pixels from `(px, py)` to this annotation's anchor,
    /// for picking one with the mouse. Axis-rule annotations measure only
    /// along their meaningful axis.
    pub fn distance_px(&self, vp: &Viewport, px: f32, py: f32) -> f32 {
        match self.kind {
            AnnotationKind::Point { at } => {
                let (ax, ay) = vp.map(at);
                ((ax - px).powi(2) + (ay - py).powi(2)).sqrt()
            }
            AnnotationKind::VLine { x } => (vp.map_x(x) - px).abs(),
            AnnotationKind::HLine { y } => (vp.map_y(y) - py).abs(),
            AnnotationKind::XBand { x0, x1 } => {
                let (a, b) = (vp.map_x(x0), vp.map_x(x1));
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                if px >= lo && px <= hi {
                    0.0
                } else {
                    (lo - px).abs().min((hi - px).abs())
                }
            }
            AnnotationKind::Region { x0, y0, x1, y1 } => {
                let (ax, ay) = vp.map(DataPoint::new((x0 + x1) / 2.0, (y0 + y1) / 2.0));
                ((ax - px).powi(2) + (ay - py).powi(2)).sqrt()
            }
        }
    }
}

/// A chart's annotation layer.
#[derive(Clone, Default, Debug)]
pub struct Annotations {
    items: Vec<Annotation>,
}

impl Annotations {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, a: Annotation) -> usize {
        self.items.push(a);
        self.items.len() - 1
    }

    pub fn remove(&mut self, index: usize) -> Option<Annotation> {
        if index < self.items.len() {
            Some(self.items.remove(index))
        } else {
            None
        }
    }

    pub fn get(&self, index: usize) -> Option<&Annotation> {
        self.items.get(index)
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut Annotation> {
        self.items.get_mut(index)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Annotation> {
        self.items.iter()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Find the annotation nearest a device point, within `max_px`.
    pub fn pick(&self, vp: &Viewport, px: f32, py: f32, max_px: f32) -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;
        for (i, a) in self.items.iter().enumerate() {
            let d = a.distance_px(vp, px, py);
            if d <= max_px && best.is_none_or(|(_, bd)| d < bd) {
                best = Some((i, d));
            }
        }
        best.map(|(i, _)| i)
    }

    /// Append this layer's geometry to a scene, in data coordinates.
    ///
    /// Annotations are emitted last so they draw over the data, and they
    /// extend axis rules to the scene's current bounds rather than to
    /// remembered pixel positions.
    pub fn draw_into(&self, scene: &mut Scene) {
        let (xmin, xmax) = (scene.x.min, scene.x.max);
        let (ymin, ymax) = (scene.y.min, scene.y.max);

        for a in &self.items {
            match a.kind {
                AnnotationKind::Point { at } => {
                    scene.push(Primitive::Circle {
                        center: at,
                        radius_px: 4.0,
                        fill: a.color,
                    });
                    if a.leader && !a.text.is_empty() {
                        // A short leader keeps the label clear of the marker
                        // without detaching it.
                        scene.push(Primitive::Polyline {
                            points: vec![at, at],
                            color: a.color,
                            width: 1.0,
                        });
                    }
                    if !a.text.is_empty() {
                        scene.push(Primitive::Text {
                            at,
                            text: a.text.clone(),
                            size_px: 11.0,
                            color: a.color,
                            anchor: Anchor::Start,
                            offset_px: a.label_offset_px,
                        });
                    }
                }
                AnnotationKind::VLine { x } => {
                    scene.push(Primitive::Polyline {
                        points: vec![DataPoint::new(x, ymin), DataPoint::new(x, ymax)],
                        color: a.color,
                        width: 1.0,
                    });
                    if !a.text.is_empty() {
                        scene.push(Primitive::Text {
                            at: DataPoint::new(x, ymax),
                            text: a.text.clone(),
                            size_px: 11.0,
                            color: a.color,
                            anchor: Anchor::Start,
                            offset_px: a.label_offset_px,
                        });
                    }
                }
                AnnotationKind::HLine { y } => {
                    scene.push(Primitive::Polyline {
                        points: vec![DataPoint::new(xmin, y), DataPoint::new(xmax, y)],
                        color: a.color,
                        width: 1.0,
                    });
                    if !a.text.is_empty() {
                        scene.push(Primitive::Text {
                            at: DataPoint::new(xmax, y),
                            text: a.text.clone(),
                            size_px: 11.0,
                            color: a.color,
                            anchor: Anchor::End,
                            offset_px: a.label_offset_px,
                        });
                    }
                }
                AnnotationKind::XBand { x0, x1 } => {
                    scene.push(Primitive::Rect {
                        x0,
                        y0: ymin,
                        x1,
                        y1: ymax,
                        fill: a.color,
                        stroke: None,
                    });
                    if !a.text.is_empty() {
                        scene.push(Primitive::Text {
                            at: DataPoint::new(x0, ymax),
                            text: a.text.clone(),
                            size_px: 11.0,
                            color: Rgba(a.color.0, a.color.1, a.color.2, 255),
                            anchor: Anchor::Start,
                            offset_px: a.label_offset_px,
                        });
                    }
                }
                AnnotationKind::Region { x0, y0, x1, y1 } => {
                    scene.push(Primitive::Rect {
                        x0,
                        y0,
                        x1,
                        y1,
                        fill: a.color,
                        stroke: Some((a.color, 1.0)),
                    });
                    if !a.text.is_empty() {
                        scene.push(Primitive::Text {
                            at: DataPoint::new(x0, y1),
                            text: a.text.clone(),
                            size_px: 11.0,
                            color: Rgba(a.color.0, a.color.1, a.color.2, 255),
                            anchor: Anchor::Start,
                            offset_px: a.label_offset_px,
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::Bounds;
    use crate::scene::to_svg;

    fn vp() -> Viewport {
        Viewport::new(
            (0.0, 0.0, 100.0, 100.0),
            Bounds::new(0.0, 100.0),
            Bounds::new(0.0, 100.0),
        )
    }

    #[test]
    fn annotations_stay_on_their_data_point_when_resized() {
        // The core guarantee. The same annotation, drawn into two viewports
        // of very different size, must map to the SAME data coordinate --
        // proportionally the same place, not the same pixel.
        let a = Annotation::point(DataPoint::new(50.0, 50.0), "spike");

        let small = Viewport::new(
            (0.0, 0.0, 100.0, 100.0),
            Bounds::new(0.0, 100.0),
            Bounds::new(0.0, 100.0),
        );
        let large = Viewport::new(
            (0.0, 0.0, 1000.0, 800.0),
            Bounds::new(0.0, 100.0),
            Bounds::new(0.0, 100.0),
        );

        let p = a.anchor_point();
        let (sx, sy) = small.map(p);
        let (lx, ly) = large.map(p);

        assert_eq!((sx / 100.0), (lx / 1000.0), "x drifted on resize");
        assert_eq!((sy / 100.0), (ly / 800.0), "y drifted on resize");

        // And unmapping returns the original data point in both.
        assert!((small.unmap_x(sx) - 50.0).abs() < 1e-9);
        assert!((large.unmap_x(lx) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn annotations_follow_the_data_when_the_axis_range_changes() {
        // Zooming the chart changes the visible range; a pixel-anchored note
        // would now point at the wrong value. A data-anchored one does not.
        let a = Annotation::point(DataPoint::new(50.0, 50.0), "here");
        let zoomed = Viewport::new(
            (0.0, 0.0, 100.0, 100.0),
            Bounds::new(40.0, 60.0),
            Bounds::new(40.0, 60.0),
        );
        let (x, y) = zoomed.map(a.anchor_point());
        // 50 is the midpoint of 40..60, so it must be centred.
        assert!((x - 50.0).abs() < 1e-4, "x = {x}");
        assert!((y - 50.0).abs() < 1e-4, "y = {y}");
    }

    #[test]
    fn picking_finds_the_nearest_annotation() {
        let mut ann = Annotations::new();
        ann.add(Annotation::point(DataPoint::new(10.0, 10.0), "a"));
        let near = ann.add(Annotation::point(DataPoint::new(80.0, 80.0), "b"));

        let v = vp();
        let (px, py) = v.map(DataPoint::new(80.0, 80.0));
        assert_eq!(ann.pick(&v, px + 3.0, py + 3.0, 10.0), Some(near));
        // Far from everything: no pick, rather than a wrong one.
        assert_eq!(ann.pick(&v, 50.0, 50.0, 5.0), None);
    }

    #[test]
    fn vline_picking_ignores_the_y_distance() {
        // A vertical rule spans the plot, so only horizontal distance means
        // anything when clicking it.
        let mut ann = Annotations::new();
        let i = ann.add(Annotation::vline(50.0, "deploy"));
        let v = vp();
        let x = v.map_x(50.0);
        assert_eq!(ann.pick(&v, x + 2.0, 5.0, 6.0), Some(i));
        assert_eq!(ann.pick(&v, x + 2.0, 95.0, 6.0), Some(i));
        assert_eq!(ann.pick(&v, x + 40.0, 50.0, 6.0), None);
    }

    #[test]
    fn band_picking_is_inside_or_nearest_edge() {
        let mut ann = Annotations::new();
        let i = ann.add(Annotation::band(20.0, 40.0, "outage"));
        let v = vp();
        // Inside the band: distance zero.
        assert_eq!(ann.pick(&v, v.map_x(30.0), 50.0, 1.0), Some(i));
        // Just outside: still within tolerance.
        assert_eq!(ann.pick(&v, v.map_x(41.0), 50.0, 5.0), Some(i));
        // Well outside: no pick.
        assert_eq!(ann.pick(&v, v.map_x(90.0), 50.0, 5.0), None);
    }

    #[test]
    fn axis_rules_span_the_current_bounds() {
        // A vline must be redrawn to the scene's CURRENT extent, so changing
        // the y range does not leave a stub line.
        let mut ann = Annotations::new();
        ann.add(Annotation::vline(5.0, "x"));

        let mut tall = Scene::new(Bounds::new(0.0, 10.0), Bounds::new(0.0, 1000.0));
        ann.draw_into(&mut tall);
        let line = tall
            .primitives
            .iter()
            .find_map(|p| match p {
                Primitive::Polyline { points, .. } => Some(points.clone()),
                _ => None,
            })
            .expect("vline");
        assert_eq!(line[0].y, 0.0);
        assert_eq!(line[1].y, 1000.0, "rule must span the full y range");
    }

    #[test]
    fn annotations_reach_the_svg_export() {
        // What you see must be what you export.
        let mut scene = Scene::new(Bounds::new(0.0, 100.0), Bounds::new(0.0, 100.0));
        let mut ann = Annotations::new();
        ann.add(Annotation::point(DataPoint::new(50.0, 50.0), "peak"));
        ann.add(Annotation::hline(80.0, "threshold"));
        ann.draw_into(&mut scene);

        let svg = to_svg(&scene, 600.0, 400.0);
        assert!(svg.contains("peak"), "point label missing from export");
        assert!(svg.contains("threshold"), "hline label missing from export");
        assert!(svg.contains("<circle"));
        assert!(svg.ends_with("</svg>"));
    }

    #[test]
    fn annotation_text_is_escaped_in_export() {
        // User-authored text goes straight into XML; a stray & must not
        // produce an unopenable file.
        let mut scene = Scene::new(Bounds::new(0.0, 1.0), Bounds::new(0.0, 1.0));
        let mut ann = Annotations::new();
        ann.add(Annotation::point(DataPoint::new(0.5, 0.5), "P&L <est>"));
        ann.draw_into(&mut scene);
        let svg = to_svg(&scene, 200.0, 200.0);
        assert!(svg.contains("P&amp;L &lt;est&gt;"));
        assert!(!svg.contains("P&L <est>"));
    }

    #[test]
    fn removing_an_annotation_shifts_later_indices() {
        let mut ann = Annotations::new();
        ann.add(Annotation::point(DataPoint::new(1.0, 1.0), "a"));
        ann.add(Annotation::point(DataPoint::new(2.0, 2.0), "b"));
        ann.add(Annotation::point(DataPoint::new(3.0, 3.0), "c"));
        let removed = ann.remove(1).unwrap();
        assert_eq!(removed.text, "b");
        assert_eq!(ann.len(), 2);
        assert_eq!(ann.get(1).unwrap().text, "c");
        assert!(ann.remove(99).is_none(), "out of range must not panic");
    }

    #[test]
    fn empty_text_draws_geometry_without_a_label() {
        let mut scene = Scene::new(Bounds::new(0.0, 10.0), Bounds::new(0.0, 10.0));
        let mut ann = Annotations::new();
        ann.add(Annotation::point(DataPoint::new(5.0, 5.0), ""));
        ann.draw_into(&mut scene);
        let texts = scene
            .primitives
            .iter()
            .filter(|p| matches!(p, Primitive::Text { .. }))
            .count();
        assert_eq!(texts, 0, "empty text must not emit an empty label");
        assert!(scene
            .primitives
            .iter()
            .any(|p| matches!(p, Primitive::Circle { .. })));
    }
}
