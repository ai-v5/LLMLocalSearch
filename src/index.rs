use crate::query::{self, Scope};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs::OpenOptions;
use std::path::Path;

const APPLICATION_ID: i64 = 0x4c4c5358;
const INDEX_SCHEMA_VERSION: i64 = 2;

#[derive(Clone, Debug)]
pub struct FileRecord {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub modified_ms: i64,
    pub size: u64,
    pub state: String,
}

#[derive(Clone, Debug)]
pub struct BodyUpdate {
    pub path: String,
    pub modified_ms: i64,
    pub size: u64,
    pub body: Option<String>,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    /// Byte offsets inside `text`, on UTF-8 boundaries.
    pub hit_start: usize,
    pub hit_end: usize,
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub id: i64,
    pub file: FileRecord,
    pub name_match: bool,
    pub snippet: Option<Snippet>,
}

#[derive(Clone, Debug, Default)]
pub struct IndexStats {
    pub files: u64,
    pub pending: u64,
    pub indexed: u64,
    pub failed: u64,
    pub name_only: u64,
    pub no_text: u64,
    pub cloud: u64,
    pub scan_errors: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactStep {
    Current,
    Copying { copied: u64, total: u64 },
    Finished { copied: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimPhase {
    None,
    Vacuum,
    Checkpoint,
}

fn supported_schema(connection: &Connection, allow_empty: bool) -> Result<Option<String>> {
    // The worker may swap the FTS table while a new reader is opening. Version
    // and schema must be inspected from one snapshot to avoid a false mismatch.
    let snapshot = connection.unchecked_transaction()?;
    let version: i64 = snapshot.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 0 && version != INDEX_SCHEMA_VERSION {
        bail!("索引版本不受当前程序支持");
    }
    let terms: Option<String> = snapshot
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='terms'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match terms.as_deref() {
        Some(sql) => {
            let sql = sql.to_ascii_lowercase();
            let valid_legacy =
                version == 0 && sql.contains("using fts5") && !sql.contains("contentless_delete");
            let valid_current = version == INDEX_SCHEMA_VERSION
                && sql.contains("contentless_delete")
                && sql.contains("detail=column");
            if !valid_legacy && !valid_current {
                bail!("索引结构与版本不一致");
            }
        }
        None if allow_empty && version == 0 => {
            let files_exist: bool = snapshot.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='files')",
                [],
                |row| row.get(0),
            )?;
            if files_exist {
                let files: i64 =
                    snapshot.query_row("SELECT count(*) FROM files", [], |row| row.get(0))?;
                if files != 0 {
                    bail!("索引缺少搜索表，未修改现有文件记录");
                }
            }
        }
        None => bail!("索引缺少搜索表"),
    }
    snapshot.commit()?;
    Ok(terms)
}

pub fn open(path: &Path) -> Result<Connection> {
    let created = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e).context("无法创建索引数据库"),
    };
    let connection = Connection::open(path).with_context(|| "无法打开索引数据库")?;
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    let version: String = connection.query_row("select sqlite_version()", [], |r| r.get(0))?;
    let numbers: Vec<_> = version
        .split('.')
        .filter_map(|x| x.parse::<u32>().ok())
        .collect();
    if numbers.as_slice() < [3, 51, 3].as_slice() {
        bail!("SQLite 版本不满足索引安全要求");
    }
    if created {
        connection.pragma_update(None, "application_id", APPLICATION_ID)?;
    } else {
        let id: i64 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
        if id != APPLICATION_ID {
            bail!("目标位置存在其他数据库，未修改");
        }
    }
    let terms = supported_schema(&connection, true)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            modified_ms INTEGER NOT NULL,
            size INTEGER NOT NULL,
            state TEXT NOT NULL,
            body TEXT
        );
        CREATE INDEX IF NOT EXISTS files_modified ON files(modified_ms DESC, id);
        CREATE INDEX IF NOT EXISTS files_state_modified ON files(state, modified_ms DESC, id);
        CREATE TABLE IF NOT EXISTS retries(file_id INTEGER PRIMARY KEY, attempts INTEGER NOT NULL, next_ms INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS retries_next ON retries(next_ms);
        CREATE TABLE IF NOT EXISTS scan_issues(path TEXT PRIMARY KEY, category TEXT NOT NULL);",
    )?;
    if terms.is_none() {
        // Original text is kept in `files`; FTS only needs the per-column posting lists.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE VIRTUAL TABLE terms USING fts5(name_grams, body_grams,
                 tokenize='ascii', content='', contentless_delete=1, detail=column);
             PRAGMA user_version=2;
             COMMIT;",
        )?;
    }
    Ok(connection)
}

pub fn open_reader(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    let id: i64 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if id != APPLICATION_ID {
        bail!("目标位置存在其他数据库，未修改");
    }
    supported_schema(&connection, false)?;
    Ok(connection)
}

/// Convert one bounded batch of a version-0 FTS table. The old search table stays
/// readable until the compact replacement is complete and swapped atomically.
pub fn compact_legacy_step(connection: &mut Connection, batch_size: usize) -> Result<CompactStep> {
    if batch_size == 0 {
        bail!("索引优化批量大小无效");
    }
    let batch_limit = i64::try_from(batch_size).context("索引优化批量大小无效")?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == INDEX_SCHEMA_VERSION {
        tx.commit()?;
        return Ok(CompactStep::Current);
    }
    if version != 0 {
        bail!("索引版本不受当前程序支持");
    }
    // Block a concurrently launched older worker from changing files while a
    // partially built replacement exists. A failed old transaction rolls back.
    tx.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS terms_compacting USING fts5(name_grams, body_grams,
             tokenize='ascii', content='', contentless_delete=1, detail=column);
         CREATE TABLE IF NOT EXISTS index_compaction_state(
             singleton INTEGER PRIMARY KEY CHECK(singleton=1), last_id INTEGER NOT NULL,
             copied INTEGER NOT NULL, total INTEGER NOT NULL);
         CREATE TRIGGER IF NOT EXISTS index_compaction_guard_insert BEFORE INSERT ON files
             BEGIN SELECT RAISE(ABORT, 'Index optimization in progress'); END;
         CREATE TRIGGER IF NOT EXISTS index_compaction_guard_update BEFORE UPDATE ON files
             BEGIN SELECT RAISE(ABORT, 'Index optimization in progress'); END;
         CREATE TRIGGER IF NOT EXISTS index_compaction_guard_delete BEFORE DELETE ON files
             BEGIN SELECT RAISE(ABORT, 'Index optimization in progress'); END;",
    )?;
    let state: Option<(i64, i64, i64)> = tx
        .query_row(
            "SELECT last_id,copied,total FROM index_compaction_state WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (last_id, previous, total) = if let Some(state) = state {
        state
    } else {
        let total: i64 = tx.query_row("SELECT count(*) FROM files", [], |row| row.get(0))?;
        tx.execute(
            "INSERT INTO index_compaction_state VALUES(1,0,0,?1)",
            [total],
        )?;
        (0, 0, total)
    };
    if last_id < 0 || previous < 0 || total < 0 {
        bail!("索引优化状态无效");
    }
    let (rows, exhausted) = {
        let mut statement = tx.prepare(
            "SELECT rowid,name_grams,body_grams FROM terms WHERE rowid>?1 ORDER BY rowid LIMIT ?2",
        )?;
        let mut query = statement.query(params![last_id, batch_limit])?;
        let mut rows = Vec::new();
        let mut bytes = 0usize;
        let mut exhausted = false;
        while rows.len() < batch_size && bytes < 4 * 1024 * 1024 {
            let Some(row) = query.next()? else {
                exhausted = true;
                break;
            };
            let name: String = row.get(1)?;
            let body: String = row.get(2)?;
            bytes = bytes.saturating_add(name.len()).saturating_add(body.len());
            rows.push((row.get::<_, i64>(0)?, name, body));
        }
        (rows, exhausted)
    };
    for (id, name, body) in &rows {
        tx.execute(
            "INSERT INTO terms_compacting(rowid,name_grams,body_grams) VALUES(?1,?2,?3)",
            params![id, name, body],
        )?;
    }
    let copied = previous
        .checked_add(rows.len() as i64)
        .context("索引优化计数溢出")?;
    let newest = rows.last().map_or(last_id, |row| row.0);
    tx.execute(
        "UPDATE index_compaction_state SET last_id=?1,copied=?2 WHERE singleton=1",
        params![newest, copied],
    )?;
    if exhausted {
        let files: i64 = tx.query_row("SELECT count(*) FROM files", [], |row| row.get(0))?;
        let original: i64 = tx.query_row("SELECT count(*) FROM terms", [], |row| row.get(0))?;
        let replacement: i64 =
            tx.query_row("SELECT count(*) FROM terms_compacting", [], |row| {
                row.get(0)
            })?;
        if total != files || files != original || original != replacement || replacement != copied {
            bail!("索引优化校验失败，原搜索表仍保留");
        }
        tx.execute_batch(
            "DROP TABLE terms;
             ALTER TABLE terms_compacting RENAME TO terms;
             DROP TRIGGER index_compaction_guard_insert;
             DROP TRIGGER index_compaction_guard_update;
             DROP TRIGGER index_compaction_guard_delete;
             DROP TABLE index_compaction_state;
             CREATE TABLE IF NOT EXISTS index_maintenance(
                 singleton INTEGER PRIMARY KEY CHECK(singleton=1), reclaim_pending INTEGER NOT NULL);
             INSERT OR REPLACE INTO index_maintenance VALUES(1,1);
             PRAGMA user_version=2;",
        )?;
        tx.commit()?;
        Ok(CompactStep::Finished {
            copied: copied as u64,
        })
    } else {
        tx.commit()?;
        Ok(CompactStep::Copying {
            copied: copied as u64,
            total: total as u64,
        })
    }
}

pub fn reclaim_phase(connection: &Connection) -> Result<ReclaimPhase> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='index_maintenance')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(ReclaimPhase::None);
    }
    let state = connection
        .query_row(
            "SELECT reclaim_pending FROM index_maintenance WHERE singleton=1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    match state {
        None | Some(0) => Ok(ReclaimPhase::None),
        Some(1) => Ok(ReclaimPhase::Vacuum),
        Some(2) => Ok(ReclaimPhase::Checkpoint),
        _ => bail!("索引空间整理状态无效"),
    }
}

/// VACUUM is atomic in SQLite; an interruption leaves the current database
/// usable and the pending marker intact for a later retry.
pub fn reclaim_vacuum(
    connection: &Connection,
    mut cancelled: impl FnMut() -> bool + Send + 'static,
) -> Result<bool> {
    if cancelled() {
        return Ok(false);
    }
    connection.progress_handler(1000, Some(cancelled))?;
    let result = connection.execute_batch("VACUUM");
    connection.progress_handler(0, None::<fn() -> bool>)?;
    match result {
        Ok(()) => {
            connection.execute(
                "UPDATE index_maintenance SET reclaim_pending=2 WHERE singleton=1",
                [],
            )?;
            Ok(true)
        }
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::OperationInterrupted =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

/// Finish the WAL checkpoint without waiting on an interactive reader.
pub fn reclaim_checkpoint(connection: &Connection) -> Result<bool> {
    connection.busy_timeout(std::time::Duration::from_millis(100))?;
    let result = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    });
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    let (busy, _, _) = result?;
    if busy != 0 {
        return Ok(false);
    }
    connection.execute("DROP TABLE IF EXISTS index_maintenance", [])?;
    Ok(true)
}

/// Discard only the derived, incomplete replacement. The version-0 search
/// table and file records remain intact; callers must report a failed cleanup.
pub fn abort_legacy_compaction(connection: &mut Connection) -> Result<bool> {
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == INDEX_SCHEMA_VERSION {
        tx.commit()?;
        return Ok(false);
    }
    if version != 0 {
        bail!("索引版本不受当前程序支持");
    }
    let original_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='terms')",
        [],
        |row| row.get(0),
    )?;
    if !original_exists {
        bail!("原搜索表不存在，无法撤销索引优化");
    }
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS index_compaction_guard_insert;
         DROP TRIGGER IF EXISTS index_compaction_guard_update;
         DROP TRIGGER IF EXISTS index_compaction_guard_delete;
         DROP TABLE IF EXISTS terms_compacting;
         DROP TABLE IF EXISTS index_compaction_state;",
    )?;
    tx.commit()?;
    Ok(true)
}

/// Metadata changes invalidate stale content in the same transaction as the FTS update.
pub fn upsert_metadata(connection: &mut Connection, record: &FileRecord) -> Result<bool> {
    upsert_metadata_inner(connection, record, false).map(|(changed, _)| changed)
}

pub(crate) fn upsert_metadata_with_id(
    connection: &mut Connection,
    record: &FileRecord,
) -> Result<(bool, i64)> {
    upsert_metadata_inner(connection, record, false)
}

/// A watcher event can signal a write even when size and mtime were preserved.
pub fn invalidate_metadata(connection: &mut Connection, record: &FileRecord) -> Result<bool> {
    upsert_metadata_inner(connection, record, true).map(|(changed, _)| changed)
}

fn upsert_metadata_inner(
    connection: &mut Connection,
    record: &FileRecord,
    force: bool,
) -> Result<(bool, i64)> {
    let tx = connection.transaction()?;
    let previous: Option<(i64, i64, i64, String, String)> = tx
        .query_row(
            "SELECT id, modified_ms, size, name, state FROM files WHERE path=?1",
            [&record.path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    if let Some((id, modified, size, ref name, ref state)) = previous
        && !force
        && modified == record.modified_ms
        && size == record.size as i64
        && name == &record.name
        && !(state == "cloud_placeholder" && record.state != "cloud_placeholder")
        && !(state == "name_only" && record.state == "pending")
        && !((record.state == "cloud_placeholder" || record.state == "name_only")
            && state != &record.state)
        && !matches!(
            state.as_str(),
            "retry_wait" | "failed" | "inaccessible" | "parser_unavailable"
        )
    {
        tx.commit()?;
        return Ok((false, id));
    }
    tx.execute(
        "INSERT INTO files(path,name,kind,modified_ms,size,state,body)
         VALUES(?1,?2,?3,?4,?5,?6,NULL)
         ON CONFLICT(path) DO UPDATE SET name=excluded.name,kind=excluded.kind,
         modified_ms=excluded.modified_ms,size=excluded.size,state=excluded.state,body=NULL",
        params![
            record.path,
            record.name,
            record.kind,
            record.modified_ms,
            record.size as i64,
            record.state
        ],
    )?;
    let id: i64 = tx.query_row("SELECT id FROM files WHERE path=?1", [&record.path], |r| {
        r.get(0)
    })?;
    tx.execute("DELETE FROM retries WHERE file_id=?1", [id])?;
    tx.execute("DELETE FROM terms WHERE rowid=?1", [id])?;
    tx.execute(
        "INSERT INTO terms(rowid,name_grams,body_grams) VALUES(?1,?2,'')",
        params![id, query::grams(&record.name)],
    )?;
    tx.commit()?;
    Ok((true, id))
}

/// Apply extraction only if the metadata still corresponds to the parsed version.
pub fn set_body(
    connection: &mut Connection,
    path: &str,
    modified_ms: i64,
    size: u64,
    body: Option<&str>,
    state: &str,
) -> Result<bool> {
    let update = BodyUpdate {
        path: path.to_owned(),
        modified_ms,
        size,
        body: body.map(str::to_owned),
        state: state.to_owned(),
    };
    Ok(set_bodies(connection, &[update])? == 1)
}

pub fn set_bodies(connection: &mut Connection, updates: &[BodyUpdate]) -> Result<usize> {
    let encoded: Vec<String> = updates
        .iter()
        .map(|update| update.body.as_deref().map(query::grams).unwrap_or_default())
        .collect();
    let tx = connection.transaction()?;
    let mut applied = 0;
    for (update, body_grams) in updates.iter().zip(encoded) {
        let entry: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, name FROM files WHERE path=?1 AND modified_ms=?2 AND size=?3",
                params![update.path, update.modified_ms, update.size as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, name)) = entry else {
            continue;
        };
        tx.execute("DELETE FROM retries WHERE file_id=?1", [id])?;
        tx.execute(
            "UPDATE files SET body=?1,state=?2 WHERE id=?3",
            params![update.body, update.state, id],
        )?;
        tx.execute("DELETE FROM terms WHERE rowid=?1", [id])?;
        tx.execute(
            "INSERT INTO terms(rowid,name_grams,body_grams) VALUES(?1,?2,?3)",
            params![id, query::grams(&name), body_grams],
        )?;
        applied += 1;
    }
    tx.commit()?;
    Ok(applied)
}

pub fn schedule_retry(
    connection: &mut Connection,
    path: &str,
    modified_ms: i64,
    size: u64,
) -> Result<bool> {
    let tx = connection.transaction()?;
    let id = tx
        .query_row(
            "SELECT id FROM files WHERE path=?1 AND modified_ms=?2 AND size=?3",
            params![path, modified_ms, size as i64],
            |r| r.get::<_, i64>(0),
        )
        .optional()?;
    let Some(id) = id else {
        tx.commit()?;
        return Ok(false);
    };
    let attempts = tx
        .query_row("SELECT attempts FROM retries WHERE file_id=?1", [id], |r| {
            r.get::<_, u32>(0)
        })
        .optional()?
        .unwrap_or(0)
        .saturating_add(1);
    let delay_ms = (5_000_i64 * (1_i64 << attempts.saturating_sub(1).min(6))).min(300_000);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    tx.execute("UPDATE files SET state='retry_wait' WHERE id=?1", [id])?;
    tx.execute("INSERT INTO retries(file_id,attempts,next_ms) VALUES(?1,?2,?3) ON CONFLICT(file_id) DO UPDATE SET attempts=excluded.attempts,next_ms=excluded.next_ms", params![id, attempts, now.saturating_add(delay_ms)])?;
    tx.commit()?;
    Ok(true)
}

#[derive(Clone, Copy)]
pub enum PathTable {
    Files,
    ScanIssues,
}

pub fn path_batch(
    connection: &Connection,
    table: PathTable,
    next_id: i64,
) -> Result<Vec<(i64, String)>> {
    const BATCH_SIZE: i64 = 256;
    let sql = match table {
        PathTable::Files => "SELECT id,path FROM files WHERE id>=?1 ORDER BY id LIMIT ?2",
        PathTable::ScanIssues => {
            "SELECT rowid,path FROM scan_issues WHERE rowid>=?1 ORDER BY rowid LIMIT ?2"
        }
    };
    let mut statement = connection.prepare(sql)?;
    let paths = statement
        .query_map([next_id, BATCH_SIZE], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(paths)
}

/// Visit stable row IDs in small batches so callers can modify the table between reads.
pub fn visit_paths(
    connection: &mut Connection,
    table: PathTable,
    mut visit: impl FnMut(&mut Connection, i64, &str) -> Result<bool>,
) -> Result<()> {
    let mut next_id = i64::MIN;
    loop {
        let batch = path_batch(connection, table, next_id)?;
        let Some(next) = batch.last().map(|(id, _)| id.checked_add(1)) else {
            return Ok(());
        };
        for (id, path) in batch {
            if !visit(connection, id, &path)? {
                return Ok(());
            }
        }
        match next {
            Some(id) => next_id = id,
            None => return Ok(()),
        }
    }
}

pub fn remove_path(connection: &mut Connection, path: &str) -> Result<()> {
    let tx = connection.transaction()?;
    if let Some(id) = tx
        .query_row("SELECT id FROM files WHERE path=?1", [path], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?
    {
        tx.execute("DELETE FROM terms WHERE rowid=?1", [id])?;
        tx.execute("DELETE FROM retries WHERE file_id=?1", [id])?;
        tx.execute("DELETE FROM files WHERE id=?1", [id])?;
    }
    tx.commit()?;
    Ok(())
}

pub fn stats(connection: &Connection) -> Result<IndexStats> {
    Ok(connection.query_row(
        "SELECT count(*), coalesce(sum(state IN ('pending','retry_wait')),0), coalesce(sum(state='indexed'),0),
         coalesce(sum(state IN ('failed','encrypted','too_large','inaccessible','parser_unavailable')),0),
         coalesce(sum(state='name_only'),0), coalesce(sum(state='no_text'),0), coalesce(sum(state='cloud_placeholder'),0),
         (SELECT count(*) FROM scan_issues) FROM files", [],
        |r| Ok(IndexStats { files: r.get::<_, i64>(0)? as u64, pending: r.get::<_, i64>(1)? as u64, indexed: r.get::<_, i64>(2)? as u64, failed: r.get::<_, i64>(3)? as u64, name_only: r.get::<_, i64>(4)? as u64, no_text: r.get::<_, i64>(5)? as u64, cloud: r.get::<_, i64>(6)? as u64, scan_errors: r.get::<_, i64>(7)? as u64 }),
    )?)
}

/// Materializes a verified result snapshot for non-interactive consumers.
pub fn search(
    connection: &Connection,
    value: &str,
    scope: Scope,
    kinds: &[String],
    excluded: &[std::path::PathBuf],
    cancelled: impl Fn() -> bool,
) -> Result<Vec<SearchHit>> {
    let mut result = Vec::new();
    search_stream(
        connection,
        value,
        scope,
        kinds,
        excluded,
        cancelled,
        |batch| result.extend(batch),
    )?;
    Ok(result)
}

/// Emits verified hits in final sort order. The read statement ends when the search finishes;
/// pagination uses already-emitted results and does not retain a database transaction.
pub fn search_stream(
    connection: &Connection,
    value: &str,
    scope: Scope,
    kinds: &[String],
    excluded: &[std::path::PathBuf],
    cancelled: impl Fn() -> bool,
    mut emit: impl FnMut(Vec<SearchHit>),
) -> Result<usize> {
    let terms = query::parse(value);
    if terms.is_empty() {
        return Ok(0);
    }
    let expression = query::fts_expression(&terms, scope);
    let tx = connection.unchecked_transaction()?;
    // Sort only IDs and timestamps. Sorting full document bodies would spill large
    // amounts of text to disk before the first page can be displayed.
    let ids = {
        let mut statement = tx.prepare(
            "SELECT files.id FROM terms JOIN files ON files.id=terms.rowid WHERE terms MATCH ?1
             ORDER BY files.modified_ms DESC, files.id ASC",
        )?;
        let rows = statement.query_map([expression], |r| r.get::<_, i64>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let row_query = if scope == Scope::Name {
        "SELECT id,path,name,kind,modified_ms,size,state,NULL FROM files WHERE id=?1"
    } else {
        "SELECT id,path,name,kind,modified_ms,size,state,body FROM files WHERE id=?1"
    };
    let mut statement = tx.prepare(row_query)?;
    let mut batch = Vec::with_capacity(50);
    let mut total = 0usize;
    for id in ids {
        if cancelled() {
            break;
        }
        let mut rows = statement.query([id])?;
        let Some(row) = rows.next()? else { continue };
        let path: String = row.get(1)?;
        if excluded
            .iter()
            .any(|dir| crate::paths::within(Path::new(&path), dir))
        {
            continue;
        }
        let name: String = row.get(2)?;
        let kind: String = row.get(3)?;
        if !kinds.is_empty() && !kinds.contains(&kind) {
            continue;
        }
        let body: Option<String> = row.get(7)?;
        if !query::matches(&terms, &name, body.as_deref().unwrap_or(""), scope) {
            continue;
        }
        let name_match =
            scope != Scope::Content && terms.iter().any(|t| query::normalize(&name).contains(t));
        let excerpt = if scope != Scope::Name {
            body.as_deref().and_then(|s| snippet(s, &terms))
        } else {
            None
        };
        batch.push(SearchHit {
            id: row.get(0)?,
            file: FileRecord {
                path,
                name,
                kind,
                modified_ms: row.get(4)?,
                size: row.get::<_, i64>(5)? as u64,
                state: row.get(6)?,
            },
            name_match,
            snippet: excerpt,
        });
        total += 1;
        if batch.len() == 50 {
            emit(std::mem::take(&mut batch));
        }
    }
    if !batch.is_empty() {
        emit(batch);
    }
    drop(statement);
    tx.commit()?;
    Ok(total)
}

pub fn snippet(value: &str, terms: &[String]) -> Option<Snippet> {
    let normalized = query::normalize(value);
    let (start, term) = terms
        .iter()
        .filter_map(|term| normalized.find(term).map(|pos| (pos, term)))
        .min_by_key(|x| x.0)?;
    let end = start + term.len();
    let mut norm_offset = 0;
    let mut raw_start = None;
    let mut raw_end = None;
    for (i, c) in value.char_indices() {
        let lowered_len: usize = c.to_lowercase().map(char::len_utf8).sum();
        if raw_start.is_none() && norm_offset + lowered_len > start {
            raw_start = Some(i);
        }
        if norm_offset < end && norm_offset + lowered_len >= end {
            raw_end = Some(i + c.len_utf8());
            break;
        }
        norm_offset += lowered_len;
    }
    let (raw_start, raw_end) = (raw_start?, raw_end?);
    let before = value[..raw_start]
        .char_indices()
        .rev()
        .nth(39)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let after = value[raw_end..]
        .char_indices()
        .nth(40)
        .map(|(i, _)| raw_end + i)
        .unwrap_or(value.len());
    let prefix = if before > 0 { "…" } else { "" };
    let suffix = if after < value.len() { "…" } else { "" };
    let text = format!("{prefix}{}{suffix}", &value[before..after]);
    Some(Snippet {
        text,
        hit_start: prefix.len() + raw_start - before,
        hit_end: prefix.len() + raw_end - before,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn batched_paths_include_boundary_ids_when_rows_are_deleted() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT NOT NULL);
             CREATE TABLE scan_issues(path TEXT PRIMARY KEY,category TEXT NOT NULL);",
        )?;
        let mut expected = vec![i64::MIN, -1];
        expected.extend(0..600);
        expected.push(i64::MAX);
        for id in &expected {
            db.execute(
                "INSERT INTO files(id,path) VALUES(?1,?2)",
                params![id, id.to_string()],
            )?;
        }
        let mut visited = Vec::new();
        visit_paths(&mut db, PathTable::Files, |db, id, _| {
            visited.push(id);
            db.execute("DELETE FROM files WHERE id=?1", [id])?;
            Ok(true)
        })?;
        assert_eq!(visited, expected);
        assert_eq!(
            db.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))?,
            0
        );

        for id in 1..=600 {
            db.execute(
                "INSERT INTO scan_issues(rowid,path,category) VALUES(?1,?2,'inaccessible')",
                params![id, id.to_string()],
            )?;
        }
        let mut count = 0;
        visit_paths(&mut db, PathTable::ScanIssues, |db, id, _| {
            count += 1;
            db.execute("DELETE FROM scan_issues WHERE rowid=?1", [id])?;
            Ok(true)
        })?;
        assert_eq!(count, 600);
        assert_eq!(
            db.query_row("SELECT count(*) FROM scan_issues", [], |r| r
                .get::<_, i64>(0))?,
            0
        );
        Ok(())
    }

    fn record(path: &str, name: &str, time: i64) -> FileRecord {
        FileRecord {
            path: path.into(),
            name: name.into(),
            kind: "txt".into(),
            modified_ms: time,
            size: 1,
            state: "pending".into(),
        }
    }

    #[test]
    fn fresh_index_does_not_duplicate_generated_gram_text() -> Result<()> {
        let temp = tempdir()?;
        let db = open(&temp.path().join("index.db"))?;
        let redundant_tables: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='terms_content'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            redundant_tables, 0,
            "generated n-grams must not be stored twice"
        );
        Ok(())
    }

    fn legacy_index(path: &Path) -> Result<Connection> {
        let db = open(path)?;
        db.execute_batch(
            "DROP TABLE terms;
             CREATE VIRTUAL TABLE terms USING fts5(name_grams,body_grams,tokenize='ascii');
             PRAGMA user_version=0;",
        )?;
        Ok(db)
    }

    #[test]
    fn legacy_compaction_can_resume_without_losing_search_or_writes() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("legacy.db");
        let mut db = legacy_index(&path)?;
        upsert_metadata(&mut db, &record("one", "项目报告.txt", 10))?;
        upsert_metadata(&mut db, &record("two", "其他.txt", 20))?;
        upsert_metadata(&mut db, &record("three", "最近.txt", 30))?;
        set_body(
            &mut db,
            "one",
            10,
            1,
            Some("包含 GPU 和中文短语"),
            "indexed",
        )?;
        set_body(&mut db, "two", 20, 1, Some("项目报告 GPU"), "indexed")?;
        let queries = [
            ("项", Scope::Both),
            ("项目", Scope::Content),
            ("项目报告 GPU", Scope::Both),
            ("报告", Scope::Name),
            ("\"中文短语\"", Scope::Content),
        ];
        let snapshot = |connection: &Connection| -> Result<Vec<Vec<String>>> {
            queries
                .iter()
                .map(|(value, scope)| {
                    Ok(search(connection, value, *scope, &[], &[], || false)?
                        .into_iter()
                        .map(|hit| hit.file.path)
                        .collect())
                })
                .collect()
        };
        let expected = snapshot(&db)?;
        assert_eq!(
            compact_legacy_step(&mut db, 1)?,
            CompactStep::Copying {
                copied: 1,
                total: 3
            }
        );
        assert_eq!(snapshot(&open_reader(&path)?)?, expected);
        assert!(upsert_metadata(&mut db, &record("new", "new.txt", 40)).is_err());
        assert_eq!(stats(&db)?.files, 3);
        drop(db);

        let mut db = open(&path)?;
        loop {
            match compact_legacy_step(&mut db, 1)? {
                CompactStep::Copying { .. } => {}
                CompactStep::Finished { copied } => {
                    assert_eq!(copied, 3);
                    break;
                }
                CompactStep::Current => panic!("unfinished migration was silently skipped"),
            }
        }
        assert_eq!(snapshot(&db)?, expected);
        assert_eq!(compact_legacy_step(&mut db, 1)?, CompactStep::Current);
        assert_eq!(reclaim_phase(&db)?, ReclaimPhase::Vacuum);
        assert!(!reclaim_vacuum(&db, || true)?);
        assert_eq!(reclaim_phase(&db)?, ReclaimPhase::Vacuum);
        assert_eq!(snapshot(&db)?, expected);
        assert!(reclaim_vacuum(&db, || false)?);
        assert_eq!(reclaim_phase(&db)?, ReclaimPhase::Checkpoint);
        assert!(reclaim_checkpoint(&db)?);
        assert_eq!(reclaim_phase(&db)?, ReclaimPhase::None);
        assert_eq!(snapshot(&db)?, expected);
        let content: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('terms_content','terms_compacting_content')",
            [], |row| row.get(0),
        )?;
        assert_eq!(content, 0);
        upsert_metadata(&mut db, &record("new", "new.txt", 40))?;
        set_body(&mut db, "new", 40, 1, Some("恢复写入"), "indexed")?;
        assert_eq!(
            search(&db, "恢复写入", Scope::Content, &[], &[], || false)?.len(),
            1
        );
        remove_path(&mut db, "new")?;
        assert!(search(&db, "恢复写入", Scope::Content, &[], &[], || false)?.is_empty());
        Ok(())
    }

    #[test]
    fn inconsistent_legacy_index_is_not_swapped() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("legacy.db");
        let mut db = legacy_index(&path)?;
        upsert_metadata(&mut db, &record("one", "one.txt", 10))?;
        db.execute("DELETE FROM terms", [])?;
        assert!(compact_legacy_step(&mut db, 10).is_err());
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(version, 0);
        let old_content: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='terms_content'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(old_content, 1);
        assert_eq!(stats(&db)?.files, 1);
        Ok(())
    }

    #[test]
    fn failed_partial_compaction_can_restore_legacy_writes() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("legacy.db");
        let mut db = legacy_index(&path)?;
        for (id, name) in [("one", "一.txt"), ("two", "二.txt"), ("three", "三.txt")] {
            upsert_metadata(&mut db, &record(id, name, 1))?;
            set_body(&mut db, id, 1, 1, Some("合成内容"), "indexed")?;
        }
        assert_eq!(
            compact_legacy_step(&mut db, 1)?,
            CompactStep::Copying {
                copied: 1,
                total: 3
            }
        );
        db.execute("DELETE FROM terms_compacting WHERE rowid=1", [])?;
        assert!(matches!(
            compact_legacy_step(&mut db, 1)?,
            CompactStep::Copying { .. }
        ));
        assert!(matches!(
            compact_legacy_step(&mut db, 1)?,
            CompactStep::Copying { .. }
        ));
        assert!(compact_legacy_step(&mut db, 1).is_err());
        assert_eq!(
            search(&db, "合成内容", Scope::Content, &[], &[], || false)?.len(),
            3
        );
        assert!(abort_legacy_compaction(&mut db)?);
        assert_eq!(
            search(&db, "合成内容", Scope::Content, &[], &[], || false)?.len(),
            3
        );
        upsert_metadata(&mut db, &record("new", "新.txt", 2))?;
        assert_eq!(stats(&db)?.files, 4);
        let leftovers: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('terms_compacting','index_compaction_state','index_compaction_guard_insert','index_compaction_guard_update','index_compaction_guard_delete')",
            [], |row| row.get(0),
        )?;
        assert_eq!(leftovers, 0);
        Ok(())
    }

    #[test]
    fn full_database_during_compaction_preserves_old_index() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("legacy.db");
        let mut db = legacy_index(&path)?;
        let alphabet: Vec<char> = "本地搜索文件内容模型数据文档索引排序后台测试"
            .chars()
            .collect();
        let mut seed = 0x5eed_u64;
        for number in 0..20 {
            let mut body = String::from("合成磁盘空间标记");
            for _ in 0..1500 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                body.push(alphabet[((seed >> 32) as usize) % alphabet.len()]);
            }
            let name = format!("synthetic-{number}.txt");
            upsert_metadata(&mut db, &record(&name, &name, number))?;
            set_body(&mut db, &name, number, 1, Some(&body), "indexed")?;
        }
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        let pages: i64 = db.pragma_query_value(None, "page_count", |row| row.get(0))?;
        db.pragma_update(None, "max_page_count", pages + 2)?;
        let mut full = false;
        for _ in 0..30 {
            match compact_legacy_step(&mut db, 1) {
                Err(error) => {
                    full = error.chain().any(|cause| {
                        cause.downcast_ref::<rusqlite::Error>().is_some_and(|error| {
                            matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::DiskFull)
                        })
                    });
                    break;
                }
                Ok(CompactStep::Copying { .. }) => {}
                Ok(_) => break,
            }
        }
        assert!(
            full,
            "synthetic page limit did not trigger SQLite disk-full handling"
        );
        assert_eq!(
            search(&db, "合成磁盘空间标记", Scope::Content, &[], &[], || false)?.len(),
            20
        );
        db.pragma_update(None, "max_page_count", pages + 1000)?;
        assert!(abort_legacy_compaction(&mut db)?);
        upsert_metadata(&mut db, &record("after", "after.txt", 99))?;
        assert_eq!(
            search(&db, "after", Scope::Name, &[], &[], || false)?.len(),
            1
        );
        Ok(())
    }

    #[test]
    fn reclaim_checkpoint_defers_for_a_live_reader() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("legacy.db");
        let mut db = legacy_index(&path)?;
        upsert_metadata(&mut db, &record("one", "一.txt", 1))?;
        set_body(&mut db, "one", 1, 1, Some("合成读取"), "indexed")?;
        let reader = open_reader(&path)?;
        reader.execute_batch("BEGIN")?;
        let count: i64 = reader.query_row("SELECT count(*) FROM files", [], |row| row.get(0))?;
        assert_eq!(count, 1);
        assert!(matches!(
            compact_legacy_step(&mut db, 8)?,
            CompactStep::Finished { copied: 1 }
        ));
        assert!(reclaim_vacuum(&db, || false)?);
        let start = std::time::Instant::now();
        assert!(!reclaim_checkpoint(&db)?);
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(reclaim_phase(&db)?, ReclaimPhase::Checkpoint);
        reader.execute_batch("COMMIT")?;
        drop(reader);
        assert!(reclaim_checkpoint(&db)?);
        assert_eq!(reclaim_phase(&db)?, ReclaimPhase::None);
        assert_eq!(
            search(&db, "合成读取", Scope::Content, &[], &[], || false)?.len(),
            1
        );
        Ok(())
    }

    #[test]
    fn compacted_synthetic_index_reclaims_disk_space() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("legacy.db");
        let mut db = legacy_index(&path)?;
        let alphabet: Vec<char> =
            "本地搜索文件内容性能系统模型数据文档索引排序后台测试电脑时间目录状态更新样例"
                .chars()
                .collect();
        let mut seed = 0x5eed_u64;
        for number in 0..80 {
            let mut body = String::from("合成空间标记");
            for _ in 0..900 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                body.push(alphabet[((seed >> 32) as usize) % alphabet.len()]);
            }
            let name = format!("synthetic-{number}.txt");
            upsert_metadata(&mut db, &record(&name, &name, number))?;
            set_body(&mut db, &name, number, 1, Some(&body), "indexed")?;
        }
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        let before = std::fs::metadata(&path)?.len();
        while !matches!(
            compact_legacy_step(&mut db, 16)?,
            CompactStep::Finished { .. }
        ) {}
        let callbacks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let interrupt = callbacks.clone();
        assert!(!reclaim_vacuum(&db, move || interrupt
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            >= 1)?);
        assert!(callbacks.load(std::sync::atomic::Ordering::SeqCst) >= 2);
        assert_eq!(reclaim_phase(&db)?, ReclaimPhase::Vacuum);
        assert_eq!(
            search(&db, "合成空间标记", Scope::Content, &[], &[], || {
                false
            })?
            .len(),
            80
        );
        assert!(reclaim_vacuum(&db, || false)?);
        assert!(reclaim_checkpoint(&db)?);
        let after = std::fs::metadata(&path)?.len();
        assert!(
            after < before,
            "synthetic index did not shrink: {before} -> {after}"
        );
        assert_eq!(
            search(&db, "合成空间标记", Scope::Content, &[], &[], || {
                false
            })?
            .len(),
            80
        );
        Ok(())
    }

    #[test]
    fn exact_matches_union_fields_order_and_reject_false_candidates() -> Result<()> {
        let temp = tempdir()?;
        let mut db = open(&temp.path().join("index.db"))?;
        upsert_metadata(&mut db, &record("p1", "项目报告.txt", 10))?;
        upsert_metadata(&mut db, &record("p2", "x.txt", 20))?;
        upsert_metadata(&mut db, &record("p3", "x2.txt", 30))?;
        set_body(&mut db, "p1", 10, 1, Some("这里有 GPU 加速"), "indexed")?;
        set_body(&mut db, "p2", 20, 1, Some("项目报告 GPU"), "indexed")?;
        set_body(
            &mut db,
            "p3",
            30,
            1,
            Some("项目和报告不是完整短语 GPU"),
            "indexed",
        )?;
        let hits = search(&db, "项目报告 GPU", Scope::Both, &[], &[], || false)?;
        assert_eq!(
            hits.iter()
                .map(|x| x.file.path.as_str())
                .collect::<Vec<_>>(),
            ["p2", "p1"]
        );
        assert_eq!(
            search(&db, "项", Scope::Content, &[], &[], || false)?.len(),
            2
        );
        assert_eq!(
            search(&db, "报告", Scope::Name, &[], &[], || false)?.len(),
            1
        );
        assert_eq!(
            search(&db, "GPU", Scope::Name, &[], &[], || false)?.len(),
            0
        );
        assert_eq!(
            hits[0]
                .snippet
                .as_ref()
                .map(|x| &x.text[x.hit_start..x.hit_end]),
            Some("项目报告")
        );
        Ok(())
    }

    #[test]
    fn changed_files_do_not_keep_old_body_and_old_parser_cannot_overwrite() -> Result<()> {
        let temp = tempdir()?;
        let mut db = open(&temp.path().join("index.db"))?;
        upsert_metadata(&mut db, &record("p", "a.txt", 10))?;
        set_body(&mut db, "p", 10, 1, Some("旧内容"), "indexed")?;
        upsert_metadata(&mut db, &record("p", "a.txt", 20))?;
        assert!(!set_body(&mut db, "p", 10, 1, Some("旧内容"), "indexed")?);
        assert!(search(&db, "旧内容", Scope::Both, &[], &[], || false)?.is_empty());
        remove_path(&mut db, "p")?;
        assert_eq!(stats(&db)?.files, 0);
        Ok(())
    }

    #[test]
    fn refuses_foreign_database_without_modification() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("foreign.db");
        let db = Connection::open(&path)?;
        db.execute("CREATE TABLE user_data(value TEXT)", [])?;
        drop(db);
        assert!(open(&path).is_err());
        let db = Connection::open(&path)?;
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='user_data'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='files'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    }

    #[test]
    fn refuses_unknown_schema_before_changing_it() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("newer.db");
        let db = Connection::open(&path)?;
        db.pragma_update(None, "application_id", APPLICATION_ID)?;
        db.pragma_update(None, "user_version", 99)?;
        db.execute("CREATE TABLE preserved(value TEXT)", [])?;
        drop(db);
        assert!(open(&path).is_err());
        let db = Connection::open(&path)?;
        let unexpected: i64 = db.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('files','terms','retries','scan_issues')", [], |row| row.get(0))?;
        assert_eq!(unexpected, 0);
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(version, 99);
        drop(db);
        assert!(open_reader(&path).is_err());
        Ok(())
    }

    #[test]
    fn batch_commits_only_matching_file_versions() -> Result<()> {
        let temp = tempdir()?;
        let mut db = open(&temp.path().join("index.db"))?;
        upsert_metadata(&mut db, &record("a", "a.txt", 1))?;
        upsert_metadata(&mut db, &record("b", "b.txt", 2))?;
        upsert_metadata(&mut db, &record("b", "b.txt", 3))?;
        let updates = vec![
            BodyUpdate {
                path: "a".into(),
                modified_ms: 1,
                size: 1,
                body: Some("新内容".into()),
                state: "indexed".into(),
            },
            BodyUpdate {
                path: "b".into(),
                modified_ms: 2,
                size: 1,
                body: Some("陈旧内容".into()),
                state: "indexed".into(),
            },
        ];
        assert_eq!(set_bodies(&mut db, &updates)?, 1);
        assert_eq!(
            search(&db, "新内容", Scope::Content, &[], &[], || false)?.len(),
            1
        );
        assert!(search(&db, "陈旧内容", Scope::Content, &[], &[], || false)?.is_empty());
        Ok(())
    }
}
