use eframe::egui;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const SCAN_BUDGET: Duration = Duration::from_millis(250);
const ENTRY_LIMIT: usize = 5_000;
const INDEX_FILES: [&str; 4] = [
    "index.db",
    "index.db-wal",
    "index.db-shm",
    "index.db-journal",
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Usage {
    index_bytes: u64,
    other_bytes: u64,
    has_database: bool,
    complete: bool,
}

struct Reply {
    path: PathBuf,
    usage: io::Result<Usage>,
}

pub struct StorageMonitor {
    path: Option<PathBuf>,
    last_request: Option<Instant>,
    in_flight: bool,
    usage: Option<io::Result<Usage>>,
    tx: mpsc::Sender<Reply>,
    rx: mpsc::Receiver<Reply>,
}

impl StorageMonitor {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            path: None,
            last_request: None,
            in_flight: false,
            usage: None,
            tx,
            rx,
        }
    }

    pub fn poll(&mut self, path: &Path, ctx: &egui::Context) {
        if self.path.as_deref() != Some(path) {
            self.path = Some(path.to_owned());
            self.last_request = None;
            self.usage = None;
        }
        while let Ok(reply) = self.rx.try_recv() {
            self.in_flight = false;
            if self.path.as_ref() == Some(&reply.path) {
                self.usage = Some(reply.usage);
            } else {
                self.last_request = None;
            }
        }
        if !self.in_flight
            && self
                .last_request
                .is_none_or(|time| time.elapsed() >= REFRESH_INTERVAL)
        {
            self.in_flight = true;
            self.last_request = Some(Instant::now());
            let path = path.to_owned();
            let sender = self.tx.clone();
            let wake = ctx.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("index-storage".into())
                .spawn(move || {
                    enter_background_mode();
                    let usage = measure(&path, ENTRY_LIMIT, SCAN_BUDGET);
                    let _ = sender.send(Reply { path, usage });
                    wake.request_repaint();
                })
            {
                self.in_flight = false;
                self.usage = Some(Err(error));
            }
        }
        let wait = self.last_request.map_or(REFRESH_INTERVAL, |time| {
            REFRESH_INTERVAL.saturating_sub(time.elapsed())
        });
        ctx.request_repaint_after(wait.max(Duration::from_secs(1)));
    }

    pub fn label(&self) -> String {
        match &self.usage {
            None => "索引空间：统计中…".to_owned(),
            Some(Err(_)) => "索引空间：暂时无法统计".to_owned(),
            Some(Ok(usage)) => {
                let index = if usage.has_database {
                    format!("约 {}", format_size(usage.index_bytes))
                } else if usage.index_bytes > 0 {
                    format!("未创建（残留文件约 {}）", format_size(usage.index_bytes))
                } else {
                    "未创建".to_owned()
                };
                let note = if usage.complete {
                    ""
                } else {
                    "（统计不完整）"
                };
                format!(
                    "索引文件：{}；目录其他文件（含备份）：约 {}{}",
                    index,
                    format_size(usage.other_bytes),
                    note
                )
            }
        }
    }
}

fn measure(directory: &Path, limit: usize, budget: Duration) -> io::Result<Usage> {
    let root = fs::canonicalize(directory)?;
    if !root.is_dir() {
        return Err(io::Error::other("index directory is unavailable"));
    }
    let mut usage = Usage {
        complete: true,
        ..Usage::default()
    };
    for name in INDEX_FILES {
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if is_regular_file(&metadata) => {
                if name == "index.db" {
                    usage.has_database = true;
                }
                let (bytes, accurate) = disk_size(&path, &metadata);
                usage.index_bytes = usage.index_bytes.saturating_add(bytes);
                usage.complete &= accurate;
            }
            Ok(_) => usage.complete = false,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => usage.complete = false,
        }
    }

    let start = Instant::now();
    let mut visited = 0usize;
    let mut directories = vec![root.clone()];
    'walk: while let Some(dir) = directories.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                usage.complete = false;
                continue;
            }
        };
        for entry in entries {
            if visited >= limit || start.elapsed() >= budget {
                usage.complete = false;
                break 'walk;
            }
            visited += 1;
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    usage.complete = false;
                    continue;
                }
            };
            if dir == root && is_index_name(&entry.file_name()) {
                continue;
            }
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    usage.complete = false;
                    continue;
                }
            };
            if is_skipped(&metadata) {
                usage.complete = false;
            } else if metadata.is_dir() {
                directories.push(path);
            } else if metadata.is_file() {
                let (bytes, accurate) = disk_size(&path, &metadata);
                usage.other_bytes = usage.other_bytes.saturating_add(bytes);
                usage.complete &= accurate;
            } else {
                usage.complete = false;
            }
        }
    }
    Ok(usage)
}

fn is_regular_file(metadata: &Metadata) -> bool {
    metadata.is_file() && !is_skipped(metadata)
}

fn is_index_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        INDEX_FILES.iter().any(|index| {
            if cfg!(windows) {
                name.eq_ignore_ascii_case(index)
            } else {
                name == *index
            }
        })
    })
}

fn is_skipped(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Do not follow reparse points or request access to offline placeholders.
        const SKIP: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;
        metadata.file_attributes() & SKIP != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
fn disk_size(path: &Path, metadata: &Metadata) -> (u64, bool) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut high = 0u32;
    unsafe {
        SetLastError(0);
        let low = GetCompressedFileSizeW(path.as_ptr(), &mut high);
        if low == u32::MAX && GetLastError() != 0 {
            (metadata.len(), false)
        } else {
            (((high as u64) << 32) | low as u64, true)
        }
    }
}

#[cfg(not(windows))]
fn disk_size(_path: &Path, metadata: &Metadata) -> (u64, bool) {
    (metadata.len(), true)
}

#[cfg(windows)]
fn enter_background_mode() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    };
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
}

#[cfg(not(windows))]
fn enter_background_mode() {}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_index_sidecars_and_nested_other_files_separately() -> io::Result<()> {
        let temporary = tempfile::tempdir()?;
        fs::write(temporary.path().join("index.db"), vec![1; 4096])?;
        fs::write(temporary.path().join("index.db-wal"), vec![2; 2048])?;
        fs::write(temporary.path().join("index.db-shm"), vec![3; 128])?;
        let backup = temporary.path().join("backup");
        fs::create_dir(&backup)?;
        fs::write(backup.join("index.db"), vec![4; 1024])?;
        let usage = measure(temporary.path(), 100, Duration::from_secs(10))?;
        assert_eq!(usage.index_bytes, 4096 + 2048 + 128);
        assert_eq!(usage.other_bytes, 1024);
        assert!(usage.has_database && usage.complete);
        Ok(())
    }

    #[test]
    fn limits_work_without_reporting_a_complete_directory() -> io::Result<()> {
        let temporary = tempfile::tempdir()?;
        fs::write(temporary.path().join("index.db"), vec![1; 4096])?;
        for number in 0..10 {
            fs::write(temporary.path().join(format!("other-{number}")), [1])?;
        }
        let usage = measure(temporary.path(), 2, Duration::from_secs(10))?;
        assert_eq!(usage.index_bytes, 4096);
        assert!(usage.has_database);
        assert!(!usage.complete);
        Ok(())
    }

    #[test]
    fn monitor_refreshes_on_schedule_and_discards_old_directory_results() -> io::Result<()> {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        fs::write(first.path().join("index.db"), vec![1; 2048])?;
        fs::write(second.path().join("index.db"), vec![1; 4096])?;
        let ctx = egui::Context::default();
        let mut monitor = StorageMonitor::new();
        monitor.poll(first.path(), &ctx);
        monitor.poll(second.path(), &ctx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            monitor.poll(second.path(), &ctx);
            if let Some(Ok(usage)) = &monitor.usage
                && usage.index_bytes == 4096
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            monitor
                .usage
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .index_bytes,
            4096
        );
        let label = monitor.label();
        assert!(label.contains("索引文件：约 4.00 KiB"));
        assert!(label.contains("目录其他文件（含备份）：约 0 B"));
        let requested = monitor.last_request;
        fs::write(second.path().join("index.db"), vec![1; 8192])?;
        monitor.poll(second.path(), &ctx);
        assert_eq!(monitor.last_request, requested);
        assert_eq!(
            monitor
                .usage
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .index_bytes,
            4096
        );
        monitor.last_request = Some(Instant::now() - REFRESH_INTERVAL);
        monitor.poll(second.path(), &ctx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            monitor.poll(second.path(), &ctx);
            if let Some(Ok(usage)) = &monitor.usage
                && usage.index_bytes == 8192
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            monitor
                .usage
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .index_bytes,
            8192
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_index_names_are_case_insensitive() {
        assert!(is_index_name(std::ffi::OsStr::new("INDEX.DB-WAL")));
    }

    #[test]
    fn formats_binary_units() {
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1536), "1.50 KiB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GiB");
    }
}
