use anyhow::{Context, Result, bail};
use rusqlite::{Connection, backup::Backup};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub index_dir: PathBuf,
    #[serde(default)]
    pub exclusions: Vec<PathBuf>,
    #[serde(default)]
    pub extra_roots: Vec<PathBuf>,
    #[serde(default)]
    pub close_hint_seen: bool,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub old_index_dirs: Vec<PathBuf>,
}

impl Settings {
    pub fn default_for(app_dir: &Path) -> Self {
        Self {
            index_dir: app_dir.join("Index"),
            exclusions: Vec::new(),
            extra_roots: Vec::new(),
            close_hint_seen: false,
            autostart: false,
            old_index_dirs: Vec::new(),
        }
    }
}

pub fn load(path: &Path, app_dir: &Path) -> Result<Settings> {
    if !path.exists() {
        return Ok(Settings::default_for(app_dir));
    }
    serde_json::from_slice(&fs::read(path)?).context("无法读取设置")
}

pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    let parent = path.parent().context("无效的设置目录")?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .context("设置临时文件已存在")?;
    let result = (|| -> Result<()> {
        file.write_all(&serde_json::to_vec_pretty(settings)?)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(windows)]
fn move_new(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileW;
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(windows)]
fn available_space(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0u64;
    if unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(available)
}
#[cfg(not(windows))]
fn available_space(_path: &Path) -> Result<u64> {
    Ok(u64::MAX)
}

#[cfg(not(windows))]
fn move_new(from: &Path, to: &Path) -> Result<()> {
    fs::hard_link(from, to)?;
    fs::remove_file(from)?;
    Ok(())
}

/// Call only after stopping the worker and active searches. Source remains intact on failure.
pub fn migrate(source: &Path, target_dir: &Path) -> Result<PathBuf> {
    if !source.is_file() {
        bail!("原索引不存在");
    }
    if !target_dir.is_absolute() {
        bail!("目标目录必须使用绝对路径");
    }
    if !target_dir.is_dir() {
        bail!("目标目录不存在");
    }
    let target_dir = target_dir.canonicalize()?;
    if !crate::paths::local_volume(&target_dir) {
        bail!("索引目录必须位于本地磁盘");
    }
    let wal = PathBuf::from(format!("{}-wal", source.display()));
    let required = fs::metadata(source)?
        .len()
        .saturating_add(fs::metadata(wal).map(|m| m.len()).unwrap_or(0))
        .saturating_mul(2);
    if available_space(&target_dir)? < required {
        bail!("目标目录可用空间不足");
    }
    let target = target_dir.join("index.db");
    if target.exists() {
        bail!("目标索引已存在，未覆盖");
    }
    let temp = target_dir.join("index.migrating.db");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .context("目标临时索引已存在或目录不可写")?;
    let result = (|| -> Result<()> {
        let source_db = crate::index::open(source)?;
        let mut target_db = Connection::open(&temp)?;
        {
            let backup = Backup::new(&source_db, &mut target_db)?;
            backup.run_to_completion(128, Duration::from_millis(10), None)?;
        }
        let check: String = target_db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if check != "ok" {
            bail!("目标索引校验失败");
        }
        let _: i64 = target_db.query_row("SELECT count(*) FROM files", [], |r| r.get(0))?;
        drop(target_db);
        drop(source_db);
        move_new(&temp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    Ok(target)
}

/// Delete only a verified LLMLocalSearch index and its own SQLite sidecars.
pub fn remove_old_index(directory: &Path, current: &Path) -> Result<()> {
    let old_dir = directory.canonicalize()?;
    let current_dir = current.canonicalize()?;
    if old_dir == current_dir {
        bail!("不能删除当前索引");
    }
    remove_index(&old_dir)
}

/// Remove only a known application-owned database, never its parent directory.
pub fn remove_index(directory: &Path) -> Result<()> {
    let path = directory.join("index.db");
    if !path.exists() {
        return Ok(());
    }
    let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let id: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if id != 0x4c4c5358 {
        bail!("旧目录中的数据库不属于 LLMLocalSearch");
    }
    drop(db);
    fs::remove_file(&path)?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
        match fs::remove_file(sidecar) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index;
    use tempfile::tempdir;

    #[test]
    fn migrate_preserves_source_and_refuses_conflicts() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("source.db");
        let target = temp.path().join("target");
        fs::create_dir(&target)?;
        let db = index::open(&src)?;
        drop(db);
        let output = migrate(&src, &target)?;
        assert!(src.exists() && output.exists());
        assert!(migrate(&src, &target).is_err());
        assert!(src.exists() && output.exists());
        Ok(())
    }

    #[test]
    fn settings_can_be_replaced_without_losing_index_path() -> Result<()> {
        let temp = tempdir()?;
        let file = temp.path().join("settings.json");
        let mut config = Settings::default_for(temp.path());
        save(&file, &config)?;
        config.close_hint_seen = true;
        save(&file, &config)?;
        let loaded = load(&file, temp.path())?;
        assert!(loaded.close_hint_seen);
        assert_eq!(loaded.index_dir, config.index_dir);
        Ok(())
    }

    #[test]
    fn cleanup_only_removes_verified_old_index() -> Result<()> {
        let temp = tempdir()?;
        let old = temp.path().join("old");
        let current = temp.path().join("current");
        fs::create_dir_all(&old)?;
        fs::create_dir_all(&current)?;
        drop(index::open(&old.join("index.db"))?);
        drop(index::open(&current.join("index.db"))?);
        fs::write(old.join("unrelated.txt"), "keep")?;
        assert!(remove_old_index(&current, &current).is_err());
        remove_old_index(&old, &current)?;
        assert!(old.join("unrelated.txt").exists());
        assert!(!old.join("index.db").exists());
        assert!(current.join("index.db").exists());
        Ok(())
    }

    #[test]
    fn migrated_directory_is_canonicalized_for_scan_exclusion() -> Result<()> {
        let temp = tempdir()?;
        let src = temp.path().join("source.db");
        let target = temp.path().join("target");
        let alias = temp.path().join("other").join("..").join("target");
        fs::create_dir(&target)?;
        fs::create_dir(temp.path().join("other"))?;
        drop(index::open(&src)?);
        let output = migrate(&src, &alias)?;
        assert_eq!(
            output.parent().expect("index parent"),
            target.canonicalize()?
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn occupied_index_is_preserved_until_released() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("index.db");
        let db = index::open(&path)?;
        assert!(remove_index(temp.path()).is_err());
        assert!(path.exists());
        drop(db);
        remove_index(temp.path())?;
        assert!(!path.exists());
        Ok(())
    }
}
