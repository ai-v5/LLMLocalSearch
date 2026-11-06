//! Seed synthetic filename metadata to measure extended query scales.
//! This does not measure directory traversal or make a capacity guarantee.

use anyhow::{Context, Result, bail};
use llmlocalsearch::{index, query, query::Scope};
use rusqlite::params;
use std::cell::Cell;
use std::path::Path;
use std::time::Instant;

fn main() -> Result<()> {
    let argument = std::env::args_os()
        .nth(1)
        .context("usage: benchmark_names <new-synthetic-index.db>")?;
    let path = Path::new(&argument);
    if path.exists() {
        bail!("benchmark database already exists");
    }
    let mut database = index::open(path)?;
    let started = Instant::now();
    let mut inserted = 0usize;
    for target in [100_000, 500_000, 1_000_000] {
        while inserted < target {
            let end = (inserted + 5000).min(target);
            let transaction = database.transaction()?;
            {
                let mut file = transaction.prepare("INSERT INTO files(id,path,name,kind,modified_ms,size,state,body) VALUES(?1,?2,?3,'other',?4,0,'name_only',NULL)")?;
                let mut terms = transaction
                    .prepare("INSERT INTO terms(rowid,name_grams,body_grams) VALUES(?1,?2,'')")?;
                for number in inserted..end {
                    let id = number as i64 + 1;
                    let name = format!("合成文档-{number:07}.txt");
                    let path = format!("S:\\Synthetic\\{name}");
                    file.execute(params![id, path, name, id / 10])?;
                    terms.execute(params![id, query::grams(&name)])?;
                }
            }
            transaction.commit()?;
            inserted = end;
        }
        let actual = index::stats(&database)?.files as usize;
        assert_eq!(actual, target);
        for term in ["合", "合成", "文档", "合成文档-0001"] {
            let mut durations = Vec::with_capacity(10);
            for _ in 0..10 {
                let stopped = Cell::new(false);
                let first = Cell::new(None);
                let timer = Instant::now();
                index::search_stream(
                    &database,
                    term,
                    Scope::Name,
                    &[],
                    &[],
                    || stopped.get(),
                    |batch| {
                        if first.get().is_none() && !batch.is_empty() {
                            assert!(batch.windows(2).all(|pair| {
                                pair[0].file.modified_ms > pair[1].file.modified_ms
                                    || (pair[0].file.modified_ms == pair[1].file.modified_ms
                                        && pair[0].id < pair[1].id)
                            }));
                            first.set(Some(timer.elapsed()));
                            stopped.set(true);
                        }
                    },
                )?;
                durations.push(first.get().context("no synthetic hits")?.as_secs_f64() * 1000.0);
            }
            durations.sort_by(f64::total_cmp);
            println!(
                "filenames={target} term_chars={} runs=10 first_page_p95_ms={:.1} elapsed_s={:.1}",
                term.chars().count(),
                durations[9],
                started.elapsed().as_secs_f64()
            );
        }
    }
    Ok(())
}
