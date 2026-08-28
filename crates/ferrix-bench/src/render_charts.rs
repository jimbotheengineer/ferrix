//! Render sample charts to SVG so a human can look at them.
//!
//! Unit tests prove the SVG is well-formed. They cannot tell you the axis
//! labels overlap or the line is upside down. This writes real files.

use std::fs;

use ferrix_core::annotation::{Annotation, Annotations};
use ferrix_core::chart::{
    decimate_min_max, density_grid, group_by, histogram, Aggregate, Bounds, DataPoint,
};
use ferrix_core::scene::{to_svg, Primitive, Rgba, Scene};

const BLUE: Rgba = Rgba::rgb(0x1f, 0x77, 0xb4);
const ORANGE: Rgba = Rgba::rgb(0xff, 0x7f, 0x0e);

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "chartout".to_string());
    fs::create_dir_all(&out).expect("create output dir");

    // --- 1. Line chart from a million rows, with a deliberate spike ---
    let mut data: Vec<Option<f64>> = (0..1_000_000)
        .map(|i| {
            let t = i as f64 / 1_000_000.0;
            Some((t * std::f64::consts::TAU * 3.0).sin() * 40.0 + t * 60.0 + 50.0)
        })
        .collect();
    data[640_000] = Some(210.0); // the anomaly a sampling renderer would lose

    let series = decimate_min_max(&data, 600);
    let mut y = Bounds::unbounded();
    for p in &series.points {
        y.include(p.y);
    }
    let mut scene = Scene::new(Bounds::new(0.0, 1_000_000.0), y)
        .with_title("1,000,000 rows decimated to 600 buckets")
        .with_axis_labels("row", "value");
    scene.push(Primitive::Polyline {
        points: series.points.clone(),
        color: BLUE,
        width: 1.2,
    });

    let mut ann = Annotations::new();
    ann.add(Annotation::point(
        DataPoint::new(640_000.0, 210.0),
        "anomaly survives decimation",
    ));
    ann.add(Annotation::hline(150.0, "threshold"));
    ann.draw_into(&mut scene);

    write(&out, "line.svg", &to_svg(&scene, 900.0, 420.0));
    println!(
        "line.svg        {} points from {} rows, {} primitives",
        series.points.len(),
        series.source_rows,
        scene.len()
    );

    // Same scene, 4x size -- proves resolution independence.
    write(&out, "line_4x.svg", &to_svg(&scene, 3600.0, 1680.0));
    println!("line_4x.svg     same scene at 4x, identical geometry");

    // --- 2. Histogram ---
    let normalish: Vec<Option<f64>> = (0..500_000)
        .map(|i| {
            let a = ((i * 2654435761usize) % 1000) as f64 / 1000.0;
            let b = ((i * 40503usize) % 1000) as f64 / 1000.0;
            let c = ((i * 12289usize) % 1000) as f64 / 1000.0;
            Some((a + b + c) / 3.0 * 100.0)
        })
        .collect();
    let bins = histogram(&normalish, 40, None);
    let max_count = bins.iter().map(|b| b.count).max().unwrap_or(1) as f64;
    let mut hs = Scene::new(
        Bounds::new(bins[0].lo, bins[bins.len() - 1].hi),
        Bounds::new(0.0, max_count),
    )
    .with_title("Distribution of 500,000 values (40 bins)")
    .with_axis_labels("value", "count");
    for b in &bins {
        hs.push(Primitive::Rect {
            x0: b.lo,
            y0: 0.0,
            x1: b.hi,
            y1: b.count as f64,
            fill: BLUE,
            stroke: None,
        });
    }
    write(&out, "histogram.svg", &to_svg(&hs, 900.0, 420.0));
    println!("histogram.svg   {} bars from 500,000 values", bins.len());

    // --- 3. Bar chart from categories ---
    let labels: Vec<String> = (0..200_000)
        .map(|i| ["north", "south", "east", "west", "central"][i % 5].to_string())
        .collect();
    let values: Vec<Option<f64>> = (0..200_000).map(|i| Some((i % 997) as f64)).collect();
    let cats = group_by(&labels, &values, Aggregate::Sum);
    let maxv = cats.iter().map(|c| c.value).fold(0.0, f64::max);
    let mut bs = Scene::new(
        Bounds::new(-0.5, cats.len() as f64 - 0.5),
        Bounds::new(0.0, maxv),
    )
    .with_title("Sum by region (200,000 rows -> 5 bars)")
    .with_axis_labels("region", "total");
    for (i, c) in cats.iter().enumerate() {
        bs.push(Primitive::Rect {
            x0: i as f64 - 0.38,
            y0: 0.0,
            x1: i as f64 + 0.38,
            y1: c.value,
            fill: ORANGE,
            stroke: None,
        });
        bs.push(Primitive::Text {
            at: DataPoint::new(i as f64, 0.0),
            text: c.label.clone(),
            size_px: 12.0,
            color: Rgba::rgb(0x40, 0x40, 0x40),
            anchor: ferrix_core::scene::Anchor::Middle,
            offset_px: (0.0, 16.0),
        });
    }
    write(&out, "bar.svg", &to_svg(&bs, 900.0, 420.0));
    println!("bar.svg         {} bars from 200,000 rows", cats.len());

    // --- 4. Scatter as a density grid ---
    let xs: Vec<Option<f64>> = (0..300_000)
        .map(|i| Some(((i * 7919) % 1000) as f64))
        .collect();
    let ys: Vec<Option<f64>> = (0..300_000)
        .map(|i| Some(((i * 7919) % 1000) as f64 * 0.6 + ((i * 104729) % 400) as f64))
        .collect();
    let (cells, xb, yb) = density_grid(&xs, &ys, 80, 60);
    let maxc = cells.iter().map(|c| c.count).max().unwrap_or(1) as f64;
    let mut ss = Scene::new(xb, yb)
        .with_title("300,000 points as an 80x60 density grid")
        .with_axis_labels("x", "y");
    let (xw, yh) = (xb.span() / 80.0, yb.span() / 60.0);
    for c in &cells {
        let t = (c.count as f64 / maxc).sqrt();
        let alpha = (40.0 + t * 215.0) as u8;
        ss.push(Primitive::Rect {
            x0: xb.min + c.x_bin as f64 * xw,
            y0: yb.min + c.y_bin as f64 * yh,
            x1: xb.min + (c.x_bin + 1) as f64 * xw,
            y1: yb.min + (c.y_bin + 1) as f64 * yh,
            fill: Rgba(0x1f, 0x77, 0xb4, alpha),
            stroke: None,
        });
    }
    write(&out, "scatter.svg", &to_svg(&ss, 900.0, 500.0));
    println!(
        "scatter.svg     {} occupied cells from 300,000 points",
        cells.len()
    );

    println!("\nwrote SVGs to {out}/");
}

fn write(dir: &str, name: &str, svg: &str) {
    let path = format!("{dir}/{name}");
    fs::write(&path, svg).unwrap_or_else(|e| panic!("write {path}: {e}"));
}
