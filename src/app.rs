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

    /// Generation of the results currently on screen.
    displayed_generation: u64,
    /// Last time a dirty-flag refresh re-ran the search.
    last_dirty_refresh: Option<Instant>,
    results: Vec<Hit>,
    /// Scroll the table to the selected row next frame (set by keyboard nav).
    scroll_to_selected: bool,
    total: usize,
    truncated: bool,
    search_ms: f32,
    selected: Option<usize>,
    sort: SortKey,
    sort_descending: bool,

    preview: PreviewContent,
    preview_for: Option<u32>,

    /// Live copy of settings for background threads (scheduler).
    settings_shared: Arc<std::sync::Mutex<Settings>>,
    /// True while a scan owns the index (blocks watcher writes).
    index_locked: Arc<AtomicBool>,
    /// Roots whose scan finished (drives the checkmarks in Settings).
    completed_roots: Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>>,
    /// Image URI to evict from egui's texture cache next frame.
    pending_forget: Option<String>,
    show_settings: bool,
    show_help: bool,
    settings_roots_draft: String,
    settings_exclusions_draft: String,
    first_frame: bool,
    /// True while the window is hidden in the tray.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    hidden: bool,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    quit_requested: bool,
    #[cfg(target_os = "windows")]
    tray: Option<crate::tray::Tray>,
    /// Native window handle, shared with the tray so it can restore the
    /// window even while the egui loop is asleep.
    #[cfg(target_os = "windows")]
    hwnd: std::sync::Arc<std::sync::atomic::AtomicIsize>,
}

/// Theme colors resolved from the user's preset + accent choices.
struct Theme {
    bg: egui::Color32,
    bar: egui::Color32,
    stripe: egui::Color32,
    input_bg: egui::Color32,
    hover: egui::Color32,
    accent: egui::Color32,
    accent_light: egui::Color32,
}

fn theme_of(settings: &Settings) -> Theme {
    use egui::Color32;
    use find_core::settings::{AccentColor, ThemePreset};
    let (bg, bar, stripe, input_bg, hover) = match settings.theme {
        // Warm dark grays/browns — easy on the eyes, zero blue cast.
        ThemePreset::Graphite => (
            Color32::from_rgb(22, 21, 19),
            Color32::from_rgb(33, 30, 27),
            Color32::from_rgb(27, 26, 24),
            Color32::from_rgb(15, 14, 13),
            Color32::from_rgb(45, 42, 38),
        ),
        ThemePreset::Carbon => (
            Color32::from_rgb(16, 16, 16),
            Color32::from_rgb(26, 26, 26),
            Color32::from_rgb(21, 21, 21),
            Color32::from_rgb(10, 10, 10),
            Color32::from_rgb(38, 38, 38),
        ),
        ThemePreset::Navy => (
            Color32::from_rgb(16, 18, 27),
            Color32::from_rgb(24, 27, 40),
            Color32::from_rgb(22, 25, 36),
            Color32::from_rgb(9, 11, 18),
            Color32::from_rgb(38, 44, 66),
        ),
    };
    let accent = match settings.accent {
        AccentColor::Blue => Color32::from_rgb(66, 133, 244),
        AccentColor::Green => Color32::from_rgb(80, 170, 105),
        AccentColor::Amber => Color32::from_rgb(214, 140, 60),
        AccentColor::Violet => Color32::from_rgb(160, 125, 235),
    };
    let accent_light = Color32::from_rgb(
        accent.r().saturating_add(70),
        accent.g().saturating_add(70),
        accent.b().saturating_add(50),
    );
    Theme { bg, bar, stripe, input_bg, hover, accent, accent_light }
}

fn apply_theme(ctx: &egui::Context, settings: &Settings) {
    let t = theme_of(settings);
    let mut v = egui::Visuals::dark();
    v.panel_fill = t.bg;
    v.window_fill = t.bg;
    v.extreme_bg_color = t.input_bg;
    v.faint_bg_color = t.stripe;
    v.selection.bg_fill = t.accent;
    v.selection.stroke = egui::Stroke::new(1.0, t.accent_light);
    v.hyperlink_color = t.accent_light;
    v.widgets.hovered.bg_fill = t.hover;
    v.widgets.active.bg_fill = t.accent;
    // Bright warm-white text for contrast against the dark background.
    v.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(232, 229, 224);
    v.widgets.inactive.fg_stroke.color = egui::Color32::from_rgb(226, 223, 218);
    v.widgets.hovered.fg_stroke.color = egui::Color32::from_rgb(248, 246, 242);
    v.widgets.active.fg_stroke.color = egui::Color32::WHITE;
    ctx.set_visuals(v);
}

/// Register MesloLGS Nerd Font Mono as the primary UI font (with egui's
/// defaults as fallback for any glyph it lacks).
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "meslo-nf".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/MesloLGS-NF-Regular.ttf"
        ))),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        if let Some(list) = fonts.families.get_mut(&family) {
            list.insert(0, "meslo-nf".into());
        }
    }
    ctx.set_fonts(fonts);
}

/// Larger default type; users can still zoom the whole UI with Ctrl+= / Ctrl+-.
fn brand_text_styles(ctx: &egui::Context) {
    use egui::{FontId, TextStyle};
    let mut style = (*ctx.style()).clone();
    style.text_styles.insert(TextStyle::Body, FontId::proportional(15.5));
    style.text_styles.insert(TextStyle::Button, FontId::proportional(15.5));
    style.text_styles.insert(TextStyle::Small, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(14.0));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(21.0));
    ctx.set_style(style);
}

impl FindApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        install_fonts(&cc.egui_ctx);
        brand_text_styles(&cc.egui_ctx);

        let settings = Settings::load();
        apply_theme(&cc.egui_ctx, &settings);
        if (settings.ui_scale - 1.0).abs() > 0.01 {
            cc.egui_ctx.set_zoom_factor(settings.ui_scale);
        }
        let index = Arc::new(RwLock::new(Index::default()));
        let scanning = Arc::new(AtomicBool::new(false));
        let scan_progress = Arc::new(AtomicUsize::new(0));
        let scan_cancel = Arc::new(AtomicBool::new(false));
        let dirty = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));

        let settings_shared = Arc::new(std::sync::Mutex::new(settings.clone()));
        let completed_roots = Arc::new(std::sync::Mutex::new(
            std::collections::HashSet::new(),
        ));
        let index_locked = Arc::new(AtomicBool::new(false));

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
            completed_roots.clone(),
            index_locked.clone(),
            cc.egui_ctx.clone(),
        );
        // Daily scheduled rescan (checks the configured HH:MM every 30s;
        // works even while the window sleeps hidden in the tray).
        spawn_rescan_scheduler(
            settings_shared.clone(),
            index.clone(),
            scanning.clone(),
            scan_progress.clone(),
            scan_cancel.clone(),
            dirty.clone(),
            completed_roots.clone(),
            index_locked.clone(),
            cc.egui_ctx.clone(),
        );

        let watch = if settings.watch_filesystem {
            watcher::watch(
                settings.roots.clone(),
                settings.exclusions.clone(),
                index.clone(),
                dirty.clone(),
                index_locked.clone(),
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

        #[cfg(target_os = "windows")]
        let hwnd = std::sync::Arc::new(std::sync::atomic::AtomicIsize::new(0));

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
            displayed_generation: 0,
            last_dirty_refresh: None,
            results: Vec::new(),
            scroll_to_selected: false,
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
            settings_shared,
            index_locked,
            completed_roots,
            pending_forget: None,
            settings_roots_draft,
            settings_exclusions_draft,
            first_frame: true,
            hidden: false,
            quit_requested: false,
            #[cfg(target_os = "windows")]
            tray: crate::tray::init(cc.egui_ctx.clone(), hwnd.clone()),
            #[cfg(target_os = "windows")]
            hwnd,
        }
    }

    /// Save settings to disk and update the copy background threads see.
    fn persist_settings(&self) {
        if let Ok(mut shared) = self.settings_shared.lock() {
            *shared = self.settings.clone();
        }
        self.settings.save();
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
        let completed = self.completed_roots.clone();
        let locked = self.index_locked.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("find-scan".into())
            .spawn(move || {
                locked.store(true, Ordering::Relaxed);
                let new_index = index::scan(
                    &settings.roots,
                    &settings.exclusions,
                    &progress,
                    &cancel,
                    &completed,
                );
                let _ = index::save_to_disk(&new_index);
                *index.write().unwrap() = new_index;
                locked.store(false, Ordering::Relaxed);
                scanning.store(false, Ordering::SeqCst);
                dirty.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            })
            .ok();
    }

    /// Re-sort, keeping the selection on the same entry.
    fn apply_sort(&mut self) {
        let selected_entry = self
            .selected
            .and_then(|s| self.results.get(s))
            .map(|h| h.idx);
        self.sort_results();
        self.selected =
            selected_entry.and_then(|key| self.results.iter().position(|h| h.idx == key));
    }

    fn sort_results(&mut self) {
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
                if self.sort_descending {
                    self.sort_descending = false;
                } else {
                    // Third click returns to relevance ordering.
                    self.sort = SortKey::Relevance;
                    self.sort_descending = true;
                }
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
                    if let PreviewContent::Image { uri, .. } = &self.preview {
                        self.pending_forget = Some(uri.clone());
                    }
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
        // Explorer's /select needs the path quoted as ONE raw argument.
        // Command::arg's automatic quoting wraps "/select,path" in a way
        // Explorer can't parse when the path contains spaces, and it falls
        // back to opening the Documents folder.
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{path}\""))
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

/// Normalize "4:00" / "04:00" to "HH:MM"; None if not a valid 24h time.
fn normalize_hhmm(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(format!("{h:02}:{m:02}"))
}

/// Background thread: fires a full rescan once a day at the configured time.
/// Runs independently of the UI loop, so it works while the window is hidden
/// in the tray.
#[allow(clippy::too_many_arguments)]
fn spawn_rescan_scheduler(
    settings_shared: Arc<std::sync::Mutex<Settings>>,
    index: Arc<RwLock<Index>>,
    scanning: Arc<AtomicBool>,
    progress: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    completed: Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>>,
    index_locked: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    std::thread::Builder::new()
        .name("find-scheduler".into())
        .spawn(move || {
            let mut last_run_day: Option<chrono::NaiveDate> = None;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                let (time_cfg, roots, exclusions) = match settings_shared.lock() {
                    Ok(s) => (
                        s.auto_rescan_time.clone(),
                        s.roots.clone(),
                        s.exclusions.clone(),
                    ),
                    Err(_) => continue,
                };
                if time_cfg.is_empty() {
                    continue;
                }
                let now = chrono::Local::now();
                let today = now.date_naive();
                if now.format("%H:%M").to_string() != time_cfg || last_run_day == Some(today) {
                    continue;
                }
                last_run_day = Some(today);
                if scanning.swap(true, Ordering::SeqCst) {
                    continue; // a scan is already running
                }
                cancel.store(false, Ordering::Relaxed);
                index_locked.store(true, Ordering::Relaxed);
                let new_index = index::scan(&roots, &exclusions, &progress, &cancel, &completed);
                index_locked.store(false, Ordering::Relaxed);
                if !cancel.load(Ordering::Relaxed) {
                    let _ = index::save_to_disk(&new_index);
                    if let Ok(mut guard) = index.write() {
                        *guard = new_index;
                    }
                }
                scanning.store(false, Ordering::SeqCst);
                dirty.store(true, Ordering::Relaxed);
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
    completed: Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>>,
    index_locked: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    std::thread::Builder::new()
        .name("find-init".into())
        .spawn(move || {
            let cached = index::load_from_disk().filter(|l| l.roots == settings.roots);
            if scanning.swap(true, Ordering::SeqCst) {
                return;
            }
            index_locked.store(true, Ordering::Relaxed);
            match cached {
                Some(loaded) => {
                    // Instant startup from the saved index. Only rescan if it
                    // is stale: the file watcher keeps a fresh index current,
                    // so re-walking every drive on every launch is wasted work.
                    let age = index::system_time_secs(Some(std::time::SystemTime::now()))
                        - loaded.scanned_at;
                    let stale = age > 12 * 3600 || loaded.scanned_at == 0;
                    let roots_covered: std::collections::HashSet<_> =
                        loaded.roots.iter().cloned().collect();
                    *index.write().unwrap() = loaded;
                    dirty.store(true, Ordering::Relaxed);
                    ctx.request_repaint();
                    if !stale {
                        if let Ok(mut c) = completed.lock() {
                            c.extend(roots_covered);
                        }
                        index_locked.store(false, Ordering::Relaxed);
                        scanning.store(false, Ordering::SeqCst);
                        dirty.store(true, Ordering::Relaxed);
                        ctx.request_repaint();
                        return;
                    }
                    let new_index = index::scan(
                        &settings.roots,
                        &settings.exclusions,
                        &progress,
                        &cancel,
                        &completed,
                    );
                    if !cancel.load(Ordering::Relaxed) {
                        let _ = index::save_to_disk(&new_index);
                        *index.write().unwrap() = new_index;
                    }
                }
                None => {
                    // First run: stream the scan into the live index so search
                    // works immediately, with results filling in as it goes.
                    index::scan_into(
                        &index,
                        &settings.roots,
                        &settings.exclusions,
                        &progress,
                        &cancel,
                        &dirty,
                        &completed,
                    );
                    if !cancel.load(Ordering::Relaxed) {
                        let _ = index::save_to_disk(&index.read().unwrap());
                    }
                }
            }
            index_locked.store(false, Ordering::Relaxed);
            scanning.store(false, Ordering::SeqCst);
            dirty.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        })
        .ok();
}

impl eframe::App for FindApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "windows")]
        self.capture_hwnd(_frame);
        self.handle_tray(ctx);
        if let Some(uri) = self.pending_forget.take() {
            ctx.forget_image(&uri);
        }

        // Drain search responses, keeping the newest. Accept anything newer
        // than what's displayed — requiring an exact generation match starves
        // the UI while the index is being built, because background flushes
        // bump the generation faster than searches complete.
        let mut fresh: Option<SearchResponse> = None;
        while let Ok(res) = self.res_rx.try_recv() {
            if res.generation > self.displayed_generation
                && fresh.as_ref().is_none_or(|f| res.generation > f.generation)
            {
                fresh = Some(res);
            }
        }
        if let Some(res) = fresh {
            // Selection follows the entry, not the row number: background
            // refreshes (indexing, watcher) must never steal the user's spot.
            let selected_entry = self
                .selected
                .and_then(|s| self.results.get(s))
                .map(|h| h.idx);
            self.displayed_generation = res.generation;
            self.results = res.hits;
            self.total = res.total;
            self.truncated = res.truncated;
            self.search_ms = res.elapsed_ms;
            if self.sort != SortKey::Relevance {
                self.sort_results();
            }
            self.selected = selected_entry
                .and_then(|key| self.results.iter().position(|h| h.idx == key));
        }

        let scanning = self.scanning.load(Ordering::Relaxed);

        // Watcher / scan progress: rerun the current query so new files show
        // up, but gently — every rerun replaces and re-sorts the visible
        // results, so doing it fast reads as stutter. While a scan is
        // running results refresh every 2.5s; for watcher events when idle,
        // after 500ms. Typing always searches immediately (separate path).
        let refresh_interval = if scanning { 2500 } else { 500 };
        let refresh_due = self
            .last_dirty_refresh
            .is_none_or(|t| t.elapsed().as_millis() >= refresh_interval);
        if refresh_due && self.dirty.swap(false, Ordering::Relaxed) {
            self.last_dirty_refresh = Some(Instant::now());
            // While scanning, never touch the index lock from the UI thread —
            // a queued writer behind a long reader would freeze the window.
            self.index_count = if scanning {
                self.scan_progress.load(Ordering::Relaxed)
            } else {
                self.index
                    .read()
                    .map(|i| i.live_count())
                    .unwrap_or(self.index_count)
            };
            self.resend_search();
        } else if self.dirty.load(Ordering::Relaxed) && !self.hidden {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        // Don't schedule repaints while hidden in the tray — a hidden window
        // can't paint, and the pending repaint request spins the CPU.
        if scanning && !self.hidden {
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

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Normal window close (tray disabled or non-Windows): stop the scan
        // and persist whatever is indexed so the next launch starts warm.
        self.scan_cancel.store(true, Ordering::Relaxed);
        if let Ok(guard) = self.index.read() {
            if !guard.entries.is_empty() {
                let _ = index::save_to_disk(&guard);
            }
        }
        #[cfg(target_os = "windows")]
        {
            self.tray = None;
        }
    }
}

impl FindApp {
    /// Save whatever is indexed so the next launch starts warm, remove the
    /// tray icon (otherwise Windows leaves a ghost icon), and exit for real.
    /// A plain window-close can silently fail when the window is hidden in
    /// the tray, so Quit must not depend on the event loop winding down.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    fn quit_now(&mut self) {
        self.quit_requested = true;
        self.scan_cancel.store(true, Ordering::Relaxed);
        if let Ok(guard) = self.index.read() {
            if !guard.entries.is_empty() {
                let _ = index::save_to_disk(&guard);
            }
        }
        #[cfg(target_os = "windows")]
        {
            self.tray = None;
        }
        std::process::exit(0);
    }

    /// Remember the native window handle so tray events can restore the
    /// window even while the egui update loop is asleep (hidden windows
    /// receive no paint events, so the loop can't do it itself).
    #[cfg(target_os = "windows")]
    fn capture_hwnd(&self, frame: &eframe::Frame) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if self.hwnd.load(Ordering::Relaxed) != 0 {
            return;
        }
        if let Ok(handle) = frame.window_handle() {
            if let RawWindowHandle::Win32(win) = handle.as_raw() {
                self.hwnd.store(win.hwnd.get(), Ordering::Relaxed);
            }
        }
    }

    /// Tray events + close-to-tray behavior. No-op outside Windows.
    fn handle_tray(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "windows")]
        {
            let mut msgs = Vec::new();
            if let Some(tray) = &self.tray {
                while let Ok(msg) = tray.rx.try_recv() {
                    msgs.push(msg);
                }
            }
            for msg in msgs {
                match msg {
                    crate::tray::TrayMsg::Show => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                        self.hidden = false;
                    }
                    crate::tray::TrayMsg::Rescan => {
                        self.hidden = false;
                        self.start_scan(ctx);
                    }
                    crate::tray::TrayMsg::Quit => self.quit_now(),
                }
            }
            if self.settings.minimize_to_tray
                && self.tray.is_some()
                && !self.quit_requested
                && ctx.input(|i| i.viewport().close_requested())
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.hidden = true;
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = ctx;
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        // Distinct toolbar tint with an accent edge, so the app's toolbar
        // reads clearly against the OS title bar above it.
        let t = theme_of(&self.settings);
        let frame = egui::Frame::default()
            .fill(t.bar)
            .inner_margin(egui::Margin::symmetric(8, 4))
            .stroke(egui::Stroke::new(1.0, t.accent.gamma_multiply(0.6)));
        egui::TopBottomPanel::top("top").frame(frame).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Search everything…  (try: report ext:pdf size:>1mb  or  content:\"todo\" ext:rs)")
                        .font(egui::FontId::proportional(19.0))
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
                    self.persist_settings();
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
                    self.persist_settings();
                    self.send_search();
                }

                if ui
                    .selectable_label(self.settings.show_preview, "👁 Preview")
                    .clicked()
                {
                    self.settings.show_preview = !self.settings.show_preview;
                    self.persist_settings();
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
        let frame = egui::Frame::default()
            .fill(theme_of(&self.settings).bar)
            .inner_margin(egui::Margin::symmetric(8, 4));
        egui::TopBottomPanel::bottom("status").frame(frame).show(ctx, |ui| {
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
                    if ui
                        .button("\u{f021} Rescan")
                        .on_hover_text(
                            "Re-walks every indexed location from scratch and replaces \
                             the index. Searching stays available the whole time.",
                        )
                        .clicked()
                    {
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
                    PreviewContent::Image { uri, bytes } => {
                        egui::ScrollArea::both().show(ui, |ui| {
                            ui.add(
                                egui::Image::from_bytes(
                                    uri.clone(),
                                    egui::load::Bytes::Shared(bytes.clone()),
                                )
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
                                    egui::RichText::new(text.as_str()).monospace().size(13.5),
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
        // Splash: shown until the first index (cached or fresh) is available.
        if self.index_count == 0 && self.results.is_empty() && self.query.is_empty() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        let avail = ui.available_height();
                        ui.add_space((avail * 0.18).max(0.0));
                        ui.add(
                            egui::Image::new(egui::ImageSource::Bytes {
                                uri: "bytes://splash.png".into(),
                                bytes: egui::load::Bytes::Static(include_bytes!(
                                    "../assets/splash.png"
                                )),
                            })
                            .max_height(avail * 0.55)
                            .maintain_aspect_ratio(true),
                        );
                        ui.add_space(12.0);
                        if self.scanning.load(Ordering::Relaxed) {
                            ui.horizontal(|ui| {
                                ui.add_space(ui.available_width() / 2.0 - 90.0);
                                ui.spinner();
                                ui.label(format!(
                                    "Indexing your drives… {}",
                                    thousands(self.scan_progress.load(Ordering::Relaxed))
                                ));
                            });
                        } else {
                            ui.label("Loading index…");
                        }
                    });
                });
            });
            return;
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            use egui_extras::{Column, TableBuilder};

            let text_height = 26.0;
            let mut clicked_row: Option<usize> = None;
            let mut double_clicked: Option<usize> = None;
            let mut context_action: Option<(usize, RowAction)> = None;

            let has_content = self.results.iter().any(|h| h.content_line.is_some());

            let mut table = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click());
            if self.scroll_to_selected {
                if let Some(sel) = self.selected {
                    table = table.scroll_to_row(sel, None);
                }
                self.scroll_to_selected = false;
            }
            let mut table = table
                .column(Column::initial(320.0).at_least(120.0).clip(true))
                .column(Column::remainder().at_least(150.0).clip(true))
                .column(Column::initial(90.0).at_least(60.0))
                .column(Column::initial(130.0).at_least(90.0));
            if has_content {
                table = table.column(Column::initial(260.0).at_least(100.0).clip(true));
            }

            table
                .header(26.0, |mut header| {
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
                            let (icon, color) =
                                if hit.is_dir { FOLDER_ICON } else { file_icon(&hit.name) };
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(icon).color(color),
                                    )
                                    .selectable(false),
                                );
                                ui.add(
                                    egui::Label::new(&hit.name)
                                        .truncate()
                                        .selectable(false),
                                );
                            });
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
                                                .size(12.5),
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
                            if ui.button("Open File").clicked() {
                                context_action = Some((i, RowAction::Open));
                                ui.close();
                            }
                            if ui.button("Open Location").clicked() {
                                context_action = Some((i, RowAction::Reveal));
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Copy Path").clicked() {
                                context_action = Some((i, RowAction::CopyPath));
                                ui.close();
                            }
                            if ui.button("Copy Name").clicked() {
                                context_action = Some((i, RowAction::CopyName));
                                ui.close();
                            }
                            if ui.button("Copy Folder Path").clicked() {
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
                // --- Appearance ---
                let mut theme_changed = false;
                ui.horizontal(|ui| {
                    ui.label("Theme:");
                    for preset in find_core::settings::ThemePreset::ALL {
                        if ui
                            .selectable_label(self.settings.theme == preset, preset.label())
                            .clicked()
                        {
                            self.settings.theme = preset;
                            theme_changed = true;
                        }
                    }
                    ui.separator();
                    ui.label("Accent:");
                    for accent in find_core::settings::AccentColor::ALL {
                        if ui
                            .selectable_label(self.settings.accent == accent, accent.label())
                            .clicked()
                        {
                            self.settings.accent = accent;
                            theme_changed = true;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Text size:");
                    let mut scale = self.settings.ui_scale;
                    if ui
                        .add(
                            egui::Slider::new(&mut scale, 0.8..=2.0)
                                .step_by(0.05)
                                .show_value(false),
                        )
                        .changed()
                    {
                        self.settings.ui_scale = scale;
                        theme_changed = true;
                    }
                    ui.label(format!("{:.0}%", self.settings.ui_scale * 100.0));
                    ui.label(
                        egui::RichText::new("(or Ctrl+= / Ctrl+-)")
                            .small()
                            .weak(),
                    );
                });
                if theme_changed {
                    apply_theme(ctx, &self.settings);
                    ctx.set_zoom_factor(self.settings.ui_scale);
                    self.persist_settings();
                }
                ui.separator();

                ui.label("Indexed locations (one per line):");
                // Per-root scan status, so you can see what's actually indexed.
                {
                    let completed = self
                        .completed_roots
                        .lock()
                        .map(|c| c.clone())
                        .unwrap_or_default();
                    let scanning = self.scanning.load(Ordering::Relaxed);
                    for line in self
                        .settings_roots_draft
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                    {
                        let path = std::path::PathBuf::from(line);
                        let (icon, color, note) = if completed.contains(&path) {
                            ("\u{f00c}", egui::Color32::from_rgb(120, 200, 130), "indexed")
                        } else if scanning {
                            ("\u{f254}", egui::Color32::from_rgb(220, 180, 90), "indexing…")
                        } else {
                            ("\u{f071}", egui::Color32::from_rgb(200, 120, 110), "not indexed yet")
                        };
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(icon).color(color));
                            ui.label(line);
                            ui.label(egui::RichText::new(note).small().weak());
                        });
                    }
                }
                egui::ScrollArea::vertical()
                    .id_salt("roots_scroll")
                    .max_height(110.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.settings_roots_draft)
                                .desired_rows(4)
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Exclude paths containing (one per line):");
                    if ui
                        .button("Restore defaults")
                        .on_hover_text(
                            "Reset to the built-in noise list (node_modules, venvs, \
                             conda, docker, caches...). Click Save & Rescan to apply.",
                        )
                        .clicked()
                    {
                        self.settings_exclusions_draft =
                            find_core::util::default_exclusions().join("\n");
                    }
                });
                egui::ScrollArea::vertical()
                    .id_salt("exclusions_scroll")
                    .max_height(220.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.settings_exclusions_draft)
                                .desired_rows(6)
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                    });
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
                #[cfg(target_os = "windows")]
                ui.checkbox(
                    &mut self.settings.minimize_to_tray,
                    "Keep running in the system tray when the window is closed",
                );
                ui.horizontal(|ui| {
                    ui.label("Daily rescan at:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.auto_rescan_time)
                            .hint_text("04:00")
                            .desired_width(70.0),
                    );
                    ui.label(
                        egui::RichText::new(
                            "24h time, blank = off. Runs while FIND sits in the tray.",
                        )
                        .small()
                        .weak(),
                    );
                });
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
        // Normalize the rescan time ("4:00" -> "04:00"; invalid -> off).
        let t = self.settings.auto_rescan_time.trim().to_string();
        self.settings.auto_rescan_time = normalize_hhmm(&t).unwrap_or_default();
        self.persist_settings();
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
                     content:\"foo bar\"   search inside files — plain text,\n\
                                         and PDF, DOCX, PPTX, XLSX, ODF too\n\
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
            self.scroll_to_selected = true;
        }
        if up && !self.results.is_empty() {
            let prev = self.selected.map(|s| s.saturating_sub(1)).unwrap_or(0);
            self.select(prev);
            self.scroll_to_selected = true;
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

/// Nerd Font glyph + color per file kind (the colorful icons in the list).
fn file_icon(name: &str) -> (&'static str, egui::Color32) {
    use egui::Color32;
    if Category::Images.matches(name, false) {
        ("\u{f1c5}", Color32::from_rgb(198, 120, 221)) // violet
    } else if Category::Audio.matches(name, false) {
        ("\u{f001}", Color32::from_rgb(152, 195, 121)) // green
    } else if Category::Video.matches(name, false) {
        ("\u{f03d}", Color32::from_rgb(224, 108, 117)) // red
    } else if Category::Archives.matches(name, false) {
        ("\u{f1c6}", Color32::from_rgb(176, 137, 104)) // tan
    } else if Category::Code.matches(name, false) {
        ("\u{f121}", Color32::from_rgb(97, 175, 239)) // blue
    } else if Category::Executables.matches(name, false) {
        ("\u{f013}", Color32::from_rgb(229, 192, 123)) // gold
    } else if Category::Documents.matches(name, false) {
        ("\u{f15c}", Color32::from_rgb(171, 178, 191)) // gray
    } else {
        ("\u{f15b}", Color32::from_rgb(140, 146, 160)) // dim gray
    }
}

const FOLDER_ICON: (&str, egui::Color32) = ("\u{f07b}", egui::Color32::from_rgb(229, 192, 123));
