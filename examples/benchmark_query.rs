use anyhow::{Context, Result};
use llmlocalsearch::{index, query::Scope};
use std::cell::Cell;
use std::time::Instant;

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: benchmark_query <synthetic-index.db>")?;
    let db = index::open_reader(std::path::Path::new(&path))?;
    for term in ["锚", "锚点", "固定短"] {
        let mut times = Vec::new();
        for _ in 0..20 {
            let stop = Cell::new(false);
            let first = Cell::new(None);
            let start = Instant::now();
            index::search_stream(
                &db,
                term,
                Scope::Both,
                &[],
                &[],
                || stop.get(),
                |batch| {
                    if !batch.is_empty() && first.get().is_none() {
                        first.set(Some(start.elapsed()));
                        stop.set(true);
                    }
                },
            )?;
            times.push(first.get().context("no synthetic results")?.as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.total_cmp(b));
        let full = Instant::now();
        let total = index::search_stream(&db, term, Scope::Both, &[], &[], || false, |_| {})?;
        println!(
            "term_length={} runs=20 first_page_p95_ms={:.1} full_ms={:.1} hits={}",
            term.chars().count(),
            times[18],
            full.elapsed().as_secs_f64() * 1000.0,
            total
        );
    }
    Ok(())
}
