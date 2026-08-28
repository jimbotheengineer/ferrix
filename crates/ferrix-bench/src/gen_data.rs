//! Generate a large synthetic CSV for benchmarking.
//!
//! Usage: gen-data <rows> <output.csv>

use std::io::{BufWriter, Write};

/// Deterministic xorshift so runs are reproducible without a rand dependency.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows: usize = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000_000);
    let out = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "bench_data.csv".to_string());

    const REGIONS: [&str; 6] = ["north", "south", "east", "west", "central", "overseas"];
    const CATEGORIES: [&str; 8] = [
        "hardware",
        "software",
        "services",
        "support",
        "training",
        "licensing",
        "hosting",
        "consulting",
    ];
    const STATUSES: [&str; 4] = ["open", "closed", "pending", "cancelled"];

    let file = std::fs::File::create(&out).expect("create output file");
    // A large buffer keeps write syscalls rare on a multi-GB file.
    let mut w = BufWriter::with_capacity(8 << 20, file);
    let mut rng = Rng(0x2545F4914F6CDD1D);

    writeln!(
        w,
        "id,region,category,status,quantity,unit_price,revenue,score"
    )
    .unwrap();

    let start = std::time::Instant::now();
    for i in 0..rows {
        let region = REGIONS[rng.below(REGIONS.len() as u64) as usize];
        let category = CATEGORIES[rng.below(CATEGORIES.len() as u64) as usize];
        let status = STATUSES[rng.below(STATUSES.len() as u64) as usize];
        let quantity = rng.below(500) + 1;
        let unit_price = (rng.unit() * 999.0 + 1.0 * 100.0).round() / 100.0;
        let revenue = (quantity as f64 * unit_price * 100.0).round() / 100.0;
        let score = (rng.unit() * 10000.0).round() / 100.0;
        writeln!(
            w,
            "{i},{region},{category},{status},{quantity},{unit_price},{revenue},{score}"
        )
        .unwrap();

        if i % 1_000_000 == 0 && i > 0 {
            eprintln!("  {i} rows...");
        }
    }
    w.flush().unwrap();

    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!(
        "wrote {rows} rows ({:.2} GB) to {out} in {:.1}s",
        bytes as f64 / 1e9,
        start.elapsed().as_secs_f64()
    );
}
