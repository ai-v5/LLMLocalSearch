use crate::query::{self, Scope};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs::OpenOptions;
use std::path::Path;

const APPLICATION_ID: i64 = 0x4c4c5358;

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
        CREATE TABLE IF NOT EXISTS scan_issues(path TEXT PRIMARY KEY, category TEXT NOT NULL);
        CREATE VIRTUAL TABLE IF NOT EXISTS terms USING fts5(name_grams, body_grams, tokenize='ascii');",
    )?;
    Ok(connection)
}

pub fn open_reader(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    let id: i64 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if id != APPLICATION_ID {
        bail!("目标位置存在其他数据库，未修改");
    }
    Ok(connection)
}

/// Metadata changes invalidate stale content in the same transaction as the FTS update.
pub fn upsert_metadata(connection: &mut Connection, record: &FileRecord) -> Result<bool> {
    upsert_metadata_inner(connection, record, false)
}

/// A watcher event can signal a write even when size and mtime were preserved.
pub fn invalidate_metadata(connection: &mut Connection, record: &FileRecord) -> Result<bool> {
    upsert_metadata_inner(connection, record, true)
}

fn upsert_metadata_inner(
    connection: &mut Connection,
    record: &FileRecord,
    force: bool,
) -> Result<bool> {
    let tx = connection.transaction()?;
    let previous: Option<(i64, i64, i64, String, String)> = tx
        .query_row(
            "SELECT id, modified_ms, size, name, state FROM files WHERE path=?1",
            [&record.path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    if let Some((_, modified, size, ref name, ref state)) = previous
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
        return Ok(false);
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
    Ok(true)
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
