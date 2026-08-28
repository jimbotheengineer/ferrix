//! Chart aggregation at scale: prove output size tracks the canvas, not the data.
//!
//! Usage: bench-chart <file.csv>

use std::time::Instant;

use ferrix_core::chart::{decimate_min_max, density_grid, group_by, histogram, Aggregate};
use ferrix_core::{CellRef, Value};
use ferrix_io::{load_csv, CsvOptions};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "benchdata/chart.csv".to_string());

    let t = Instant::now();
    let (sheet, stats) = match load_csv(std::path::Path::new(&path), CsvOptions::default()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("load failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "loaded {} rows x {} cols in {:.1}s",
        stats.rows,
        stats.cols,
        t.elapsed().as_secs_f64()
    );

    // Column E (index 4) is numeric in the generated data.
    let t = Instant::now();
    let col: Vec<Option<f64>> = (0..sheet.row_count())
        .map(|r| match sheet.get(CellRef::new(r as u32, 4)) {
            Value::Number(n) => Some(n),
            _ => None,
        })
        .collect();
    println!("extracted column in {:.2}s", t.elapsed().as_secs_f64());

    println!("\n--- min/max decimation (line chart) ---");
    for buckets in [500usize, 1000, 2000] {
        let t = Instant::now();
        let s = decimate_min_max(&col, buckets);
        println!(
            "{:>5} buckets -> {:>6} points  {:>7.0} ms  (from {} rows, {:.0}x reduction)",
            buckets,
            s.points.len(),
            t.elapsed().as_secs_f64() * 1000.0,
            s.source_rows,
            s.source_rows as f64 / s.points.len().max(1) as f64
        );
    }

    println!("\n--- histogram ---");
    for bins in [32usize, 64, 256] {
        let t = Instant::now();
        let h = histogram(&col, bins, None);
        let total: u64 = h.iter().map(|b| b.count).sum();
        println!(
            "{:>5} bins    -> {:>6} bars    {:>7.0} ms  (counted {} values)",
            bins,
            h.len(),
            t.elapsed().as_secs_f64() * 1000.0,
            total
        );
    }

    println!("\n--- density grid (scatter) ---");
    let ys: Vec<Option<f64>> = (0..sheet.row_count())
        .map(|r| match sheet.get(CellRef::new(r as u32, 7)) {
            Value::Number(n) => Some(n),
            _ => None,
        })
        .collect();
    let t = Instant::now();
    let (cells, _, _) = density_grid(&col, &ys, 128, 128);
    let total: u64 = cells.iter().map(|c| c.count).sum();
    println!(
        "128x128 grid -> {:>6} cells   {:>7.0} ms  (counted {} points)",
        cells.len(),
        t.elapsed().as_secs_f64() * 1000.0,
        total
    );

    println!("\n--- group-by (bar chart) ---");
    let labels: Vec<String> = (0..sheet.row_count())
        .map(|r| sheet.display(CellRef::new(r as u32, 1)))
        .collect();
    let t = Instant::now();
    let bars = group_by(&labels, &col, Aggregate::Sum);
    println!(
        "region column -> {:>4} bars     {:>7.0} ms",
        bars.len(),
        t.elapsed().as_secs_f64() * 1000.0
    );
    for b in bars.iter().take(6) {
        println!("    {:<12} {:>18.0}  ({} rows)", b.label, b.value, b.count);
    }
}
