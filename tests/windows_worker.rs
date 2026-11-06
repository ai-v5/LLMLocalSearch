#![cfg(windows)]

#[path = "../src/worker_client.rs"]
mod worker_client;

use anyhow::{Result, bail};
use llmlocalsearch::{index, query::Scope};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(label: &str, mut check: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if check() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("timed out waiting for synthetic event: {label}")
}

fn count(db: &Path, term: &str) -> usize {
    index::open_reader(db)
        .ok()
        .and_then(|db| index::search(&db, term, Scope::Both, &[], &[], || false).ok())
        .map_or(0, |result| result.len())
}

fn state(db: &Path, name: &str) -> Option<String> {
    index::open_reader(db)
        .ok()?
        .query_row("SELECT state FROM files WHERE name=?1", [name], |r| {
            r.get(0)
        })
        .ok()
}

#[test]
fn worker_observes_creation_modification_and_deletion() -> Result<()> {
    let temp = tempdir()?;
    let files = temp.path().join("files");
    let runtime = temp.path().join("runtime");
    let db = temp.path().join("index.db");
    std::fs::create_dir(&files)?;
    std::fs::write(files.join("初始.txt"), "合成初始内容")?;
    let child = Command::new(env!("CARGO_BIN_EXE_index-worker"))
        .arg(&db)
        .arg(&runtime)
        .args(["0", "1"])
        .arg(&files)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()?;
    let mut child = ChildGuard(child);
    let input = child.0.stdin.as_mut().expect("piped stdin");
    writeln!(input, "start")?;
    input.flush()?;
    wait_until("initial", || count(&db, "初始内容") == 1)?;
    let new = files.join("新增.txt");
    std::fs::write(&new, "合成木星")?;
    wait_until("create", || count(&db, "木星") == 1)?;
    std::fs::write(&new, "合成水星")?;
    wait_until("modify", || {
        count(&db, "水星") == 1 && count(&db, "木星") == 0
    })?;
    let lower = files.join("lower.txt");
    let upper = files.join("LOWER.txt");
    std::fs::write(&lower, "大小写重命名")?;
    wait_until("before case rename", || count(&db, "lower") == 1)?;
    std::fs::rename(&lower, &upper)?;
    wait_until("case rename", || {
        count(&db, "lower") == 1
            && index::open_reader(&db)
                .ok()
                .and_then(|c| index::search(&c, "lower", Scope::Name, &[], &[], || false).ok())
                .is_some_and(|hits| hits.first().is_some_and(|hit| hit.file.name == "LOWER.txt"))
    })?;
    std::fs::remove_file(new)?;
    wait_until("delete", || {
        count(&db, "新增") == 0 && count(&db, "水星") == 0
    })?;
    let input = child.0.stdin.as_mut().expect("piped stdin");
    writeln!(input, "stop")?;
    input.flush()?;
    Ok(())
}

#[test]
fn automatically_retries_temporarily_locked_file() -> Result<()> {
    let temp = tempdir()?;
    let files = temp.path().join("files");
    let db = temp.path().join("index.db");
    std::fs::create_dir(&files)?;
    let path = files.join("locked.txt");
    std::fs::write(&path, "合成文件恢复")?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&path)?;
    let child = Command::new(env!("CARGO_BIN_EXE_index-worker"))
        .arg(&db)
        .arg(temp.path().join("runtime"))
        .args(["0", "1"])
        .arg(&files)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()?;
    let mut child = ChildGuard(child);
    let input = child.0.stdin.as_mut().expect("piped stdin");
    writeln!(input, "start")?;
    input.flush()?;
    wait_until("locked", || {
        state(&db, "locked.txt").is_some_and(|s| s == "retry_wait")
    })?;
    drop(lock);
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        if count(&db, "合成文件恢复") == 1 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("previously locked file was not retried after reconciliation")
}

#[test]
fn restarting_a_paused_worker_keeps_indexing_paused() -> Result<()> {
    let temporary = tempdir()?;
    let files = temporary.path().join("files");
    let database = temporary.path().join("index.db");
    std::fs::create_dir_all(&files)?;
    std::fs::write(files.join("synthetic.txt"), "合成暂停测试")?;
    let executable = Path::new(env!("CARGO_BIN_EXE_index-worker"));
    let mut worker = worker_client::Worker::start(
        executable.parent().expect("worker directory"),
        &database,
        &[files],
        &[],
        false,
        true,
        eframe::egui::Context::default(),
    )?;
    wait_until("worker database", || database.exists())?;
    assert!(worker.running());
    std::thread::sleep(Duration::from_secs(2));
    let connection = index::open_reader(&database)?;
    assert_eq!(index::stats(&connection)?.files, 0);
    drop(connection);
    let _ = worker.messages.try_recv();
    worker.send("resume")?;
    wait_until("resume", || count(&database, "合成暂停测试") == 1)
}
