#![cfg(windows)]

#[path = "../src/worker_client.rs"]
mod worker_client;

use anyhow::{Result, bail};
use llmlocalsearch::{
    index,
    query::{self, Scope},
    scanner,
};
use rusqlite::params;
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
fn indexes_only_office_and_markdown_content_but_keeps_every_filename() -> Result<()> {
    let temporary = tempdir()?;
    let files = temporary.path().join("files");
    let database = temporary.path().join("index.db");
    std::fs::create_dir(&files)?;
    std::fs::write(files.join("allowed.md"), "允许正文合成标记")?;
    for name in [
        "excluded.txt",
        "excluded.html",
        "excluded.pdf",
        "excluded.py",
        "excluded.json",
        "excluded.yaml",
        "excluded",
    ] {
        std::fs::write(files.join(name), "不应进入正文索引")?;
    }
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
    wait_until("mixed synthetic content scope", || {
        count(&database, "允许正文合成标记") == 1
            && index::open_reader(&database)
                .ok()
                .and_then(|db| index::stats(&db).ok())
                .is_some_and(|stats| stats.files == 8)
    })?;
    let db = index::open_reader(&database)?;
    assert_eq!(
        index::search(&db, "excluded", Scope::Name, &[], &[], || false)?.len(),
        7
    );
    assert!(index::search(&db, "不应进入正文索引", Scope::Content, &[], &[], || false)?.is_empty());
    let excluded: i64 = db.query_row(
        "SELECT count(*) FROM files WHERE kind NOT IN ('markdown','word','powerpoint','excel') AND state='name_only' AND body IS NULL",
        [], |row| row.get(0),
    )?;
    assert_eq!(excluded, 7);
    drop(db);
    worker.stop();
    Ok(())
}

#[test]
fn worker_migrates_legacy_content_scope_without_losing_names() -> Result<()> {
    let temporary = tempdir()?;
    let files = temporary.path().join("files");
    let database = temporary.path().join("index.db");
    std::fs::create_dir(&files)?;
    let old = files.join("legacy.txt");
    let keep = files.join("keep.md");
    std::fs::write(&old, "旧版正文合成标记")?;
    std::fs::write(&keep, "保留Markdown合成正文")?;
    let mut db = index::open(&database)?;
    let old_record = scanner::metadata_record(&old)?;
    index::upsert_metadata(&mut db, &old_record)?;
    let old_id: i64 = db.query_row(
        "SELECT id FROM files WHERE path=?1",
        [&old_record.path],
        |r| r.get(0),
    )?;
    db.execute(
        "UPDATE files SET body='旧版正文合成标记',state='indexed' WHERE id=?1",
        [old_id],
    )?;
    db.execute("DELETE FROM terms WHERE rowid=?1", [old_id])?;
    db.execute(
        "INSERT INTO terms(rowid,name_grams,body_grams) VALUES(?1,?2,?3)",
        params![
            old_id,
            query::grams(&old_record.name),
            query::grams("旧版正文合成标记")
        ],
    )?;
    let kept_record = scanner::metadata_record(&keep)?;
    index::upsert_metadata(&mut db, &kept_record)?;
    assert!(index::set_body(
        &mut db,
        &kept_record.path,
        kept_record.modified_ms,
        kept_record.size,
        Some("保留Markdown合成正文"),
        "indexed"
    )?);
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
    wait_until("legacy content scope completed", || {
        index::open_reader(&database).ok().is_some_and(|db| {
            db.query_row(
                "SELECT complete FROM content_scope_state WHERE singleton=1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .ok()
                == Some(1)
        })
    })?;
    wait_until("content scope completion status", || {
        worker.messages.try_iter().any(|message| {
            message.get("state").and_then(|value| value.as_str()) == Some("content_scope_complete")
        })
    })?;
    let db = index::open_reader(&database)?;
    assert_eq!(
        db.query_row("SELECT state FROM files WHERE id=?1", [old_id], |r| r
            .get::<_, String>(0))?,
        "name_only"
    );
    assert!(db.query_row(
        "SELECT body IS NULL FROM files WHERE id=?1",
        [old_id],
        |r| r.get::<_, bool>(0)
    )?);
    assert_eq!(
        index::search(&db, "legacy.txt", Scope::Name, &[], &[], || false)?.len(),
        1
    );
    assert!(index::search(&db, "旧版正文合成标记", Scope::Content, &[], &[], || false)?.is_empty());
    assert_eq!(
        index::search(
            &db,
            "保留Markdown合成正文",
            Scope::Content,
            &[],
            &[],
            || false
        )?
        .len(),
        1
    );
    drop(db);
    worker.stop();
    Ok(())
}

#[test]
#[ignore = "requires verified Tika and Java runtime; set LLMLOCALSEARCH_TIKA_RUNTIME"]
fn worker_extracts_synthetic_office_content() -> Result<()> {
    let runtime = std::env::var_os("LLMLOCALSEARCH_TIKA_RUNTIME")
        .ok_or_else(|| anyhow::anyhow!("missing synthetic test runtime"))?;
    let temporary = tempdir()?;
    let files = temporary.path().join("files");
    let database = temporary.path().join("index.db");
    std::fs::create_dir(&files)?;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in ["synthetic.docx", "synthetic.pptx", "synthetic.xlsx"] {
        std::fs::copy(fixtures.join(name), files.join(name))?;
    }
    let child = Command::new(env!("CARGO_BIN_EXE_index-worker"))
        .arg(&database)
        .arg(runtime)
        .args(["0", "1"])
        .arg(&files)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()?;
    let mut child = ChildGuard(child);
    writeln!(child.0.stdin.as_mut().expect("piped stdin"), "start")?;
    child.0.stdin.as_mut().expect("piped stdin").flush()?;
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if child.0.try_wait()?.is_some() {
            bail!("synthetic Office worker exited");
        }
        if ["表格内容香蕉", "合成幻灯备注海王星", "搜索橘子"]
            .iter()
            .all(|term| count(&database, term) == 1)
        {
            writeln!(child.0.stdin.as_mut().expect("piped stdin"), "stop")?;
            child.0.stdin.as_mut().expect("piped stdin").flush()?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("synthetic Office content was not searchable before the deadline")
}

#[test]
fn first_content_is_indexed_before_all_roots_finish_scanning() -> Result<()> {
    let temporary = tempdir()?;
    let first = temporary.path().join("first");
    let later = temporary.path().join("later");
    let database = temporary.path().join("index.db");
    std::fs::create_dir_all(&first)?;
    std::fs::create_dir_all(&later)?;
    std::fs::write(first.join("synthetic.md"), "合成渐进正文")?;
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
    std::fs::write(root.join("old-name.md"), "合成恢复内容")?;
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
    std::fs::rename(root.join("old-name.md"), root.join("new-name.md"))?;
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
    std::fs::write(files.join("初始.md"), "合成初始内容")?;
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
    let new = files.join("新增.md");
    std::fs::write(&new, "合成木星")?;
    wait_until("create", || count(&db, "木星") == 1)?;
    std::fs::write(&new, "合成水星")?;
    wait_until("modify", || {
        count(&db, "水星") == 1 && count(&db, "木星") == 0
    })?;
    let lower = files.join("lower.md");
    let upper = files.join("LOWER.md");
    std::fs::write(&lower, "大小写重命名")?;
    wait_until("before case rename", || count(&db, "lower") == 1)?;
    std::fs::rename(&lower, &upper)?;
    wait_until("case rename", || {
        count(&db, "lower") == 1
            && index::open_reader(&db)
                .ok()
                .and_then(|c| index::search(&c, "lower", Scope::Name, &[], &[], || false).ok())
                .is_some_and(|hits| hits.first().is_some_and(|hit| hit.file.name == "LOWER.md"))
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
    let path = files.join("locked.md");
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
        state(&db, "locked.md").is_some_and(|s| s == "retry_wait")
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
    std::fs::write(files.join("synthetic.md"), "合成暂停测试")?;
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
            before.join(format!("synthetic-{number:02}.md")),
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
            after.join(format!("added-{number:02}.md")),
            "批量新增合成标记",
        )?;
    }
    wait_until("batch create", || {
        count(&database, "批量新增合成标记") == 20
    })?;
    worker.stop();
    drop(worker);
    std::fs::write(after.join("offline.md"), "离线新增合成标记")?;
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
        let path = files.join(format!("synthetic-{number:02}.md"));
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
    let path = files.join("existing.md");
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
    std::fs::write(files.join("new.md"), "回退后新增合成文档")?;
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
        let path = files.join(format!("large-{number}.md"));
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
