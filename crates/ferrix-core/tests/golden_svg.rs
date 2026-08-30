//! Golden-file tests for the SVG chart backend.
//!
//! A fixed scene is rendered through [`to_svg`] and byte-compared against a
//! checked-in `.svg` under `tests/fixtures/`. This pins the *exact* output of
//! the vector backend: a refactor that silently moves a label by a pixel,
//! reorders attributes, or drops the log-axis decade ticks changes the bytes
//! and fails here, where a "well-formed SVG" smoke test would sail through.
//!
//! Both a linear-axis and a log-axis chart are covered, so the two mapping
//! paths and the two tick generators each have a frozen reference.
//!
//! Regenerating: set `UPDATE_GOLDEN=1` to rewrite the fixtures from the current
//! renderer, then review the diff before committing. Do this only for an
//! intended change to the output.
//!
//! Data is synthetic numeric only — no PII.

use std::fs;
use std::path::PathBuf;

use ferrix_core::chart::{Bounds, DataPoint};
use ferrix_core::scene::{to_svg, Primitive, Rgba, Scale, ScaleHint, Scene};

const SERIES_BLUE: Rgba = Rgba::rgb(0x1f, 0x77, 0xb4);
const MARKER_RED: Rgba = Rgba::rgb(0xd6, 0x27, 0x28);

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Render `scene` at a fixed size and either rewrite the fixture (when
/// `UPDATE_GOLDEN` is set) or assert the output matches it byte for byte.
fn check_golden(name: &str, scene: &Scene) {
    let svg = to_svg(scene, 480.0, 320.0);
    let path = fixtures_dir().join(name);

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        fs::create_dir_all(fixtures_dir()).expect("create fixtures dir");
        fs::write(&path, svg.as_bytes()).expect("write golden fixture");
        return;
    }

    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden fixture {}: {e}\nrun with UPDATE_GOLDEN=1 to create it",
            path.display()
        )
    });
    // Normalise line endings so a CRLF checkout does not spuriously differ from
    // the LF the renderer emits.
    let expected = expected.replace("\r\n", "\n");
    let actual = svg.replace("\r\n", "\n");
    assert_eq!(
        actual,
        expected,
        "SVG output drifted from golden fixture {}.\n\
         If this change is intended, regenerate with UPDATE_GOLDEN=1 and review the diff.",
        path.display()
    );
}

/// A small line chart with a handful of markers and a title/axis labels, on
/// linear axes. Deterministic synthetic data.
fn linear_line_scene() -> Scene {
    let points = vec![
        DataPoint::new(0.0, 0.0),
        DataPoint::new(1.0, 20.0),
        DataPoint::new(2.0, 15.0),
        DataPoint::new(3.0, 45.0),
        DataPoint::new(4.0, 40.0),
        DataPoint::new(5.0, 80.0),
    ];
    let mut s = Scene::new(Bounds::new(0.0, 5.0), Bounds::new(0.0, 80.0))
        .with_title("golden line")
        .with_axis_labels("x", "y");
    s.push(Primitive::Polyline {
        points: points.clone(),
        color: SERIES_BLUE,
        width: 1.5,
    });
    // A few markers on the vertices.
    for p in &points {
        s.push(Primitive::Circle {
            center: *p,
            radius_px: 3.0,
            fill: MARKER_RED,
        });
    }
    s
}

/// The same shape of chart but with a logarithmic y axis spanning several
/// decades, so the golden file pins the log tick + mapping path.
fn log_line_scene() -> Scene {
    let points = vec![
        DataPoint::new(0.0, 1.0),
        DataPoint::new(1.0, 10.0),
        DataPoint::new(2.0, 100.0),
        DataPoint::new(3.0, 1000.0),
        DataPoint::new(4.0, 10000.0),
    ];
    let mut s = Scene::new(Bounds::new(0.0, 4.0), Bounds::new(1.0, 10000.0))
        .with_title("golden log")
        .with_axis_labels("x", "y")
        .with_scale(ScaleHint::new(Scale::Linear, Scale::Log));
    s.push(Primitive::Polyline {
        points: points.clone(),
        color: SERIES_BLUE,
        width: 1.5,
    });
    for p in &points {
        s.push(Primitive::Circle {
            center: *p,
            radius_px: 3.0,
            fill: MARKER_RED,
        });
    }
    s
}

#[test]
fn golden_linear_line_chart() {
    check_golden("line_linear.svg", &linear_line_scene());
}

#[test]
fn golden_log_line_chart() {
    check_golden("line_log.svg", &log_line_scene());
}

/// Independent of byte-equality: the log fixture must render on the log path.
/// At this canvas size the overlap-elide pass thins the crowded interior decade
/// labels, but the surviving endpoints are the decade majors `1` and `10k` —
/// the compact log formatting a linear axis would never produce for this range
/// (a linear 1..10000 axis ticks at 0, 2000, 4000, …). Seeing both endpoints
/// proves the log tick generator and formatter ran.
#[test]
fn log_fixture_renders_on_the_log_path() {
    let svg = to_svg(&log_line_scene(), 480.0, 320.0);
    assert!(
        svg.contains(">1<"),
        "log axis missing its bottom decade '1'"
    );
    assert!(
        svg.contains(">10k<"),
        "log axis missing its top decade '10k'"
    );
    // A linear axis over 1..10000 would emit a 2000-step tick; the log axis
    // must not.
    assert!(
        !svg.contains(">2000<") && !svg.contains(">4000<"),
        "linear-style ticks leaked onto a log axis"
    );
}
