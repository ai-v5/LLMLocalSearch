use anyhow::{Context, Result};
use std::time::Instant;

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: benchmark_grams <synthetic-text-file>")?;
    let sample = std::fs::read_to_string(path)?;
    let start = Instant::now();
    let mut bytes = 0usize;
    for _ in 0..200 {
        bytes += std::hint::black_box(llmlocalsearch::query::grams(&sample)).len();
    }
    println!(
        "runs=200 total_ms={:.1} encoded_bytes={bytes}",
        start.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}
