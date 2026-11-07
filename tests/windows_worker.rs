#![cfg(windows)]

#[path = "../src/worker_client.rs"]
mod worker_client;

use anyhow::{Result, bail};
use llmlocalsearch::{index, query::Scope, scanner};
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
fn first_content_is_indexed_before_all_roots_finish_scanning() -> Result<()> {
    let temporary = tempdir()?;
    let first = temporary.path().join("first");
    let later = temporary.path().join("later");
    let database = temporary.path().join("index.db");
    std::fs::create_dir_all(&first)?;
    std::fs::create_dir_all(&later)?;
    std::fs::write(first.join("synthetic.txt"), "合成渐进正文")?;
    const OTHER_FILES: usize = 1500;
    for number in 0..OTHER_FILES {
        std::fs::write(later.join(format!("synthetic-{number:04}.bin")), [])?;
    }
    let executable = Path::new(env!("CARGO_BIN_EXE_index-worker"));
    let mut worker = worker_client::Worker::start(
        executable.parent().expect("worker directory"),
        &database,
        &[first, later],
        &[],
        false,
        false,
        eframe::egui::Context::default(),
    )?;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if !worker.running() {
            bail!("synthetic worker exited while scanning");
        }
        if let Ok(connection) = index::open_reader(&database)
            && let Ok(stats) = index::stats(&connection)
            && stats.indexed > 0
        {
            assert!(
                stats.files < (OTHER_FILES + 1) as u64,
                "content stayed pending until all synthetic roots were scanned"
            );
            assert_eq!(count(&database, "合成渐进正文"), 1);
            worker.stop();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("synthetic content did not become searchable during scanning")
}

#[test]
fn offline_root_recovers_and_reconciles_queued_changes() -> Result<()> {
    let temporary = tempdir()?;
    let root = temporary.path().join("files");
    let offline = temporary.path().join("offline");
    let database = temporary.path().join("index.db");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("old-name.txt"), "合成恢复内容")?;
    let executable = Path::new(env!("CARGO_BIN_EXE_index-worker"));
    let mut worker = worker_client::Worker::start(
        executable.parent().expect("worker directory"),
        &database,
        std::slice::from_ref(&root),
        &[],
        false,
        false,
        eframe::egui::Context::default(),
    )?;
    wait_until("initial synthetic index", || {
        count(&database, "old-name") == 1
    })?;
    worker.send("pause")?;
    wait_until("synthetic worker paused", || {
        worker
            .messages
            .try_iter()
            .any(|message| message["state"] == "paused")
    })?;
    std::fs::rename(root.join("old-name.txt"), root.join("new-name.txt"))?;
    std::fs::rename(&root, &offline)?;
    while worker.messages.try_recv().is_ok() {}
    worker.send("resume")?;
    wait_until("offline synthetic root observed", || {
        worker
            .messages
            .try_iter()
            .any(|message| message["state"] == "running")
    })?;
    assert_eq!(count(&database, "old-name"), 1);
    std::fs::rename(&offline, &root)?;
    wait_until("recovered synthetic root reconciled", || {
        count(&database, "new-name") == 1 && count(&database, "old-name") == 0
    })?;
    worker.stop();
    Ok(())
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
    wait_until("initialized worker database", || {
        index::open_reader(&database)
            .and_then(|connection| index::stats(&connection))
            .is_ok()
    })?;
    assert!(worker.running());
    std::thread::sleep(Duration::from_secs(2));
    let connection = index::open_reader(&database)?;
    assert_eq!(index::stats(&connection)?.files, 0);
    drop(connection);
    let _ = worker.messages.try_recv();
    worker.send("resume")?;
    wait_until("resume", || count(&database, "合成暂停测试") == 1)
}

#[test]
fn detects_directory_moves_and_changes_while_stopped() -> Result<()> {
    let temporary = tempdir()?;
    let root = temporary.path().join("files");
    let before = root.join("before");
    let after = root.join("after");
    let database = temporary.path().join("index.db");
    std::fs::create_dir_all(&before)?;
    for number in 0..40 {
        std::fs::write(
            before.join(format!("synthetic-{number:02}.txt")),
            "批量合成标记",
        )?;
    }
    let executable = Path::new(env!("CARGO_BIN_EXE_index-worker"));
    let start = || {
        worker_client::Worker::start(
            executable.parent().expect("worker directory"),
            &database,
            std::slice::from_ref(&root),
            &[],
            false,
            false,
            eframe::egui::Context::default(),
        )
    };
    let mut worker = start()?;
    wait_until("initial batch", || count(&database, "批量合成标记") == 40)?;
    std::fs::rename(&before, &after)?;
    wait_until("directory move", || {
        index::open_reader(&database)
            .and_then(|connection| {
                index::search(&connection, "批量合成标记", Scope::Both, &[], &[], || false)
            })
            .is_ok_and(|hits| {
                hits.len() == 40
                    && hits
                        .iter()
                        .all(|hit| Path::new(&hit.file.path).starts_with(&after))
            })
    })?;
    for number in 0..20 {
        std::fs::write(
            after.join(format!("added-{number:02}.txt")),
            "批量新增合成标记",
        )?;
    }
    wait_until("batch create", || {
        count(&database, "批量新增合成标记") == 20
    })?;
    worker.stop();
    drop(worker);
    std::fs::write(after.join("offline.txt"), "离线新增合成标记")?;
    let mut worker = start()?;
    wait_until("change while stopped", || {
        count(&database, "离线新增合成标记") == 1
    })?;
    let _ = worker.messages.try_recv();
    worker.stop();
    Ok(())
}

#[test]
fn worker_upgrades_legacy_index_and_preserves_search() -> Result<()> {
    let temporary = tempdir()?;
    let files = temporary.path().join("files");
    let database = temporary.path().join("index.db");
    std::fs::create_dir_all(&files)?;
    let mut db = index::open(&database)?;
    db.execute_batch("DROP TABLE terms; CREATE VIRTUAL TABLE terms USING fts5(name_grams,body_grams,tokenize='ascii'); PRAGMA user_version=0;")?;
    for number in 0..40 {
        let path = files.join(format!("synthetic-{number:02}.txt"));
        std::fs::write(&path, "合成旧索引内容")?;
        let record = scanner::metadata_record(&path)?;
        index::upsert_metadata(&mut db, &record)?;
        index::set_body(
            &mut db,
            &record.path,
            record.modified_ms,
            record.size,
            Some("合成旧索引内容"),
            "indexed",
        )?;
    }
    drop(db);
    let executable = Path::new(env!("CARGO_BIN_EXE_index-worker"));
    let mut worker = worker_client::Worker::start(
        executable.parent().expect("worker directory"),
        &database,
        std::slice::from_ref(&files),
        &[],
        false,
        false,
        eframe::egui::Context::default(),
    )?;
    wait_until("legacy conversion", || {
        index::open_reader(&database).ok().is_some_and(|db| {
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .ok()
                == Some(2)
        })
    })?;
    assert_eq!(count(&database, "合成旧索引内容"), 40);
    let db = index::open_reader(&database)?;
    let redundant: i64 = db.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name='terms_content'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(redundant, 0);
    drop(db);
    worker.stop();
    Ok(())
}

#[test]
fn failed_legacy_upgrade_keeps_old_index_writable() -> Result<()> {
    let temporary = tempdir()?;
    let files = temporary.path().join("files");
    let database = temporary.path().join("index.db");
    std::fs::create_dir_all(&files)?;
    let path = files.join("existing.txt");
    std::fs::write(&path, "原始合成文档")?;
    let mut db = index::open(&database)?;
    db.execute_batch("DROP TABLE terms; CREATE VIRTUAL TABLE terms USING fts5(name_grams,body_grams,tokenize='ascii'); PRAGMA user_version=0;")?;
    let record = scanner::metadata_record(&path)?;
    index::upsert_metadata(&mut db, &record)?;
    db.execute("DELETE FROM terms", [])?;
    drop(db);
    let executable = Path::new(env!("CARGO_BIN_EXE_index-worker"));
    let mut worker = worker_client::Worker::start(
        executable.parent().expect("worker directory"),
        &database,
        std::slice::from_ref(&files),
        &[],
        false,
        false,
        eframe::egui::Context::default(),
    )?;
    wait_until("legacy fallback reported", || {
        worker.messages.try_iter().any(|value| {
            value.get("state").and_then(|v| v.as_str()) == Some("optimization_fallback")
        })
    })?;
    std::fs::write(files.join("new.txt"), "回退后新增合成文档")?;
    wait_until("legacy fallback writes", || {
        count(&database, "回退后新增合成文档") == 1
    })?;
    worker.stop();
    Ok(())
}

#[test]
#[ignore = "large synthetic rows for the 512 MiB worker-job acceptance test"]
fn large_legacy_rows_upgrade_under_worker_memory_limit() -> Result<()> {
    let temporary = tempdir()?;
    let files = temporary.path().join("files");
    let database = temporary.path().join("index.db");
    std::fs::create_dir_all(&files)?;
    let mut db = index::open(&database)?;
    db.execute_batch("DROP TABLE terms; CREATE VIRTUAL TABLE terms USING fts5(name_grams,body_grams,tokenize='ascii'); PRAGMA user_version=0;")?;
    let mut seed = 0x5eed_u64;
    for number in 0..2 {
        let mut body = String::from("合成大文档标记");
        for _ in 0..300_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            body.push(
                char::from_u32(0x4e00 + ((seed >> 32) as u32 % 4096)).expect("synthetic character"),
            );
        }
        let path = files.join(format!("large-{number}.txt"));
        std::fs::write(&path, &body)?;
        let record = scanner::metadata_record(&path)?;
        index::upsert_metadata(&mut db, &record)?;
        index::set_body(
            &mut db,
            &record.path,
            record.modified_ms,
            record.size,
            Some(&body),
            "indexed",
        )?;
    }
    let longest: i64 = db.query_row("SELECT max(length(body_grams)) FROM terms", [], |row| {
        row.get(0)
    })?;
    assert!(longest > 4 * 1024 * 1024);
    drop(db);
    let executable = Path::new(env!("CARGO_BIN_EXE_index-worker"));
    let mut worker = worker_client::Worker::start(
        executable.parent().expect("worker directory"),
        &database,
        std::slice::from_ref(&files),
        &[],
        false,
        true,
        eframe::egui::Context::default(),
    )?;
    std::thread::sleep(Duration::from_millis(500));
    assert!(worker.running());
    let paused_version: i64 =
        index::open_reader(&database)?
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
    assert_eq!(paused_version, 0);
    worker.send("resume")?;
    // This checks correctness under the real CPU/memory job limits, not a
    // migration-time SLA; shared disks can make large synthetic rows slow.
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut last_status = String::from("no status received");
    while Instant::now() < deadline {
        while let Ok(message) = worker.messages.try_recv() {
            let state = message
                .get("state")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");
            let copied = message
                .get("copied")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let total = message
                .get("total")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            last_status = format!("{state} ({copied}/{total})");
        }
        if !worker.running() {
            bail!("resource-limited synthetic worker exited; last status: {last_status}")
        }
        if index::open_reader(&database).ok().is_some_and(|db| {
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .ok()
                == Some(2)
        }) {
            assert_eq!(count(&database, "合成大文档标记"), 2);
            worker.stop();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("synthetic large-row upgrade did not complete; last status: {last_status}")
}
