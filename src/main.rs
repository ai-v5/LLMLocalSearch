#![cfg_attr(windows, windows_subsystem = "windows")]

mod worker_client;

use eframe::egui::{self, Color32, RichText};
use llmlocalsearch::{
    index::{self, SearchHit},
    query::Scope,
    scanner,
    settings::{self, Settings},
};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
    mpsc,
};
use tray_icon::{
    TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem},
};
use worker_client::Worker;

const TYPES: [(&str, &str); 8] = [
    ("txt", "文本"),
    ("markdown", "Markdown"),
    ("html", "HTML"),
    ("word", "Word"),
    ("powerpoint", "PPT"),
    ("excel", "Excel"),
    ("pdf", "PDF"),
    ("other", "其他文件"),
];

enum SearchEvent {
    Batch(Vec<SearchHit>),
    Done(Result<usize, String>),
}
struct SearchReply {
    generation: u64,
    event: SearchEvent,
}

struct MigrationOutcome {
    index_dir: PathBuf,
    previous_dir: Option<PathBuf>,
}

struct DeskApp {
    app_dir: PathBuf,
    settings_path: PathBuf,
    settings: Settings,
    roots: Vec<PathBuf>,
    dynamic_fixed: bool,
    worker: Option<Worker>,
    _tray: Option<TrayIcon>,
    tray_events: mpsc::Receiver<String>,
    open_id: MenuId,
    pause_id: MenuId,
    resume_id: MenuId,
    exit_id: MenuId,
    query: String,
    scope: Scope,
    kinds: [bool; 8],
    result: Vec<SearchHit>,
    page: usize,
    generation: Arc<AtomicU64>,
    active_searches: Arc<AtomicUsize>,
    search_tx: mpsc::Sender<SearchReply>,
    search_rx: mpsc::Receiver<SearchReply>,
    searching: bool,
    status: String,
    index_status: String,
    paused: bool,
    show_settings: bool,
    first_close: bool,
    quit: bool,
    migrate_path: String,
    migrate_wait: Option<PathBuf>,
    migration_inflight: bool,
    restart_requested: bool,
    cleanup_confirm: Option<PathBuf>,
    cleanup_inflight: bool,
    cleanup_tx: mpsc::Sender<(PathBuf, Result<(), String>)>,
    cleanup_rx: mpsc::Receiver<(PathBuf, Result<(), String>)>,
    migration_tx: mpsc::Sender<Result<MigrationOutcome, String>>,
    migration_rx: mpsc::Receiver<Result<MigrationOutcome, String>>,
    show_issues: bool,
    issues: Vec<(String, String)>,
    issue_tx: mpsc::Sender<Result<Vec<(String, String)>, String>>,
    issue_rx: mpsc::Receiver<Result<Vec<(String, String)>, String>>,
}

impl DeskApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        app_dir: PathBuf,
        settings_path: PathBuf,
        settings: Settings,
        roots: Vec<PathBuf>,
        dynamic_fixed: bool,
    ) -> Self {
        setup_fonts(&cc.egui_ctx);
        let (event_tx, event_rx) = mpsc::channel();
        let open = MenuItem::new("打开 LLMLocalSearch", true, None);
        let pause = MenuItem::new("暂停索引", true, None);
        let resume = MenuItem::new("恢复索引", true, None);
        let exit = MenuItem::new("退出", true, None);
        let menu = Menu::new();
        let _ = menu.append_items(&[&open, &pause, &resume, &exit]);
        let sender = event_tx.clone();
        let wake = cc.egui_ctx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = sender.send(e.id.0);
            wake.request_repaint();
        }));
        let sender = event_tx;
        let wake = cc.egui_ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    ..
                }
            ) {
                let _ = sender.send("__open__".into());
                wake.request_repaint();
            }
        }));
        let tray = image::load_from_memory(include_bytes!("../assets/icon.png"))
            .ok()
            .and_then(|image| {
                let image = image.to_rgba8();
                tray_icon::Icon::from_rgba(image.into_raw(), 256, 256).ok()
            })
            .and_then(|icon| {
                TrayIconBuilder::new()
                    .with_tooltip("LLMLocalSearch")
                    .with_icon(icon)
                    .with_menu(Box::new(menu))
                    .build()
                    .ok()
            });
        let (search_tx, search_rx) = mpsc::channel();
        let (migration_tx, migration_rx) = mpsc::channel();
        let (cleanup_tx, cleanup_rx) = mpsc::channel();
        let (issue_tx, issue_rx) = mpsc::channel();
        let mut app = Self {
            app_dir,
            settings_path,
            migrate_path: settings.index_dir.to_string_lossy().into_owned(),
            settings,
            roots,
            dynamic_fixed,
            worker: None,
            _tray: tray,
            tray_events: event_rx,
            open_id: open.id().clone(),
            pause_id: pause.id().clone(),
            resume_id: resume.id().clone(),
            exit_id: exit.id().clone(),
            query: String::new(),
            scope: Scope::Both,
            kinds: [true; 8],
            result: Vec::new(),
            page: 0,
            generation: Arc::new(AtomicU64::new(0)),
            active_searches: Arc::new(AtomicUsize::new(0)),
            search_tx,
            search_rx,
            searching: false,
            status: "输入关键词后开始搜索".into(),
            index_status: "正在启动索引…".into(),
            paused: false,
            show_settings: false,
            first_close: false,
            quit: false,
            migrate_wait: None,
            migration_inflight: false,
            restart_requested: false,
            cleanup_confirm: None,
            cleanup_inflight: false,
            cleanup_tx,
            cleanup_rx,
            migration_tx,
            migration_rx,
            show_issues: false,
            issues: Vec::new(),
            issue_tx,
            issue_rx,
        };
        app.start_worker(&cc.egui_ctx);
        app
    }

    fn database(&self) -> PathBuf {
        self.settings.index_dir.join("index.db")
    }

    fn effective_exclusions(&self) -> Vec<PathBuf> {
        let mut result = self.settings.exclusions.clone();
        result.push(
            self.settings
                .index_dir
                .canonicalize()
                .unwrap_or_else(|_| self.settings.index_dir.clone()),
        );
        result.extend(self.settings.old_index_dirs.iter().cloned());
        result.push(self.app_dir.join("runtime"));
        result
    }

    fn start_worker(&mut self, ctx: &egui::Context) {
        if let Err(e) = std::fs::create_dir_all(&self.settings.index_dir) {
            self.index_status = format!("索引目录不可写：{e}");
            self.show_settings = true;
            return;
        }
        let exclusions = self.effective_exclusions();
        let mut roots = self.roots.clone();
        for path in &self.settings.extra_roots {
            if !roots
                .iter()
                .any(|root| llmlocalsearch::paths::within(path, root))
            {
                roots.push(path.clone());
            }
        }
        match Worker::start(
            &self.app_dir,
            &self.database(),
            &roots,
            &exclusions,
            self.dynamic_fixed,
            self.paused,
            ctx.clone(),
        ) {
            Ok(worker) => {
                self.worker = Some(worker);
                self.index_status = if self.paused {
                    "后台索引已暂停"
                } else {
                    "后台索引已启动"
                }
                .into();
            }
            Err(e) => self.index_status = format!("后台索引未启动：{e}"),
        }
    }

    fn start_search(&mut self, ctx: &egui::Context) {
        if self.migrate_wait.is_some() || self.migration_inflight {
            return;
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.searching = false;
        self.result.clear();
        self.page = 0;
        if let Some(worker) = &mut self.worker {
            let _ = worker.send("search_end");
        }
        let value = self.query.clone();
        if value.trim().is_empty() {
            self.result.clear();
            self.status = "输入关键词后开始搜索".into();
            return;
        }
        let kinds = TYPES
            .iter()
            .zip(self.kinds)
            .filter(|(_, enabled)| *enabled)
            .map(|((key, _), _)| (*key).to_owned())
            .collect::<Vec<_>>();
        if kinds.is_empty() {
            self.result.clear();
            self.status = "请至少选择一种文件类型".into();
            return;
        }
        self.searching = true;
        self.status = "正在搜索…".into();
        if let Some(worker) = &mut self.worker {
            let _ = worker.send("search_begin");
        }
        let db = self.database();
        let sender = self.search_tx.clone();
        let generation_shared = self.generation.clone();
        let active = self.active_searches.clone();
        let scope = self.scope;
        let exclusions = self.effective_exclusions();
        let wake = ctx.clone();
        active.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let stream_sender = sender.clone();
            let stream_wake = wake.clone();
            let result = index::open_reader(&db)
                .and_then(|db| {
                    let interrupt = generation_shared.clone();
                    db.progress_handler(
                        2000,
                        Some(move || interrupt.load(Ordering::SeqCst) != generation),
                    )?;
                    index::search_stream(
                        &db,
                        &value,
                        scope,
                        &kinds,
                        &exclusions,
                        || generation_shared.load(Ordering::SeqCst) != generation,
                        |batch| {
                            let _ = stream_sender.send(SearchReply {
                                generation,
                                event: SearchEvent::Batch(batch),
                            });
                            stream_wake.request_repaint();
                        },
                    )
                })
                .map_err(|_| "搜索失败，请检查索引目录".to_owned());
            active.fetch_sub(1, Ordering::SeqCst);
            let _ = sender.send(SearchReply {
                generation,
                event: SearchEvent::Done(result),
            });
            wake.request_repaint();
        });
    }

    fn begin_migration(&mut self, path: PathBuf) {
        if self.migration_inflight || self.migrate_wait.is_some() {
            return;
        }
        if self.cleanup_inflight {
            self.status = "请等待旧索引清理完成后再迁移".into();
            return;
        }
        if !path.is_absolute() {
            self.status = "目标目录必须使用绝对路径".into();
            return;
        }
        if path == self.settings.index_dir {
            self.status = "目标目录与当前目录相同".into();
            return;
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.searching = false;
        self.cleanup_confirm = None;
        self.migrate_wait = Some(path);
        self.status = "正在停止索引以迁移…".into();
        if let Some(worker) = &mut self.worker {
            let _ = worker.send("stop");
        }
    }

    fn restart_worker(&mut self) {
        self.restart_requested = true;
        if let Some(worker) = &mut self.worker {
            let _ = worker.send("stop");
        }
    }

    fn load_issues(&mut self, ctx: &egui::Context) {
        self.show_issues = true;
        self.issues.clear();
        let db = self.database();
        let tx = self.issue_tx.clone();
        let wake = ctx.clone();
        let active = self.active_searches.clone();
        active.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<Vec<(String, String)>> {
                let db = index::open_reader(&db)?;
                let mut stmt = db.prepare("SELECT path,state FROM files WHERE state IN ('retry_wait','failed','encrypted','too_large','inaccessible','parser_unavailable','cloud_placeholder','no_text') UNION ALL SELECT path,category FROM scan_issues LIMIT 500")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })().map_err(|_| "无法读取异常列表".to_owned());
            active.fetch_sub(1, Ordering::SeqCst);
            let _ = tx.send(result);
            wake.request_repaint();
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.tray_events.try_recv() {
            if event == "__open__" || event == self.open_id.0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            } else if event == self.pause_id.0 {
                self.paused = true;
                if let Some(w) = &mut self.worker {
                    let _ = w.send("pause");
                }
            } else if event == self.resume_id.0 {
                self.paused = false;
                if let Some(w) = &mut self.worker {
                    let _ = w.send("resume");
                }
            } else if event == self.exit_id.0 {
                self.quit = true;
                if self.migrate_wait.is_some() || self.migration_inflight || self.cleanup_inflight {
                    self.status = "正在完成索引操作，然后退出…".into();
                } else {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        while let Ok(reply) = self.search_rx.try_recv() {
            if reply.generation != self.generation.load(Ordering::SeqCst) {
                continue;
            }
            match reply.event {
                SearchEvent::Batch(batch) => {
                    self.result.extend(batch);
                    self.status = format!("已找到 {} 个文件，继续搜索…", self.result.len());
                }
                SearchEvent::Done(result) => {
                    self.searching = false;
                    if let Some(worker) = &mut self.worker {
                        let _ = worker.send("search_end");
                    }
                    match result {
                        Ok(total) => {
                            self.status =
                                format!("找到 {total} 个文件；尚未完成索引的正文可能未覆盖")
                        }
                        Err(e) => self.status = e,
                    }
                }
            }
        }
        if let Some(worker) = &mut self.worker {
            while let Ok(message) = worker.messages.try_recv() {
                if self.migrate_wait.is_none() && !self.migration_inflight {
                    if let (Some(files), Some(pending)) = (
                        message.get("files").and_then(|v| v.as_u64()),
                        message.get("pending").and_then(|v| v.as_u64()),
                    ) {
                        let state = message
                            .get("state")
                            .and_then(|v| v.as_str())
                            .unwrap_or("running");
                        let indexed = message.get("indexed").and_then(|v| v.as_u64()).unwrap_or(0);
                        let failed = message.get("failed").and_then(|v| v.as_u64()).unwrap_or(0);
                        let cloud = message.get("cloud").and_then(|v| v.as_u64()).unwrap_or(0);
                        let scan_errors = message
                            .get("scan_errors")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        self.index_status = format!(
                            "{}：文件 {}，正文 {}，待处理 {}，解析异常 {}，云占位 {}，访问失败 {}",
                            if state == "paused" {
                                "已暂停"
                            } else if state == "resource_paused" {
                                "系统繁忙，已暂停"
                            } else if state == "scanning" {
                                "扫描中"
                            } else {
                                "后台索引"
                            },
                            files,
                            indexed,
                            pending,
                            failed,
                            cloud,
                            scan_errors
                        );
                    } else if message.get("state").and_then(|v| v.as_str()) == Some("watch_failed")
                    {
                        self.index_status = "部分磁盘无法监听，将通过补扫更新".into();
                    }
                }
            }
        }
        if !self.restart_requested
            && self.migrate_wait.is_none()
            && !self.migration_inflight
            && self.worker.as_mut().is_some_and(|worker| !worker.running())
        {
            self.worker = None;
            self.index_status = "后台索引已停止，请检查索引目录和资源限制".into();
        }
        if self.restart_requested
            && !self.migration_inflight
            && self.migrate_wait.is_none()
            && self.worker.as_mut().is_none_or(|w| !w.running())
        {
            self.worker = None;
            self.restart_requested = false;
            self.start_worker(ctx);
        }
        if self.migrate_wait.is_some()
            && self.active_searches.load(Ordering::SeqCst) == 0
            && self.worker.as_mut().is_none_or(|w| !w.running())
        {
            self.worker = None;
            let target = self.migrate_wait.take().expect("checked");
            let source = self.database();
            let previous_dir = self.settings.index_dir.clone();
            let tx = self.migration_tx.clone();
            let wake = ctx.clone();
            self.migration_inflight = true;
            self.status = "正在复制并校验索引…".into();
            std::thread::spawn(move || {
                let result = (|| -> anyhow::Result<_> {
                    let target = target.canonicalize()?;
                    if source.exists() {
                        let migrated = settings::migrate(&source, &target)?;
                        Ok(MigrationOutcome {
                            index_dir: migrated.parent().expect("index parent").to_path_buf(),
                            previous_dir: Some(previous_dir),
                        })
                    } else {
                        if !target.is_dir()
                            || target.join("index.db").exists()
                            || !llmlocalsearch::paths::local_volume(&target)
                        {
                            anyhow::bail!("目标目录不存在、不是本地目录或已有索引");
                        }
                        let probe = target.join(".llmlocalsearch-write-test");
                        let f = std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&probe)?;
                        drop(f);
                        std::fs::remove_file(probe)?;
                        Ok(MigrationOutcome {
                            index_dir: target,
                            previous_dir: None,
                        })
                    }
                })()
                .map_err(|e| format!("迁移失败，原索引未删除：{e}"));
                let _ = tx.send(result);
                wake.request_repaint();
            });
        }
        while let Ok(result) = self.migration_rx.try_recv() {
            self.migration_inflight = false;
            match result {
                Ok(outcome) => {
                    let mut updated = self.settings.clone();
                    if let Some(previous_dir) = outcome.previous_dir
                        && !updated.old_index_dirs.contains(&previous_dir)
                    {
                        updated.old_index_dirs.push(previous_dir);
                    }
                    updated.index_dir = outcome.index_dir;
                    match settings::save(&self.settings_path, &updated) {
                        Ok(()) => {
                            self.settings = updated;
                            self.migrate_path =
                                self.settings.index_dir.to_string_lossy().into_owned();
                            self.status = "索引迁移完成；原索引已保留".into();
                        }
                        Err(_) => self.status = "迁移设置保存失败，仍使用原索引".into(),
                    }
                }
                Err(error) => self.status = error,
            }
            self.restart_requested = false;
            if !self.quit {
                self.start_worker(ctx);
            }
        }
        while let Ok((path, result)) = self.cleanup_rx.try_recv() {
            self.cleanup_inflight = false;
            match result {
                Ok(()) => {
                    self.settings.old_index_dirs.retain(|p| p != &path);
                    if settings::save(&self.settings_path, &self.settings).is_ok() {
                        self.status = "旧索引副本已删除".into();
                    } else {
                        self.status = "旧索引已删除，但设置保存失败".into();
                    }
                }
                Err(message) => self.status = message,
            }
        }
        while let Ok(result) = self.issue_rx.try_recv() {
            match result {
                Ok(issues) => self.issues = issues,
                Err(e) => self.status = e,
            }
        }
        if self.quit
            && self.migrate_wait.is_none()
            && !self.migration_inflight
            && !self.cleanup_inflight
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for DeskApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ui.input(|i| i.viewport().close_requested()) {
            let busy =
                self.migrate_wait.is_some() || self.migration_inflight || self.cleanup_inflight;
            if busy && (self.quit || self._tray.is_none()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.quit = true;
                self.status = "正在完成索引操作，然后退出…".into();
            } else if !self.quit && self._tray.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if self.settings.close_hint_seen {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                } else {
                    self.first_close = true;
                }
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading("LLMLocalSearch");
            ui.label(RichText::new("本地离线搜索").weak());
            if ui.button("设置").clicked() {
                self.show_settings = true;
            }
        });
        ui.label(RichText::new(&self.index_status).small().weak());
        if ui.small_button("查看索引异常").clicked() {
            self.load_issues(&ctx);
        }
        ui.separator();
        ui.horizontal(|ui| {
            let input = ui.add_sized(
                [ui.available_width().max(160.0) - 85.0, 32.0],
                egui::TextEdit::singleline(&mut self.query)
                    .id(egui::Id::new("search_input"))
                    .hint_text("输入文件名或内容关键词"),
            );
            if ui.button("搜索").clicked()
                || (input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            {
                self.start_search(&ctx);
            }
        });
        ui.horizontal(|ui| {
            ui.label("搜索范围：");
            ui.selectable_value(&mut self.scope, Scope::Both, "文件名和内容");
            ui.selectable_value(&mut self.scope, Scope::Name, "仅文件名");
            ui.selectable_value(&mut self.scope, Scope::Content, "仅内容");
        });
        ui.horizontal_wrapped(|ui| {
            for (i, (_, label)) in TYPES.iter().enumerate() {
                ui.checkbox(&mut self.kinds[i], *label);
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.status).small().weak());
            if self.searching {
                ui.spinner();
                if ui.small_button("取消搜索").clicked() {
                    self.generation.fetch_add(1, Ordering::SeqCst);
                    self.searching = false;
                    self.status = format!("搜索已取消，当前 {} 条结果不完整", self.result.len());
                    if let Some(worker) = &mut self.worker {
                        let _ = worker.send("search_end");
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.page > 0, egui::Button::new("上一页"))
                .clicked()
            {
                self.page -= 1;
            }
            ui.label(format!("第 {} 页", self.page + 1));
            if ui
                .add_enabled(
                    (self.page + 1) * 50 < self.result.len(),
                    egui::Button::new("下一页"),
                )
                .clicked()
            {
                self.page += 1;
            }
        });
        ui.separator();
        let start = self.page * 50;
        let page_len = self.result.len().saturating_sub(start).min(50);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, 80.0, page_len, |ui, rows| {
                for row in rows {
                    let hit = &self.result[start + row];
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            let title = if hit.name_match {
                                RichText::new(&hit.file.name)
                                    .color(Color32::from_rgb(25, 150, 133))
                                    .strong()
                            } else {
                                RichText::new(&hit.file.name).strong()
                            };
                            if ui.link(title).clicked() {
                                open_file(Path::new(&hit.file.path));
                            }
                            let when =
                                chrono::DateTime::from_timestamp_millis(hit.file.modified_ms)
                                    .map(|t| {
                                        t.with_timezone(&chrono::Local)
                                            .format("%Y-%m-%d %H:%M")
                                            .to_string()
                                    })
                                    .unwrap_or_default();
                            ui.label(RichText::new(when).weak());
                            if ui.small_button("所在目录").clicked() {
                                reveal_file(Path::new(&hit.file.path));
                            }
                            if ui.small_button("复制路径").clicked() {
                                ctx.copy_text(hit.file.path.clone());
                            }
                        });
                        ui.label(RichText::new(&hit.file.path).small().weak());
                        if let Some(snippet) = &hit.snippet {
                            let mut job = egui::text::LayoutJob::default();
                            let format = egui::TextFormat {
                                font_id: egui::FontId::proportional(13.0),
                                color: ui.visuals().text_color(),
                                ..Default::default()
                            };
                            job.append(&snippet.text[..snippet.hit_start], 0.0, format.clone());
                            let mut highlight = format.clone();
                            highlight.background = Color32::from_rgb(255, 218, 120);
                            highlight.color = Color32::BLACK;
                            job.append(
                                &snippet.text[snippet.hit_start..snippet.hit_end],
                                0.0,
                                highlight,
                            );
                            job.append(&snippet.text[snippet.hit_end..], 0.0, format);
                            ui.label(job);
                        } else if hit.file.state != "indexed" {
                            ui.label(
                                RichText::new(format!(
                                    "正文状态：{}",
                                    state_label(&hit.file.state)
                                ))
                                .small()
                                .weak(),
                            );
                        }
                    });
                    ui.separator();
                }
            });
        if self.first_close {
            egui::Window::new("继续后台索引")
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.label(
                        "关闭窗口后程序会保留在系统托盘并继续索引。通过托盘菜单可暂停或退出。",
                    );
                    if ui.button("知道了").clicked() {
                        self.first_close = false;
                        self.settings.close_hint_seen = true;
                        let _ = settings::save(&self.settings_path, &self.settings);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    }
                });
        }
        if self.show_issues {
            let mut open = self.show_issues;
            egui::Window::new("索引异常及未提取正文")
                .open(&mut open)
                .show(&ctx, |ui| {
                    ui.label("最多显示前 500 项；统计数量见主界面。云占位文件不会被自动下载。");
                    egui::ScrollArea::vertical()
                        .max_height(360.0)
                        .show(ui, |ui| {
                            for (path, state) in &self.issues {
                                ui.label(format!("{} — {}", path, state_label(state)));
                            }
                        });
                });
            self.show_issues = open;
        }
        if self.show_settings {
            let mut open = self.show_settings;
            let mut migrate = None;
            let mut add_exclusion = None;
            let mut remove_exclusion = None;
            let mut autostart_change = None;
            let mut cleanup = None;
            let mut add_root = None;
            let mut remove_root = None;
            egui::Window::new("设置").open(&mut open).show(&ctx, |ui| {
                ui.label("索引包含提取的文档文字，请保存到受控目录。迁移后原索引保留。");
                ui.label(format!(
                    "当前索引目录：{}",
                    self.settings.index_dir.display()
                ));
                for path in &self.settings.old_index_dirs {
                    ui.horizontal(|ui| {
                        ui.label(format!("旧索引：{}", path.display()));
                        if ui
                            .add_enabled(
                                !self.cleanup_inflight
                                    && self.migrate_wait.is_none()
                                    && !self.migration_inflight,
                                egui::Button::new("清理旧副本").small(),
                            )
                            .clicked()
                        {
                            cleanup = Some(path.clone());
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.migrate_path);
                    if ui.button("浏览").clicked()
                        && let Some(path) = choose_folder()
                    {
                        self.migrate_path = path.to_string_lossy().into_owned();
                    }
                    if ui.button("迁移索引").clicked() {
                        migrate = Some(PathBuf::from(&self.migrate_path));
                    }
                });
                ui.label("默认扫描本机固定磁盘；未下载的云文件不会主动下载。");
                for (i, path) in self.settings.extra_roots.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("附加扫描位置：{}", path.display()));
                        if ui.small_button("移除").clicked() {
                            remove_root = Some(i);
                        }
                    });
                }
                if ui.button("添加扫描位置").clicked() {
                    add_root = choose_folder();
                }
                ui.separator();
                ui.label("排除目录：");
                for (i, path) in self.settings.exclusions.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(path.display().to_string());
                        if ui.small_button("移除").clicked() {
                            remove_exclusion = Some(i);
                        }
                    });
                }
                if ui.button("添加排除目录").clicked() {
                    add_exclusion = choose_folder();
                }
                let mut autostart = self.settings.autostart;
                if ui.checkbox(&mut autostart, "登录 Windows 时启动").changed() {
                    autostart_change = Some(autostart);
                }
                if ui
                    .button(if self.paused {
                        "恢复索引"
                    } else {
                        "暂停索引"
                    })
                    .clicked()
                {
                    self.paused = !self.paused;
                    if let Some(worker) = &mut self.worker {
                        let _ = worker.send(if self.paused { "pause" } else { "resume" });
                    }
                }
            });
            self.show_settings = open;
            if let Some(path) = migrate {
                self.begin_migration(path);
            }
            if let Some(path) = cleanup
                && self.migrate_wait.is_none()
                && !self.migration_inflight
            {
                self.cleanup_confirm = Some(path);
            }
            if let Some(path) = add_root {
                match path.canonicalize() {
                    Ok(path) => {
                        if self
                            .settings
                            .exclusions
                            .iter()
                            .any(|root| llmlocalsearch::paths::within(&path, root))
                        {
                            self.status = "所选位置已被排除，请先移除排除项".into();
                        } else if self
                            .roots
                            .iter()
                            .chain(self.settings.extra_roots.iter())
                            .any(|root| llmlocalsearch::paths::within(&path, root))
                        {
                            self.status = "所选目录已包含在扫描范围中".into();
                        } else {
                            self.settings.extra_roots.push(path);
                            if settings::save(&self.settings_path, &self.settings).is_ok() {
                                self.restart_worker();
                            } else {
                                self.settings.extra_roots.pop();
                                self.status = "保存扫描位置失败".into();
                            }
                        }
                    }
                    Err(_) => self.status = "无法访问所选扫描位置".into(),
                }
            }
            if let Some(i) = remove_root {
                let old = self.settings.extra_roots.remove(i);
                let added_exclusion = !self
                    .settings
                    .exclusions
                    .iter()
                    .any(|p| llmlocalsearch::paths::within(&old, p));
                if added_exclusion {
                    self.settings.exclusions.push(old.clone());
                }
                if settings::save(&self.settings_path, &self.settings).is_ok() {
                    self.generation.fetch_add(1, Ordering::SeqCst);
                    self.searching = false;
                    self.result.clear();
                    self.restart_worker();
                } else {
                    if added_exclusion {
                        self.settings.exclusions.pop();
                    }
                    self.settings.extra_roots.insert(i, old);
                    self.status = "保存扫描位置失败".into();
                }
            }
            if let Some(path) = add_exclusion {
                match path.canonicalize() {
                    Ok(path) => {
                        if !self
                            .settings
                            .exclusions
                            .iter()
                            .any(|p| llmlocalsearch::paths::within(&path, p))
                        {
                            self.settings.exclusions.push(path);
                            if settings::save(&self.settings_path, &self.settings).is_ok() {
                                self.generation.fetch_add(1, Ordering::SeqCst);
                                self.searching = false;
                                self.result.clear();
                                self.restart_worker();
                            } else {
                                self.settings.exclusions.pop();
                                self.status = "保存排除目录失败".into();
                            }
                        }
                    }
                    Err(_) => self.status = "无法访问所选目录".into(),
                }
            }
            if let Some(i) = remove_exclusion {
                let old = self.settings.exclusions.remove(i);
                if settings::save(&self.settings_path, &self.settings).is_ok() {
                    self.restart_worker();
                } else {
                    self.settings.exclusions.insert(i, old);
                    self.status = "保存排除目录失败".into();
                }
            }
            if let Some(enabled) = autostart_change {
                if set_autostart(enabled).is_ok() {
                    self.settings.autostart = enabled;
                    if settings::save(&self.settings_path, &self.settings).is_err() {
                        self.settings.autostart = !enabled;
                        let _ = set_autostart(!enabled);
                        self.status = "保存开机启动设置失败".into();
                    }
                } else {
                    self.status = "无法更改开机启动设置".into();
                }
            }
        }
        if let Some(path) = self.cleanup_confirm.clone() {
            egui::Window::new("删除旧索引副本")
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.label(format!(
                        "将删除此目录中的 LLMLocalSearch 旧索引文件：{}",
                        path.display()
                    ));
                    ui.label("不会删除该目录或其他文件。");
                    ui.horizontal(|ui| {
                        if ui.button("删除旧索引").clicked() {
                            self.cleanup_confirm = None;
                            self.cleanup_inflight = true;
                            let current = self.settings.index_dir.clone();
                            let sender = self.cleanup_tx.clone();
                            let wake = ctx.clone();
                            let path = path.clone();
                            std::thread::spawn(move || {
                                let result = settings::remove_old_index(&path, &current)
                                    .map_err(|_| "旧索引未能删除，请检查文件状态".to_owned());
                                let _ = sender.send((path, result));
                                wake.request_repaint();
                            });
                        }
                        if ui.button("取消").clicked() {
                            self.cleanup_confirm = None;
                        }
                    });
                });
        }
    }
}

fn state_label(value: &str) -> &'static str {
    match value {
        "pending" => "等待解析",
        "retry_wait" => "文件暂不可读，等待重试",
        "name_only" => "仅搜索文件名",
        "cloud_placeholder" => "云文件尚未下载",
        "no_text" => "无可提取文字",
        "inaccessible" => "无权访问",
        "too_large" => "文件或文本过大",
        "parser_unavailable" => "解析组件不可用",
        "encrypted" => "文件已加密",
        "failed" => "解析失败",
        _ => "未完成",
    }
}

fn setup_fonts(ctx: &egui::Context) {
    if let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\msyh.ttc") {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "chinese".into(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "chinese".into());
        ctx.set_fonts(fonts);
    }
}

#[cfg(windows)]
fn open_file(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        );
    }
}
#[cfg(not(windows))]
fn open_file(_path: &Path) {}

fn reveal_file(path: &Path) {
    let _ = std::process::Command::new("explorer.exe")
        .arg("/select,")
        .arg(path)
        .spawn();
}

#[cfg(windows)]
fn set_autostart(enabled: bool) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let mut command = std::process::Command::new("reg.exe");
    if enabled {
        let exe = std::env::current_exe()?;
        let value = format!("\"{}\"", exe.display());
        command.args([
            "add",
            key,
            "/v",
            "LLMLocalSearch",
            "/t",
            "REG_SZ",
            "/d",
            &value,
            "/f",
        ]);
    } else {
        let exists = std::process::Command::new("reg.exe")
            .args(["query", key, "/v", "LLMLocalSearch"])
            .creation_flags(0x08000000)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?
            .success();
        if !exists {
            return Ok(());
        }
        command.args(["delete", key, "/v", "LLMLocalSearch", "/f"]);
    }
    let status = command
        .creation_flags(0x08000000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!("无法更新开机启动设置");
    }
    Ok(())
}
#[cfg(not(windows))]
fn set_autostart(_enabled: bool) -> anyhow::Result<()> {
    anyhow::bail!("当前系统不支持")
}

#[cfg(windows)]
fn choose_folder() -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW, SHGetPathFromIDListW,
    };
    unsafe {
        let mut info: BROWSEINFOW = std::mem::zeroed();
        info.ulFlags = BIF_RETURNONLYFSDIRS;
        let id = SHBrowseForFolderW(&info);
        if id.is_null() {
            return None;
        }
        let mut buffer = [0u16; 32768];
        let ok = SHGetPathFromIDListW(id, buffer.as_mut_ptr()) != 0;
        CoTaskMemFree(id as _);
        if !ok {
            return None;
        }
        let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        Some(PathBuf::from(String::from_utf16_lossy(&buffer[..end])))
    }
}
#[cfg(not(windows))]
fn choose_folder() -> Option<PathBuf> {
    None
}

fn main() -> eframe::Result {
    let app_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let mut args = std::env::args_os().skip(1);
    let mut root_override = None;
    let mut settings_override = None;
    while let Some(arg) = args.next() {
        if arg == "--test-root" {
            root_override = args.next().map(PathBuf::from);
        } else if arg == "--settings-dir" {
            settings_override = args.next().map(PathBuf::from);
        }
    }
    let data_dir = settings_override
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .unwrap_or_else(|| app_dir.clone())
        .join("LLMLocalSearch");
    let settings_path = data_dir.join("settings.json");
    let settings = match settings::load(&settings_path, &app_dir) {
        Ok(settings) => settings,
        Err(_) => {
            show_error("无法读取设置。为保护已有索引，程序不会自动切换索引目录。");
            return Ok(());
        }
    };
    if !settings_path.exists() && settings::save(&settings_path, &settings).is_err() {
        show_error("无法保存索引目录设置，请检查当前用户的本地应用数据目录。");
        return Ok(());
    }
    let dynamic_fixed = root_override.is_none();
    let roots = root_override
        .map(|r| vec![r])
        .unwrap_or_else(scanner::fixed_drives);
    let icon = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .ok()
        .map(|img| {
            let rgba = img.to_rgba8();
            egui::IconData {
                rgba: rgba.into_raw(),
                width: 256,
                height: 256,
            }
        });
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([980.0, 720.0])
        .with_min_inner_size([650.0, 420.0]);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "LLMLocalSearch",
        options,
        Box::new(move |cc| {
            Ok(Box::new(DeskApp::new(
                cc,
                app_dir,
                settings_path,
                settings,
                roots,
                dynamic_fixed,
            )))
        }),
    )
}

#[cfg(windows)]
fn show_error(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "LLMLocalSearch".encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}
#[cfg(not(windows))]
fn show_error(_message: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_for_test(directory: &Path, settings: Settings) -> DeskApp {
        let (_, tray_events) = mpsc::channel();
        let (search_tx, search_rx) = mpsc::channel();
        let (migration_tx, migration_rx) = mpsc::channel();
        let (cleanup_tx, cleanup_rx) = mpsc::channel();
        let (issue_tx, issue_rx) = mpsc::channel();
        DeskApp {
            app_dir: directory.to_path_buf(),
            settings_path: directory.join("settings.json"),
            migrate_path: String::new(),
            settings,
            roots: Vec::new(),
            dynamic_fixed: false,
            worker: None,
            _tray: None,
            tray_events,
            open_id: MenuId::new("open"),
            pause_id: MenuId::new("pause"),
            resume_id: MenuId::new("resume"),
            exit_id: MenuId::new("exit"),
            query: String::new(),
            scope: Scope::Both,
            kinds: [true; 8],
            result: Vec::new(),
            page: 0,
            generation: Arc::new(AtomicU64::new(0)),
            active_searches: Arc::new(AtomicUsize::new(0)),
            search_tx,
            search_rx,
            searching: false,
            status: String::new(),
            index_status: String::new(),
            paused: false,
            show_settings: false,
            first_close: false,
            quit: false,
            migrate_wait: None,
            migration_inflight: true,
            restart_requested: false,
            cleanup_confirm: None,
            cleanup_inflight: false,
            cleanup_tx,
            cleanup_rx,
            migration_tx,
            migration_rx,
            show_issues: false,
            issues: Vec::new(),
            issue_tx,
            issue_rx,
        }
    }

    #[test]
    fn migration_preserves_settings_saved_while_it_was_running() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let target = temporary.path().join("target");
        std::fs::create_dir_all(&source)?;
        std::fs::create_dir_all(&target)?;
        let mut latest = Settings::default_for(temporary.path());
        latest.index_dir = source.clone();
        latest.exclusions.push(temporary.path().join("excluded"));
        latest.close_hint_seen = true;
        let mut app = app_for_test(temporary.path(), latest.clone());
        settings::save(&app.settings_path, &latest)?;
        app.migration_tx.send(Ok(MigrationOutcome {
            index_dir: target.clone(),
            previous_dir: Some(source.clone()),
        }))?;
        app.poll(&egui::Context::default());
        assert_eq!(app.settings.index_dir, target);
        assert_eq!(app.settings.old_index_dirs, vec![source]);
        assert_eq!(app.settings.exclusions, latest.exclusions);
        assert!(app.settings.close_hint_seen);
        let stored = settings::load(&app.settings_path, temporary.path())?;
        assert_eq!(stored.exclusions, latest.exclusions);
        assert!(stored.close_hint_seen);
        Ok(())
    }

    #[test]
    fn failed_migration_settings_save_keeps_the_original_index() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let target = temporary.path().join("target");
        std::fs::create_dir_all(&source)?;
        std::fs::create_dir_all(&target)?;
        let mut latest = Settings::default_for(temporary.path());
        latest.index_dir = source.clone();
        let mut app = app_for_test(temporary.path(), latest.clone());
        settings::save(&app.settings_path, &latest)?;
        std::fs::write(app.settings_path.with_extension("tmp"), "occupied")?;
        app.migration_tx.send(Ok(MigrationOutcome {
            index_dir: target,
            previous_dir: Some(source.clone()),
        }))?;
        app.poll(&egui::Context::default());
        assert_eq!(app.settings.index_dir, source);
        assert_eq!(
            settings::load(&app.settings_path, temporary.path())?.index_dir,
            source
        );
        Ok(())
    }

    #[test]
    fn exit_waits_until_migration_commits() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let target = temporary.path().join("target");
        std::fs::create_dir_all(&source)?;
        std::fs::create_dir_all(&target)?;
        let mut latest = Settings::default_for(temporary.path());
        latest.index_dir = source.clone();
        let mut app = app_for_test(temporary.path(), latest);
        let (tray_tx, tray_rx) = mpsc::channel();
        app.tray_events = tray_rx;
        tray_tx.send(app.exit_id.0.clone())?;
        let ctx = egui::Context::default();
        let first = ctx.run_logic(&egui::RawInput::default(), |ctx| app.poll(ctx));
        assert!(app.quit && app.migration_inflight);
        assert!(
            !first
                .viewport_commands
                .values()
                .flatten()
                .any(|c| matches!(c, egui::ViewportCommand::Close))
        );
        app.migration_tx.send(Ok(MigrationOutcome {
            index_dir: target.clone(),
            previous_dir: Some(source),
        }))?;
        let second = ctx.run_logic(&egui::RawInput::default(), |ctx| app.poll(ctx));
        assert!(
            second
                .viewport_commands
                .values()
                .flatten()
                .any(|c| matches!(c, egui::ViewportCommand::Close))
        );
        assert_eq!(
            settings::load(&app.settings_path, temporary.path())?.index_dir,
            target
        );
        Ok(())
    }

    #[test]
    #[ignore = "run during a synthetic indexing workload to measure egui CPU-side response"]
    fn input_to_egui_paint_baseline() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let settings = Settings::default_for(temporary.path());
        let mut app = app_for_test(temporary.path(), settings);
        app.migration_inflight = false;
        let ctx = egui::Context::default();
        setup_fonts(&ctx);
        let mut frame = eframe::Frame::_new_kittest();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(980.0, 720.0),
            )),
            focused: true,
            ..Default::default()
        };
        let warmup = ctx.run_ui(input.clone(), |ui| {
            eframe::App::ui(&mut app, ui, &mut frame)
        });
        let _ = ctx.tessellate(warmup.shapes, warmup.pixels_per_point);
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("search_input")));
        let mut durations = Vec::new();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut event = input.clone();
            event.events.push(egui::Event::Text("测".into()));
            let started = std::time::Instant::now();
            let output = ctx.run_ui(event, |ui| eframe::App::ui(&mut app, ui, &mut frame));
            let _ = ctx.tessellate(output.shapes, output.pixels_per_point);
            durations.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        assert_eq!(app.query.chars().count(), 100);
        durations.sort_by(f64::total_cmp);
        println!(
            "synthetic egui text event to tessellation p95_ms={:.3} samples=100",
            durations[94]
        );
        Ok(())
    }
}
