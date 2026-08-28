//! Load a CSV and report ingest throughput, memory, and query latency.
//!
//! Usage: bench-load <file.csv>

use ferrix_core::CellRef;
use ferrix_formula::{eval, parse};
use ferrix_io::{load_csv, CsvOptions};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("usage: bench-load <file.csv>");
        std::process::exit(2);
    });
    let path = std::path::Path::new(&path);

    let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    println!(
        "file: {} ({:.2} GB)",
        path.display(),
        file_bytes as f64 / 1e9
    );

    let (sheet, stats) = match load_csv(path, CsvOptions::default()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("load failed: {e}");
            std::process::exit(1);
        }
    };

    println!("\n--- ingest ---");
    println!("rows:        {}", stats.rows);
    println!("cols:        {}", stats.cols);
    println!("chunks:      {}", stats.chunks);
    println!("parse time:  {} ms", stats.parse_millis);
    println!("throughput:  {:.1} MB/s", stats.throughput_mbps());

    let heap = sheet.heap_bytes();
    println!("\n--- memory ---");
    println!("heap:        {:.2} GB", heap as f64 / 1e9);
    println!(
        "bytes/cell:  {:.2}",
        heap as f64 / (stats.rows.max(1) * stats.cols.max(1)) as f64
    );
    println!(
        "file->heap:  {:.2}x",
        heap as f64 / file_bytes.max(1) as f64
    );

    println!("\n--- random access (simulates scrolling) ---");
    // Touch a viewport-sized window at 200 scattered offsets: this is exactly
    // what the renderer does each frame.
    let mut checksum = 0.0f64;
    let t = std::time::Instant::now();
    let mut probes = 0usize;
    for i in 0..200 {
        let row_base = (i * 7919 * 13) % stats.rows.max(1);
        for r in row_base..(row_base + 50).min(stats.rows) {
            for c in 0..stats.cols {
                if let Some(n) = sheet.get(CellRef::new(r as u32, c as u32)).as_number() {
                    checksum += n;
                }
                probes += 1;
            }
        }
    }
    let elapsed = t.elapsed();
    println!("cells read:  {probes}");
    println!("total time:  {:.2} ms", elapsed.as_secs_f64() * 1000.0);
    println!(
        "per viewport:{:.3} ms  (budget at 60fps: 16.67 ms)",
        elapsed.as_secs_f64() * 1000.0 / 200.0
    );
    println!("checksum:    {checksum:.0}");

    println!("\n--- full-column aggregate ---");
    for (label, formula) in [
        ("SUM(E:E)", format!("=SUM(E1:E{})", stats.rows)),
        ("MAX(G:G)", format!("=MAX(G1:G{})", stats.rows)),
        ("AVERAGE(H:H)", format!("=AVERAGE(H1:H{})", stats.rows)),
    ] {
        let expr = match parse(&formula) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("{label}: parse error {e}");
                continue;
            }
        };
        let t = std::time::Instant::now();
        let v = eval(&expr, &sheet);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let shown = match v {
            ferrix_core::Value::Number(n) => ferrix_core::format_number(n),
            other => format!("{other:?}"),
        };
        println!("{label:14} = {shown:>20}   ({ms:.1} ms)");
    }
}
