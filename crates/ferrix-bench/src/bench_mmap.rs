//! Out-of-core benchmark: convert a large CSV, map it, and measure.
//!
//! Usage: bench-mmap <file.csv>
//!
//! Reports conversion throughput, mapped size, cold-open time, random-access
//! latency, and full-column aggregate time — the numbers that decide whether
//! the 10GB claim holds.

use std::path::Path;

use ferrix_core::CellRef;
use ferrix_io::{cache_is_fresh, cache_path_for, convert_csv, MappedSheet};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("usage: bench-mmap <file.csv>");
        std::process::exit(2);
    });
    let src = Path::new(&path);
    let cache = cache_path_for(src);

    let src_bytes = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    println!(
        "source: {} ({:.2} GB)",
        src.display(),
        src_bytes as f64 / 1e9
    );

    // --- conversion (skipped when a fresh cache exists) ---
    if cache_is_fresh(src, &cache) {
        println!("\n--- conversion ---");
        println!("cache is fresh, skipping conversion");
    } else {
        println!("\n--- conversion ---");
        let start = std::time::Instant::now();
        let mut last_pct = 0u64;
        let stats = match convert_csv(src, &cache, b',', true, |done, total| {
            let pct = done * 100 / total.max(1);
            if pct >= last_pct + 10 {
                last_pct = pct;
                eprintln!("  {pct}%  ({:.1} GB)", done as f64 / 1e9);
            }
        }) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("conversion failed: {e}");
                std::process::exit(1);
            }
        };
        println!("rows:        {}", fmt_int(stats.rows as usize));
        println!("cols:        {}", stats.cols);
        println!("time:        {:.1} s", start.elapsed().as_secs_f64());
        println!("throughput:  {:.1} MB/s", stats.throughput_mbps());
        println!("output:      {:.2} GB", stats.output_bytes as f64 / 1e9);
        println!("distinct strings: {}", fmt_int(stats.distinct_strings));
        println!(
            "peak buffer: {} MB  (independent of file size)",
            stats.peak_block_bytes >> 20
        );
    }

    // --- cold open ---
    println!("\n--- open (mmap) ---");
    let t = std::time::Instant::now();
    let sheet = match MappedSheet::open(&cache) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open failed: {e}");
            std::process::exit(1);
        }
    };
    let open_us = t.elapsed().as_micros();
    println!("rows:        {}", fmt_int(sheet.row_count()));
    println!("cols:        {}", sheet.col_count());
    println!(
        "mapped:      {:.2} GB (address space, not RAM)",
        sheet.mapped_bytes() as f64 / 1e9
    );
    println!("open time:   {open_us} µs");

    // --- random access, simulating scrolling ---
    println!("\n--- random access (simulates scrolling) ---");
    let rows = sheet.row_count();
    let cols = sheet.col_count();
    let mut checksum = 0.0f64;
    let mut probes = 0usize;
    let t = std::time::Instant::now();
    for i in 0..200 {
        // Scatter viewports across the whole file so we exercise cold pages.
        let base = (i * 7_919_131) % rows.max(1);
        for r in base..(base + 50).min(rows) {
            for c in 0..cols {
                if let Some(n) = sheet.get(CellRef::new(r as u32, c as u32)).as_number() {
                    checksum += n;
                }
                probes += 1;
            }
        }
    }
    let el = t.elapsed();
    println!("cells read:  {}", fmt_int(probes));
    println!("total:       {:.1} ms", el.as_secs_f64() * 1000.0);
    println!(
        "per viewport:{:.3} ms  (60fps budget: 16.67 ms)",
        el.as_secs_f64() * 1000.0 / 200.0
    );
    println!("checksum:    {checksum:.0}");

    // --- full-column aggregates ---
    println!("\n--- full-column aggregates (all rows) ---");
    let last = (rows.saturating_sub(1)) as u32;
    for col in 0..cols.min(8) {
        let c = col as u32;
        let t = std::time::Instant::now();
        let sum = sheet.sum_rect(CellRef::new(0, c), CellRef::new(last, c));
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if sum != 0.0 {
            println!(
                "col {:>2} ({:>12}) SUM = {:>22.2}  ({ms:.0} ms)",
                col,
                sheet.header_or_letter(col),
                sum
            );
        }
    }

    // --- search ---
    // Each needle is searched twice. A 12 GB cache exceeds RAM, so the first
    // pass pays major page faults pulling columns off disk and the second
    // measures steady-state cost. Reporting only the first pass would blame
    // the needle for what is really I/O.
    println!("\n--- search ---");
    println!(
        "{:>12}  {:>13}  {:>10}  {:>10}",
        "needle", "hits", "pass 1", "pass 2"
    );
    let needles = ["north", "consulting", "cancelled", "zzz-absent", "4242"];
    let mut first = Vec::new();
    for needle in needles {
        let Some(q) = ferrix_core::Query::new(needle, false, false) else {
            first.push((0usize, 0.0));
            continue;
        };
        let t = std::time::Instant::now();
        let r = sheet.search(&q, 100_000);
        first.push((r.total, t.elapsed().as_secs_f64() * 1000.0));
    }
    for (i, needle) in needles.iter().enumerate() {
        let Some(q) = ferrix_core::Query::new(needle, false, false) else {
            continue;
        };
        let t = std::time::Instant::now();
        let r = sheet.search(&q, 100_000);
        let second = t.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{:>12}  {:>13}  {:>7.0} ms  {:>7.0} ms",
            needle,
            fmt_int(r.total),
            first[i].1,
            second
        );
    }
}

fn fmt_int(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}
