use crate::index::{self, FileRecord};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::{DirEntry, WalkDir};

#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub discovered: u64,
    pub changed: u64,
    pub inaccessible: u64,
    pub root_unavailable: bool,
}

pub fn kind(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "txt" => "txt",
        "md" | "markdown" => "markdown",
        "html" | "htm" => "html",
        "doc" | "docx" => "word",
        "ppt" | "pptx" => "powerpoint",
        "xls" | "xlsx" => "excel",
        "pdf" => "pdf",
        _ => "other",
    }
}

#[cfg(windows)]
fn attributes(metadata: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes()
}
#[cfg(not(windows))]
fn attributes(_metadata: &fs::Metadata) -> u32 {
    0
}

fn is_cloud_placeholder(metadata: &fs::Metadata) -> bool {
    // FILE_ATTRIBUTE_OFFLINE / RECALL_ON_OPEN / RECALL_ON_DATA_ACCESS.
    attributes(metadata) & (0x1000 | 0x40000 | 0x400000) != 0
}

fn content_state(file_kind: &str, is_link: bool, attributes: u32, cloud: bool) -> &'static str {
    if cloud {
        "cloud_placeholder"
    } else if is_link || attributes & 0x400 != 0 || file_kind == "other" {
        "name_only"
    } else {
        "pending"
    }
}

fn enter(entry: &DirEntry, exclusions: &[PathBuf]) -> bool {
    if exclusions
        .iter()
        .any(|p| crate::paths::within(entry.path(), p))
    {
        return false;
    }
    if entry.depth() == 0 {
        return true;
    }
    if entry.file_type().is_dir()
        && entry
            .metadata()
            .ok()
            .is_some_and(|m| attributes(&m) & 0x400 != 0)
    {
        return false;
    }
    true
}

pub fn metadata_record(path: &Path) -> Result<FileRecord> {
    let metadata = fs::symlink_metadata(path).with_context(|| "无法读取文件属性")?;
    let path_str = path.to_str().context("文件路径不是有效 Unicode")?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("文件名不是有效 Unicode")?;
    let modified_ms = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let file_kind = kind(path);
    let state = content_state(
        file_kind,
        metadata.file_type().is_symlink(),
        attributes(&metadata),
        is_cloud_placeholder(&metadata),
    );
    Ok(FileRecord {
        path: path_str.to_owned(),
        name: name.to_owned(),
        kind: file_kind.into(),
        modified_ms,
        size: metadata.len(),
        state: state.into(),
    })
}

/// Filesystem notifications can contain stale casing after a case-only rename.
#[cfg(windows)]
pub fn event_path(path: &Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
    if handle == INVALID_HANDLE_VALUE {
        return path.to_path_buf();
    }
    unsafe {
        FindClose(handle);
    }
    let len = data
        .cFileName
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(data.cFileName.len());
    let name = std::ffi::OsString::from_wide(&data.cFileName[..len]);
    path.parent()
        .map(|parent| parent.join(name))
        .unwrap_or_else(|| path.to_path_buf())
}
#[cfg(not(windows))]
pub fn event_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Scan only the supplied root. Missing/offline roots never delete existing rows.
pub fn scan_root(
    connection: &mut Connection,
    root: &Path,
    exclusions: &[PathBuf],
    cancelled: impl Fn() -> bool,
) -> Result<ScanStats> {
    scan_root_with_progress(connection, root, exclusions, cancelled, |_| Ok(()))
}

/// Allow bounded background work between individual metadata commits.
pub fn scan_root_with_progress(
    connection: &mut Connection,
    root: &Path,
    exclusions: &[PathBuf],
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(&mut Connection) -> Result<()>,
) -> Result<ScanStats> {
    let mut stats = ScanStats::default();
    if !root.is_dir() {
        stats.root_unavailable = true;
        return Ok(stats);
    }
    let mut seen = HashSet::new();
    let mut issues = HashSet::new();
    let mut completed = true;
    for item in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| enter(entry, exclusions))
    {
        if cancelled() {
            completed = false;
            break;
        }
        match item {
            Ok(entry) if entry.file_type().is_file() || entry.file_type().is_symlink() => {
                match metadata_record(entry.path()) {
                    Ok(record) => {
                        seen.insert(record.path.clone());
                        stats.discovered += 1;
                        if index::upsert_metadata(connection, &record)? {
                            stats.changed += 1;
                        }
                    }
                    Err(_) => {
                        stats.inaccessible += 1;
                        if let Some(path) = entry.path().to_str() {
                            issues.insert(path.to_owned());
                        }
                        if !root.is_dir() {
                            stats.root_unavailable = true;
                            completed = false;
                            break;
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                stats.inaccessible += 1;
                if let Some(path) = error.path().and_then(|p| p.to_str()) {
                    issues.insert(path.to_owned());
                }
                if !root.is_dir() {
                    stats.root_unavailable = true;
                    completed = false;
                    break;
                }
            }
        }
        if stats.discovered > 0 {
            progress(connection)?;
        }
    }
    // A root can disappear after traversal starts. Never interpret that as
    // deletion of every file that the interrupted walk did not visit.
    if !root.is_dir() {
        stats.root_unavailable = true;
    }
    if completed && !stats.root_unavailable {
        let previous_issues = {
            let mut stmt = connection.prepare("SELECT path FROM scan_issues")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let tx = connection.transaction()?;
        for path in previous_issues {
            if crate::paths::within(Path::new(&path), root) {
                tx.execute("DELETE FROM scan_issues WHERE path=?1", [&path])?;
            }
        }
        for path in &issues {
            tx.execute(
                "INSERT OR REPLACE INTO scan_issues(path,category) VALUES(?1,'inaccessible')",
                [path],
            )?;
        }
        tx.commit()?;
        let paths = {
            let mut stmt = connection.prepare("SELECT path FROM files")?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for path in paths {
            if cancelled() {
                break;
            }
            let p = Path::new(&path);
            if !crate::paths::within(p, root)
                || seen.contains(&path)
                || exclusions.iter().any(|e| crate::paths::within(p, e))
                || issues
                    .iter()
                    .any(|issue| crate::paths::within(p, Path::new(issue)))
            {
                continue;
            }
            if !root.is_dir() {
                stats.root_unavailable = true;
                break;
            }
            // Only a confirmed missing file on an online root is removed. Access denied is retained.
            if fs::symlink_metadata(p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
                if root.is_dir() {
                    index::remove_path(connection, &path)?;
                } else {
                    stats.root_unavailable = true;
                    break;
                }
            }
        }
    }
    if !root.is_dir() {
        stats.root_unavailable = true;
    }
    Ok(stats)
}

#[cfg(windows)]
pub fn fixed_drives() -> Vec<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    let bits = unsafe { GetLogicalDrives() };
    (0..26)
        .filter_map(|i| {
            if bits & (1 << i) == 0 {
                return None;
            }
            let drive = format!("{}:\\", (b'A' + i as u8) as char);
            let wide: Vec<u16> = drive.encode_utf16().chain(std::iter::once(0)).collect();
            // DRIVE_FIXED = 3.
            (unsafe { GetDriveTypeW(wide.as_ptr()) } == 3).then(|| PathBuf::from(drive))
        })
        .collect()
}
#[cfg(not(windows))]
pub fn fixed_drives() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn links_and_cloud_placeholders_are_not_queued_for_content() {
        assert_eq!(content_state("txt", true, 0, false), "name_only");
        assert_eq!(content_state("word", false, 0x400, false), "name_only");
        assert_eq!(
            content_state("txt", false, 0x400, true),
            "cloud_placeholder"
        );
        assert_eq!(content_state("txt", false, 0, false), "pending");
    }

    #[test]
    fn scan_indexes_every_name_and_handles_delete_and_offline_root() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path().join("files");
        let ignored = root.join("index");
        fs::create_dir_all(&ignored)?;
        fs::write(root.join("项目.txt"), "合成内容")?;
        fs::write(root.join("binary.bin"), [0u8, 1, 2])?;
        fs::write(ignored.join("hidden.db"), "index")?;
        let mut db = index::open(&temp.path().join("db.sqlite"))?;
        let result = scan_root(&mut db, &root, std::slice::from_ref(&ignored), || false)?;
        assert_eq!(result.discovered, 2);
        assert_eq!(index::stats(&db)?.files, 2);
        fs::remove_file(root.join("binary.bin"))?;
        scan_root(&mut db, &root, std::slice::from_ref(&ignored), || false)?;
        assert_eq!(index::stats(&db)?.files, 1);
        fs::rename(&root, temp.path().join("offline"))?;
        let offline_scan = scan_root(&mut db, &root, &[], || false)?;
        assert!(offline_scan.root_unavailable);
        assert_eq!(index::stats(&db)?.files, 1);
        Ok(())
    }

    #[test]
    fn drive_disappearing_during_scan_preserves_unseen_files() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path().join("files");
        let offline = temp.path().join("offline");
        fs::create_dir(&root)?;
        for number in 0..20 {
            fs::write(root.join(format!("synthetic-{number}.txt")), "合成正文")?;
        }
        let mut db = index::open(&temp.path().join("index.db"))?;
        scan_root(&mut db, &root, &[], || false)?;
        assert_eq!(index::stats(&db)?.files, 20);
        let mut moved = false;
        let interrupted = scan_root_with_progress(
            &mut db,
            &root,
            &[],
            || false,
            |_| {
                if !moved {
                    fs::rename(&root, &offline)?;
                    moved = true;
                }
                Ok(())
            },
        )?;
        assert!(moved);
        assert!(interrupted.root_unavailable);
        assert_eq!(index::stats(&db)?.files, 20, "offline files were removed");
        fs::rename(&offline, &root)?;
        let recovered = scan_root(&mut db, &root, &[], || false)?;
        assert!(!recovered.root_unavailable);
        assert_eq!(index::stats(&db)?.files, 20);
        Ok(())
    }
}
