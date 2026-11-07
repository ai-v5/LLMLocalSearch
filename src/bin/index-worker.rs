#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{Context, Result, bail};
use llmlocalsearch::{index, paths, scanner, settings, text, tika};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Control {
    stop: AtomicBool,
    ready: AtomicBool,
    paused: AtomicBool,
    resource_paused: AtomicBool,
    searching: AtomicBool,
    rescan: AtomicBool,
}

fn send_status(state: &str, stats: Option<index::IndexStats>) {
    let mut output = std::io::stdout().lock();
    let value = match stats {
        Some(s) => {
            serde_json::json!({"state":state,"files":s.files,"pending":s.pending,"indexed":s.indexed,"failed":s.failed,"name_only":s.name_only,"no_text":s.no_text,"cloud":s.cloud,"scan_errors":s.scan_errors})
        }
        None => serde_json::json!({"state":state}),
    };
    let _ = writeln!(output, "{value}");
}

fn send_compaction_progress(stats: index::IndexStats, copied: u64, total: u64) {
    let mut output = std::io::stdout().lock();
    let value = serde_json::json!({
        "state":"optimizing", "files":stats.files, "pending":stats.pending,
        "indexed":stats.indexed, "failed":stats.failed, "name_only":stats.name_only,
        "no_text":stats.no_text, "cloud":stats.cloud, "scan_errors":stats.scan_errors,
        "copied":copied, "total":total
    });
    let _ = writeln!(output, "{value}");
}

fn start_control() -> Arc<Control> {
    let control = Arc::new(Control::default());
    let shared = control.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            match line.as_deref() {
                Ok("start") => shared.ready.store(true, Ordering::Relaxed),
                Ok("pause") => shared.paused.store(true, Ordering::Relaxed),
                Ok("resume") => {
                    shared.paused.store(false, Ordering::Relaxed);
                    shared.rescan.store(true, Ordering::Relaxed);
                }
                Ok("search_begin") => shared.searching.store(true, Ordering::Relaxed),
                Ok("search_end") => shared.searching.store(false, Ordering::Relaxed),
                Ok("reconcile") => shared.rescan.store(true, Ordering::Relaxed),
                Ok("stop") | Err(_) => {
                    shared.stop.store(true, Ordering::Relaxed);
                    return;
                }
                _ => {}
            }
        }
        shared.stop.store(true, Ordering::Relaxed);
    });
    control
}

enum UpgradeOutcome {
    Current,
    Upgraded,
    LegacyFallback,
    Stopped,
}

fn upgrade_index(
    connection: &mut Connection,
    database: &Path,
    control: &Arc<Control>,
) -> Result<UpgradeOutcome> {
    let mut last_report = Instant::now() - Duration::from_secs(2);
    let mut space_check = Instant::now() - Duration::from_secs(5);
    loop {
        if control.stop.load(Ordering::Relaxed) {
            return Ok(UpgradeOutcome::Stopped);
        }
        if control.paused.load(Ordering::Relaxed) || control.resource_paused.load(Ordering::Relaxed)
        {
            if last_report.elapsed() >= Duration::from_secs(1) {
                send_status(
                    if control.paused.load(Ordering::Relaxed) {
                        "paused"
                    } else {
                        "resource_paused"
                    },
                    index::stats(connection).ok(),
                );
                last_report = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if control.searching.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let step = (|| -> Result<index::CompactStep> {
            if space_check.elapsed() >= Duration::from_secs(5) {
                let directory = database.parent().context("索引目录无效")?;
                if settings::available_space(directory)? < 256 * 1024 * 1024 {
                    bail!("索引目录可用空间不足");
                }
                space_check = Instant::now();
            }
            index::compact_legacy_step(connection, 16)
        })();
        match step {
            Ok(index::CompactStep::Current) => return Ok(UpgradeOutcome::Current),
            Ok(index::CompactStep::Finished { .. }) => return Ok(UpgradeOutcome::Upgraded),
            Ok(index::CompactStep::Copying { copied, total }) => {
                if last_report.elapsed() >= Duration::from_secs(1) {
                    if let Ok(stats) = index::stats(connection) {
                        send_compaction_progress(stats, copied, total);
                    }
                    last_report = Instant::now();
                }
            }
            Err(_) => {
                // A failed conversion must not leave the old index silently
                // write-locked. If cleanup itself fails, keep the old table
                // queryable and wait for an explicit resume after repair.
                loop {
                    match index::abort_legacy_compaction(connection) {
                        Ok(true) => return Ok(UpgradeOutcome::LegacyFallback),
                        Ok(false) => return Ok(UpgradeOutcome::Current),
                        Err(_) => send_status("optimization_blocked", None),
                    }
                    loop {
                        if control.stop.load(Ordering::Relaxed) {
                            return Ok(UpgradeOutcome::Stopped);
                        }
                        if control.rescan.swap(false, Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        }
    }
}

fn reclaim_space_available(database: &Path) -> Result<bool> {
    let parent = database.parent().context("索引目录无效")?;
    let mut wal_name = database.as_os_str().to_os_string();
    wal_name.push("-wal");
    let wal = PathBuf::from(wal_name);
    let size = std::fs::metadata(database)?
        .len()
        .saturating_add(std::fs::metadata(wal).map_or(0, |metadata| metadata.len()));
    let required = size.saturating_mul(2).saturating_add(64 * 1024 * 1024);
    Ok(settings::available_space(parent)? >= required)
}

fn reclaim_step(connection: &Connection, database: &Path, control: &Arc<Control>) -> Result<bool> {
    match index::reclaim_phase(connection)? {
        index::ReclaimPhase::None => Ok(true),
        index::ReclaimPhase::Vacuum => {
            if !reclaim_space_available(database)? {
                send_status("reclaim_wait_space", None);
                return Ok(false);
            }
            send_status("reclaiming", index::stats(connection).ok());
            let control = control.clone();
            let finished = index::reclaim_vacuum(connection, move || {
                control.stop.load(Ordering::Relaxed)
                    || control.paused.load(Ordering::Relaxed)
                    || control.resource_paused.load(Ordering::Relaxed)
                    || control.searching.load(Ordering::Relaxed)
            })?;
            if finished && index::reclaim_checkpoint(connection)? {
                send_status("reclaimed", None);
                Ok(true)
            } else {
                send_status("reclaim_deferred", None);
                Ok(false)
            }
        }
        index::ReclaimPhase::Checkpoint => {
            let finished = index::reclaim_checkpoint(connection)?;
            if finished {
                send_status("reclaimed", None);
            } else {
                send_status("reclaim_deferred", None);
            }
            Ok(finished)
        }
    }
}

#[cfg(windows)]
fn start_resource_monitor(control: Arc<Control>) {
    std::thread::spawn(move || {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        use windows_sys::Win32::System::Threading::GetSystemTimes;
        fn ticks(v: FILETIME) -> u64 {
            ((v.dwHighDateTime as u64) << 32) | v.dwLowDateTime as u64
        }
        let mut previous: Option<(u64, u64)> = None;
        let mut high = 0u8;
        let mut low = 0u8;
        let mut cpu_paused = false;
        while !control.stop.load(Ordering::Relaxed) {
            let mut idle: FILETIME = unsafe { std::mem::zeroed() };
            let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
            let mut user: FILETIME = unsafe { std::mem::zeroed() };
            if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } != 0 {
                let now = (ticks(idle), ticks(kernel).saturating_add(ticks(user)));
                if let Some(old) = previous {
                    let total = now.1.saturating_sub(old.1);
                    let idle = now.0.saturating_sub(old.0);
                    let busy = total
                        .saturating_sub(idle)
                        .saturating_mul(100)
                        .checked_div(total)
                        .unwrap_or(0);
                    if busy >= 90 {
                        high = high.saturating_add(1);
                        low = 0;
                    } else if busy < 75 {
                        low = low.saturating_add(1);
                        high = 0;
                    } else {
                        high = 0;
                        low = 0;
                    }
                    if high >= 5 {
                        cpu_paused = true;
                    }
                    if low >= 5 {
                        cpu_paused = false;
                    }
                }
                previous = Some(now);
            }
            let mut memory: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
            memory.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            let memory_paused = unsafe { GlobalMemoryStatusEx(&mut memory) } != 0
                && memory.ullAvailPhys < 1024 * 1024 * 1024
                && memory.ullAvailPhys.saturating_mul(10) < memory.ullTotalPhys;
            control
                .resource_paused
                .store(cpu_paused || memory_paused, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}
#[cfg(not(windows))]
fn start_resource_monitor(_control: Arc<Control>) {}

fn file_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<index::FileRecord> {
    Ok(index::FileRecord {
        path: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        modified_ms: row.get(3)?,
        size: row.get::<_, i64>(4)? as u64,
        state: row.get(5)?,
    })
}

fn pending(connection: &Connection) -> Result<Vec<index::FileRecord>> {
    let ready = {
        let mut statement = connection.prepare("SELECT path,name,kind,modified_ms,size,state FROM files WHERE state='pending' ORDER BY modified_ms DESC,id LIMIT 10")?;
        statement
            .query_map([], file_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    if !ready.is_empty() {
        return Ok(ready);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let mut statement = connection.prepare("SELECT files.path,files.name,files.kind,files.modified_ms,files.size,files.state FROM retries JOIN files ON files.id=retries.file_id WHERE retries.next_ms<=?1 ORDER BY retries.next_ms LIMIT 10")?;
    let rows = statement
        .query_map([now], file_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn transient(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::TimedOut
            ) || matches!(error.raw_os_error(), Some(32 | 33))
        })
}

fn extract_one(
    connection: &mut Connection,
    runtime: &Path,
    record: index::FileRecord,
) -> Result<Option<index::BodyUpdate>> {
    let path = Path::new(&record.path);
    let outcome = if record.kind == "txt" || record.kind == "markdown" {
        text::read_plain(path)
    } else {
        tika::extract(runtime, path)
    };
    let fresh = match scanner::metadata_record(path) {
        Ok(value) => value,
        Err(_) => {
            index::schedule_retry(connection, &record.path, record.modified_ms, record.size)?;
            return Ok(None);
        }
    };
    if fresh.modified_ms != record.modified_ms
        || fresh.size != record.size
        || fresh.state != "pending"
    {
        index::invalidate_metadata(connection, &fresh)?;
        return Ok(None);
    }
    if let Err(ref error) = outcome
        && transient(error)
    {
        index::schedule_retry(connection, &record.path, record.modified_ms, record.size)?;
        return Ok(None);
    }
    let (body, state) = match outcome {
        Ok(ref s) if s.trim().is_empty() => (None, "no_text"),
        Ok(s) => (Some(s), "indexed"),
        Err(_) if record.size > text::MAX_SOURCE_BYTES => (None, "too_large"),
        Err(_)
            if !runtime.join("tika-app.jar").is_file()
                && record.kind != "txt"
                && record.kind != "markdown" =>
        {
            (None, "parser_unavailable")
        }
        Err(ref e) if e.to_string().contains("大小限制") => (None, "too_large"),
        Err(ref e) if e.to_string().contains("文档已加密") => (None, "encrypted"),
        Err(_) => (None, "failed"),
    };
    Ok(Some(index::BodyUpdate {
        path: record.path,
        modified_ms: record.modified_ms,
        size: record.size,
        body,
        state: state.to_owned(),
    }))
}

fn flush_batch(connection: &mut Connection, batch: &mut Vec<index::BodyUpdate>) -> Result<()> {
    let mut valid = Vec::new();
    for update in batch.drain(..) {
        match scanner::metadata_record(Path::new(&update.path)) {
            Ok(fresh)
                if fresh.modified_ms == update.modified_ms
                    && fresh.size == update.size
                    && fresh.state == "pending" =>
            {
                valid.push(update)
            }
            Ok(fresh) => {
                index::invalidate_metadata(connection, &fresh)?;
            }
            Err(_) => {
                index::schedule_retry(connection, &update.path, update.modified_ms, update.size)?;
            }
        }
    }
    if !valid.is_empty() {
        index::set_bodies(connection, &valid)?;
    }
    Ok(())
}

fn remove_missing(connection: &mut Connection, missing: &Path) -> Result<()> {
    let paths = {
        let mut stmt = connection.prepare("SELECT path FROM files")?;
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for path in paths {
        if paths::within(Path::new(&path), missing) {
            index::remove_path(connection, &path)?;
        }
    }
    let issues = {
        let mut stmt = connection.prepare("SELECT path FROM scan_issues")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for path in issues {
        if paths::within(Path::new(&path), missing) {
            connection.execute("DELETE FROM scan_issues WHERE path=?1", [&path])?;
        }
    }
    Ok(())
}

fn handle_rename_from(connection: &mut Connection, roots: &[PathBuf], path: &Path) -> Result<bool> {
    // Rename events may have waited in the queue while a root went offline.
    // Preserve its old records and reconcile after the root is available.
    if !roots
        .iter()
        .any(|root| root.is_dir() && paths::within(path, root))
    {
        return Ok(true);
    }
    // A queued rename can be older than a later reconciliation. If the same
    // spelling exists again, keep it and rescan. A case-only rename needs the
    // old key removed and a scan of the actual spelling.
    let different_spelling = match std::fs::symlink_metadata(path) {
        Ok(_) if scanner::event_path(path) == path => return Ok(true),
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Ok(true),
    };
    if !roots
        .iter()
        .any(|root| root.is_dir() && paths::within(path, root))
    {
        return Ok(true);
    }
    let known_file = path.to_str().is_some_and(|value| {
        connection
            .query_row("SELECT 1 FROM files WHERE path=?1", [value], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    });
    if known_file {
        if let Some(value) = path.to_str() {
            index::remove_path(connection, value)?;
        }
        Ok(different_spelling)
    } else {
        remove_missing(connection, path)?;
        Ok(true)
    }
}

fn mark_offline_roots(roots: &[PathBuf], available: &mut [bool], path: &Path) -> bool {
    let mut offline = false;
    for (root, was_available) in roots.iter().zip(available) {
        if paths::within(path, root) && !root.is_dir() {
            *was_available = false;
            offline = true;
        }
    }
    offline
}

fn root_returned(root: &Path, was_available: &mut bool) -> bool {
    let present = root.is_dir();
    let returned = present && !*was_available;
    *was_available = present;
    returned
}

fn main() -> Result<()> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, PROCESS_MODE_BACKGROUND_BEGIN, SetPriorityClass,
        };
        let _ = SetPriorityClass(GetCurrentProcess(), PROCESS_MODE_BACKGROUND_BEGIN);
    }
    let mut args = std::env::args_os().skip(1);
    let db = PathBuf::from(args.next().context("missing database")?);
    let runtime = PathBuf::from(args.next().context("missing runtime")?);
    let dynamic_fixed = args.next().context("missing drive discovery mode")? == "1";
    let root_count: usize = args
        .next()
        .context("missing root count")?
        .to_string_lossy()
        .parse()?;
    let mut roots = (0..root_count)
        .map(|_| args.next().map(PathBuf::from).context("missing root"))
        .collect::<Result<Vec<_>>>()?;
    let exclusions = args.map(PathBuf::from).collect::<Vec<_>>();
    let control = start_control();
    start_resource_monitor(control.clone());
    while !control.ready.load(Ordering::Relaxed) {
        if control.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let mut connection = index::open(&db)?;
    // The legacy table remains queryable during this restartable conversion.
    // Reconciliation follows the conversion so no worker writes can race it.
    let upgrade = upgrade_index(&mut connection, &db, &control)?;
    match upgrade {
        UpgradeOutcome::Stopped => return Ok(()),
        UpgradeOutcome::LegacyFallback => send_status("optimization_fallback", None),
        UpgradeOutcome::Upgraded => send_status("optimized", None),
        UpgradeOutcome::Current => {}
    }
    for excluded in &exclusions {
        remove_missing(&mut connection, excluded)?;
    }
    let (sender, events) = mpsc::sync_channel::<notify::Result<Event>>(4096);
    let dropped = control.clone();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
        if sender.try_send(event).is_err() {
            dropped.rescan.store(true, Ordering::Relaxed);
        }
    })?;
    for root in &roots {
        if root.is_dir() && watcher.watch(root, RecursiveMode::Recursive).is_err() {
            send_status("watch_failed", None);
        }
    }
    let mut available = roots.iter().map(|root| root.is_dir()).collect::<Vec<_>>();
    let mut availability_check = Instant::now();
    control.rescan.store(true, Ordering::Relaxed);
    let mut last_check = Instant::now();
    let mut pending_events: HashMap<PathBuf, Instant> = HashMap::new();
    let mut last_report = Instant::now() - Duration::from_secs(2);
    let mut reclaim_pending = index::reclaim_phase(&connection)? != index::ReclaimPhase::None;
    let mut reclaim_retry = Instant::now() + Duration::from_secs(10);
    loop {
        if control.stop.load(Ordering::Relaxed) {
            break;
        }
        if availability_check.elapsed() >= Duration::from_secs(5) {
            if dynamic_fixed {
                for drive in scanner::fixed_drives() {
                    if !roots
                        .iter()
                        .any(|root| paths::within(&drive, root) && paths::within(root, &drive))
                    {
                        if watcher.watch(&drive, RecursiveMode::Recursive).is_err() {
                            send_status("watch_failed", None);
                        }
                        roots.push(drive);
                        available.push(true);
                        control.rescan.store(true, Ordering::Relaxed);
                    }
                }
            }
            for (i, root) in roots.iter().enumerate() {
                if root_returned(root, &mut available[i]) {
                    if watcher.watch(root, RecursiveMode::Recursive).is_err() {
                        send_status("watch_failed", None);
                    }
                    control.rescan.store(true, Ordering::Relaxed);
                }
            }
            availability_check = Instant::now();
        }
        if control.paused.load(Ordering::Relaxed) || control.resource_paused.load(Ordering::Relaxed)
        {
            if last_report.elapsed() >= Duration::from_secs(1) {
                send_status(
                    if control.paused.load(Ordering::Relaxed) {
                        "paused"
                    } else {
                        "resource_paused"
                    },
                    index::stats(&connection).ok(),
                );
                last_report = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if reclaim_pending
            && !control.searching.load(Ordering::Relaxed)
            && Instant::now() >= reclaim_retry
        {
            match reclaim_step(&connection, &db, &control) {
                Ok(true) => reclaim_pending = false,
                Ok(false) => reclaim_retry = Instant::now() + Duration::from_secs(30),
                Err(_) => {
                    send_status("reclaim_failed", None);
                    reclaim_retry = Instant::now() + Duration::from_secs(300);
                }
            }
        }
        if last_check.elapsed() >= Duration::from_secs(24 * 60 * 60) {
            control.rescan.store(true, Ordering::Relaxed);
        }
        if !control.searching.load(Ordering::Relaxed)
            && control.rescan.swap(false, Ordering::Relaxed)
        {
            send_status("scanning", index::stats(&connection).ok());
            let mut last_scan_content = Instant::now() - Duration::from_secs(5);
            let mut last_scan_report = Instant::now() - Duration::from_secs(5);
            for (i, root) in roots.iter().enumerate() {
                if control.stop.load(Ordering::Relaxed) {
                    break;
                }
                let scan = scanner::scan_root_with_progress(
                    &mut connection,
                    root,
                    &exclusions,
                    || {
                        control.stop.load(Ordering::Relaxed)
                            || control.paused.load(Ordering::Relaxed)
                            || control.resource_paused.load(Ordering::Relaxed)
                    },
                    |connection| {
                        if control.stop.load(Ordering::Relaxed)
                            || control.paused.load(Ordering::Relaxed)
                            || control.resource_paused.load(Ordering::Relaxed)
                            || control.searching.load(Ordering::Relaxed)
                        {
                            return Ok(());
                        }
                        if last_scan_content.elapsed() >= Duration::from_secs(5) {
                            if let Some(record) = pending(connection)?.into_iter().next()
                                && let Some(update) = extract_one(connection, &runtime, record)?
                            {
                                flush_batch(connection, &mut vec![update])?;
                            }
                            last_scan_content = Instant::now();
                        }
                        if last_scan_report.elapsed() >= Duration::from_secs(5) {
                            send_status("scanning", index::stats(connection).ok());
                            last_scan_report = Instant::now();
                        }
                        Ok(())
                    },
                )?;
                if scan.root_unavailable {
                    available[i] = false;
                }
                if control.paused.load(Ordering::Relaxed)
                    || control.resource_paused.load(Ordering::Relaxed)
                {
                    control.rescan.store(true, Ordering::Relaxed);
                    break;
                }
            }
            last_check = Instant::now();
        }
        while let Ok(event) = events.try_recv() {
            match event {
                Ok(event) => {
                    if event.need_rescan() {
                        control.rescan.store(true, Ordering::Relaxed);
                    }
                    let renamed_from = matches!(
                        event.kind,
                        notify::EventKind::Modify(notify::event::ModifyKind::Name(
                            notify::event::RenameMode::From
                        ))
                    );
                    for path in event.paths {
                        if exclusions.iter().any(|e| paths::within(&path, e)) {
                            continue;
                        }
                        if renamed_from {
                            pending_events.retain(|queued, _| !paths::within(queued, &path));
                            let rescan = handle_rename_from(&mut connection, &roots, &path)?;
                            let offline = mark_offline_roots(&roots, &mut available, &path);
                            if rescan && !offline {
                                control.rescan.store(true, Ordering::Relaxed);
                            }
                        } else {
                            pending_events.insert(path, Instant::now());
                        }
                    }
                }
                Err(_) => control.rescan.store(true, Ordering::Relaxed),
            }
            if pending_events.len() > 4096 {
                pending_events.clear();
                control.rescan.store(true, Ordering::Relaxed);
            }
        }
        let ready = pending_events
            .iter()
            .filter_map(|(path, time)| {
                (time.elapsed() >= Duration::from_secs(2)).then_some(path.clone())
            })
            .collect::<Vec<_>>();
        for path in ready {
            pending_events.remove(&path);
            let path = scanner::event_path(&path);
            if mark_offline_roots(&roots, &mut available, &path) {
                continue;
            }
            if path.is_dir() {
                control.rescan.store(true, Ordering::Relaxed);
                continue;
            }
            match scanner::metadata_record(&path) {
                Ok(record) => {
                    index::invalidate_metadata(&mut connection, &record)?;
                }
                Err(_)
                    if roots.iter().any(|r| r.is_dir() && paths::within(&path, r))
                        && std::fs::symlink_metadata(&path)
                            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    remove_missing(&mut connection, &path)?
                }
                Err(_) => {
                    mark_offline_roots(&roots, &mut available, &path);
                }
            }
        }
        if !control.searching.load(Ordering::Relaxed) {
            let records = pending(&connection)?;
            if records.is_empty() {
                std::thread::sleep(Duration::from_millis(100));
            } else {
                let mut batch = Vec::new();
                let mut bytes = 0usize;
                for record in records {
                    if control.stop.load(Ordering::Relaxed)
                        || control.paused.load(Ordering::Relaxed)
                        || control.resource_paused.load(Ordering::Relaxed)
                        || control.searching.load(Ordering::Relaxed)
                    {
                        break;
                    }
                    if let Some(update) = extract_one(&mut connection, &runtime, record)? {
                        bytes += update.body.as_ref().map_or(0, String::len);
                        batch.push(update);
                        if bytes >= 4 * 1024 * 1024 {
                            flush_batch(&mut connection, &mut batch)?;
                            bytes = 0;
                        }
                    }
                }
                flush_batch(&mut connection, &mut batch)?;
            }
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            send_status("running", index::stats(&connection).ok());
            last_report = Instant::now();
        }
    }
    send_status("stopped", None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_rename_event_does_not_remove_an_offline_root_file() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("files");
        let offline = temporary.path().join("offline");
        std::fs::create_dir(&root)?;
        let file = root.join("synthetic.txt");
        std::fs::write(&file, "合成正文")?;
        let mut db = index::open(&temporary.path().join("index.db"))?;
        index::upsert_metadata(&mut db, &scanner::metadata_record(&file)?)?;
        std::fs::rename(&root, &offline)?;
        assert!(handle_rename_from(
            &mut db,
            std::slice::from_ref(&root),
            &file
        )?);
        assert_eq!(index::stats(&db)?.files, 1);
        Ok(())
    }

    #[test]
    fn delayed_directory_rename_preserves_offline_descendants() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("files");
        let directory = root.join("nested");
        std::fs::create_dir_all(&directory)?;
        let mut db = index::open(&temporary.path().join("index.db"))?;
        for number in 0..2 {
            let file = directory.join(format!("synthetic-{number}.txt"));
            std::fs::write(&file, "合成正文")?;
            index::upsert_metadata(&mut db, &scanner::metadata_record(&file)?)?;
        }
        std::fs::rename(&root, temporary.path().join("offline"))?;
        assert!(handle_rename_from(
            &mut db,
            std::slice::from_ref(&root),
            &directory
        )?);
        assert_eq!(index::stats(&db)?.files, 2);
        Ok(())
    }

    #[test]
    fn short_offline_event_is_remembered_until_the_root_returns() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("files");
        let offline = temporary.path().join("offline");
        std::fs::create_dir(&root)?;
        let path = root.join("synthetic.txt");
        std::fs::write(&path, "合成正文")?;
        let roots = vec![root.clone()];
        let mut available = vec![true];
        std::fs::rename(&root, &offline)?;
        assert!(mark_offline_roots(&roots, &mut available, &path));
        assert!(!available[0]);
        std::fs::rename(&offline, &root)?;
        assert!(root_returned(&root, &mut available[0]));
        assert!(!root_returned(&root, &mut available[0]));
        Ok(())
    }

    #[test]
    fn rename_event_on_an_online_root_removes_the_old_name() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("files");
        std::fs::create_dir(&root)?;
        let old = root.join("old.txt");
        std::fs::write(&old, "合成正文")?;
        let mut db = index::open(&temporary.path().join("index.db"))?;
        index::upsert_metadata(&mut db, &scanner::metadata_record(&old)?)?;
        std::fs::rename(&old, root.join("new.txt"))?;
        assert!(!handle_rename_from(
            &mut db,
            std::slice::from_ref(&root),
            &old
        )?);
        assert_eq!(index::stats(&db)?.files, 0);
        Ok(())
    }

    #[test]
    fn stale_rename_event_keeps_a_reused_file_path() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("files");
        std::fs::create_dir(&root)?;
        let old = root.join("old.txt");
        std::fs::write(&old, "旧合成正文")?;
        let mut db = index::open(&temporary.path().join("index.db"))?;
        scanner::scan_root(&mut db, &root, &[], || false)?;
        std::fs::rename(&old, root.join("new.txt"))?;
        std::fs::write(&old, "重新创建的合成正文")?;
        scanner::scan_root(&mut db, &root, &[], || false)?;
        assert_eq!(index::stats(&db)?.files, 2);
        handle_rename_from(&mut db, std::slice::from_ref(&root), &old)?;
        assert_eq!(index::stats(&db)?.files, 2);
        Ok(())
    }

    #[test]
    fn stale_directory_rename_keeps_a_reused_subtree() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("files");
        let old = root.join("old");
        std::fs::create_dir_all(&old)?;
        std::fs::write(old.join("first.txt"), "合成正文")?;
        let mut db = index::open(&temporary.path().join("index.db"))?;
        scanner::scan_root(&mut db, &root, &[], || false)?;
        std::fs::rename(&old, root.join("new"))?;
        std::fs::create_dir(&old)?;
        std::fs::write(old.join("second.txt"), "新合成正文")?;
        scanner::scan_root(&mut db, &root, &[], || false)?;
        assert_eq!(index::stats(&db)?.files, 2);
        handle_rename_from(&mut db, std::slice::from_ref(&root), &old)?;
        assert_eq!(index::stats(&db)?.files, 2);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn case_only_rename_replaces_the_old_spelling() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("files");
        std::fs::create_dir(&root)?;
        let old = root.join("lower.txt");
        std::fs::write(&old, "合成正文")?;
        let mut db = index::open(&temporary.path().join("index.db"))?;
        scanner::scan_root(&mut db, &root, &[], || false)?;
        std::fs::rename(&old, root.join("LOWER.txt"))?;
        assert!(handle_rename_from(
            &mut db,
            std::slice::from_ref(&root),
            &old
        )?);
        scanner::scan_root(&mut db, &root, &[], || false)?;
        let hits = index::search(
            &db,
            "lower",
            llmlocalsearch::query::Scope::Name,
            &[],
            &[],
            || false,
        )?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file.name, "LOWER.txt");
        Ok(())
    }
}
