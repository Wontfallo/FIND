//! The FIND application window: search bar, filter chips, results table,
//! preview pane, settings, and all the plumbing between UI and worker threads.

use crate::preview::{self, PreviewContent};
use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use find_core::content::{filter_by_content, MAX_GREP_FILES};
use find_core::index::{self, Index};
use find_core::query::{self, MatchMode};
use find_core::search::{self, Hit};
use find_core::settings::Settings;
use find_core::util::{human_date, human_size, thousands, Category};
use find_core::watcher::{self, WatchHandle};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

#[derive(Clone)]
struct SearchRequest {
    generation: u64,
    query: String,
    mode: MatchMode,
    case_sensitive: bool,
    category: Category,
    max_results: usize,
}

struct SearchResponse {
    generation: u64,
    hits: Vec<Hit>,
    total: usize,
    truncated: bool,
    elapsed_ms: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Relevance,
    Name,
    Path,
    Size,
    Modified,
}

pub struct FindApp {
    settings: Settings,
    query: String,
    category: Category,

    index: Arc<RwLock<Index>>,
    index_count: usize,
    scanning: Arc<AtomicBool>,
    scan_progress: Arc<AtomicUsize>,
    scan_cancel: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    _watch: Option<WatchHandle>,

    generation: Arc<AtomicU64>,
    req_tx: Sender<SearchRequest>,
    res_rx: Receiver<SearchResponse>,
    last_request: Option<SearchRequest>,

    results: Vec<Hit>,
    total: usize,
    truncated: bool,
    search_ms: f32,
    selected: Option<usize>,
    sort: SortKey,
    sort_descending: bool,

    preview: PreviewContent,
    preview_for: Option<u32>,

    show_settings: bool,
    show_help: bool,
    settings_roots_draft: String,
    settings_exclusions_draft: String,
    first_frame: bool,
}

impl FindApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let settings = Settings::load();
        let index = Arc::new(RwLock::new(Index::default()));
        let scanning = Arc::new(AtomicBool::new(false));
        let scan_progress = Arc::new(AtomicUsize::new(0));
        let scan_cancel = Arc::new(AtomicBool::new(false));
        let dirty = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));

        let (req_tx, req_rx) = crossbeam_channel::unbounded::<SearchRequest>();
        let (res_tx, res_rx) = crossbeam_channel::unbounded::<SearchResponse>();
        spawn_search_worker(
            req_rx,
            res_tx,
            index.clone(),
            generation.clone(),
            cc.egui_ctx.clone(),
        );

        // Load the cached index (instant startup), then rescan in background.
        spawn_initial_load(
            settings.clone(),
            index.clone(),
            scanning.clone(),
            scan_progress.clone(),
            scan_cancel.clone(),
            dirty.clone(),
            cc.egui_ctx.clone(),
        );

        let watch = if settings.watch_filesystem {
            watcher::watch(
                settings.roots.clone(),
                settings.exclusions.clone(),
                index.clone(),
                dirty.clone(),
            )
        } else {
            None
        };

        let settings_roots_draft = settings
            .roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let settings_exclusions_draft = settings.exclusions.join("\n");

        FindApp {
            settings,
            query: String::new(),
            category: Category::All,
            index,
            index_count: 0,
            scanning,
            scan_progress,
            scan_cancel,
            dirty,
            _watch: watch,
            generation,
            req_tx,
            res_rx,
            last_request: None,
            results: Vec::new(),
            total: 0,
            truncated: false,
            search_ms: 0.0,
            selected: None,
            sort: SortKey::Relevance,
            sort_descending: true,
            preview: PreviewContent::Empty,
            preview_for: None,
            show_settings: false,
            show_help: false,
            settings_roots_draft,
            settings_exclusions_draft,
            first_frame: true,
        }
    }

    fn send_search(&mut self) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let req = SearchRequest {
            generation,
            query: self.query.clone(),
            mode: self.settings.match_mode,
            case_sensitive: self.settings.case_sensitive,
            category: self.category,
            max_results: self.settings.max_results,
        };
        self.last_request = Some(req.clone());
        let _ = self.req_tx.send(req);
    }

    fn resend_search(&mut self) {
        self.send_search();
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        if self.scanning.swap(true, Ordering::SeqCst) {
            return;
        }
        self.scan_cancel.store(false, Ordering::Relaxed);
        let settings = self.settings.clone();
        let index = self.index.clone();
        let scanning = self.scanning.clone();
        let progress = self.scan_progress.clone();
        let cancel = self.scan_cancel.clone();
        let dirty = self.dirty.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("find-scan".into())
            .spawn(move || {
                let new_index =
                    index::scan(&settings.roots, &settings.exclusions, &progress, &cancel);
                let _ = index::save_to_disk(&new_index);
                *index.write().unwrap() = new_index;
                scanning.store(false, Ordering::SeqCst);
                dirty.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            })
            .ok();
    }

    fn apply_sort(&mut self) {
        match self.sort {
            SortKey::Relevance => {
                self.results.sort_by(|a, b| b.score.cmp(&a.score));
                if !self.sort_descending {
                    self.results.reverse();
                }
            }
            SortKey::Name => {
                self.results
                    .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                if self.sort_descending {
                    self.results.reverse();
                }
            }
            SortKey::Path => {
                self.results.sort_by(|a, b| a.path.cmp(&b.path));
                if self.sort_descending {
                    self.results.reverse();
                }
            }
            SortKey::Size => {
                self.results.sort_by(|a, b| b.size.cmp(&a.size));
                if !self.sort_descending {
                    self.results.reverse();
                }
            }
            SortKey::Modified => {
                self.results.sort_by(|a, b| b.modified.cmp(&a.modified));
                if !self.sort_descending {
                    self.results.reverse();
                }
            }
        }
        self.selected = None;
    }

    fn header_sort_button(&mut self, ui: &mut egui::Ui, label: &str, key: SortKey) {
        let arrow = if self.sort == key {
            if self.sort_descending {
                " ▼"
            } else {
                " ▲"
            }
        } else {
            ""
        };
        if ui
            .add(egui::Button::new(format!("{label}{arrow}")).frame(false))
            .clicked()
        {
            if self.sort == key {
                self.sort_descending = !self.sort_descending;
            } else {
                self.sort = key;
                self.sort_descending = true;
            }
            self.apply_sort();
        }
    }

    fn select(&mut self, row: usize) {
        self.selected = Some(row);
        if let Some(hit) = self.results.get(row) {
            if self.preview_for != Some(hit.idx) {
                self.preview_for = Some(hit.idx);
                if self.settings.show_preview {
                    self.preview = preview::load(hit);
                }
            }
        }
    }

    fn open_hit(&self, row: usize) {
        if let Some(hit) = self.results.get(row) {
            let _ = open::that_detached(&hit.path);
        }
    }

    fn reveal_hit(&self, row: usize) {
        let Some(hit) = self.results.get(row) else {
            return;
        };
        reveal_in_file_manager(&hit.path);
    }
}

fn reveal_in_file_manager(path: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{path}"))
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").args(["-R", path]).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = open::that_detached(parent);
        }
    }
}

fn spawn_search_worker(
    req_rx: Receiver<SearchRequest>,
    res_tx: Sender<SearchResponse>,
    index: Arc<RwLock<Index>>,
    generation: Arc<AtomicU64>,
    ctx: egui::Context,
) {
    std::thread::Builder::new()
        .name("find-search".into())
        .spawn(move || {
            while let Ok(mut req) = req_rx.recv() {
                // Coalesce: only the newest pending request matters.
                while let Ok(newer) = req_rx.try_recv() {
                    req = newer;
                }
                let started = Instant::now();
                let spec = query::parse(&req.query, req.mode, req.case_sensitive);
                let content_query = spec.content.clone();

                let outcome = {
                    let guard = match index.read() {
                        Ok(g) => g,
                        Err(_) => continue,
                    };
                    // For content searches, gather a wide candidate set first;
                    // the grep pass narrows it down.
                    let cap = if content_query.is_some() {
                        MAX_GREP_FILES
                    } else {
                        req.max_results
                    };
                    search::execute(
                        &guard,
                        &spec,
                        req.category,
                        cap,
                        req.generation,
                        &generation,
                    )
                };
                let Some(mut outcome) = outcome else { continue };

                if let Some(pattern) = &content_query {
                    let as_regex = req.mode == MatchMode::Regex;
                    let Some(mut hits) = filter_by_content(
                        std::mem::take(&mut outcome.hits),
                        pattern,
                        as_regex,
                        req.case_sensitive,
                        req.generation,
                        &generation,
                    ) else {
                        continue;
                    };
                    outcome.total = hits.len();
                    outcome.truncated = hits.len() > req.max_results;
                    hits.truncate(req.max_results);
                    outcome.hits = hits;
                }

                if generation.load(Ordering::Relaxed) != req.generation {
                    continue;
                }
                let _ = res_tx.send(SearchResponse {
                    generation: req.generation,
                    hits: outcome.hits,
                    total: outcome.total,
                    truncated: outcome.truncated,
                    elapsed_ms: started.elapsed().as_secs_f32() * 1000.0,
                });
                ctx.request_repaint();
            }
        })
        .ok();
}

#[allow(clippy::too_many_arguments)]
fn spawn_initial_load(
    settings: Settings,
    index: Arc<RwLock<Index>>,
    scanning: Arc<AtomicBool>,
    progress: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    std::thread::Builder::new()
        .name("find-init".into())
        .spawn(move || {
            if let Some(loaded) = index::load_from_disk() {
                if loaded.roots == settings.roots {
                    *index.write().unwrap() = loaded;
                    dirty.store(true, Ordering::Relaxed);
                    ctx.request_repaint();
                }
            }
            // Always rescan to pick up changes made while we weren't running.
            if !scanning.swap(true, Ordering::SeqCst) {
                let new_index =
                    index::scan(&settings.roots, &settings.exclusions, &progress, &cancel);
                if !cancel.load(Ordering::Relaxed) {
                    let _ = index::save_to_disk(&new_index);
                    *index.write().unwrap() = new_index;
                }
                scanning.store(false, Ordering::SeqCst);
                dirty.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            }
        })
        .ok();
}

impl eframe::App for FindApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain search responses (keep only the newest valid one).
        let mut fresh = None;
        while let Ok(res) = self.res_rx.try_recv() {
            if res.generation == self.generation.load(Ordering::Relaxed) {
                fresh = Some(res);
            }
        }
        if let Some(res) = fresh {
            self.results = res.hits;
            self.total = res.total;
            self.truncated = res.truncated;
            self.search_ms = res.elapsed_ms;
            self.selected = None;
            if self.sort != SortKey::Relevance {
                self.apply_sort();
            }
        }

        // Watcher / scan finished: refresh counts and rerun the query.
        if self.dirty.swap(false, Ordering::Relaxed) {
            self.index_count = self
                .index
                .read()
                .map(|i| i.live_count())
                .unwrap_or(self.index_count);
            self.resend_search();
        }

        let scanning = self.scanning.load(Ordering::Relaxed);
        if scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }

        self.top_bar(ctx);
        self.status_bar(ctx, scanning);
        if self.settings.show_preview {
            self.preview_panel(ctx);
        }
        self.results_panel(ctx);
        self.settings_window(ctx);
        self.help_window(ctx);
        self.handle_keys(ctx);
        self.first_frame = false;
    }
}

impl FindApp {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Search everything…  (try: report ext:pdf size:>1mb  or  content:\"todo\" ext:rs)")
                        .font(egui::FontId::proportional(17.0))
                        .desired_width(ui.available_width() - 300.0),
                );
                if self.first_frame {
                    search.request_focus();
                }
                if search.changed() {
                    self.send_search();
                }

                let mut mode = self.settings.match_mode;
                egui::ComboBox::from_id_salt("match_mode")
                    .selected_text(mode.label())
                    .width(95.0)
                    .show_ui(ui, |ui| {
                        for m in [MatchMode::Substring, MatchMode::Fuzzy, MatchMode::Regex] {
                            ui.selectable_value(&mut mode, m, m.label());
                        }
                    });
                if mode != self.settings.match_mode {
                    self.settings.match_mode = mode;
                    self.settings.save();
                    self.send_search();
                }

                let mut case = self.settings.case_sensitive;
                if ui
                    .selectable_label(case, "Aa")
                    .on_hover_text("Case sensitive")
                    .clicked()
                {
                    case = !case;
                    self.settings.case_sensitive = case;
                    self.settings.save();
                    self.send_search();
                }

                if ui
                    .selectable_label(self.settings.show_preview, "👁 Preview")
                    .clicked()
                {
                    self.settings.show_preview = !self.settings.show_preview;
                    self.settings.save();
                }
                if ui.button("⚙").on_hover_text("Settings").clicked() {
                    self.show_settings = !self.show_settings;
                }
                if ui.button("?").on_hover_text("Search syntax help").clicked() {
                    self.show_help = !self.show_help;
                }
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for cat in Category::ALL {
                    if ui
                        .selectable_label(self.category == cat, cat.label())
                        .clicked()
                    {
                        self.category = cat;
                        self.send_search();
                    }
                }
            });
            ui.add_space(4.0);
        });
    }

    fn status_bar(&mut self, ctx: &egui::Context, scanning: bool) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("{} objects indexed", thousands(self.index_count)));
                ui.separator();
                let shown = self.results.len();
                if self.truncated {
                    ui.label(format!(
                        "{} results (showing {}) in {:.0} ms",
                        thousands(self.total),
                        thousands(shown),
                        self.search_ms
                    ));
                } else {
                    ui.label(format!(
                        "{} results in {:.0} ms",
                        thousands(self.total),
                        self.search_ms
                    ));
                }
                if scanning {
                    ui.separator();
                    ui.spinner();
                    ui.label(format!(
                        "Indexing… {} entries",
                        thousands(self.scan_progress.load(Ordering::Relaxed))
                    ));
                } else {
                    ui.separator();
                    if ui.button("⟳ Rescan").clicked() {
                        self.start_scan(ctx);
                    }
                }
            });
        });
    }

    fn preview_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("preview")
            .resizable(true)
            .default_width(360.0)
            .min_width(220.0)
            .show(ctx, |ui| {
                let Some(row) = self.selected else {
                    ui.centered_and_justified(|ui| {
                        ui.label("Select a file to preview it");
                    });
                    return;
                };
                let (name, size, modified, path, content_line) = match self.results.get(row) {
                    Some(h) => (
                        h.name.clone(),
                        h.size,
                        h.modified,
                        h.path.clone(),
                        h.content_line.clone(),
                    ),
                    None => return,
                };
                ui.add_space(4.0);
                ui.strong(&name);
                ui.label(
                    egui::RichText::new(&path)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
                ui.label(format!("{} • {}", human_size(size), human_date(modified)));
                if let Some((line_num, line)) = &content_line {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!("line {line_num}: {line}"))
                            .monospace()
                            .color(ui.visuals().hyperlink_color),
                    );
                }
                ui.separator();
                match &self.preview {
                    PreviewContent::Empty => {}
                    PreviewContent::Info(text) => {
                        ui.label(text.as_str());
                    }
                    PreviewContent::Image { uri } => {
                        egui::ScrollArea::both().show(ui, |ui| {
                            ui.add(
                                egui::Image::new(uri.as_str())
                                    .max_size(ui.available_size())
                                    .maintain_aspect_ratio(true),
                            );
                        });
                    }
                    PreviewContent::Text { text, truncated } => {
                        let truncated = *truncated;
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(text.as_str()).monospace().size(12.0),
                                )
                                .wrap(),
                            );
                            if truncated {
                                ui.label(
                                    egui::RichText::new("… preview truncated")
                                        .italics()
                                        .weak(),
                                );
                            }
                        });
                    }
                }
            });
    }

    fn results_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            use egui_extras::{Column, TableBuilder};

            let text_height = 20.0;
            let mut clicked_row: Option<usize> = None;
            let mut double_clicked: Option<usize> = None;
            let mut context_action: Option<(usize, RowAction)> = None;

            let has_content = self.results.iter().any(|h| h.content_line.is_some());

            let mut table = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .column(Column::initial(320.0).at_least(120.0).clip(true))
                .column(Column::remainder().at_least(150.0).clip(true))
                .column(Column::initial(90.0).at_least(60.0))
                .column(Column::initial(130.0).at_least(90.0));
            if has_content {
                table = table.column(Column::initial(260.0).at_least(100.0).clip(true));
            }

            table
                .header(22.0, |mut header| {
                    header.col(|ui| self.header_sort_button(ui, "Name", SortKey::Name));
                    header.col(|ui| self.header_sort_button(ui, "Path", SortKey::Path));
                    header.col(|ui| self.header_sort_button(ui, "Size", SortKey::Size));
                    header.col(|ui| self.header_sort_button(ui, "Modified", SortKey::Modified));
                    if has_content {
                        header.col(|ui| {
                            ui.strong("Match");
                        });
                    }
                })
                .body(|body| {
                    body.rows(text_height, self.results.len(), |mut row| {
                        let i = row.index();
                        let hit = &self.results[i];
                        row.set_selected(self.selected == Some(i));
                        row.col(|ui| {
                            let icon = if hit.is_dir { "📁" } else { file_icon(&hit.name) };
                            ui.add(
                                egui::Label::new(format!("{icon} {}", hit.name))
                                    .truncate()
                                    .selectable(false),
                            );
                        });
                        row.col(|ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&hit.path)
                                        .color(ui.visuals().weak_text_color()),
                                )
                                .truncate()
                                .selectable(false),
                            );
                        });
                        row.col(|ui| {
                            if !hit.is_dir {
                                ui.add(
                                    egui::Label::new(human_size(hit.size)).selectable(false),
                                );
                            }
                        });
                        row.col(|ui| {
                            ui.add(
                                egui::Label::new(human_date(hit.modified)).selectable(false),
                            );
                        });
                        if has_content {
                            row.col(|ui| {
                                if let Some((n, line)) = &hit.content_line {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(format!("{n}: {line}"))
                                                .monospace()
                                                .size(11.0),
                                        )
                                        .truncate()
                                        .selectable(false),
                                    );
                                }
                            });
                        }

                        let response = row.response();
                        if response.clicked() {
                            clicked_row = Some(i);
                        }
                        if response.double_clicked() {
                            double_clicked = Some(i);
                        }
                        response.context_menu(|ui| {
                            if ui.button("Open").clicked() {
                                context_action = Some((i, RowAction::Open));
                                ui.close();
                            }
                            if ui.button("Open location").clicked() {
                                context_action = Some((i, RowAction::Reveal));
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Copy full path").clicked() {
                                context_action = Some((i, RowAction::CopyPath));
                                ui.close();
                            }
                            if ui.button("Copy name").clicked() {
                                context_action = Some((i, RowAction::CopyName));
                                ui.close();
                            }
                            if ui.button("Copy folder path").clicked() {
                                context_action = Some((i, RowAction::CopyFolder));
                                ui.close();
                            }
                        });
                    });
                });

            if let Some(i) = clicked_row {
                self.select(i);
            }
            if let Some(i) = double_clicked {
                self.select(i);
                self.open_hit(i);
            }
            if let Some((i, action)) = context_action {
                self.select(i);
                match action {
                    RowAction::Open => self.open_hit(i),
                    RowAction::Reveal => self.reveal_hit(i),
                    RowAction::CopyPath => {
                        if let Some(h) = self.results.get(i) {
                            ctx.copy_text(h.path.clone());
                        }
                    }
                    RowAction::CopyName => {
                        if let Some(h) = self.results.get(i) {
                            ctx.copy_text(h.name.clone());
                        }
                    }
                    RowAction::CopyFolder => {
                        if let Some(h) = self.results.get(i) {
                            let folder = std::path::Path::new(&h.path)
                                .parent()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                            ctx.copy_text(folder);
                        }
                    }
                }
            }
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let mut rescan = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .default_width(480.0)
            .show(ctx, |ui| {
                ui.label("Indexed locations (one per line):");
                ui.add(
                    egui::TextEdit::multiline(&mut self.settings_roots_draft)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );
                ui.add_space(6.0);
                ui.label("Exclude paths containing (one per line):");
                ui.add(
                    egui::TextEdit::multiline(&mut self.settings_exclusions_draft)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Max results:");
                    let mut max = self.settings.max_results as u32;
                    ui.add(
                        egui::DragValue::new(&mut max)
                            .range(100..=100_000)
                            .speed(100),
                    );
                    self.settings.max_results = max as usize;
                });
                ui.checkbox(
                    &mut self.settings.watch_filesystem,
                    "Watch filesystem for live updates (takes effect after restart)",
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        self.apply_settings_draft();
                    }
                    if ui.button("Save && Rescan").clicked() {
                        self.apply_settings_draft();
                        rescan = true;
                    }
                });
            });
        self.show_settings = open;
        if rescan {
            self.start_scan(ctx);
        }
    }

    fn apply_settings_draft(&mut self) {
        let roots: Vec<std::path::PathBuf> = self
            .settings_roots_draft
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(std::path::PathBuf::from)
            .collect();
        if !roots.is_empty() {
            self.settings.roots = roots;
        }
        self.settings.exclusions = self
            .settings_exclusions_draft
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        self.settings.save();
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        if !self.show_help {
            return;
        }
        let mut open = true;
        egui::Window::new("Search syntax")
            .open(&mut open)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.monospace(
                    "Plain words match file names (all words must match).\n\
                     \n\
                     Filters (combine freely with words):\n\
                     ext:pdf,docx        only these extensions\n\
                     path:projects       full path must contain this\n\
                     size:>10mb          also <, >=, <=, and 1mb..100mb\n\
                     date:>2024-01-01    modified after; also ranges a..b\n\
                     type:file           or type:folder\n\
                     content:\"foo bar\"   grep inside text files\n\
                     \n\
                     Modes (dropdown next to the search box):\n\
                     Substring  fast 'contains' matching (default)\n\
                     Fuzzy      type parts of a name: rpt2024 → report_2024.pdf\n\
                     Regex      full regular expressions, e.g. ^inv.*\\.pdf$\n\
                     \n\
                     Keyboard:\n\
                     ↑ / ↓      move selection\n\
                     Enter      open selected (or top) result\n\
                     Ctrl+Shift+C  copy full path\n\
                     Esc        clear search",
                );
            });
        self.show_help = open;
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.show_settings || self.show_help {
            return;
        }
        let (down, up, enter, escape, copy_path) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
                i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::C),
            )
        });
        if down && !self.results.is_empty() {
            let next = self.selected.map(|s| (s + 1).min(self.results.len() - 1)).unwrap_or(0);
            self.select(next);
        }
        if up && !self.results.is_empty() {
            let prev = self.selected.map(|s| s.saturating_sub(1)).unwrap_or(0);
            self.select(prev);
        }
        if enter && !self.results.is_empty() {
            let row = self.selected.unwrap_or(0);
            self.open_hit(row);
        }
        if escape {
            if !self.query.is_empty() {
                self.query.clear();
                self.send_search();
            }
        }
        if copy_path {
            if let Some(hit) = self.selected.and_then(|s| self.results.get(s)) {
                ctx.copy_text(hit.path.clone());
            }
        }
    }
}

enum RowAction {
    Open,
    Reveal,
    CopyPath,
    CopyName,
    CopyFolder,
}

fn file_icon(name: &str) -> &'static str {
    if Category::Images.matches(name, false) {
        "🖼"
    } else if Category::Audio.matches(name, false) {
        "🎵"
    } else if Category::Video.matches(name, false) {
        "🎬"
    } else if Category::Archives.matches(name, false) {
        "📦"
    } else if Category::Code.matches(name, false) {
        "📜"
    } else if Category::Executables.matches(name, false) {
        "⚙"
    } else {
        "📄"
    }
}
