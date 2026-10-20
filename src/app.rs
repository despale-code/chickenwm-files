//! The chickenwm-files window: tabs, toolbar with breadcrumbs, sidebar, the Details / Icons /
//! Columns / Dual-pane views, context menus, drag and drop, and the status bar.
//!
//! UI code never mutates panes directly while drawing them; it records `Action`s which are
//! applied after the frame's layout, so every view (and every menu) shares one implementation
//! of each command.

mod preview;
mod terminal;
mod tools;

use crate::icons::IconCache;
use crate::model::{
    archive_stem, compress_zip, extract_archive, format_bytes, format_time, is_archive, read_directory, Favorite,
    text_preview, unique_path, Entry, Pane, Prefs, SortKey, ViewMode,
};
use crate::shell::{self, FileOp};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use chickenwm_core::config::{config_dir, RgbaColor};
use chickenwm_core::css::CssStyleSheet;
use notify::{RecursiveMode, Watcher};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_INSERT, VK_SHIFT};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Side {
    Main,
    Second,
}

#[derive(Clone)]
struct Marquee {
    side: Side,
    start: Pos2,
    current: Pos2,
    initial_selection: std::collections::HashSet<PathBuf>,
}

#[derive(Clone)]
struct CompressPrompt {
    paths: Vec<PathBuf>,
    dir: PathBuf,
    name_buffer: String,
    initial_focus: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropKind {
    Copy,
    Move,
    Shortcut,
}

enum Action {
    /// Folder: browse into it (in that pane). File: open with its default app.
    Open(Side, PathBuf),
    OpenNewTab(PathBuf),
    Navigate(Side, PathBuf),
    Click { side: Side, pos: usize, ctrl: bool, shift: bool },
    /// Right-click on an item: select it unless it's already part of the selection.
    ContextSelect { side: Side, pos: usize },
    ClearSelection(Side),
    Focus(Side),
    Copy(Vec<PathBuf>),
    Cut(Vec<PathBuf>),
    Paste(PathBuf),
    StartRename(PathBuf),
    Recycle(Vec<PathBuf>),
    Delete(Vec<PathBuf>),
    NewFolder(PathBuf),
    NewTextFile(PathBuf),
    CopyPath(Vec<PathBuf>),
    OpenWith(PathBuf),
    RunAsAdmin(PathBuf),
    Properties(Vec<PathBuf>),
    NativeMenu(Vec<PathBuf>),
    Extract { archive: PathBuf, into_folder: bool },
    Compress(Vec<PathBuf>),
    ExecuteCompress { paths: Vec<PathBuf>, zip_path: PathBuf },
    Shortcut(Vec<PathBuf>),
    Terminal(PathBuf),
    /// Add a folder to the sidebar's Favorites.
    Pin(PathBuf),
    PinAt { path: PathBuf, index: usize },
    /// Remove it from Favorites.
    Unpin(PathBuf),
    MoveFavorite { from: usize, to: usize },
    /// `None` restores the folder's own name.
    RenameFavorite { path: PathBuf, name: Option<String> },
    ResetFavorites,
    Transfer { sources: Vec<PathBuf>, dest: PathBuf, kind: DropKind },
    Sort(SortKey),
    SortDesc(bool),
    SetView(ViewMode),
    ToggleHidden,
    TogglePreview,
    Refresh,
    NewTab,
    CloseTab(usize),
    #[allow(dead_code)]
    DuplicateTab(usize),
    #[allow(dead_code)]
    CloseOtherTabs(usize),
    SwitchTab(usize),
    Back(Side),
    Forward(Side),
    Up(Side),
    BeginDrag {
        paths: Vec<PathBuf>,
        secondary: bool,
        origin_rect: Rect,
        grab_offset: Vec2,
        icon_key: String,
        is_dir: bool,
        name: String,
    },
    EditAddress,
    FocusSearch,
    /// Several items: rename them together (find / replace, numbering).
    BatchRename(Vec<PathBuf>),
    /// Count these folders' sizes (shown in the Size column).
    FolderSize(Vec<PathBuf>),
    DiskUsage(PathBuf),
    BrowseArchive(PathBuf),
    GoAnywhere,
    Undo,
    ToggleTerminal,
    ToggleDualPane,
    SetIconScale(f32),
    SetSelection(Side, std::collections::HashSet<PathBuf>),
}

/// Colors and metrics from `files.css`.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct Theme {
    bg: Color32,
    pane: Color32,
    sidebar: Color32,
    fg: Color32,
    dim: Color32,
    dir: Color32,
    exec: Color32,
    archive: Color32,
    media: Color32,
    accent: Color32,
    selected: Color32,
    border: Color32,
    radius: u8,
    row_h: f32,
    font: f32,
}

fn c(rgba: RgbaColor) -> Color32 {
    Color32::from_rgba_unmultiplied(rgba.r, rgba.g, rgba.b, rgba.a)
}

fn rgb(r: u8, g: u8, b: u8) -> RgbaColor {
    RgbaColor { r, g, b, a: 255 }
}

impl Theme {
    pub fn hairline(&self) -> Color32 {
        self.border.gamma_multiply(0.65)
    }
    fn from_css(sheet: &CssStyleSheet) -> Self {
        let fg = c(sheet.var_color("--fg-color", rgb(205, 214, 244)));
        let subtext = c(sheet.var_color("--subtext-color", rgb(174, 174, 163)));
        let dim = if subtext != fg { subtext } else { fg.gamma_multiply(0.65) };
        let accent = c(sheet.var_color("--accent-color", rgb(51, 204, 255)));
        Self {
            bg: c(sheet.var_color("--bg-color", rgb(24, 24, 37))),
            pane: c(sheet.var_color("--pane-bg", rgb(30, 30, 46))),
            sidebar: c(sheet.var_color("--sidebar-bg", rgb(17, 17, 27))),
            fg,
            dim,
            dir: c(sheet.var_color("--dir-color", rgb(137, 180, 250))),
            exec: c(sheet.var_color("--exec-color", rgb(166, 227, 161))),
            archive: c(sheet.var_color("--archive-color", rgb(249, 226, 175))),
            media: c(sheet.var_color("--media-color", rgb(203, 166, 247))),
            accent,
            selected: c(sheet.var_color(
                "--selected-bg",
                RgbaColor { r: 51, g: 204, b: 255, a: 56 },
            )),
            border: {
                let mut b = c(sheet.var_color(
                    "--border-color",
                    RgbaColor { r: 255, g: 255, b: 255, a: 22 },
                ));
                if b.a() == 255 {
                    b = b.gamma_multiply(0.4);
                }
                b
            },
            radius: sheet.var_px("--border-radius", 8.0).clamp(0.0, 16.0) as u8,
            row_h: sheet.var_px("--row-height", 28.0).clamp(20.0, 48.0),
            font: sheet.var_px("--font-size", 14.0).clamp(10.0, 22.0),
        }
    }

    fn name_color(&self, e: &Entry) -> Color32 {
        if e.hidden {
            self.dim
        } else {
            self.fg
        }
    }

    fn apply(&self, ctx: &egui::Context) {
        let [br, bgc, bb, _] = self.bg.to_array();
        let light = (0.2126 * br as f32 + 0.7152 * bgc as f32 + 0.0722 * bb as f32) / 255.0 > 0.5;
        // Pin egui's theme to the palette: otherwise it follows the OS and its per-theme style
        // slot (and `dark_mode` flag) can disagree with our colours, leaving the app stuck dark.
        ctx.set_theme(if light { egui::Theme::Light } else { egui::Theme::Dark });
        let mut v = if light { egui::Visuals::light() } else { egui::Visuals::dark() };
        v.panel_fill = self.bg;
        // Acrylic frosted-glass translucent background for context menus and popups
        let menu_bg = Color32::from_rgba_unmultiplied(self.sidebar.r(), self.sidebar.g(), self.sidebar.b(), 180);
        v.window_fill = menu_bg;
        v.extreme_bg_color = self.sidebar;
        v.faint_bg_color = self.pane;
        v.override_text_color = Some(self.fg);
        v.selection.bg_fill = self.accent.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0_f32, self.accent);
        v.hyperlink_color = self.accent;
        v.window_stroke = Stroke::new(1.0_f32, self.hairline());
        v.window_corner_radius = CornerRadius::same(self.radius);
        v.menu_corner_radius = CornerRadius::same(self.radius.min(8));
        v.popup_shadow = egui::epaint::Shadow {
            offset: [0, 8],
            blur: 20,
            spread: 0,
            color: Color32::from_black_alpha(100),
        };
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(self.radius.min(6));
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, self.border);
        v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        v.widgets.inactive.bg_fill = self.pane;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, self.hairline());
        v.widgets.hovered.weak_bg_fill = self.accent.gamma_multiply(0.15);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, self.accent.gamma_multiply(0.5));
        v.widgets.active.weak_bg_fill = self.accent.gamma_multiply(0.3);
        ctx.set_visuals(v);
        ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(6.0, 4.0);
            s.spacing.button_padding = egui::vec2(8.0, 4.0);
            s.spacing.menu_margin = egui::Margin::same(6);
            for (style, id) in s.text_styles.iter_mut() {
                if matches!(style, egui::TextStyle::Body | egui::TextStyle::Button) {
                    id.size = self.font;
                }
            }
        });
    }
}

struct Rename {
    path: PathBuf,
    buffer: String,
    just_started: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProcessStatus {
    Running,
    Completed,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct ProcessItem {
    pub id: u64,
    pub title: String,
    pub detail: String,
    pub progress: Option<f32>,
    pub status: ProcessStatus,
    pub started: Instant,
    pub finished: Option<Instant>,
}

pub struct ProcessManager {
    next_id: AtomicU64,
    items: Mutex<Vec<ProcessItem>>,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            items: Mutex::new(Vec::new()),
        }
    }

    pub fn add(&self, title: String, detail: String) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut list = self.items.lock();
        list.insert(0, ProcessItem {
            id,
            title,
            detail,
            progress: None,
            status: ProcessStatus::Running,
            started: Instant::now(),
            finished: None,
        });
        if list.len() > 25 {
            list.truncate(25);
        }
        id
    }

    pub fn update_progress(&self, id: u64, progress: Option<f32>) {
        let mut list = self.items.lock();
        if let Some(item) = list.iter_mut().find(|it| it.id == id) {
            item.progress = progress;
        }
    }

    pub fn finish(&self, id: u64, result: Result<String, String>) {
        let mut list = self.items.lock();
        if let Some(item) = list.iter_mut().find(|it| it.id == id) {
            item.finished = Some(Instant::now());
            item.progress = Some(1.0);
            match result {
                Ok(_) => item.status = ProcessStatus::Completed,
                Err(err) => item.status = ProcessStatus::Failed(err),
            }
        }
    }

    pub fn clear_finished(&self) {
        let mut list = self.items.lock();
        list.retain(|it| it.status == ProcessStatus::Running);
    }

    pub fn active(&self) -> Vec<ProcessItem> {
        self.items.lock().iter().filter(|it| it.status == ProcessStatus::Running).cloned().collect()
    }

    pub fn all(&self) -> Vec<ProcessItem> {
        self.items.lock().clone()
    }
}

#[derive(Clone, Debug)]
pub struct ArchiveDrag {
    pub archive: PathBuf,
    pub member: String,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub origin_rect: Rect,
    pub grab_offset: Vec2,
}

#[derive(Clone, Debug)]
pub struct ExportFolderPrompt {
    pub archive: PathBuf,
    pub dest_dir: PathBuf,
    pub folder_name: String,
    pub initial_focus: bool,
}

#[derive(Clone, Debug)]
pub struct ExportHerePrompt {
    pub archive: PathBuf,
    pub dest_dir: PathBuf,
    pub folder_name: String,
    pub initial_focus: bool,
}

struct Drag {
    paths: Vec<PathBuf>,
    secondary: bool,
    origin_rect: Rect,
    grab_offset: Vec2,
    icon_key: String,
    is_dir: bool,
    name: String,
}

struct SnapBackAnim {
    path: PathBuf,
    icon_key: String,
    is_dir: bool,
    name: String,
    start_rect: Rect,
    target_rect: Rect,
    start_time: Instant,
    duration: Duration,
}

/// A place something can be dropped on, collected while drawing each frame.
struct DropZone {
    rect: Rect,
    dir: PathBuf,
}

struct DropMenu {
    pos: Pos2,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    opened: Instant,
}

/// Background file operations (the shell shows its own progress dialog for copy/move/delete).
struct Jobs {
    running: AtomicUsize,
    status: Mutex<String>,
    progress: Mutex<Option<f32>>,
}

struct DriveInfo {
    root: PathBuf,
    label: String,
    space: Option<(u64, u64)>,
}

#[derive(Default)]
struct Preview {
    path: Option<PathBuf>,
    pending: Option<(Entry, Instant)>,
    cancel: Arc<AtomicBool>,
    text: String,
    image: Option<(egui::TextureHandle, [usize; 2])>,
    loading_image: bool,
    kind: preview::Kind,
}

struct ScrollMemo {
    offset: f32,
    viewport: f32,
}

pub struct FilesApp {
    ctx: egui::Context,
    tabs: Vec<Pane>,
    active: usize,
    second: Pane,
    focus: Side,
    prefs: Prefs,
    css: Arc<RwLock<CssStyleSheet>>,
    /// Keeps the UI font in step with `--font-family` in files.css.
    font: chickenwm_core::fonts::EguiFont,
    theme: Option<Theme>,
    target_fps: u32,
    icons: IconCache,
    drives: Vec<DriveInfo>,
    drives_at: Instant,
    rename: Option<Rename>,
    address: Option<String>,
    focus_search: bool,
    drag: Option<Drag>,
    marquee: Option<Marquee>,
    compress_prompt: Option<CompressPrompt>,
    export_folder_prompt: Option<ExportFolderPrompt>,
    export_here_prompt: Option<ExportHerePrompt>,
    pending_focus: Option<(Side, PathBuf)>,
    zones: Vec<DropZone>,
    drop_menu: Option<DropMenu>,
    fav_drop_hover: bool,
    snap_back: Option<SnapBackAnim>,
    archive_drag: Option<ArchiveDrag>,
    archive_snap_back: Option<SnapBackAnim>,
    jobs: Arc<Jobs>,
    processes: Arc<ProcessManager>,
    show_processes: bool,
    dirty: Arc<AtomicBool>,
    last_reload: Instant,
    watcher: Option<notify::RecommendedWatcher>,
    watched: Vec<PathBuf>,
    hwnd: HWND,
    pending_key: Option<char>,
    paste_down: bool,
    toast: Option<(String, Instant, bool)>,
    preview: Preview,
    image_tx: Sender<(PathBuf, Option<egui::ColorImage>, [usize; 2])>,
    image_rx: Receiver<(PathBuf, Option<egui::ColorImage>, [usize; 2])>,
    scroll: HashMap<Side, ScrollMemo>,
    icon_cols: usize,
    parent_cache: (PathBuf, Vec<Entry>),
    /// Columns view: contents of the folder under the cursor.
    child_cache: (PathBuf, Vec<Entry>),
    title: String,
    /// Set when chickenwm.conf changes (icon theme may have changed).
    config_changed: Arc<AtomicBool>,
    /// This app's light/dark choice in the master theme provider (`app_modes = files:…`).
    app_mode: chickenwm_core::theme::AppMode,
    _config_watcher: Option<notify::RecommendedWatcher>,
    /// Favorite being dragged to a new place in the sidebar (its index).
    fav_drag: Option<usize>,
    /// Favorite being renamed inline: (folder, text, focus requested yet).
    fav_rename: Option<(PathBuf, String, bool)>,
    /// Sidebar sections the user folded away.
    sidebar_collapsed: std::collections::HashSet<&'static str>,
    /// Batch rename, disk usage, archives, git badges, go-anywhere, undo.
    tools: tools::Tools,
    pub terminal: terminal::EmbeddedTerminal,
}

fn hwnd_of(frame: &eframe::Frame) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as _)),
        _ => None,
    }
}

fn key_down(vk: u16) -> bool {
    unsafe { GetKeyState(vk as i32) < 0 }
}

/// Explorer's default for a plain drag: move within a volume, copy across volumes.
fn default_drop_kind(sources: &[PathBuf], dest: &Path, mods: egui::Modifiers) -> DropKind {
    if mods.alt {
        return DropKind::Shortcut;
    }
    if mods.ctrl {
        return DropKind::Copy;
    }
    if mods.shift {
        return DropKind::Move;
    }
    let root = |p: &Path| p.components().next().map(|c| c.as_os_str().to_ascii_lowercase());
    if sources.iter().all(|s| root(s) == root(dest)) {
        DropKind::Move
    } else {
        DropKind::Copy
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn quick_access() -> Vec<(String, PathBuf)> {
    let mut v = Vec::new();
    let mut add = |name: &str, p: Option<PathBuf>| {
        if let Some(p) = p.filter(|p| p.is_dir()) {
            v.push((name.to_string(), p));
        }
    };
    add("Home", dirs::home_dir());
    add("Desktop", dirs::desktop_dir());
    add("Downloads", dirs::download_dir());
    add("Documents", dirs::document_dir());
    add("Pictures", dirs::picture_dir());
    add("Music", dirs::audio_dir());
    add("Videos", dirs::video_dir());
    add("ChickenWM config", Some(config_dir()));
    v
}

/// The standard folders as Favorites, keeping names that differ from the folder's own
/// (`Home`, `ChickenWM config`).
fn default_favorites() -> Vec<Favorite> {
    quick_access()
        .into_iter()
        .map(|(name, path)| {
            let custom = (name != display_name(&path)).then_some(name);
            Favorite { path, name: custom }
        })
        .collect()
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp"))
}

impl FilesApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        initial_dir: PathBuf,
        initial_select: Option<PathBuf>,
        css: Arc<RwLock<CssStyleSheet>>,
        target_fps: u32,
    ) -> Self {
        let ctx = cc.egui_ctx.clone();
        let mut prefs = Prefs::load();
        if prefs.ensure_favorites(default_favorites()) {
            prefs.save();
        }
        let mut tab = Pane::new(initial_dir.clone());
        tab.rebuild(&prefs);
        if let Some(sel) = &initial_select {
            tab.select_path(sel);
        }
        let mut second = Pane::new(initial_dir.clone());
        second.rebuild(&prefs);

        let dirty = Arc::new(AtomicBool::new(false));
        let watcher = {
            let dirty = dirty.clone();
            let ctx = ctx.clone();
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if res.is_ok() {
                    dirty.store(true, Ordering::Relaxed);
                    ctx.request_repaint();
                }
            })
            .ok()
        };
        let (image_tx, image_rx) = mpsc::channel();
        let config_changed = Arc::new(AtomicBool::new(false));
        let config_watcher = {
            let flag = config_changed.clone();
            let ctx = ctx.clone();
            chickenwm_core::css::spawn_config_watcher(move |ev| {
                match ev {
                    chickenwm_core::css::ConfigWatchEvent::HyprConfChanged(_) => {
                        flag.store(true, Ordering::Relaxed);
                        ctx.request_repaint();
                    }
                    // main.rs reloads the sheet on its own watcher thread; wake the UI now and
                    // again shortly after so the new palette is picked up even if we raced it.
                    chickenwm_core::css::ConfigWatchEvent::CssChanged { file_name, .. }
                        if file_name.eq_ignore_ascii_case("files.css") =>
                    {
                        ctx.request_repaint();
                        ctx.request_repaint_after(Duration::from_millis(150));
                    }
                    _ => {}
                }
            })
            .ok()
        };
        let mut app = Self {
            icons: IconCache::new(ctx.clone()),
            ctx,
            tabs: vec![tab],
            active: 0,
            second,
            focus: Side::Main,
            prefs,
            css,
            font: Default::default(),
            theme: None,
            target_fps: target_fps.max(60),
            drives: Vec::new(),
            drives_at: Instant::now(),
            rename: None,
            address: None,
            focus_search: false,
            drag: None,
            marquee: None,
            compress_prompt: None,
            export_folder_prompt: None,
            export_here_prompt: None,
            pending_focus: None,
            zones: Vec::new(),
            drop_menu: None,
            fav_drop_hover: false,
            snap_back: None,
            archive_drag: None,
            archive_snap_back: None,
            jobs: Arc::new(Jobs {
                running: AtomicUsize::new(0),
                status: Mutex::new(String::new()),
                progress: Mutex::new(None),
            }),
            processes: Arc::new(ProcessManager::new()),
            show_processes: false,
            dirty,
            last_reload: Instant::now(),
            watcher,
            watched: Vec::new(),
            hwnd: HWND::default(),
            pending_key: None,
            paste_down: false,
            toast: None,
            preview: Preview::default(),
            image_tx,
            image_rx,
            scroll: HashMap::new(),
            icon_cols: 1,
            parent_cache: (PathBuf::new(), Vec::new()),
            child_cache: (PathBuf::new(), Vec::new()),
            title: String::new(),
            config_changed,
            app_mode: chickenwm_core::config::load_or_init_config()
                .map(|(_, c)| c.theme.app_mode("files"))
                .unwrap_or(chickenwm_core::theme::AppMode::System),
            _config_watcher: config_watcher,
            fav_drag: None,
            fav_rename: None,
            sidebar_collapsed: Default::default(),
            tools: Default::default(),
            terminal: terminal::EmbeddedTerminal::new(initial_dir.clone()),
        };
        app.refresh_drives();
        if tools::is_browsable_archive(&display_name(&initial_dir)) {
            app.tools.archive = Some(tools::ArchiveView::open(initial_dir.clone()));
        } else if let Some(sel) = initial_select.as_ref() {
            if tools::is_browsable_archive(&display_name(sel)) {
                app.tools.archive = Some(tools::ArchiveView::open(sel.clone()));
            }
        }
        app
    }

    fn pane(&self, side: Side) -> &Pane {
        match side {
            Side::Second if self.prefs.view == ViewMode::Dual => &self.second,
            _ => &self.tabs[self.active],
        }
    }

    fn pane_mut(&mut self, side: Side) -> &mut Pane {
        match side {
            Side::Second if self.prefs.view == ViewMode::Dual => &mut self.second,
            _ => &mut self.tabs[self.active],
        }
    }

    fn focused(&self) -> Side {
        if self.prefs.view == ViewMode::Dual { self.focus } else { Side::Main }
    }

    fn toast(&mut self, msg: impl Into<String>, error: bool) {
        self.toast = Some((msg.into(), Instant::now(), error));
    }

    fn refresh_drives(&mut self) {
        self.drives = crate::enumerate_logical_drives()
            .into_iter()
            .map(|root| DriveInfo {
                label: shell::drive_label(&root),
                space: shell::disk_space(&root),
                root,
            })
            .collect();
        self.drives_at = Instant::now();
    }

    /// Re-reads every visible pane (and keeps the cursor where it was).
    fn reload_all(&mut self) {
        let prefs = self.prefs.clone();
        let tab = &mut self.tabs[self.active];
        tab.reload();
        tab.rebuild(&prefs);
        if prefs.view == ViewMode::Dual {
            self.second.reload();
            self.second.rebuild(&prefs);
        }
        self.parent_cache.0 = PathBuf::new();
        self.child_cache.0 = PathBuf::new();
        self.preview.path = None;
        self.last_reload = Instant::now();
    }

    fn rebuild_all(&mut self) {
        let prefs = self.prefs.clone();
        for t in &mut self.tabs {
            t.rebuild(&prefs);
        }
        self.second.rebuild(&prefs);
    }

    fn sync_watcher(&mut self) {
        let mut want = vec![self.tabs[self.active].cwd.clone()];
        if self.prefs.view == ViewMode::Dual && !want.contains(&self.second.cwd) {
            want.push(self.second.cwd.clone());
        }
        if want == self.watched {
            return;
        }
        if let Some(w) = &mut self.watcher {
            for p in &self.watched {
                let _ = w.unwatch(p);
            }
            for p in &want {
                let _ = w.watch(p, RecursiveMode::NonRecursive);
            }
        }
        self.watched = want;
    }

    /// Runs `work` on a worker thread; visible folders refresh and updates ProcessManager.
    fn spawn_job(
        &self,
        label: String,
        detail: String,
        work: impl FnOnce(&dyn Fn(f32)) -> Result<String, String> + Send + 'static,
    ) {
        let jobs = self.jobs.clone();
        let procs = self.processes.clone();
        let dirty = self.dirty.clone();
        let ctx = self.ctx.clone();
        let id = procs.add(label.clone(), detail);
        jobs.running.fetch_add(1, Ordering::SeqCst);
        *jobs.status.lock() = label;
        *jobs.progress.lock() = None;
        std::thread::spawn(move || {
            shell::init_com_thread();
            let report = |p: f32| {
                *jobs.progress.lock() = Some(p);
                procs.update_progress(id, Some(p));
                ctx.request_repaint();
            };
            let result = work(&report);
            *jobs.status.lock() = match &result {
                Ok(msg) => msg.clone(),
                Err(msg) => format!("⚠ {msg}"),
            };
            *jobs.progress.lock() = None;
            procs.finish(id, result);
            jobs.running.fetch_sub(1, Ordering::SeqCst);
            dirty.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        });
    }

    fn file_job(&self, op: FileOp, sources: Vec<PathBuf>, dest: Option<PathBuf>, clear_clipboard: bool) {
        let n = sources.len();
        let (verb, done) = match op {
            FileOp::Copy => ("Copying", "Copied"),
            FileOp::Move => ("Moving", "Moved"),
            FileOp::Recycle => ("Recycling", "Moved to Recycle Bin:"),
            FileOp::Delete => ("Deleting", "Deleted"),
        };
        let what = if n == 1 { display_name(&sources[0]) } else { format!("{n} items") };
        let label = format!("{verb} {what}");
        let detail = dest.as_ref().map(|d| format!("To {}", d.display())).unwrap_or_default();
        self.spawn_job(label, detail, move |_| {
            if shell::file_op(op, &sources, dest.as_deref()) {
                if clear_clipboard {
                    shell::clipboard_clear();
                }
                Ok(format!("{done} {what}"))
            } else {
                Err(format!("{verb} {what} was cancelled or failed"))
            }
        });
    }

    fn apply(&mut self, action: Action) {
        let prefs = self.prefs.clone();
        match action {
            Action::Open(side, path) => {
                if path.is_dir() {
                    self.pane_mut(side).navigate(path, &prefs);
                } else if tools::is_browsable_archive(&display_name(&path)) {
                    // Archives open like folders (in a browser window).
                    self.tools.archive = Some(tools::ArchiveView::open(path));
                } else {
                    shell::shell_open(self.hwnd, &path, None);
                }
            }
            Action::OpenNewTab(path) => {
                let mut p = Pane::new(path);
                p.rebuild(&prefs);
                self.tabs.push(p);
                self.active = self.tabs.len() - 1;
            }
            Action::Navigate(side, path) => self.pane_mut(side).navigate(path, &prefs),
            Action::Click { side, pos, ctrl, shift } => {
                self.focus = side;
                self.pane_mut(side).click(pos, ctrl, shift);
            }
            Action::ContextSelect { side, pos } => {
                self.focus = side;
                let pane = self.pane_mut(side);
                let in_sel = pane.entry_at(pos).is_some_and(|e| pane.selection.contains(&e.path));
                if !in_sel {
                    pane.click(pos, false, false);
                }
            }
            Action::ClearSelection(side) => {
                self.focus = side;
                self.pane_mut(side).selection.clear();
            }
            Action::Focus(side) => self.focus = side,
            Action::Copy(paths) | Action::Cut(paths) if paths.is_empty() => {}
            Action::Copy(paths) => {
                if shell::clipboard_set_files(&paths, false) {
                    self.toast(format!("Copied {} item(s) — paste anywhere", paths.len()), false);
                }
            }
            Action::Cut(paths) => {
                if shell::clipboard_set_files(&paths, true) {
                    self.toast(format!("Cut {} item(s) — paste to move", paths.len()), false);
                }
            }
            Action::Paste(dest) => match shell::clipboard_get_files() {
                Some((files, cut)) if !files.is_empty() => {
                    let op = if cut { FileOp::Move } else { FileOp::Copy };
                    if cut && files.iter().all(|f| f.parent() == Some(dest.as_path())) {
                        return; // moving into the folder it's already in
                    }
                    self.record_transfer(op, &files, &dest);
                    self.file_job(op, files, Some(dest), cut);
                }
                _ => self.toast("Nothing to paste", false),
            },
            Action::StartRename(path) => {
                let buffer = display_name(&path);
                self.rename = Some(Rename { path, buffer, just_started: true });
            }
            Action::Recycle(paths) if !paths.is_empty() => self.file_job(FileOp::Recycle, paths, None, false),
            Action::Delete(paths) if !paths.is_empty() => self.file_job(FileOp::Delete, paths, None, false),
            Action::Recycle(_) | Action::Delete(_) => {}
            Action::NewFolder(dir) => {
                let path = unique_path(&dir, "New folder", "");
                match std::fs::create_dir(&path) {
                    Ok(()) => self.after_create(path),
                    Err(e) => self.toast(format!("Couldn't create folder: {e}"), true),
                }
            }
            Action::NewTextFile(dir) => {
                let path = unique_path(&dir, "New Text Document", "txt");
                match std::fs::write(&path, b"") {
                    Ok(()) => self.after_create(path),
                    Err(e) => self.toast(format!("Couldn't create file: {e}"), true),
                }
            }
            Action::CopyPath(paths) => {
                let text = paths.iter().map(|p| format!("\"{}\"", p.display())).collect::<Vec<_>>().join("\r\n");
                self.ctx.copy_text(text);
                self.toast("Path copied", false);
            }
            Action::OpenWith(path) => shell::open_with(self.hwnd, &path),
            Action::RunAsAdmin(path) => shell::shell_open(self.hwnd, &path, Some("runas")),
            Action::Properties(paths) => shell::properties(self.hwnd, &paths),
            Action::NativeMenu(paths) => {
                if shell::native_context_menu(self.hwnd, &paths) {
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
            Action::Extract { archive, into_folder } => {
                let parent = archive.parent().map(Path::to_path_buf).unwrap_or_default();
                let name = display_name(&archive);
                let dest = if into_folder { unique_path(&parent, archive_stem(&name), "") } else { parent };
                let detail = format!("To {}", dest.display());
                self.spawn_job(format!("Extracting {name}"), detail, move |progress| {
                    extract_archive(&archive, &dest, progress)
                        .map(|_| format!("Extracted {name}"))
                        .map_err(|e| format!("Extract failed: {e}"))
                });
            }
            Action::Compress(paths) if !paths.is_empty() => {
                let dir = paths[0].parent().map(Path::to_path_buf).unwrap_or_default();
                let stem = if paths.len() == 1 {
                    let s = paths[0].file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    if s.is_empty() { "archive".into() } else { s }
                } else {
                    display_name(&dir)
                };
                let default_name = format!("{stem}.zip");
                self.compress_prompt = Some(CompressPrompt {
                    paths,
                    dir,
                    name_buffer: default_name,
                    initial_focus: true,
                });
            }
            Action::Compress(_) => {}
            Action::ExecuteCompress { paths, zip_path } if !paths.is_empty() => {
                let name = display_name(&zip_path);
                let dirty = self.dirty.clone();
                let target_path = zip_path.clone();
                let side = self.focused();
                self.pending_focus = Some((side, target_path));
                let detail = format!("{} items", paths.len());
                self.spawn_job(format!("Compressing to {name}"), detail, move |progress| {
                    let res = compress_zip(&paths, &zip_path, progress)
                        .map(|_| format!("Created {name}"))
                        .map_err(|e| format!("Compress failed: {e}"));
                    dirty.store(true, Ordering::Relaxed);
                    res
                });
            }
            Action::ExecuteCompress { .. } => {}
            Action::Shortcut(paths) => {
                for p in &paths {
                    let dir = p.parent().unwrap_or(p);
                    let link = unique_path(dir, &format!("{} - Shortcut", display_name(p)), "lnk");
                    if let Err(e) = shell::create_shortcut(p, &link) {
                        self.toast(format!("Couldn't create shortcut: {e}"), true);
                    }
                }
                self.dirty.store(true, Ordering::Relaxed);
            }
            Action::Terminal(dir) => {
                let spawned = std::process::Command::new("wt.exe").arg("-d").arg(&dir).spawn();
                if spawned.is_err() {
                    let _ = std::process::Command::new("powershell.exe")
                        .arg("-NoExit")
                        .current_dir(&dir)
                        .spawn();
                }
            }
            Action::Pin(path) => {
                self.prefs.add_favorite(path);
                self.prefs.save();
            }
            Action::PinAt { path, index } => {
                self.prefs.insert_favorite(path, index);
                self.prefs.save();
            }
            Action::Unpin(path) => {
                self.prefs.remove_favorite(&path);
                self.prefs.save();
            }
            Action::MoveFavorite { from, to } => {
                self.prefs.move_favorite(from, to);
                self.prefs.save();
            }
            Action::RenameFavorite { path, name } => {
                self.prefs.rename_favorite(&path, name);
                self.prefs.save();
            }
            Action::ResetFavorites => {
                self.prefs.favorites = Some(default_favorites());
                self.prefs.save();
            }
            Action::Transfer { sources, dest, kind } => {
                let sources: Vec<PathBuf> = sources
                    .into_iter()
                    .filter(|s| s != &dest && !dest.starts_with(s))
                    .collect();
                if sources.is_empty() {
                    return;
                }
                match kind {
                    DropKind::Copy => {
                        self.record_transfer(FileOp::Copy, &sources, &dest);
                        self.file_job(FileOp::Copy, sources, Some(dest), false)
                    }
                    DropKind::Move => {
                        if sources.iter().all(|s| s.parent() == Some(dest.as_path())) {
                            return;
                        }
                        self.record_transfer(FileOp::Move, &sources, &dest);
                        self.file_job(FileOp::Move, sources, Some(dest), false)
                    }
                    DropKind::Shortcut => {
                        for s in &sources {
                            let link = unique_path(&dest, &format!("{} - Shortcut", display_name(s)), "lnk");
                            if let Err(e) = shell::create_shortcut(s, &link) {
                                self.toast(format!("Couldn't create shortcut: {e}"), true);
                            }
                        }
                        self.dirty.store(true, Ordering::Relaxed);
                    }
                }
            }
            Action::Sort(key) => {
                if self.prefs.sort == key {
                    self.prefs.sort_desc = !self.prefs.sort_desc;
                } else {
                    self.prefs.sort = key;
                    self.prefs.sort_desc = matches!(key, SortKey::Modified | SortKey::Size);
                }
                self.prefs.save();
                self.rebuild_all();
            }
            Action::SortDesc(desc) => {
                self.prefs.sort_desc = desc;
                self.prefs.save();
                self.rebuild_all();
            }
            Action::SetView(view) => {
                self.prefs.view = view;
                self.prefs.save();
                let prefs = self.prefs.clone();
                if view == ViewMode::Dual {
                    self.second.reload();
                    self.second.rebuild(&prefs);
                }
            }
            Action::ToggleHidden => {
                self.prefs.show_hidden = !self.prefs.show_hidden;
                self.prefs.save();
                self.rebuild_all();
            }
            Action::TogglePreview => {
                self.prefs.show_preview = !self.prefs.show_preview;
                self.prefs.save();
            }
            Action::Refresh => {
                self.reload_all();
                self.refresh_drives();
            }
            Action::NewTab => {
                let cwd = self.tabs[self.active].cwd.clone();
                self.apply(Action::OpenNewTab(cwd));
            }
            Action::CloseTab(i) => {
                if self.tabs.len() > 1 && i < self.tabs.len() {
                    self.tabs.remove(i);
                    if self.active >= i && self.active > 0 {
                        self.active -= 1;
                    }
                    self.active = self.active.min(self.tabs.len() - 1);
                    self.reload_all();
                }
            }
            Action::DuplicateTab(i) => {
                if let Some(t) = self.tabs.get(i).cloned() {
                    self.tabs.insert(i + 1, t);
                    self.active = i + 1;
                }
            }
            Action::CloseOtherTabs(i) => {
                if let Some(t) = self.tabs.get(i).cloned() {
                    self.tabs = vec![t];
                    self.active = 0;
                }
            }
            Action::SwitchTab(i) => {
                if i < self.tabs.len() && i != self.active {
                    self.active = i;
                    self.focus = Side::Main;
                    self.reload_all();
                }
            }
            Action::Back(side) => self.pane_mut(side).go_back(&prefs),
            Action::Forward(side) => self.pane_mut(side).go_forward(&prefs),
            Action::Up(side) => self.pane_mut(side).go_up(&prefs),
            Action::BeginDrag { paths, secondary, origin_rect, grab_offset, icon_key, is_dir, name } => {
                if !paths.is_empty() {
                    self.drag = Some(Drag { paths, secondary, origin_rect, grab_offset, icon_key, is_dir, name });
                    self.snap_back = None;
                }
            }
            Action::EditAddress => {
                self.address = Some(self.pane(self.focused()).cwd.display().to_string());
            }
            Action::FocusSearch => self.focus_search = true,
            Action::BatchRename(paths) if paths.len() > 1 => self.tools.batch = Some(tools::BatchRename::new(paths)),
            Action::BatchRename(paths) => {
                if let Some(p) = paths.into_iter().next() {
                    self.apply(Action::StartRename(p));
                }
            }
            Action::FolderSize(dirs) => self.compute_folder_sizes(dirs),
            Action::DiskUsage(dir) => self.tools.usage = Some(tools::DiskUsage::start(dir, &self.ctx)),
            Action::BrowseArchive(path) => self.tools.archive = Some(tools::ArchiveView::open(path)),
            Action::GoAnywhere => self.open_go_anywhere(),
            Action::Undo => self.undo(),
            Action::ToggleTerminal => {
                self.terminal.toggle();
            }
            Action::ToggleDualPane => {
                if self.prefs.view == ViewMode::Dual {
                    self.apply(Action::SetView(ViewMode::Details));
                } else {
                    self.apply(Action::SetView(ViewMode::Dual));
                }
            }
            Action::SetIconScale(scale) => {
                self.prefs.icon_scale = scale.clamp(0.65, 2.2);
                self.prefs.save();
            }
            Action::SetSelection(side, paths) => {
                self.focus = side;
                self.pane_mut(side).selection = paths;
            }
        }
    }

    /// New folder / file: show it selected and start renaming, like Explorer.
    fn after_create(&mut self, path: PathBuf) {
        self.push_undo(tools::UndoOp::Created(vec![path.clone()]));
        let side = self.focused();
        let prefs = self.prefs.clone();
        let pane = self.pane_mut(side);
        pane.filter.clear();
        pane.reload();
        pane.rebuild(&prefs);
        pane.select_path(&path);
        self.apply(Action::StartRename(path));
    }

    fn commit_rename(&mut self) {
        let Some(r) = self.rename.take() else { return };
        let new_name = r.buffer.trim();
        if new_name.is_empty() || new_name == display_name(&r.path) {
            return;
        }
        if new_name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
            self.toast("A file name can't contain \\ / : * ? \" < > |", true);
            return;
        }
        let target = r.path.with_file_name(new_name);
        if target.exists() && !target.to_string_lossy().eq_ignore_ascii_case(&r.path.to_string_lossy()) {
            self.toast(format!("\"{new_name}\" already exists"), true);
            return;
        }
        match std::fs::rename(&r.path, &target) {
            Ok(()) => {
                self.push_undo(tools::UndoOp::Rename(vec![(r.path.clone(), target.clone())]));
                let side = self.focused();
                let prefs = self.prefs.clone();
                let pane = self.pane_mut(side);
                pane.reload();
                pane.rebuild(&prefs);
                pane.select_path(&target);
            }
            Err(e) => self.toast(format!("Rename failed: {e}"), true),
        }
    }

    /// Address bar input: a folder, a file (opens its folder with it selected), or a command.
    fn go_to_input(&mut self, input: &str) {
        let input = input.trim().trim_matches('"');
        let expanded = if let Some(rest) = input.strip_prefix('~') {
            dirs::home_dir().map(|h| h.join(rest.trim_start_matches(['\\', '/']))).unwrap_or_default()
        } else {
            PathBuf::from(input)
        };
        let side = self.focused();
        let prefs = self.prefs.clone();
        if expanded.is_dir() {
            self.pane_mut(side).navigate(expanded, &prefs);
        } else if expanded.is_file() {
            if let Some(parent) = expanded.parent().map(Path::to_path_buf) {
                let pane = self.pane_mut(side);
                pane.navigate(parent, &prefs);
                pane.select_path(&expanded);
            }
        } else if !input.is_empty() {
            self.toast(format!("Can't find \"{input}\""), true);
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        // Ctrl+V / Shift+Insert: egui only reports paste when the clipboard holds text, so the
        // key state is checked directly.
        let paste_now = ctx.input(|i| i.focused)
            && key_down(VK_CONTROL.0) && key_down(b'V' as u16)
            || key_down(VK_SHIFT.0) && key_down(VK_INSERT.0);
        let paste_edge = paste_now && !self.paste_down;
        self.paste_down = paste_now;

        if ctx.wants_keyboard_input() || self.rename.is_some() || self.address.is_some() || self.fav_rename.is_some() || self.tool_window_open() {
            return;
        }
        let side = self.focused();
        let pane = self.pane(side);
        let cwd = pane.cwd.clone();
        let targets = pane.targets();
        let current = pane.current().cloned();
        let view = self.prefs.view;
        let cols = if view == ViewMode::Icons { self.icon_cols.max(1) as isize } else { 1 };

        if paste_edge {
            actions.push(Action::Paste(cwd.clone()));
        }
        let mut moves: Vec<(isize, bool)> = Vec::new();
        ctx.input(|i| {
            let m = i.modifiers;
            for ev in &i.events {
                match ev {
                    egui::Event::Copy => actions.push(Action::Copy(targets.clone())),
                    egui::Event::Cut => actions.push(Action::Cut(targets.clone())),
                    _ => {}
                }
            }
            let plain = !m.ctrl && !m.alt;
            let pressed = |k| i.key_pressed(k);
            // Navigation
            if m.alt && pressed(egui::Key::ArrowLeft) {
                actions.push(Action::Back(side));
            } else if m.alt && pressed(egui::Key::ArrowRight) {
                actions.push(Action::Forward(side));
            } else if m.alt && pressed(egui::Key::ArrowUp) || plain && pressed(egui::Key::Backspace) {
                actions.push(Action::Up(side));
            }
            // Cursor
            if !m.alt {
                if pressed(egui::Key::ArrowDown) {
                    moves.push((cols, m.shift));
                }
                if pressed(egui::Key::ArrowUp) {
                    moves.push((-cols, m.shift));
                }
                if view == ViewMode::Icons && pressed(egui::Key::ArrowRight) {
                    moves.push((1, m.shift));
                }
                if view == ViewMode::Icons && pressed(egui::Key::ArrowLeft) {
                    moves.push((-1, m.shift));
                }
                if pressed(egui::Key::PageDown) {
                    moves.push((15 * cols, m.shift));
                }
                if pressed(egui::Key::PageUp) {
                    moves.push((-15 * cols, m.shift));
                }
                if pressed(egui::Key::Home) {
                    moves.push((isize::MIN / 2, m.shift));
                }
                if pressed(egui::Key::End) {
                    moves.push((isize::MAX / 2, m.shift));
                }
            }
            if m.alt && pressed(egui::Key::Enter) {
                actions.push(Action::Properties(if targets.is_empty() { vec![cwd.clone()] } else { targets.clone() }));
            } else if pressed(egui::Key::Enter) {
                if let Some(e) = &current {
                    actions.push(Action::Open(side, e.path.clone()));
                }
            }
            if view != ViewMode::Icons && plain && pressed(egui::Key::ArrowRight) {
                if let Some(e) = current.as_ref().filter(|e| e.is_dir) {
                    actions.push(Action::Open(side, e.path.clone()));
                }
            }
            if view != ViewMode::Icons && plain && pressed(egui::Key::ArrowLeft) {
                actions.push(Action::Up(side));
            }
            // Commands
            if pressed(egui::Key::F2) {
                if targets.len() > 1 {
                    actions.push(Action::BatchRename(targets.clone()));
                } else if let Some(e) = &current {
                    actions.push(Action::StartRename(e.path.clone()));
                }
            }
            if m.ctrl && pressed(egui::Key::Z) {
                actions.push(Action::Undo);
            }
            if m.ctrl && pressed(egui::Key::P) {
                actions.push(Action::GoAnywhere);
            }
            if pressed(egui::Key::Delete) {
                actions.push(if m.shift { Action::Delete(targets.clone()) } else { Action::Recycle(targets.clone()) });
            }
            if pressed(egui::Key::F5) || m.ctrl && pressed(egui::Key::R) {
                actions.push(Action::Refresh);
            }
            if m.ctrl && pressed(egui::Key::L) || pressed(egui::Key::F4) {
                actions.push(Action::EditAddress);
            }
            if m.ctrl && pressed(egui::Key::F) {
                actions.push(Action::FocusSearch);
            }
            if m.ctrl && m.shift && pressed(egui::Key::N) {
                actions.push(Action::NewFolder(cwd.clone()));
            }
            if m.ctrl && pressed(egui::Key::H) {
                actions.push(Action::ToggleHidden);
            }
            if m.ctrl && pressed(egui::Key::T) {
                actions.push(Action::NewTab);
            }
            if m.ctrl && pressed(egui::Key::W) {
                actions.push(Action::CloseTab(self.active));
            }
            if m.ctrl && pressed(egui::Key::Tab) {
                let n = self.tabs.len();
                let next = if m.shift { (self.active + n - 1) % n } else { (self.active + 1) % n };
                actions.push(Action::SwitchTab(next));
            } else if !m.ctrl && pressed(egui::Key::Tab) && view == ViewMode::Dual {
                actions.push(Action::Focus(if side == Side::Main { Side::Second } else { Side::Main }));
            }
            if m.ctrl && pressed(egui::Key::A) {
                // handled below (needs &mut pane)
            }
            for (k, v) in [
                (egui::Key::Num1, ViewMode::Details),
                (egui::Key::Num2, ViewMode::Icons),
                (egui::Key::Num3, ViewMode::Columns),
                (egui::Key::Num4, ViewMode::Dual),
            ] {
                if m.ctrl && m.shift && pressed(k) {
                    actions.push(Action::SetView(v));
                }
            }
            if pressed(egui::Key::F3) || (m.ctrl && !m.shift && !m.alt && pressed(egui::Key::D)) {
                actions.push(Action::ToggleDualPane);
            }
            if m.ctrl && (key_down(0xC0) || i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "`" || t == "~"))) {
                actions.push(Action::ToggleTerminal);
            }
            for (idx, key) in [
                egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4,
                egui::Key::Num5, egui::Key::Num6, egui::Key::Num7, egui::Key::Num8, egui::Key::Num9,
            ].into_iter().enumerate() {
                if m.ctrl && !m.shift && !m.alt && pressed(key) {
                    if idx < self.tabs.len() {
                        actions.push(Action::SwitchTab(idx));
                    }
                }
            }
            if pressed(egui::Key::Escape) {
                actions.push(Action::ClearSelection(side));
            }

            // Vim-style keys (no modifiers), kept from the keyboard-first design.
            if plain && !m.shift {
                if pressed(egui::Key::J) {
                    moves.push((cols, false));
                }
                if pressed(egui::Key::K) {
                    moves.push((-cols, false));
                }
                if pressed(egui::Key::H) {
                    actions.push(Action::Up(side));
                }
                if pressed(egui::Key::L) {
                    if let Some(e) = &current {
                        actions.push(Action::Open(side, e.path.clone()));
                    }
                }
                if pressed(egui::Key::Slash) {
                    actions.push(Action::FocusSearch);
                }
                if pressed(egui::Key::P) {
                    actions.push(Action::Paste(cwd.clone()));
                }
                if pressed(egui::Key::X) {
                    if let Some(e) = current.as_ref().filter(|e| is_archive(&e.name)) {
                        actions.push(Action::Extract { archive: e.path.clone(), into_folder: true });
                    }
                }
                if pressed(egui::Key::Space) {
                    if let Some(e) = &current {
                        let pos = self.pane(side).cursor;
                        let _ = e;
                        actions.push(Action::Click { side, pos, ctrl: true, shift: false });
                    }
                }
            }
            if plain && m.shift && pressed(egui::Key::G) {
                moves.push((isize::MAX / 2, false));
            }
        });

        // Two-key vim chords: gg, yy, dd, cw.
        let chord = ctx.input(|i| {
            let m = i.modifiers;
            if m.ctrl || m.alt || m.shift {
                return None;
            }
            [egui::Key::G, egui::Key::Y, egui::Key::D, egui::Key::C, egui::Key::W]
                .into_iter()
                .find(|&k| i.key_pressed(k))
        });
        if let Some(key) = chord {
            let ch = match key {
                egui::Key::G => 'g',
                egui::Key::Y => 'y',
                egui::Key::D => 'd',
                egui::Key::C => 'c',
                _ => 'w',
            };
            match (self.pending_key.take(), ch) {
                (Some('g'), 'g') => moves.push((isize::MIN / 2, false)),
                (Some('y'), 'y') => actions.push(Action::Copy(targets.clone())),
                (Some('d'), 'd') => actions.push(Action::Cut(targets.clone())),
                (Some('c'), 'w') => {
                    if let Some(e) = &current {
                        actions.push(Action::StartRename(e.path.clone()));
                    }
                }
                (_, 'w') => {}
                (_, first) => self.pending_key = Some(first),
            }
        }

        let select_all = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::A));
        let pane = self.pane_mut(side);
        if select_all {
            pane.select_all();
        }
        for (delta, shift) in moves {
            pane.move_cursor(delta, shift);
        }
    }

    fn update_preview(&mut self, ctx: &egui::Context) {
        for (path, img, dims) in self.image_rx.try_iter() {
            if self.preview.path.as_ref() == Some(&path) {
                self.preview.loading_image = false;
                self.preview.image = img.map(|img| {
                    (ctx.load_texture("preview", img, egui::TextureOptions::LINEAR), dims)
                });
            }
        }
        let current = self.pane(self.focused()).current().cloned();
        let path = current.as_ref().map(|e| e.path.clone());
        if path == self.preview.path {
            self.preview.pending = None;
            return;
        }
        let Some(entry) = current else {
            self.preview.cancel.store(true, Ordering::Relaxed);
            self.preview = Preview::default();
            return;
        };

        // Lazy load & cancel on fast scrolling: debounce rapid cursor movement by 75ms
        if self.preview.path.is_some() {
            let pending_same = self
                .preview
                .pending
                .as_ref()
                .is_some_and(|(e, _)| e.path == entry.path);
            if !pending_same {
                self.preview.cancel.store(true, Ordering::Relaxed);
                self.preview.pending = Some((entry, Instant::now()));
                ctx.request_repaint_after(Duration::from_millis(80));
                return;
            }
            if let Some((_, since)) = &self.preview.pending {
                if since.elapsed() < Duration::from_millis(75) {
                    ctx.request_repaint_after(Duration::from_millis(40));
                    return;
                }
            }
        }

        self.preview.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.preview = Preview {
            path: Some(entry.path.clone()),
            cancel: Arc::clone(&cancel),
            ..Default::default()
        };

        if !entry.is_dir && is_image(&entry.path) && entry.size < 60 * 1024 * 1024 {
            self.preview.loading_image = true;
            self.preview.kind = preview::Kind::Image;
            let tx = self.image_tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let decoded = image::open(&entry.path).ok().and_then(|img| {
                    if cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    let dims = [img.width() as usize, img.height() as usize];
                    let thumb = img.thumbnail(640, 640).to_rgba8();
                    let size = [thumb.width() as usize, thumb.height() as usize];
                    Some((egui::ColorImage::from_rgba_unmultiplied(size, thumb.as_raw()), dims))
                });
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let (img, dims) = match decoded {
                    Some((i, d)) => (Some(i), d),
                    None => (None, [0, 0]),
                };
                let _ = tx.send((entry.path, img, dims));
                ctx.request_repaint();
            });
        } else if !entry.is_dir && (is_archive(&entry.name) || tools::is_browsable_archive(&entry.name)) {
            self.preview.text = preview::archive_preview_text(&entry, &cancel);
            self.preview.kind = preview::Kind::Folder;
        } else if !entry.is_dir && (preview::is_pdf(&entry.path) || preview::is_audio_video(&entry.path)) {
            let is_pdf_file = preview::is_pdf(&entry.path);
            self.preview.kind = if is_pdf_file {
                preview::Kind::Pdf
            } else {
                preview::Kind::Media
            };
            self.preview.text = if is_pdf_file {
                preview::pdf_preview_text(&entry, &cancel)
            } else {
                preview::media_preview_text(&entry)
            };
            self.preview.loading_image = true;
            let tx = self.image_tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let thumb = preview::extract_or_synthesize_thumbnail(&entry, &cancel);
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let (img, dims) = match thumb {
                    Some((i, d)) => (Some(i), d),
                    None => (None, [0, 0]),
                };
                let _ = tx.send((entry.path, img, dims));
                ctx.request_repaint();
            });
        } else {
            self.preview.text = text_preview(&entry);
            self.preview.kind = preview::classify(&entry, &self.preview.text);
        }
    }
}

// ───────────────────────────── drawing ─────────────────────────────

/// What list drawing needs besides the pane itself.
struct Draw<'a> {
    theme: Theme,
    icons: &'a mut IconCache,
    prefs: &'a Prefs,
    rename: &'a mut Option<Rename>,
    commit_rename: &'a mut bool,
    zones: &'a mut Vec<DropZone>,
    dragging: bool,
    focused: bool,
    actions: &'a mut Vec<Action>,
    marquee: &'a mut Option<Marquee>,
    snap_back: &'a Option<SnapBackAnim>,
    drag_info: &'a Option<Drag>,
    /// Folder sizes and git badges for this frame.
    extras: &'a tools::RowExtras,
}

// ==================== MONOTONE VECTOR UI ICONS ====================

pub(super) fn paint_nav_back(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.8_f32, color);
    let pts = [c + egui::vec2(2.5, -5.0), c + egui::vec2(-2.5, 0.0), c + egui::vec2(2.5, 5.0)];
    painter.add(egui::Shape::line(pts.to_vec(), stroke));
}

fn paint_nav_forward(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.8_f32, color);
    let pts = [c + egui::vec2(-2.5, -5.0), c + egui::vec2(2.5, 0.0), c + egui::vec2(-2.5, 5.0)];
    painter.add(egui::Shape::line(pts.to_vec(), stroke));
}

fn paint_nav_up(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.8_f32, color);
    let pts = [c + egui::vec2(-5.0, 2.5), c + egui::vec2(0.0, -2.5), c + egui::vec2(5.0, 2.5)];
    painter.add(egui::Shape::line(pts.to_vec(), stroke));
}

fn paint_nav_refresh(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.6_f32, color);
    let r = 4.8;
    let n = 12;
    let mut pts = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let angle = std::f32::consts::PI * 0.25 + (std::f32::consts::PI * 1.5) * (i as f32 / n as f32);
        pts.push(c + egui::vec2(angle.cos() * r, angle.sin() * r));
    }
    painter.add(egui::Shape::line(pts, stroke));
    let tip = c + egui::vec2((std::f32::consts::PI * 1.75).cos() * r, (std::f32::consts::PI * 1.75).sin() * r);
    painter.add(egui::Shape::line(vec![tip + egui::vec2(-2.5, -1.0), tip, tip + egui::vec2(-0.5, 3.0)], stroke));
}

fn paint_search_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center() + egui::vec2(-1.0, -1.0);
    let stroke = Stroke::new(1.6_f32, color);
    painter.circle_stroke(c, 4.2, stroke);
    let handle_start = c + egui::vec2(3.0, 3.0);
    let handle_end = handle_start + egui::vec2(3.5, 3.5);
    painter.line_segment([handle_start, handle_end], Stroke::new(2.0_f32, color));
}

fn paint_clear_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = 3.5;
    let stroke = Stroke::new(1.5_f32, color);
    painter.line_segment([c + egui::vec2(-s, -s), c + egui::vec2(s, s)], stroke);
    painter.line_segment([c + egui::vec2(s, -s), c + egui::vec2(-s, s)], stroke);
}

fn paint_chevron_down(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.5_f32, color);
    let pts = [c + egui::vec2(-3.0, -1.5), c + egui::vec2(0.0, 1.8), c + egui::vec2(3.0, -1.5)];
    painter.add(egui::Shape::line(pts.to_vec(), stroke));
}

fn paint_view_list(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.5_f32, color);
    for dy in [-4.0, 0.0, 4.0] {
        painter.line_segment([c + egui::vec2(-5.5, dy), c + egui::vec2(5.5, dy)], stroke);
    }
}

fn paint_view_icons(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.4_f32, color);
    for dx in [-5.0, 1.0] {
        for dy in [-5.0, 1.0] {
            painter.rect_stroke(Rect::from_min_size(c + egui::vec2(dx, dy), egui::vec2(4.0, 4.0)), CornerRadius::same(1), stroke, egui::StrokeKind::Inside);
        }
    }
}

fn paint_view_columns(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.4_f32, color);
    for dx in [-6.0, -1.5, 3.0] {
        painter.rect_stroke(Rect::from_min_size(c + egui::vec2(dx, -5.5), egui::vec2(3.0, 11.0)), CornerRadius::same(1), stroke, egui::StrokeKind::Inside);
    }
}

fn paint_view_dual(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.4_f32, color);
    let r = Rect::from_center_size(c, egui::vec2(12.0, 11.0));
    painter.rect_stroke(r, CornerRadius::same(2), stroke, egui::StrokeKind::Inside);
    painter.line_segment([Pos2::new(c.x, r.top()), Pos2::new(c.x, r.bottom())], stroke);
}

fn paint_preview_icon(painter: &egui::Painter, rect: Rect, color: Color32, active: bool, accent: Color32) {
    let c = rect.center();
    let r = Rect::from_center_size(c, egui::vec2(13.0, 11.0));
    let stroke = Stroke::new(1.4_f32, color);
    painter.rect_stroke(r, CornerRadius::same(2), stroke, egui::StrokeKind::Inside);
    let split_x = r.right() - 4.5;
    painter.line_segment([Pos2::new(split_x, r.top()), Pos2::new(split_x, r.bottom())], stroke);
    if active {
        painter.rect_filled(Rect::from_min_max(Pos2::new(split_x + 1.0, r.top() + 1.0), r.max - egui::vec2(1.0, 1.0)), CornerRadius::same(1), accent.gamma_multiply(0.4));
    }
}

fn paint_terminal_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.6_f32, color);
    let prompt_pts = [c + egui::vec2(-5.0, -3.5), c + egui::vec2(-1.5, -0.5), c + egui::vec2(-5.0, 2.5)];
    painter.add(egui::Shape::line(prompt_pts.to_vec(), stroke));
    painter.line_segment([c + egui::vec2(1.0, 3.5), c + egui::vec2(5.5, 3.5)], stroke);
}

fn paint_process_icon(painter: &egui::Painter, rect: Rect, color: Color32, running: bool) {
    let c = rect.center();
    let r = 5.2_f32;
    let stroke = Stroke::new(1.4_f32, color);
    painter.circle_stroke(c, r, stroke);
    if running {
        painter.circle_filled(c, 2.0, color);
    } else {
        let pts = [c + egui::vec2(-2.5, 0.0), c + egui::vec2(-0.5, 2.2), c + egui::vec2(2.8, -2.0)];
        painter.add(egui::Shape::line(pts.to_vec(), Stroke::new(1.3_f32, color)));
    }
}

pub(super) fn paint_archive_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.3_f32, color);
    let box_rect = Rect::from_center_size(c, egui::vec2(13.0, 11.0));
    painter.rect_stroke(box_rect, CornerRadius::same(2), stroke, egui::StrokeKind::Inside);
    painter.line_segment([Pos2::new(box_rect.left(), c.y), Pos2::new(box_rect.right(), c.y)], Stroke::new(1.0_f32, color.gamma_multiply(0.6)));
    let buckle = Rect::from_center_size(c, egui::vec2(3.5, 4.5));
    painter.rect_filled(buckle, CornerRadius::same(1), color);
}

fn paint_gear_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.5_f32, color);
    let r = 3.2;
    painter.circle_stroke(c, r, stroke);
    for i in 0..6 {
        let angle = i as f32 * (std::f32::consts::PI / 3.0);
        let p1 = c + egui::vec2(angle.cos() * (r + 0.5), angle.sin() * (r + 0.5));
        let p2 = c + egui::vec2(angle.cos() * (r + 2.8), angle.sin() * (r + 2.8));
        painter.line_segment([p1, p2], Stroke::new(2.2_f32, color));
    }
}

pub(super) fn paint_icon_painter(painter: &egui::Painter, icons: &mut IconCache, key: &str, large: bool, rect: Rect, fallback_dir: bool, theme: &Theme) {
    if let Some(tex) = icons.get(key, large) {
        painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    } else if fallback_dir {
        paint_view_icons(painter, rect.shrink(4.0), theme.dim);
    } else {
        let r = Rect::from_center_size(rect.center(), egui::vec2(rect.width() * 0.65, rect.height() * 0.75));
        painter.rect_stroke(r, CornerRadius::same(2), Stroke::new(1.2_f32, theme.dim), egui::StrokeKind::Inside);
    }
}

fn paint_icon(ui: &egui::Ui, icons: &mut IconCache, key: &str, large: bool, rect: Rect, fallback_dir: bool, theme: &Theme) {
    paint_icon_painter(ui.painter(), icons, key, large, rect, fallback_dir, theme);
}

fn type_label(icons: &IconCache, e: &Entry) -> String {
    if let Some(t) = icons.type_name(&e.icon_key) {
        return t.to_string();
    }
    if e.is_dir {
        "File folder".into()
    } else if e.ext.is_empty() {
        "File".into()
    } else {
        format!("{} File", e.ext.to_ascii_uppercase())
    }
}

fn shown_name(e: &Entry, prefs: &Prefs) -> String {
    if prefs.show_extensions || e.is_dir || e.ext.is_empty() {
        e.name.clone()
    } else {
        e.name[..e.name.len() - e.ext.len() - 1].to_string()
    }
}

/// The right-click menu for items. `targets` is what the command applies to.
fn item_menu(ui: &mut egui::Ui, side: Side, entry: &Entry, targets: &[PathBuf], pinned: bool, dual: bool, actions: &mut Vec<Action>) {
    let t = || targets.to_vec();
    let mut item = |ui: &mut egui::Ui, label: &str, shortcut: &str, action: Action| {
        let resp = ui.add(egui::Button::new(label).shortcut_text(shortcut).min_size(egui::vec2(220.0, 0.0)));
        if resp.clicked() {
            actions.push(action);
            ui.close_menu();
        }
    };
    let single = targets.len() <= 1;
    if entry.is_dir {
        item(ui, "Open", "Enter", Action::Open(side, entry.path.clone()));
        item(ui, "Open in new tab", "", Action::OpenNewTab(entry.path.clone()));
        if dual {
            let other = if side == Side::Main { Side::Second } else { Side::Main };
            item(ui, "Open in other pane", "", Action::Navigate(other, entry.path.clone()));
        }
    } else {
        item(ui, "Open", "Enter", Action::Open(side, entry.path.clone()));
        if single {
            item(ui, "Open with…", "", Action::OpenWith(entry.path.clone()));
        }
        let ext = entry.ext.to_ascii_lowercase();
        if single && matches!(ext.as_str(), "exe" | "bat" | "cmd" | "msi" | "ps1") {
            item(ui, "Run as administrator", "", Action::RunAsAdmin(entry.path.clone()));
        }
    }
    if single && tools::is_browsable_archive(&entry.name) {
        ui.separator();
        item(ui, "Browse archive", "", Action::BrowseArchive(entry.path.clone()));
    }
    if single && is_archive(&entry.name) {
        ui.separator();
        item(ui, "Extract here", "", Action::Extract { archive: entry.path.clone(), into_folder: false });
        let stem = archive_stem(&entry.name);
        item(ui, &format!("Extract to \"{stem}\\\""), "x", Action::Extract { archive: entry.path.clone(), into_folder: true });
    }
    ui.separator();
    item(ui, "Cut", "Ctrl+X", Action::Cut(t()));
    item(ui, "Copy", "Ctrl+C", Action::Copy(t()));
    if entry.is_dir && single {
        item(ui, "Paste into folder", "", Action::Paste(entry.path.clone()));
    }
    item(ui, "Copy as path", "", Action::CopyPath(t()));
    ui.separator();
    if single {
        item(ui, "Rename", "F2", Action::StartRename(entry.path.clone()));
    } else {
        item(ui, &format!("Rename {} items…", targets.len()), "F2", Action::BatchRename(t()));
    }
    item(ui, "Delete", "Del", Action::Recycle(t()));
    item(ui, "Delete permanently", "Shift+Del", Action::Delete(t()));
    ui.separator();
    item(ui, "Compress to ZIP file", "", Action::Compress(t()));
    item(ui, "Create shortcut", "", Action::Shortcut(t()));
    if entry.is_dir && single {
        if pinned {
            item(ui, "Remove from Favorites", "", Action::Unpin(entry.path.clone()));
        } else {
            item(ui, "Add to Favorites", "", Action::Pin(entry.path.clone()));
        }
        item(ui, "Open in Terminal", "", Action::Terminal(entry.path.clone()));
    }
    if targets.iter().any(|p| p.is_dir()) {
        item(ui, "Calculate size", "", Action::FolderSize(t()));
    }
    if entry.is_dir && single {
        item(ui, "Disk usage…", "", Action::DiskUsage(entry.path.clone()));
    }
    ui.separator();
    item(ui, "Show more options", "", Action::NativeMenu(t()));
    item(ui, "Properties", "Alt+Enter", Action::Properties(t()));
}

/// The right-click menu on empty space in a folder.
fn background_menu(ui: &mut egui::Ui, side: Side, cwd: &Path, prefs: &Prefs, pinned: bool, actions: &mut Vec<Action>) {
    let mut push = |ui: &mut egui::Ui, a: Action| {
        actions.push(a);
        ui.close_menu();
    };
    ui.set_min_width(220.0);
    ui.menu_button("View", |ui| {
        for (v, label) in [
            (ViewMode::Details, "Details"),
            (ViewMode::Icons, "Icons"),
            (ViewMode::Columns, "Columns"),
            (ViewMode::Dual, "Dual pane"),
        ] {
            if ui.radio(prefs.view == v, label).clicked() {
                push(ui, Action::SetView(v));
            }
        }
    });
    ui.menu_button("Sort by", |ui| {
        for (k, label) in [
            (SortKey::Name, "Name"),
            (SortKey::Modified, "Date modified"),
            (SortKey::Type, "Type"),
            (SortKey::Size, "Size"),
        ] {
            if ui.radio(prefs.sort == k, label).clicked() && prefs.sort != k {
                push(ui, Action::Sort(k));
            }
        }
        ui.separator();
        if ui.radio(!prefs.sort_desc, "Ascending").clicked() {
            push(ui, Action::SortDesc(false));
        }
        if ui.radio(prefs.sort_desc, "Descending").clicked() {
            push(ui, Action::SortDesc(true));
        }
    });
    if ui.add(egui::Button::new("Refresh").shortcut_text("F5")).clicked() {
        push(ui, Action::Refresh);
    }
    ui.separator();
    let can_paste = shell::clipboard_get_files().is_some_and(|(f, _)| !f.is_empty());
    if ui.add_enabled(can_paste, egui::Button::new("Paste").shortcut_text("Ctrl+V")).clicked() {
        push(ui, Action::Paste(cwd.to_path_buf()));
    }
    ui.menu_button("New", |ui| {
        if ui.add(egui::Button::new("New Folder").shortcut_text("Ctrl+Shift+N")).clicked() {
            push(ui, Action::NewFolder(cwd.to_path_buf()));
        }
        if ui.button("New Text Document").clicked() {
            push(ui, Action::NewTextFile(cwd.to_path_buf()));
        }
    });
    ui.separator();
    if ui.button("Open in Terminal").clicked() {
        push(ui, Action::Terminal(cwd.to_path_buf()));
    }
    if ui.button("Disk usage…").clicked() {
        push(ui, Action::DiskUsage(cwd.to_path_buf()));
    }
    if ui.add(egui::Button::new("Go to folder…").shortcut_text("Ctrl+P")).clicked() {
        push(ui, Action::GoAnywhere);
    }
    if ui.add(egui::Button::new("Undo").shortcut_text("Ctrl+Z")).clicked() {
        push(ui, Action::Undo);
    }
    if ui.button(if pinned { "Remove from Favorites" } else { "Add to Favorites" }).clicked() {
        push(ui, if pinned { Action::Unpin(cwd.to_path_buf()) } else { Action::Pin(cwd.to_path_buf()) });
    }
    let mut hidden = prefs.show_hidden;
    if ui.checkbox(&mut hidden, "Show hidden items").clicked() {
        push(ui, Action::ToggleHidden);
    }
    ui.separator();
    if ui.button("Properties").clicked() {
        push(ui, Action::Properties(vec![cwd.to_path_buf()]));
    }
    let _ = side;
}

impl Draw<'_> {
    /// Shared row/tile interaction: click, double-click, right-click, drag start, drop zone.
    fn interact(&mut self, resp: &egui::Response, side: Side, pos: usize, entry: &Entry, pane: &Pane, rect: Rect, dual: bool) {
        let mods = resp.ctx.input(|i| i.modifiers);
        if resp.clicked() {
            self.actions.push(Action::Click { side, pos, ctrl: mods.ctrl, shift: mods.shift });
        }
        if resp.double_clicked() {
            self.actions.push(Action::Open(side, entry.path.clone()));
        }
        if resp.secondary_clicked() {
            self.actions.push(Action::ContextSelect { side, pos });
        }
        if resp.drag_started() {
            let selected = pane.selection.contains(&entry.path);
            if !selected {
                self.actions.push(Action::Click { side, pos, ctrl: false, shift: false });
            }
            let paths = if selected { pane.targets() } else { vec![entry.path.clone()] };
            let secondary = resp.dragged_by(egui::PointerButton::Secondary);
            let grab_offset = if let Some(p) = resp.interact_pointer_pos() {
                p - rect.min
            } else {
                rect.size() * 0.5
            };
            self.actions.push(Action::BeginDrag {
                paths,
                secondary,
                origin_rect: rect,
                grab_offset,
                icon_key: entry.icon_key.clone(),
                is_dir: entry.is_dir,
                name: shown_name(entry, self.prefs).to_string(),
            });
        }
        if entry.is_dir {
            self.zones.push(DropZone { rect, dir: entry.path.clone() });
        }
        let targets = if pane.selection.contains(&entry.path) { pane.targets() } else { vec![entry.path.clone()] };
        let pinned = self.prefs.is_favorite(&entry.path);
        let actions = &mut *self.actions;
        resp.context_menu(|ui| item_menu(ui, side, entry, &targets, pinned, dual, actions));
    }

    fn row_background(&self, ui: &egui::Ui, rect: Rect, resp: &egui::Response, selected: bool, cursor: bool) {
        let radius = CornerRadius::same(self.theme.radius.min(6));
        if selected {
            ui.painter().rect_filled(rect, radius, self.theme.selected);
            ui.painter().rect_stroke(rect.shrink(0.5), radius, Stroke::new(1.0_f32, self.theme.accent.gamma_multiply(0.4)), egui::StrokeKind::Inside);
        } else if resp.hovered() && !self.dragging {
            ui.painter().rect_filled(rect, radius, self.theme.fg.gamma_multiply(0.05));
        }
        if cursor && self.focused && !selected {
            ui.painter().rect_stroke(rect.shrink(0.5), radius, Stroke::new(1.0_f32, self.theme.accent.gamma_multiply(0.45)), egui::StrokeKind::Inside);
        }
    }

    /// Inline rename editor; returns true if it was drawn for this entry.
    fn rename_field(&mut self, ui: &mut egui::Ui, entry: &Entry, rect: Rect) -> bool {
        let Some(r) = self.rename.as_mut().filter(|r| r.path == entry.path) else {
            return false;
        };
        let id = egui::Id::new(("rename", &entry.path));
        let edit = egui::TextEdit::singleline(&mut r.buffer).id(id).margin(egui::Margin::symmetric(4, 2));
        let resp = ui.put(rect, edit);
        if r.just_started {
            r.just_started = false;
            resp.request_focus();
            // Select the name without its extension, like Explorer.
            let stem_len = if entry.is_dir || entry.ext.is_empty() {
                r.buffer.chars().count()
            } else {
                r.buffer.chars().count() - entry.ext.chars().count() - 1
            };
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(stem_len),
                )));
                state.store(ui.ctx(), id);
            }
        }
        if resp.lost_focus() {
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                *self.rename = None;
            } else {
                *self.commit_rename = true;
            }
        }
        true
    }

    fn scroll_area(&self, side: Side, salt: &str, pane: &Pane, row_h: f32, cols: usize, memo: Option<&ScrollMemo>) -> egui::ScrollArea {
        let mut sa = egui::ScrollArea::vertical()
            .id_salt((salt, side))
            .auto_shrink([false, false])
            .drag_to_scroll(false);
        if pane.scroll_to_cursor {
            if let Some(m) = memo {
                let row = (pane.cursor / cols.max(1)) as f32;
                let (top, bottom) = (row * row_h, row * row_h + row_h);
                let offset = if top < m.offset {
                    top
                } else if bottom > m.offset + m.viewport {
                    bottom - m.viewport
                } else {
                    m.offset
                };
                sa = sa.vertical_scroll_offset(offset.max(0.0));
            }
        }
        sa
    }

    /// Clickable empty space behind the list: clears selection, background menu, drop zone.
    fn background(&mut self, ui: &mut egui::Ui, side: Side, pane: &Pane, rect: Rect) {
        let resp = ui.interact(rect, ui.id().with(("bg", side)), Sense::click_and_drag());
        if resp.drag_started_by(egui::PointerButton::Primary) {
            if let Some(pos) = resp.interact_pointer_pos() {
                let initial_selection = if ui.input(|i| i.modifiers.ctrl) {
                    pane.selection.clone()
                } else {
                    std::collections::HashSet::new()
                };
                *self.marquee = Some(Marquee {
                    side,
                    start: pos,
                    current: pos,
                    initial_selection,
                });
                if !ui.input(|i| i.modifiers.ctrl) {
                    self.actions.push(Action::ClearSelection(side));
                }
            }
        }
        if resp.dragged_by(egui::PointerButton::Primary) {
            if let Some(m) = self.marquee.as_mut() {
                if m.side == side {
                    if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                        m.current = pos;
                    }
                }
            }
        }
        if resp.clicked() || resp.secondary_clicked() {
            self.actions.push(Action::ClearSelection(side));
        }
        self.zones.push(DropZone { rect, dir: pane.cwd.clone() });
        let pinned = self.prefs.is_favorite(&pane.cwd);
        let (cwd, prefs, actions) = (pane.cwd.clone(), self.prefs, &mut *self.actions);
        resp.context_menu(|ui| background_menu(ui, side, &cwd, prefs, pinned, actions));
    }

    fn empty_message(&self, ui: &egui::Ui, rect: Rect, pane: &Pane) {
        let msg = if let Some(e) = &pane.error {
            format!("Can't open this folder\n{e}")
        } else if !pane.filter.is_empty() {
            "No items match your search".to_string()
        } else if pane.visible.is_empty() {
            "This folder is empty".to_string()
        } else {
            return;
        };
        ui.painter().text(
            rect.center_top() + egui::vec2(0.0, 60.0),
            Align2::CENTER_CENTER,
            msg,
            FontId::proportional(self.theme.font),
            self.theme.dim,
        );
    }

    /// Details view (also used, compact, for both sides of Dual pane).
    fn details(&mut self, ui: &mut egui::Ui, side: Side, pane: &Pane, compact: bool, dual: bool, memo: Option<&ScrollMemo>) -> ScrollMemo {
        let th = self.theme;
        let row_h = th.row_h;
        let full = ui.available_rect_before_wrap();
        let (date_w, type_w, size_w) = if compact { (128.0, 0.0, 76.0) } else { (140.0, 150.0, 84.0) };
        let name_w = (full.width() - date_w - type_w - size_w - 24.0).max(120.0);

        // Header with sort buttons (aligned with 4px left gap).
        let (header, _) = ui.allocate_exact_size(egui::vec2(full.width(), 26.0), Sense::hover());
        let mut x = header.left() + 4.0 + 8.0;
        let mut col = |ui: &mut egui::Ui, label: &str, key: SortKey, w: f32, actions: &mut Vec<Action>| {
            if w <= 0.0 {
                return;
            }
            let r = Rect::from_min_size(Pos2::new(x, header.top()), egui::vec2(w, header.height()));
            x += w;
            let resp = ui.interact(r, ui.id().with(("hdr", label, side)), Sense::click());
            let arrow = if self.prefs.sort == key { if self.prefs.sort_desc { " ⬇" } else { " ⬆" } } else { "" };
            let color = if resp.hovered() { th.fg } else { th.dim };
            ui.painter().text(r.left_center() + egui::vec2(16.0 * (label == "Name") as i32 as f32 + 4.0, 0.0), Align2::LEFT_CENTER, format!("{label}{arrow}"), FontId::proportional(th.font - 1.5), color);
            if resp.clicked() {
                actions.push(Action::Sort(key));
            }
        };
        col(ui, "Name", SortKey::Name, name_w, self.actions);
        col(ui, "Date modified", SortKey::Modified, date_w, self.actions);
        col(ui, "Type", SortKey::Type, type_w, self.actions);
        col(ui, "Size", SortKey::Size, size_w, self.actions);
        ui.painter().hline(header.x_range(), header.bottom(), Stroke::new(1.0_f32, th.hairline()));

        let list_rect = ui.available_rect_before_wrap();
        self.background(ui, side, pane, list_rect);
        self.empty_message(ui, list_rect, pane);

        let mut marquee_hits = Vec::new();
        let marquee_box = if let Some(m) = self.marquee.as_ref() {
            if m.side == side {
                let r = Rect::from_two_pos(m.start, m.current);
                if r.width() > 3.0 || r.height() > 3.0 { Some(r) } else { None }
            } else {
                None
            }
        } else {
            None
        };

        let sa = self.scroll_area(side, "details", pane, row_h, 1, memo);
        let out = sa.show_rows(ui, row_h, pane.visible.len(), |ui, range| {
            for pos in range {
                let Some(entry) = pane.entry_at(pos) else { continue };
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.add_space(4.0); // 4-pixel gap on left for mouse selection box drag
                    let row_w = (ui.available_width() - 4.0).max(60.0);
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(row_w, row_h), Sense::click_and_drag());
                    let is_being_dragged = self.drag_info.as_ref().is_some_and(|d| d.paths.contains(&entry.path));
                    let is_snapping = self.snap_back.as_ref().is_some_and(|s| s.path == entry.path);
                    if is_being_dragged || is_snapping {
                        // Empty slot on the back; row stays intact so layout doesn't collapse
                        ui.painter().rect_stroke(rect, CornerRadius::same(th.radius.min(6)), Stroke::new(1.0_f32, th.hairline().gamma_multiply(0.25)), egui::StrokeKind::Inside);
                    } else {
                        let selected = pane.selection.contains(&entry.path);
                        // Finder's alternating row stripes.
                        if pos % 2 == 1 && !selected {
                            ui.painter().rect_filled(rect, CornerRadius::same(th.radius.min(6)), th.fg.gamma_multiply(0.025));
                        }
                        self.row_background(ui, rect, &resp, selected, pos == pane.cursor);
                        if let Some(mbox) = marquee_box {
                            if mbox.intersects(rect) {
                                marquee_hits.push(entry.path.clone());
                            }
                        }
                        let icon = Rect::from_center_size(Pos2::new(rect.left() + 18.0, rect.center().y), Vec2::splat(16.0));
                        paint_icon(ui, self.icons, &entry.icon_key, false, icon, entry.is_dir, &th);
                        let name_rect = Rect::from_min_max(Pos2::new(rect.left() + 32.0, rect.top() + 2.0), Pos2::new(rect.left() + 8.0 + name_w - 6.0, rect.bottom() - 2.0));
                        if !self.rename_field(ui, entry, name_rect) {
                            let painter = ui.painter().with_clip_rect(name_rect);
                            let color = if entry.hidden { th.name_color(entry).gamma_multiply(0.55) } else { th.name_color(entry) };
                            painter.text(name_rect.left_center(), Align2::LEFT_CENTER, shown_name(entry, self.prefs), FontId::proportional(th.font), color);
                        }
                        let mut cx = rect.left() + 8.0 + name_w;
                        let small = FontId::proportional(th.font - 1.5);
                        let date = entry.modified.map(format_time).unwrap_or_default();
                        ui.painter().with_clip_rect(Rect::from_min_size(Pos2::new(cx, rect.top()), egui::vec2(date_w - 6.0, row_h)))
                            .text(Pos2::new(cx + 4.0, rect.center().y), Align2::LEFT_CENTER, date, small.clone(), th.dim);
                        cx += date_w;
                        if type_w > 0.0 {
                            ui.painter().with_clip_rect(Rect::from_min_size(Pos2::new(cx, rect.top()), egui::vec2(type_w - 6.0, row_h)))
                               .text(Pos2::new(cx + 4.0, rect.center().y), Align2::LEFT_CENTER, type_label(self.icons, entry), small.clone(), th.dim);
                            cx += type_w;
                        }
                        let size_text = if entry.is_dir {
                            self.extras.sizes.get(&entry.path).map(|s| s.map(format_bytes).unwrap_or_else(|| "…".to_owned()))
                        } else {
                            Some(format_bytes(entry.size))
                        };
                        if let Some(text) = size_text {
                            ui.painter().text(Pos2::new(cx + size_w - 6.0, rect.center().y), Align2::RIGHT_CENTER, text, small.clone(), th.dim);
                        }
                        if let Some(&letter) = self.extras.git.get(&entry.path) {
                            let badge_color = tools::git_badge_color(letter, &th);
                            let badge_size = Vec2::new(16.0, 15.0);
                            let badge_rect = Rect::from_center_size(Pos2::new(name_rect.right() - 10.0, rect.center().y), badge_size);
                            ui.painter().rect_filled(badge_rect, CornerRadius::same(3), badge_color.gamma_multiply(0.18));
                            ui.painter().rect_stroke(badge_rect, CornerRadius::same(3), Stroke::new(1.0_f32, badge_color.gamma_multiply(0.6)), egui::StrokeKind::Inside);
                            ui.painter().text(
                                badge_rect.center(),
                                Align2::CENTER_CENTER,
                                letter.to_string(),
                                FontId::monospace(th.font - 3.0),
                                badge_color,
                            );
                        }
                    }
                    self.interact(&resp, side, pos, entry, pane, rect, dual);
                });
            }
        });
        if let Some(m) = self.marquee.as_ref() {
            if m.side == side {
                let r = Rect::from_two_pos(m.start, m.current);
                if r.width() > 3.0 || r.height() > 3.0 {
                    ui.painter().rect_filled(r, CornerRadius::ZERO, th.accent.gamma_multiply(0.18));
                    ui.painter().rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0_f32, th.accent), egui::StrokeKind::Inside);
                    let mut new_sel = m.initial_selection.clone();
                    for p in marquee_hits {
                        new_sel.insert(p);
                    }
                    self.actions.push(Action::SetSelection(side, new_sel));
                }
            }
        }
        if ui.input(|i| i.pointer.any_released()) {
            *self.marquee = None;
        }

        ScrollMemo { offset: out.state.offset.y, viewport: out.inner_rect.height() }
    }

    /// Icons view: a grid of tiles.
    fn icons_grid(&mut self, ui: &mut egui::Ui, side: Side, pane: &Pane, memo: Option<&ScrollMemo>, cols_out: &mut usize) -> ScrollMemo {
        let th = self.theme;
        let scale = self.prefs.icon_scale.clamp(0.65, 2.2);

        // Alt+scrollwheel (or Ctrl+scrollwheel) zoom
        let (zoom_in, zoom_out) = ui.input(|i| {
            if (i.modifiers.alt || i.modifiers.ctrl) && i.raw_scroll_delta.y != 0.0 {
                (i.raw_scroll_delta.y > 0.0, i.raw_scroll_delta.y < 0.0)
            } else {
                (false, false)
            }
        });
        if zoom_in {
            self.actions.push(Action::SetIconScale(scale + 0.1));
        } else if zoom_out {
            self.actions.push(Action::SetIconScale(scale - 0.1));
        }

        let list_rect = ui.available_rect_before_wrap();
        self.background(ui, side, pane, list_rect);
        self.empty_message(ui, list_rect, pane);

        // AUTOFIT: distribute width evenly across all columns so there is no gap on the right!
        let available_w = (list_rect.width() - 8.0).max(60.0);
        let base_w = (122.0 * scale).round();
        let base_h = (94.0 * scale).round();
        let gap = (14.0 * scale).round().max(12.0);

        let cols = ((available_w + gap) / (base_w + gap)).floor().max(1.0) as usize;
        *cols_out = cols;

        let total_gaps = (cols.saturating_sub(1)) as f32 * gap;
        let tile_w = ((available_w - total_gaps) / cols as f32).floor();
        let tile = egui::vec2(tile_w, base_h);
        let row_h = tile.y + gap;
        let rows = pane.visible.len().div_ceil(cols);
        let sa = self.scroll_area(side, "icons", pane, row_h, cols, memo);

        let mut marquee_hits = Vec::new();
        let marquee_box = if let Some(m) = self.marquee.as_ref() {
            if m.side == side {
                let r = Rect::from_two_pos(m.start, m.current);
                if r.width() > 3.0 || r.height() > 3.0 { Some(r) } else { None }
            } else {
                None
            }
        } else {
            None
        };

        let out = sa.show_rows(ui, row_h, rows, |ui, range| {
            ui.spacing_mut().item_spacing.y = gap;
            for row in range {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    ui.add_space(4.0); // 4-pixel gap on left for mouse selection box drag
                    for pos in row * cols..((row + 1) * cols).min(pane.visible.len()) {
                        let Some(entry) = pane.entry_at(pos) else { continue };
                        let (slot_rect, _) = ui.allocate_exact_size(tile, Sense::hover());
                        let rect = slot_rect.shrink2(egui::vec2(2.0, 2.0));
                        let resp = ui.interact(rect, ui.id().with(("item", side, pos)), Sense::click_and_drag());
                        let is_being_dragged = self.drag_info.as_ref().is_some_and(|d| d.paths.contains(&entry.path));
                        let is_snapping = self.snap_back.as_ref().is_some_and(|s| s.path == entry.path);
                        if is_being_dragged || is_snapping {
                            // Empty slot on the back; grid layout does NOT align/shift until item moves elsewhere
                            ui.painter().rect_stroke(rect, CornerRadius::same(th.radius.min(6)), Stroke::new(1.0_f32, th.hairline().gamma_multiply(0.25)), egui::StrokeKind::Inside);
                        } else {
                            let selected = pane.selection.contains(&entry.path);
                            self.row_background(ui, rect, &resp, selected, pos == pane.cursor);

                            if let Some(mbox) = marquee_box {
                                if mbox.intersects(rect) {
                                    marquee_hits.push(entry.path.clone());
                                }
                            }

                            let icon_size = (50.4 * scale).round();
                            let icon = Rect::from_center_size(Pos2::new(rect.center().x, rect.top() + (31.0 * scale).round()), Vec2::splat(icon_size));
                            paint_icon(ui, self.icons, &entry.icon_key, true, icon, entry.is_dir, &th);
                            let label_rect = Rect::from_min_max(Pos2::new(rect.left() + 4.0, rect.top() + (58.0 * scale).round()), Pos2::new(rect.right() - 4.0, rect.bottom() - 2.0));
                            let rename_w = (rect.width() * 0.80).round();
                            let rename_rect = Rect::from_center_size(Pos2::new(rect.center().x, label_rect.top() + 12.0), Vec2::new(rename_w, 24.0));
                            if !self.rename_field(ui, entry, rename_rect) {
                                let font_size = (th.font - 1.5) * scale.clamp(0.85, 1.3);
                                let mut job = egui::text::LayoutJob::simple(
                                    shown_name(entry, self.prefs),
                                    FontId::proportional(font_size),
                                    th.name_color(entry),
                                    label_rect.width(),
                                );
                                job.wrap.max_rows = 2;
                                job.wrap.overflow_character = Some('…');
                                job.halign = egui::Align::Center;
                                let galley = ui.fonts(|f| f.layout_job(job));
                                let painter = ui.painter().with_clip_rect(label_rect);
                                let x = label_rect.center().x - galley.size().x / 2.0;
                                painter.galley(Pos2::new(x, label_rect.top()), galley, th.fg);
                            }
                        }
                        self.interact(&resp, side, pos, entry, pane, rect, false);
                    }
                });
            }
        });

        if let Some(m) = self.marquee.as_ref() {
            if m.side == side {
                let r = Rect::from_two_pos(m.start, m.current);
                if r.width() > 3.0 || r.height() > 3.0 {
                    ui.painter().rect_filled(r, CornerRadius::ZERO, th.accent.gamma_multiply(0.18));
                    ui.painter().rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0_f32, th.accent), egui::StrokeKind::Inside);
                    let mut new_sel = m.initial_selection.clone();
                    for p in marquee_hits {
                        new_sel.insert(p);
                    }
                    self.actions.push(Action::SetSelection(side, new_sel));
                }
            }
        }
        if ui.input(|i| i.pointer.any_released()) {
            *self.marquee = None;
        }

        ScrollMemo { offset: out.state.offset.y, viewport: out.inner_rect.height() }
    }

    /// A simple column (Columns view): name-only rows; `on_click` decides what a click does.
    fn name_column(&mut self, ui: &mut egui::Ui, salt: &str, entries: &[Entry], highlight: Option<&Path>, open_side: Side) {
        let th = self.theme;
        egui::ScrollArea::vertical().id_salt(salt).auto_shrink([false, false]).show_rows(ui, th.row_h, entries.len(), |ui, range| {
            for e in &entries[range] {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.add_space(4.0); // 4-pixel gap on left
                    let row_w = (ui.available_width() - 4.0).max(60.0);
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(row_w, th.row_h), Sense::click());
                    let hl = highlight == Some(e.path.as_path());
                    self.row_background(ui, rect, &resp, hl, false);
                    let icon = Rect::from_center_size(Pos2::new(rect.left() + 16.0, rect.center().y), Vec2::splat(16.0));
                    paint_icon(ui, self.icons, &e.icon_key, false, icon, e.is_dir, &th);
                    ui.painter().with_clip_rect(rect.shrink(2.0)).text(Pos2::new(rect.left() + 30.0, rect.center().y), Align2::LEFT_CENTER, &e.name, FontId::proportional(th.font), th.name_color(e));
                    if e.is_dir {
                        ui.painter().text(rect.right_center() - egui::vec2(8.0, 0.0), Align2::RIGHT_CENTER, "›", FontId::proportional(th.font), th.dim);
                        self.zones.push(DropZone { rect, dir: e.path.clone() });
                    }
                    if resp.clicked() {
                        self.actions.push(Action::Open(open_side, e.path.clone()));
                    }
                });
            }
        });
    }
}

impl eframe::App for FilesApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if let Some(h) = hwnd_of(frame) {
            self.hwnd = h;
        }
        let theme = Theme::from_css(&self.css.read());
        self.font.sync(ctx, &self.css.read(), false);
        let config_changed = self.config_changed.swap(false, Ordering::Relaxed);
        if config_changed {
            if let Ok((_, c)) = chickenwm_core::config::load_or_init_config() {
                self.app_mode = c.theme.app_mode("files");
            }
        }
        if self.theme != Some(theme) || config_changed {
            theme.apply(ctx);
            if self.theme.is_some() {
                // New palette (built-in icons are redrawn) or a different icon theme.
                self.icons.set_theme(chickenwm_core::icon_theme::IconTheme::current());
                self.icons.reload();
            }
            self.theme = Some(theme);
        }
        self.icons.absorb(ctx);

        // Live refresh (file watcher / finished jobs), at most ~4 times a second.
        if self.dirty.load(Ordering::Relaxed) {
            if self.last_reload.elapsed() > Duration::from_millis(250) && self.drag.is_none() {
                self.dirty.store(false, Ordering::Relaxed);
                self.reload_all();
            } else {
                ctx.request_repaint_after(Duration::from_millis(260));
            }
        }

        // Auto focus newly created compressed file or created items
        if let Some((target_side, target_path)) = self.pending_focus.clone() {
            let pane = self.pane_mut(target_side);
            if pane.visible.iter().any(|&i| pane.entries[i].path == target_path) {
                pane.select_path(&target_path);
                self.pending_focus = None;
            }
        }

        if self.drives_at.elapsed() > Duration::from_secs(30) {
            self.refresh_drives();
        }
        self.sync_watcher();
        if self.jobs.running.load(Ordering::Relaxed) > 0 {
            ctx.request_repaint_after(Duration::from_secs_f64(1.0 / self.target_fps as f64));
        }

        if let Some(nav) = self.terminal.poll() {
            self.apply(Action::Navigate(self.focused(), nav));
        }
        self.terminal.set_cwd(self.pane(self.focused()).cwd.clone());

        let mut actions = Vec::new();
        self.handle_keys(ctx, &mut actions);
        for a in actions.drain(..) {
            self.apply(a);
        }
        self.update_preview(ctx);

        let side = self.focused();
        let title = format!("{} — ChickenWM Files", self.pane(side).title());
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        self.zones.clear();
        self.fav_drop_hover = false;
        let mut commit_rename = false;
        self.draw_sidebar(ctx, theme, &mut actions);
        self.draw_status(ctx, theme);
        self.draw_navbar(ctx, theme, &mut actions);
        self.draw_center(ctx, theme, &mut actions, &mut commit_rename);
        self.draw_tools(ctx, theme);
        self.compress_dialog(ctx, theme);
        self.export_folder_dialog(ctx, theme);
        self.export_here_dialog(ctx, theme);
        self.handle_drag_and_drop(ctx, theme, &mut actions);

        for a in actions {
            self.apply(a);
        }
        if commit_rename {
            self.commit_rename();
        }
        self.tabs[self.active].scroll_to_cursor = false;
        self.second.scroll_to_cursor = false;
    }
}

impl FilesApp {
    fn draw_navbar(&mut self, ctx: &egui::Context, th: Theme, actions: &mut Vec<Action>) {
        let frame = egui::Frame::NONE.fill(th.pane).inner_margin(egui::Margin { left: 10, right: 10, top: 6, bottom: 6 });
        egui::TopBottomPanel::top("files_navbar").frame(frame).show(ctx, |ui| {
            // Underneath: single hairline border separating navbar from folder view
            let bottom_y = ui.cursor().bottom() + 6.0;
            ui.painter().hline(ui.available_rect_before_wrap().x_range(), bottom_y, Stroke::new(1.0_f32, th.hairline()));

            ui.horizontal(|ui| {
                let side = self.focused();
                let pane = self.pane(side);

                // 1. Navigation capsule: Back, Forward, Up, Refresh (monotone vector icons, 28px height)
                let nav_frame = egui::Frame::NONE
                    .fill(th.pane)
                    .corner_radius(CornerRadius::same(th.radius.min(6)))
                    .stroke(Stroke::new(1.0_f32, th.hairline()))
                    .inner_margin(egui::Margin::symmetric(3, 2));

                nav_frame.show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 1.0;
                    let nav_btn = |ui: &mut egui::Ui, tip: &str, enabled: bool, painter_fn: fn(&egui::Painter, Rect, Color32)| -> bool {
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(22.0, 24.0), if enabled { Sense::click() } else { Sense::hover() });
                        let color = if enabled {
                            if resp.hovered() {
                                ui.painter().rect_filled(rect, CornerRadius::same(4), th.fg.gamma_multiply(0.08));
                                th.fg
                            } else {
                                th.fg.gamma_multiply(0.9)
                            }
                        } else {
                            th.dim.gamma_multiply(0.35)
                        };
                        painter_fn(ui.painter(), rect, color);
                        resp.on_hover_text(tip).clicked()
                    };

                    if nav_btn(ui, "Back (Alt+Left)", !pane.back.is_empty(), paint_nav_back) {
                        actions.push(Action::Back(side));
                    }
                    if nav_btn(ui, "Forward (Alt+Right)", !pane.forward.is_empty(), paint_nav_forward) {
                        actions.push(Action::Forward(side));
                    }
                    if nav_btn(ui, "Up a folder (Alt+Up / Backspace)", pane.cwd.parent().is_some(), paint_nav_up) {
                        actions.push(Action::Up(side));
                    }
                    if nav_btn(ui, "Refresh (F5)", true, paint_nav_refresh) {
                        actions.push(Action::Refresh);
                    }
                });

                ui.add_space(4.0);

                // 2. Breadcrumbs / Address bar: stretches to meet the right toolbar exactly
                let right_needed = 480.0;
                let addr_w = (ui.available_width() - right_needed).max(120.0);
                self.address_bar(ui, side, addr_w, th, actions);

                // 3. Right-anchored toolbar section (Close button on top right, Tools, Views dropdown, Search)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;

                    // Close button in place of ChickenWM label on top right
                    let (close_rect, close_resp) = ui.allocate_exact_size(Vec2::new(28.0, 26.0), Sense::click());
                    if close_resp.hovered() {
                        ui.painter().rect_filled(close_rect, CornerRadius::same(5), Color32::from_rgb(225, 60, 60));
                    }
                    let close_col = if close_resp.hovered() { Color32::WHITE } else { th.dim };
                    let c = close_rect.center();
                    ui.painter().line_segment([c + Vec2::new(-4.5, -4.5), c + Vec2::new(4.5, 4.5)], Stroke::new(1.4_f32, close_col));
                    ui.painter().line_segment([c + Vec2::new(4.5, -4.5), c + Vec2::new(-4.5, 4.5)], Stroke::new(1.4_f32, close_col));
                    if close_resp.on_hover_text("Close window").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }

                    // Maximize / Restore button
                    let (max_rect, max_resp) = ui.allocate_exact_size(Vec2::new(26.0, 26.0), Sense::click());
                    if max_resp.hovered() {
                        ui.painter().rect_filled(max_rect, CornerRadius::same(5), th.fg.gamma_multiply(0.08));
                    }
                    let max_col = if max_resp.hovered() { th.fg } else { th.dim };
                    let mc = max_rect.center();
                    ui.painter().rect_stroke(Rect::from_center_size(mc, Vec2::splat(9.0)), CornerRadius::same(2), Stroke::new(1.2_f32, max_col), egui::StrokeKind::Inside);
                    if max_resp.on_hover_text("Maximize / Restore").clicked() {
                        let is_max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_max));
                    }

                    // Minimize button
                    let (min_rect, min_resp) = ui.allocate_exact_size(Vec2::new(26.0, 26.0), Sense::click());
                    if min_resp.hovered() {
                        ui.painter().rect_filled(min_rect, CornerRadius::same(5), th.fg.gamma_multiply(0.08));
                    }
                    let min_col = if min_resp.hovered() { th.fg } else { th.dim };
                    let min_c = min_rect.center();
                    ui.painter().line_segment([min_c + Vec2::new(-4.5, 0.0), min_c + Vec2::new(4.5, 0.0)], Stroke::new(1.4_f32, min_col));
                    if min_resp.on_hover_text("Minimize").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }

                    ui.add_space(4.0);

                    // Top Processes Pill & Dropdown
                    let active_procs = self.processes.active();
                    let all_procs = self.processes.all();
                    let running = !active_procs.is_empty();
                    let has_history = !all_procs.is_empty();

                    if running || has_history {
                        let proc_label = if let Some(first) = active_procs.first() {
                            if let Some(p) = first.progress {
                                format!("{} ({}%)", first.title, (p * 100.0) as u32)
                            } else {
                                first.title.clone()
                            }
                        } else {
                            format!("Tasks ({})", all_procs.len())
                        };

                        let btn_text = egui::RichText::new(&proc_label).size(th.font - 1.5).color(if running { th.fg } else { th.dim });
                        let proc_btn = egui::Button::new(btn_text)
                            .fill(if running { th.accent.gamma_multiply(0.15) } else { th.pane })
                            .stroke(Stroke::new(1.0_f32, if running { th.accent } else { th.hairline() }))
                            .corner_radius(CornerRadius::same(th.radius.min(6)))
                            .min_size(egui::vec2(0.0, 26.0));

                        let proc_resp = ui.add(proc_btn).on_hover_text("Background file operations, extraction & compression");
                        if proc_resp.clicked() {
                            self.show_processes = !self.show_processes;
                        }

                        if self.show_processes {
                            let dropdown_pos = Pos2::new(proc_resp.rect.left().min(ui.available_rect_before_wrap().right() - 320.0), proc_resp.rect.bottom() + 4.0);
                            let area = egui::Area::new(egui::Id::new("processes_dropdown"))
                                .order(egui::Order::Foreground)
                                .fixed_pos(dropdown_pos);

                            area.show(ctx, |ui| {
                                let frame = egui::Frame::NONE
                                    .fill(th.pane)
                                    .stroke(Stroke::new(1.0_f32, th.accent.gamma_multiply(0.5)))
                                    .corner_radius(CornerRadius::same(th.radius.max(8)))
                                    .shadow(egui::epaint::Shadow {
                                        offset: [0, 8],
                                        blur: 24,
                                        spread: 0,
                                        color: Color32::from_black_alpha(150),
                                    })
                                    .inner_margin(egui::Margin::symmetric(12, 10));

                                frame.show(ui, |ui| {
                                    ui.set_width(310.0);
                                    ui.horizontal(|ui| {
                                        let (pr_icon, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                                        paint_process_icon(ui.painter(), pr_icon, th.accent, running);
                                        ui.label(egui::RichText::new("Processes").strong().size(th.font));
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            if ui.add(egui::Button::new("Clear").min_size(Vec2::new(0.0, 20.0))).clicked() {
                                                self.processes.clear_finished();
                                            }
                                        });
                                    });
                                    ui.add_space(4.0);
                                    ui.separator();
                                    ui.add_space(2.0);

                                    let procs = self.processes.all();
                                    if procs.is_empty() {
                                        ui.vertical_centered(|ui| {
                                            ui.add_space(10.0);
                                            ui.label(egui::RichText::new("No active or recent processes").color(th.dim));
                                            ui.add_space(10.0);
                                        });
                                    } else {
                                        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                                            for p in &procs {
                                                ui.horizontal(|ui| {
                                                    let is_run = p.status == ProcessStatus::Running;
                                                    let status_color = match &p.status {
                                                        ProcessStatus::Running => th.accent,
                                                        ProcessStatus::Completed => Color32::from_rgb(166, 227, 161),
                                                        ProcessStatus::Failed(_) => Color32::from_rgb(243, 139, 168),
                                                    };
                                                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                                                    ui.painter().circle_filled(dot.center(), 3.5, status_color);

                                                    ui.vertical(|ui| {
                                                        ui.horizontal(|ui| {
                                                            ui.label(egui::RichText::new(&p.title).strong().size(th.font - 1.0));
                                                            if let Some(dur) = p.finished.map(|f| f.duration_since(p.started)) {
                                                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                                    ui.label(egui::RichText::new(format!("{:.1}s", dur.as_secs_f32())).small().color(th.dim));
                                                                });
                                                            }
                                                        });
                                                        if !p.detail.is_empty() {
                                                            ui.label(egui::RichText::new(&p.detail).small().color(th.dim));
                                                        }
                                                        if is_run {
                                                            if let Some(pct) = p.progress {
                                                                ui.add(egui::ProgressBar::new(pct).desired_width(260.0));
                                                            } else {
                                                                ui.spinner();
                                                            }
                                                        } else if let ProcessStatus::Failed(err) = &p.status {
                                                            ui.label(egui::RichText::new(err).small().color(Color32::from_rgb(243, 139, 168)));
                                                        }
                                                    });
                                                });
                                                ui.separator();
                                            }
                                        });
                                    }
                                });
                            });
                        }
                    }

                    // Quick Tools: Options Gear, Terminal, Preview (monotone vector icons, 28px height)
                    let preview_active = self.prefs.show_preview && matches!(self.prefs.view, ViewMode::Details | ViewMode::Icons);
                    let term_active = self.terminal.open;

                    let tools_frame = egui::Frame::NONE
                        .fill(th.pane)
                        .corner_radius(CornerRadius::same(th.radius.min(6)))
                        .stroke(Stroke::new(1.0_f32, th.hairline()))
                        .inner_margin(egui::Margin::symmetric(3, 2));

                    tools_frame.show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 1.0;

                        // Options Gear
                        let menu_resp = ui.menu_button(egui::RichText::new("  ").size(th.font), |ui| {
                            let mut hidden = self.prefs.show_hidden;
                            if ui.checkbox(&mut hidden, "Show hidden items  (Ctrl+H)").clicked() {
                                actions.push(Action::ToggleHidden);
                                ui.close_menu();
                            }
                            let mut preview = self.prefs.show_preview;
                            if ui.checkbox(&mut preview, "Preview pane").clicked() {
                                actions.push(Action::TogglePreview);
                                ui.close_menu();
                            }
                            let mut ext = self.prefs.show_extensions;
                            if ui.checkbox(&mut ext, "File name extensions").clicked() {
                                self.prefs.show_extensions = ext;
                                self.prefs.save();
                                ui.close_menu();
                            }
                            ui.separator();
                            ui.label(egui::RichText::new("Appearance").small().color(th.dim));
                            use chickenwm_core::theme::AppMode;
                            for (m, label) in [(AppMode::System, "Follow system"), (AppMode::Light, "Light"), (AppMode::Dark, "Dark")] {
                                if ui.radio(self.app_mode == m, label).clicked() {
                                    self.app_mode = m;
                                    if let Err(e) = chickenwm_core::theme::set_app_mode("files", m) {
                                        self.toast(format!("Couldn't change appearance: {e}"), true);
                                    }
                                    ui.close_menu();
                                }
                            }
                            ui.separator();
                            if ui.button("Dual pane  (F3 / Ctrl+D)").clicked() {
                                actions.push(Action::ToggleDualPane);
                                ui.close_menu();
                            }
                            if ui.button("Embedded terminal  (Ctrl+`)").clicked() {
                                actions.push(Action::ToggleTerminal);
                                ui.close_menu();
                            }
                            if ui.button("Undo  (Ctrl+Z)").clicked() {
                                actions.push(Action::Undo);
                                ui.close_menu();
                            }
                            if ui.button("Go anywhere…  (Ctrl+P)").clicked() {
                                actions.push(Action::GoAnywhere);
                                ui.close_menu();
                            }
                            ui.separator();
                            if ui.button("New folder  (Ctrl+Shift+N)").clicked() {
                                actions.push(Action::NewFolder(self.pane(side).cwd.clone()));
                                ui.close_menu();
                            }
                            if ui.button("Open in External Terminal").clicked() {
                                actions.push(Action::Terminal(self.pane(side).cwd.clone()));
                                ui.close_menu();
                            }
                            ui.separator();
                            ui.label(egui::RichText::new("Shortcuts: F3 Dual · Ctrl+` Terminal · Ctrl+L Address").small().color(th.dim));
                        });
                        let gear_color = if menu_resp.response.hovered() { th.fg } else { th.dim };
                        paint_gear_icon(ui.painter(), menu_resp.response.rect, gear_color);

                        // Terminal button
                        let (term_r, term_resp) = ui.allocate_exact_size(egui::vec2(22.0, 24.0), Sense::click());
                        if term_active {
                            ui.painter().rect_filled(term_r, CornerRadius::same(4), th.accent.gamma_multiply(0.18));
                        } else if term_resp.hovered() {
                            ui.painter().rect_filled(term_r, CornerRadius::same(4), th.fg.gamma_multiply(0.08));
                        }
                        paint_terminal_icon(ui.painter(), term_r, if term_active { th.accent } else { th.dim });
                        if term_resp.on_hover_text("Toggle Embedded Quick Terminal (Ctrl+`)").clicked() {
                            actions.push(Action::ToggleTerminal);
                        }

                        // Preview button
                        let (prev_r, prev_resp) = ui.allocate_exact_size(egui::vec2(22.0, 24.0), Sense::click());
                        if preview_active {
                            ui.painter().rect_filled(prev_r, CornerRadius::same(4), th.accent.gamma_multiply(0.18));
                        } else if prev_resp.hovered() {
                            ui.painter().rect_filled(prev_r, CornerRadius::same(4), th.fg.gamma_multiply(0.08));
                        }
                        paint_preview_icon(ui.painter(), prev_r, if preview_active { th.accent } else { th.dim }, preview_active, th.accent);
                        if prev_resp.on_hover_text("Toggle Preview pane").clicked() {
                            actions.push(Action::TogglePreview);
                        }
                    });

                    // View Mode Dropdown
                    if let Some(v) = view_dropdown(ui, th, self.prefs.view) {
                        actions.push(Action::SetView(v));
                    }

                    // Search Box (monotone vector search & clear icons, unified 28px height)
                    let (filter_text, folder_title) = {
                        let pane = self.pane(side);
                        (pane.filter.clone(), pane.title())
                    };
                    let hint = if folder_title.is_empty() { "Search…".to_string() } else { format!("Search {folder_title}…") };
                    let mut filter_buf = filter_text;

                    let (resp, clear_clicked) = {
                        let search_frame = egui::Frame::NONE
                            .fill(th.pane)
                            .corner_radius(CornerRadius::same(th.radius.min(6)))
                            .stroke(Stroke::new(1.0_f32, th.hairline()))
                            .inner_margin(egui::Margin::symmetric(4, 2));

                        search_frame.show(ui, |ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;

                            // In right-to-left: first item added appears on the right!
                            let clear_btn = if !filter_buf.is_empty() {
                                let (clr_r, clr_resp) = ui.allocate_exact_size(egui::vec2(16.0, 24.0), Sense::click());
                                let color = if clr_resp.hovered() { th.fg } else { th.dim };
                                paint_clear_icon(ui.painter(), clr_r, color);
                                clr_resp.on_hover_text("Clear search").clicked()
                            } else {
                                false
                            };

                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut filter_buf)
                                    .hint_text(hint)
                                    .desired_width(170.0)
                                    .frame(false)
                                    .font(FontId::proportional(th.font - 1.5))
                                    .margin(egui::Margin::symmetric(2, 2)),
                            );

                            let (ic_r, _) = ui.allocate_exact_size(egui::vec2(16.0, 24.0), Sense::hover());
                            paint_search_icon(ui.painter(), ic_r, th.dim);

                            (resp, clear_btn)
                        }).inner
                    };
                    if self.focus_search {
                        resp.request_focus();
                        self.focus_search = false;
                    }
                    if resp.changed() {
                        let prefs = self.prefs.clone();
                        let pane = self.pane_mut(side);
                        pane.filter = filter_buf.clone();
                        pane.rebuild(&prefs);
                    }
                    if clear_clicked {
                        let prefs = self.prefs.clone();
                        let pane = self.pane_mut(side);
                        pane.filter.clear();
                        pane.rebuild(&prefs);
                    }
                    if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        let prefs = self.prefs.clone();
                        let pane = self.pane_mut(side);
                        pane.filter.clear();
                        pane.rebuild(&prefs);
                        resp.surrender_focus();
                    }

                    // Window drag on empty space between search bar and address bar
                    let empty_rect = ui.available_rect_before_wrap();
                    if empty_rect.width() > 10.0 {
                        let drag_resp = ui.interact(empty_rect, ui.id().with("win_drag_nav"), Sense::drag());
                        if drag_resp.drag_started() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                        }
                        if drag_resp.double_clicked() {
                            let is_max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_max));
                        }
                    }
                });
            });
        });
    }

    fn address_bar(&mut self, ui: &mut egui::Ui, side: Side, width: f32, th: Theme, actions: &mut Vec<Action>) {
        let height = 28.0;
        if let Some(text) = &mut self.address {
            let id = egui::Id::new("address_edit");
            let resp = ui.add(egui::TextEdit::singleline(text).id(id).desired_width(width).margin(egui::Margin::symmetric(8, 5)));
            if !resp.has_focus() && !resp.lost_focus() {
                resp.request_focus();
                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                    let n = text.chars().count();
                    state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(n))));
                    state.store(ui.ctx(), id);
                }
            }
            if resp.lost_focus() {
                let go = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let input = text.clone();
                self.address = None;
                if go {
                    self.go_to_input(&input);
                }
            }
            return;
        }
        let cwd = self.pane(side).cwd.clone();
        let (rect, bg_resp) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
        ui.painter().rect(rect, CornerRadius::same(th.radius.min(8)), th.pane, Stroke::new(1.0_f32, th.hairline()), egui::StrokeKind::Inside);
        if bg_resp.clicked() {
            actions.push(Action::EditAddress);
        }
        bg_resp.on_hover_text("Click to type a path (Ctrl+L)");
        let mut crumbs: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
        crumbs.reverse();
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(6.0, 2.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        child.set_clip_rect(rect.shrink(1.0));
        // When the path is too long, drop leading segments (they stay reachable via Up / typing).
        let mut start = 0;
        let approx = |p: &PathBuf| display_name(p).len() as f32 * th.font * 0.6 + 22.0;
        while start + 1 < crumbs.len() && crumbs[start..].iter().map(approx).sum::<f32>() > width - 20.0 {
            start += 1;
        }
        if start > 0 {
            child.label(egui::RichText::new("…").color(th.dim));
        }
        for (i, crumb) in crumbs.iter().enumerate().skip(start) {
            if i > start {
                child.label(egui::RichText::new("›").color(th.dim));
            }
            let last = i + 1 == crumbs.len();
            let name = display_name(crumb);
            let resp = child.add(egui::Button::new(egui::RichText::new(name).color(if last { th.fg } else { th.dim })).frame(false));
            if resp.clicked() {
                actions.push(Action::Navigate(side, crumb.clone()));
            }
            self.zones.push(DropZone { rect: resp.rect, dir: crumb.clone() });
            let crumb = crumb.clone();
            resp.context_menu(|ui| {
                if ui.button("Open in new tab").clicked() {
                    actions.push(Action::OpenNewTab(crumb.clone()));
                    ui.close_menu();
                }
                if ui.button("Copy address").clicked() {
                    actions.push(Action::CopyPath(vec![crumb.clone()]));
                    ui.close_menu();
                }
                if ui.button("Properties").clicked() {
                    actions.push(Action::Properties(vec![crumb.clone()]));
                    ui.close_menu();
                }
            });
        }
    }

    fn draw_status(&mut self, ctx: &egui::Context, th: Theme) {
        let frame = egui::Frame::NONE
            .fill(th.pane)
            .stroke(Stroke::new(1.0_f32, th.hairline()))
            .inner_margin(egui::Margin::symmetric(12, 5));
        egui::TopBottomPanel::bottom("files_status").frame(frame).show(ctx, |ui| {

            ui.horizontal(|ui| {
                let pane = self.pane(self.focused());
                let mut text = format!("{} items", pane.visible.len());
                if !pane.selection.is_empty() {
                    let sel_size: u64 = pane
                        .entries
                        .iter()
                        .filter(|e| pane.selection.contains(&e.path) && !e.is_dir)
                        .map(|e| e.size)
                        .sum();
                    text.push_str(&format!("  ·  {} selected", pane.selection.len()));
                    if sel_size > 0 {
                        text.push_str(&format!(" ({})", format_bytes(sel_size)));
                    }
                }
                ui.label(egui::RichText::new(text).size(th.font - 1.5).color(th.dim));

                let running = self.jobs.running.load(Ordering::Relaxed);
                let status = self.jobs.status.lock().clone();
                let counting_folders = !self.tools.folder_size_stops.lock().is_empty();
                let mut cancel_folders = false;
                if running > 0 {
                    ui.separator();
                    match *self.jobs.progress.lock() {
                        Some(p) => {
                            ui.add(egui::ProgressBar::new(p).desired_width(140.0).text(status));
                        }
                        None => {
                            ui.spinner();
                            ui.label(egui::RichText::new(status).size(th.font - 1.5).color(th.dim));
                        }
                    }
                } else if counting_folders {
                    ui.separator();
                    ui.spinner();
                    ui.label(egui::RichText::new("Calculating sizes…").size(th.font - 1.5).color(th.dim));
                    if ui.small_button("Cancel").clicked() {
                        cancel_folders = true;
                    }
                } else if let Some((msg, at, error)) = &self.toast {
                    if at.elapsed() < Duration::from_secs(4) {
                        ui.separator();
                        ui.label(egui::RichText::new(msg).size(th.font - 1.5).color(if *error { Color32::from_rgb(243, 139, 168) } else { th.accent }));
                        ctx.request_repaint_after(Duration::from_millis(500));
                    }
                } else if !status.is_empty() {
                    ui.separator();
                    ui.label(egui::RichText::new(status).size(th.font - 1.5).color(th.dim));
                }
                if cancel_folders {
                    self.cancel_folder_sizes();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some((free, total)) = shell::disk_space(&pane_root(&self.pane(self.focused()).cwd)) {
                        ui.label(
                            egui::RichText::new(format!("{} free / {}", format_bytes(free), format_bytes(total)))
                                .size(th.font - 1.5)
                                .color(th.dim),
                        );
                    }

                    let branch = self.tools.git.branch.lock().clone();
                    let summary = self.tools.git.summary.lock().clone();
                    if let Some(b) = branch {
                        ui.separator();
                        let git_text = if let Some(s) = summary {
                            format!(" {b}  ({s})")
                        } else {
                            format!(" {b}")
                        };
                        ui.label(egui::RichText::new(git_text).size(th.font - 1.5).color(th.accent));
                    }
                });
            });
        });
    }

    /// Finder-style sidebar: "Favorites" (the user's own folders: add, remove, rename, drag to
    /// reorder) and "Locations" (drives). Each section folds away from its header.
    fn draw_sidebar(&mut self, ctx: &egui::Context, th: Theme, actions: &mut Vec<Action>) {
        // Acrylic frosted-glass semi-transparent background
        let sidebar_bg = Color32::from_rgba_unmultiplied(th.sidebar.r(), th.sidebar.g(), th.sidebar.b(), 145);
        let frame = egui::Frame::NONE.fill(sidebar_bg).inner_margin(egui::Margin { left: 10, right: 10, top: 0, bottom: 8 });
        egui::SidePanel::left("files_sidebar")
            .resizable(true)
            .default_width(210.0)
            .width_range(160.0..=340.0)
            .show_separator_line(true)
            .frame(frame)
            .show(ctx, |ui| {
                // Top drag region matching navbar height (~38px)
                let (_, drag_resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::drag());
                if drag_resp.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if drag_resp.double_clicked() {
                    let is_max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_max));
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    let cwd = self.pane(self.focused()).cwd.clone();
                    let side = self.focused();

                    let is_folder_drag = self.drag.as_ref().map_or(false, |d| d.paths.iter().any(|p| p.is_dir()));
                    if self.sidebar_section(ui, th, "Favorites", &cwd, actions) {
                        let favorites = self.prefs.favorites().to_vec();
                        let mut rows = Vec::with_capacity(favorites.len());
                        let fav_start_y = ui.cursor().top();
                        for (i, fav) in favorites.iter().enumerate() {
                            let key = format!("path:{}", fav.path.display());
                            let resp = self.sidebar_item(ui, th, &fav.label(), &key, &fav.path, cwd == fav.path, side, Some(i), None, actions);
                            if resp.drag_started() && self.fav_rename.is_none() {
                                self.fav_drag = Some(i);
                            }
                            rows.push(resp.rect);
                        }
                        if favorites.is_empty() {
                            ui.add_space(2.0);
                            let r = ui.label(egui::RichText::new("Add folders with + or drag them here").small().color(th.dim)).rect;
                            rows.push(r);
                        }
                        let fav_end_y = ui.cursor().bottom() + 10.0;
                        let fav_rect = Rect::from_min_max(
                            Pos2::new(ui.min_rect().left(), fav_start_y - 24.0),
                            Pos2::new(ui.min_rect().right(), fav_end_y),
                        );

                        // Drag folders directly into Favorites section
                        if is_folder_drag {
                            if let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) {
                                if fav_rect.contains(pointer) {
                                    self.fav_drop_hover = true;
                                    let to = rows.iter().position(|r| pointer.y < r.center().y).unwrap_or(favorites.len());
                                    let y = if to < rows.len() {
                                        rows[to].top()
                                    } else if !rows.is_empty() {
                                        rows[rows.len() - 1].bottom()
                                    } else {
                                        fav_start_y
                                    };
                                    let left = ui.min_rect().left() + 8.0;
                                    let right = ui.min_rect().right() - 8.0;

                                    // Visual drop indicator line & circle
                                    ui.painter().line_segment([Pos2::new(left + 6.0, y), Pos2::new(right, y)], Stroke::new(2.5_f32, th.accent));
                                    ui.painter().circle_filled(Pos2::new(left + 4.0, y), 3.5, th.accent);

                                    // Drop onto Favorites
                                    if ui.input(|i| i.pointer.any_released()) {
                                        if let Some(drag) = self.drag.take() {
                                            let mut insert_idx = to;
                                            for p in drag.paths {
                                                if p.is_dir() {
                                                    actions.push(Action::PinAt { path: p, index: insert_idx });
                                                    insert_idx += 1;
                                                }
                                            }
                                            self.toast("Added to Favorites", false);
                                        }
                                    }
                                }
                            }
                        }

                        self.favorite_reorder(ui, th, &rows, &favorites, actions);
                    }

                    ui.add_space(10.0);
                    if self.sidebar_section(ui, th, "Locations", &cwd, actions) {
                        let drives: Vec<(PathBuf, String, Option<(u64, u64)>)> =
                            self.drives.iter().map(|d| (d.root.clone(), d.label.clone(), d.space)).collect();
                        for (root, label, space) in drives {
                            let letter = root.display().to_string().trim_end_matches('\\').to_string();
                            let name = format!("{label} ({letter})");
                            let key = format!("path:{}", root.display());
                            self.sidebar_item(ui, th, &name, &key, &root, cwd == root, side, None, space, actions);
                        }
                    }
                });


            });
    }

    /// A section header (small, dim, like Finder's). Click folds the section; Favorites also
    /// gets a + (add the current folder) on hover and a menu to restore the defaults.
    /// Returns whether the section is expanded.
    fn sidebar_section(&mut self, ui: &mut egui::Ui, th: Theme, title: &'static str, cwd: &Path, actions: &mut Vec<Action>) -> bool {
        let favorites = title == "Favorites";
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), Sense::click());
        let collapsed = self.sidebar_collapsed.contains(title);
        let hovered = ui.rect_contains_pointer(rect);
        ui.painter().text(
            Pos2::new(rect.left() + 6.0, rect.center().y + 1.0),
            Align2::LEFT_CENTER,
            title.to_uppercase(),
            FontId::proportional(th.font - 3.0),
            th.dim,
        );
        let mut x = rect.right() - 4.0;
        if favorites && hovered {
            let plus = Rect::from_center_size(Pos2::new(x - 9.0, rect.center().y), Vec2::splat(18.0));
            let plus_resp = ui.interact(plus, ui.id().with("fav-add"), Sense::click());
            if plus_resp.hovered() {
                ui.painter().rect_filled(plus, 4.0_f32, th.fg.gamma_multiply(0.08));
            }
            ui.painter().text(plus.center(), Align2::CENTER_CENTER, "+", FontId::proportional(th.font + 1.0), th.dim);
            if plus_resp.on_hover_text("Add this folder to Favorites").clicked() {
                actions.push(Action::Pin(cwd.to_path_buf()));
            }
            x -= 22.0;
        }
        if hovered {
            ui.painter().text(
                Pos2::new(x, rect.center().y + 1.0),
                Align2::RIGHT_CENTER,
                if collapsed { "Show" } else { "Hide" },
                FontId::proportional(th.font - 3.0),
                th.dim,
            );
        }
        if resp.clicked() {
            if collapsed {
                self.sidebar_collapsed.remove(title);
            } else {
                self.sidebar_collapsed.insert(title);
            }
        }
        if favorites {
            let cwd = cwd.to_path_buf();
            resp.context_menu(|ui| {
                if ui.button("Add this folder").clicked() {
                    actions.push(Action::Pin(cwd.clone()));
                    ui.close_menu();
                }
                if ui.button("Restore default favorites").clicked() {
                    actions.push(Action::ResetFavorites);
                    ui.close_menu();
                }
            });
        }
        !collapsed
    }

    /// While a favorite is dragged: an insertion line where it would land and a floating label
    /// under the pointer; dropping moves it there.
    fn favorite_reorder(&mut self, ui: &mut egui::Ui, th: Theme, rows: &[Rect], favorites: &[Favorite], actions: &mut Vec<Action>) {
        let Some(from) = self.fav_drag else { return };
        let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) else { return };
        if rows.is_empty() || from >= favorites.len() {
            self.fav_drag = None;
            return;
        }
        let to = rows.iter().position(|r| pointer.y < r.center().y).unwrap_or(rows.len());
        let y = if to < rows.len() { rows[to].top() } else { rows[rows.len() - 1].bottom() };
        let left = rows[0].left() + 6.0;
        let right = rows[0].right() - 6.0;
        if to != from && to != from + 1 {
            ui.painter().line_segment([Pos2::new(left + 4.0, y), Pos2::new(right, y)], Stroke::new(2.0_f32, th.accent));
            ui.painter().circle_stroke(Pos2::new(left, y), 3.0, Stroke::new(2.0_f32, th.accent));
        }
        // The dragged favorite follows the pointer.
        let ghost = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("fav-drag-ghost")));
        let galley = ghost.layout_no_wrap(favorites[from].label(), FontId::proportional(th.font - 0.5), th.fg);
        let pill = Rect::from_min_size(pointer + egui::vec2(12.0, 6.0), galley.size() + egui::vec2(16.0, 8.0));
        ghost.rect_filled(pill, 6.0_f32, th.pane);
        ghost.rect_stroke(pill, 6.0_f32, Stroke::new(1.0_f32, th.border), egui::StrokeKind::Inside);
        ghost.galley(pill.min + egui::vec2(8.0, 4.0), galley, th.fg);
        if ui.input(|i| i.pointer.any_released()) {
            actions.push(Action::MoveFavorite { from, to });
            self.fav_drag = None;
        }
    }

    /// One sidebar row: icon + name in a rounded pill. `favorite` is the row's index in
    /// Favorites (rows there can be dragged, renamed and removed); drives show a usage bar.
    #[allow(clippy::too_many_arguments)]
    fn sidebar_item(&mut self, ui: &mut egui::Ui, th: Theme, name: &str, icon_key: &str, path: &Path, active: bool, side: Side, favorite: Option<usize>, space: Option<(u64, u64)>, actions: &mut Vec<Action>) -> egui::Response {
        let h = if space.is_some() { th.row_h + 10.0 } else { th.row_h };
        let sense = if favorite.is_some() { Sense::click_and_drag() } else { Sense::click() };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), sense);
        let radius = CornerRadius::same(th.radius.clamp(4, 7));
        let dragging_this = favorite.is_some() && self.fav_drag == favorite;
        if active {
            ui.painter().rect_filled(rect, radius, th.selected);
        } else if resp.hovered() && self.fav_drag.is_none() {
            ui.painter().rect_filled(rect, radius, th.fg.gamma_multiply(0.06));
        }
        let fade = if dragging_this { 0.4 } else { 1.0 };
        let row_center = Pos2::new(rect.left(), rect.top() + th.row_h / 2.0);
        let icon = Rect::from_center_size(row_center + egui::vec2(15.0, 0.0), Vec2::splat(16.0));
        paint_icon(ui, &mut self.icons, icon_key, false, icon, true, &th);

        let renaming = favorite.is_some() && self.fav_rename.as_ref().is_some_and(|r| r.0 == path);
        if renaming {
            let field = Rect::from_min_max(Pos2::new(rect.left() + 28.0, rect.top() + 2.0), Pos2::new(rect.right() - 2.0, rect.bottom() - 2.0));
            let (enter, esc) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
            if let Some((_, buffer, focused)) = self.fav_rename.as_mut() {
                let edit = ui.put(field, egui::TextEdit::singleline(buffer).margin(egui::Margin::symmetric(4, 1)).font(FontId::proportional(th.font - 0.5)));
                if !*focused {
                    edit.request_focus();
                    *focused = true;
                } else if esc {
                    self.fav_rename = None;
                } else if enter || edit.lost_focus() {
                    if let Some((path, buffer, _)) = self.fav_rename.take() {
                        actions.push(Action::RenameFavorite { path, name: Some(buffer) });
                    }
                }
            }
        } else {
            ui.painter().with_clip_rect(rect.shrink(2.0)).text(
                row_center + egui::vec2(30.0, 0.0),
                Align2::LEFT_CENTER,
                name,
                FontId::proportional(th.font - 0.5),
                th.fg.gamma_multiply(fade),
            );
        }
        if let Some((free, total)) = space.filter(|(_, t)| *t > 0) {
            let used = (1.0 - free as f32 / total as f32).clamp(0.0, 1.0);
            let bar = Rect::from_min_size(Pos2::new(rect.left() + 30.0, rect.bottom() - 8.0), egui::vec2(rect.width() - 40.0, 3.5));
            ui.painter().rect_filled(bar, 2.0_f32, th.border);
            let fill = if used > 0.90 {
                Color32::from_rgb(243, 139, 168)
            } else if used > 0.80 {
                Color32::from_rgb(249, 226, 175)
            } else {
                th.accent
            };
            ui.painter().rect_filled(Rect::from_min_size(bar.min, egui::vec2(bar.width() * used, 3.5)), 2.0_f32, fill);
        }
        if resp.clicked() && !renaming {
            actions.push(Action::Navigate(side, path.to_path_buf()));
        }
        if resp.middle_clicked() {
            actions.push(Action::OpenNewTab(path.to_path_buf()));
        }
        if let Some((free, total)) = space {
            resp.clone().on_hover_text(format!("{} free of {}", format_bytes(free), format_bytes(total)));
        }
        let is_favorite = favorite.is_some();
        let is_folder_drag = self.drag.as_ref().map_or(false, |d| d.paths.iter().any(|p| p.is_dir()));
        if self.fav_drag.is_none() && (!is_favorite || !is_folder_drag) {
            // Files dropped on a sidebar folder go into it. But when dragging a folder over Favorites,
            // we pin it to Favorites instead of moving into a subfolder!
            self.zones.push(DropZone { rect, dir: path.to_path_buf() });
        }
        let path = path.to_path_buf();
        let is_favorite = favorite.is_some();
        let label = name.to_string();
        let mut rename = None;
        resp.context_menu(|ui| {
            ui.set_min_width(200.0);
            if ui.button("Open").clicked() {
                actions.push(Action::Navigate(side, path.clone()));
                ui.close_menu();
            }
            if ui.button("Open in new tab").clicked() {
                actions.push(Action::OpenNewTab(path.clone()));
                ui.close_menu();
            }
            if ui.button("Open in Terminal").clicked() {
                actions.push(Action::Terminal(path.clone()));
                ui.close_menu();
            }
            ui.separator();
            if is_favorite {
                if ui.button("Rename favorite…").clicked() {
                    rename = Some(label.clone());
                    ui.close_menu();
                }
                if ui.button("Remove from Favorites").clicked() {
                    actions.push(Action::Unpin(path.clone()));
                    ui.close_menu();
                }
            } else if ui.button("Add to Favorites").clicked() {
                actions.push(Action::Pin(path.clone()));
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Properties").clicked() {
                actions.push(Action::Properties(vec![path.clone()]));
                ui.close_menu();
            }
        });
        if let Some(current) = rename {
            self.fav_rename = Some((path, current, false));
        }
        resp
    }

    fn draw_preview(&mut self, ui: &mut egui::Ui, th: Theme) {
        let current = self.pane(self.focused()).current().cloned();
        let Some(entry) = current else {
            ui.centered_and_justified(|ui| ui.label(egui::RichText::new("Select a file to preview").color(th.dim)));
            return;
        };
        ui.vertical_centered(|ui| {
            ui.add_space(6.0);
            let icon_rect = ui.allocate_exact_size(Vec2::splat(56.0), Sense::hover()).0;
            paint_icon(ui, &mut self.icons, &entry.icon_key, true, icon_rect, entry.is_dir, &th);
            ui.add_space(4.0);
            ui.add(egui::Label::new(egui::RichText::new(&entry.name).strong().size(th.font + 1.0).color(th.name_color(&entry))).wrap());
            let mut meta = type_label(&self.icons, &entry);
            if !entry.is_dir {
                meta.push_str(&format!(" · {}", format_bytes(entry.size)));
            }
            ui.label(egui::RichText::new(meta).small().color(th.dim));
            if let Some(t) = entry.modified {
                ui.label(egui::RichText::new(format!("Modified {}", format_time(t))).small().color(th.dim));
            }
            ui.add_space(8.0);
        });
        preview::body(ui, &th, &self.preview, "preview");
    }
    fn draw_center(&mut self, ctx: &egui::Context, th: Theme, actions: &mut Vec<Action>, commit_rename: &mut bool) {
        if self.prefs.show_preview && matches!(self.prefs.view, ViewMode::Details | ViewMode::Icons) {
            let frame = egui::Frame::NONE.fill(th.pane).inner_margin(egui::Margin::symmetric(10, 8));
            egui::SidePanel::right("files_preview").resizable(true).default_width(300.0).width_range(200.0..=600.0).frame(frame).show(ctx, |ui| {


                self.draw_preview(ui, th);
            });
        }
        let frame = egui::Frame::NONE.fill(th.pane).inner_margin(egui::Margin::symmetric(6, 4));
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            if self.tools.archive.is_some() {
                self.draw_archive_screen(ui, th, actions);
                return;
            }
            let view = self.prefs.view;
            let dragging = self.drag.is_some();
            // Split borrows: panes are read, everything else is drawn through `Draw`.
            let main = self.tabs[self.active].clone();
            let second = (view == ViewMode::Dual).then(|| self.second.clone());
            let memo_main = self.scroll.remove(&Side::Main);
            let memo_second = self.scroll.remove(&Side::Second);
            let focus = self.focused();
            if view == ViewMode::Columns && self.parent_cache.0 != main.cwd {
                let parent = main.cwd.parent().map(|p| {
                    let mut v = read_directory(p).unwrap_or_default();
                    v.retain(|e| e.is_dir && (self.prefs.show_hidden || !e.hidden));
                    v.sort_by(|a, b| crate::model::natural_cmp(&a.name, &b.name));
                    v
                });
                self.parent_cache = (main.cwd.clone(), parent.unwrap_or_default());
            }
            let parent_entries = std::mem::take(&mut self.parent_cache.1);
            let mut icon_cols = self.icon_cols;
            let preview_entry = main.current().filter(|e| e.is_dir).map(|e| e.path.clone());
            if view == ViewMode::Columns {
                if let Some(dir) = preview_entry.as_ref().filter(|d| **d != self.child_cache.0) {
                    let mut children = read_directory(dir).unwrap_or_default();
                    children.retain(|e| self.prefs.show_hidden || !e.hidden);
                    children.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| crate::model::natural_cmp(&a.name, &b.name)));
                    self.child_cache = (dir.clone(), children);
                }
            }
            let child_entries = std::mem::take(&mut self.child_cache.1);
            let extras = tools::RowExtras {
                sizes: self.tools.folder_sizes.lock().clone(),
                git: self.tools.git.status.lock().clone(),
            };
            let preview = &self.preview;
            let mut draw = Draw {
                theme: th,
                icons: &mut self.icons,
                prefs: &self.prefs,
                rename: &mut self.rename,
                commit_rename,
                zones: &mut self.zones,
                dragging,
                focused: true,
                actions,
                marquee: &mut self.marquee,
                snap_back: &self.snap_back,
                drag_info: &self.drag,
                extras: &extras,
            };
            match view {
                ViewMode::Details => {
                    let memo = draw.details(ui, Side::Main, &main, false, false, memo_main.as_ref());
                    self.scroll.insert(Side::Main, memo);
                }
                ViewMode::Icons => {
                    let memo = draw.icons_grid(ui, Side::Main, &main, memo_main.as_ref(), &mut icon_cols);
                    self.scroll.insert(Side::Main, memo);
                }
                ViewMode::Columns => {
                    let memo = ui.columns(3, |cols| {
                        let r0 = cols[0].max_rect();
                        cols[0].painter().vline(r0.right(), r0.y_range(), Stroke::new(1.0_f32, th.border));
                        cols[0].label(egui::RichText::new(main.cwd.parent().map(display_name).unwrap_or_default()).small().color(th.dim));
                        draw.name_column(&mut cols[0], "col_parent", &parent_entries, Some(&main.cwd), Side::Main);

                        let r1 = cols[1].max_rect();
                        cols[1].painter().vline(r1.right(), r1.y_range(), Stroke::new(1.0_f32, th.border));
                        let memo = draw.details(&mut cols[1], Side::Main, &main, true, false, memo_main.as_ref());

                        if let Some(dir) = &preview_entry {
                            cols[2].label(egui::RichText::new(display_name(dir)).small().color(th.dim));
                            if child_entries.is_empty() {
                                cols[2].label(egui::RichText::new("This folder is empty").color(th.dim));
                            }
                            draw.name_column(&mut cols[2], "col_child", &child_entries, None, Side::Main);
                        } else {
                            preview::body(&mut cols[2], &th, preview, "col_preview");
                        }
                        memo
                    });
                    self.scroll.insert(Side::Main, memo);
                }
                ViewMode::Dual => {
                    let second = second.expect("dual pane");
                    let (m1, m2) = ui.columns(2, |cols| {
                        let r0 = cols[0].max_rect();
                        cols[0].painter().vline(r0.right(), r0.y_range(), Stroke::new(1.0_f32, th.border));
                        let pane_header = |ui: &mut egui::Ui, side: Side, title: &str, path: &Path, active: bool| {
                            let h_frame = egui::Frame::NONE
                                .fill(if active { th.pane } else { th.sidebar })
                                .stroke(Stroke::new(1.0_f32, if active { th.accent.gamma_multiply(0.5) } else { th.border }))
                                .corner_radius(CornerRadius::same(th.radius.clamp(4, 6)))
                                .inner_margin(egui::Margin::symmetric(8, 5));
                            h_frame.show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let tag_color = if active { th.accent } else { th.dim.gamma_multiply(0.5) };
                                    let tag_fill = if active { th.accent.gamma_multiply(0.18) } else { Color32::TRANSPARENT };
                                    let tag_label = if active { "● ACTIVE" } else { "○ INACTIVE" };
                                    let (tag_rect, _) = ui.allocate_exact_size(Vec2::new(66.0, 18.0), Sense::hover());
                                    ui.painter().rect_filled(tag_rect, CornerRadius::same(3), tag_fill);
                                    ui.painter().rect_stroke(tag_rect, CornerRadius::same(3), Stroke::new(1.0_f32, tag_color), egui::StrokeKind::Inside);
                                    ui.painter().text(tag_rect.center(), Align2::CENTER_CENTER, tag_label, FontId::proportional(th.font - 3.5), tag_color);

                                    let name = if title.is_empty() { display_name(path) } else { title.to_string() };
                                    ui.label(egui::RichText::new(name.to_string()).strong().size(th.font - 1.0).color(if active { th.fg } else { th.dim }));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.label(egui::RichText::new(if side == Side::Main { "Left (Tab)" } else { "Right (Tab)" }).small().color(th.dim));
                                    });
                                });
                            });
                            ui.add_space(2.0);
                        };

                        draw.focused = focus == Side::Main;
                        pane_header(&mut cols[0], Side::Main, &main.title(), &main.cwd, focus == Side::Main);
                        let m1 = draw.details(&mut cols[0], Side::Main, &main, true, true, memo_main.as_ref());

                        draw.focused = focus == Side::Second;
                        pane_header(&mut cols[1], Side::Second, &second.title(), &second.cwd, focus == Side::Second);
                        let m2 = draw.details(&mut cols[1], Side::Second, &second, true, true, memo_second.as_ref());

                        if cols[0].rect_contains_pointer(cols[0].max_rect()) && cols[0].input(|i| i.pointer.any_pressed()) {
                            draw.actions.push(Action::Focus(Side::Main));
                        }
                        if cols[1].rect_contains_pointer(cols[1].max_rect()) && cols[1].input(|i| i.pointer.any_pressed()) {
                            draw.actions.push(Action::Focus(Side::Second));
                        }
                        (m1, m2)
                    });
                    self.scroll.insert(Side::Main, m1);
                    self.scroll.insert(Side::Second, m2);
                }
            }
            let (term_nav, _) = self.terminal.draw(ui, &th);
            if let Some(target) = term_nav {
                actions.push(Action::Navigate(self.focused(), target));
            }
            self.icon_cols = icon_cols;
            self.parent_cache.1 = parent_entries;
            self.child_cache.1 = child_entries;
        });
        // In Columns view the preview of a selected file goes in a bottom strip.
        let _ = ctx;
    }

    fn handle_drag_and_drop(&mut self, ctx: &egui::Context, th: Theme, actions: &mut Vec<Action>) {
        let (pointer, mods, any_down, released, has_pointer) = ctx.input(|i| {
            (i.pointer.latest_pos(), i.modifiers, i.pointer.any_down(), i.pointer.any_released(), i.pointer.has_pointer())
        });
        let screen = ctx.screen_rect();

        // Drop from another app (Explorer, desktop, browser downloads, ...).
        let (hovered, dropped) = ctx.input(|i| (i.raw.hovered_files.len(), i.raw.dropped_files.clone()));
        let dest = self.pane(self.focused()).cwd.clone();
        if hovered > 0 {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("ext_drop")));
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(110));
            painter.text(screen.center(), Align2::CENTER_CENTER, format!("Drop to copy into {}\n(hold Shift to move)", display_name(&dest)), FontId::proportional(20.0), Color32::WHITE);
        }
        if !dropped.is_empty() {
            let sources: Vec<PathBuf> = dropped.into_iter().filter_map(|f| f.path).collect();
            let kind = if key_down(VK_SHIFT.0) { DropKind::Move } else { DropKind::Copy };
            actions.push(Action::Transfer { sources, dest, kind });
        }

        // Right-drag drop menu.
        if let Some(menu) = &self.drop_menu {
            let mut close = false;
            let area = egui::Area::new(egui::Id::new("drop_menu")).order(egui::Order::Foreground).fixed_pos(menu.pos).show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_min_width(200.0);
                    let mut choice = |ui: &mut egui::Ui, label: &str, kind: Option<DropKind>| {
                        if ui.button(label).clicked() {
                            if let Some(kind) = kind {
                                actions.push(Action::Transfer { sources: menu.sources.clone(), dest: menu.dest.clone(), kind });
                            }
                            close = true;
                        }
                    };
                    ui.label(egui::RichText::new(format!("{} item(s) ➡ {}", menu.sources.len(), display_name(&menu.dest))).small().color(th.dim));
                    choice(ui, "Copy here", Some(DropKind::Copy));
                    choice(ui, "Move here", Some(DropKind::Move));
                    choice(ui, "Create shortcuts here", Some(DropKind::Shortcut));
                    ui.separator();
                    choice(ui, "Cancel", None);
                });
            });
            let clicked_outside = ctx.input(|i| i.pointer.any_pressed()) && !area.response.contains_pointer() && menu.opened.elapsed() > Duration::from_millis(150);
            if close || clicked_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.drop_menu = None;
            }
        }

        // Handle dragging individual items OUT of the zip / archive screen
        if let Some(adrag) = &self.archive_drag {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("dnd")));
            let target = self.zones.iter().find(|z| pointer.is_some_and(|p| z.rect.contains(p)));
            if let Some(p) = pointer {
                let tile_pos = p - adrag.grab_offset;
                let tile_rect = Rect::from_min_size(tile_pos, adrag.origin_rect.size());

                painter.rect_filled(tile_rect.expand(2.0), CornerRadius::same(th.radius.min(8)), Color32::from_black_alpha(75));
                painter.rect_filled(tile_rect, CornerRadius::same(th.radius.min(8)), th.pane);
                painter.rect_stroke(tile_rect, CornerRadius::same(th.radius.min(8)), Stroke::new(1.5_f32, th.accent), egui::StrokeKind::Inside);

                let icon_rect = Rect::from_center_size(Pos2::new(tile_rect.left() + 20.0, tile_rect.center().y), Vec2::splat(16.0));
                if adrag.is_dir {
                    chickenwm_core::ui_icons::paint_folder(&painter, icon_rect, th.dir);
                } else {
                    let ext = adrag.name.rsplit('.').next().unwrap_or("");
                    let key = format!("ext:{ext}");
                    paint_icon_painter(&painter, &mut self.icons, &key, false, icon_rect, false, &th);
                }

                painter.text(
                    tile_rect.left_center() + egui::vec2(38.0, 0.0),
                    Align2::LEFT_CENTER,
                    &adrag.name,
                    FontId::proportional(th.font),
                    if adrag.is_dir { th.dir } else { th.fg },
                );

                let size_str = adrag.size.map(|s| format!(" ({})", format_bytes(s))).unwrap_or_default();
                let hint = if let Some(z) = target {
                    format!("➡ Extract {}{size_str} to {}", adrag.name, display_name(&z.dir))
                } else {
                    format!("Extract {}{size_str} (drop onto Favorites or a folder)", adrag.name)
                };
                let h_galley = painter.layout_no_wrap(hint, FontId::proportional(th.font - 1.5), th.fg);
                let h_rect = Rect::from_min_size(p + egui::vec2(16.0, 16.0), h_galley.size() + egui::vec2(16.0, 8.0));
                painter.rect(h_rect, CornerRadius::same(6), th.pane, Stroke::new(1.0_f32, th.accent), egui::StrokeKind::Inside);
                painter.galley(h_rect.min + egui::vec2(8.0, 4.0), h_galley, th.fg);
            }

            if released || !any_down {
                let drag = self.archive_drag.take().unwrap();
                if let Some(z) = target {
                    let archive_path = drag.archive.clone();
                    let member = drag.member.clone();
                    let is_dir = drag.is_dir;
                    let name = drag.name.clone();
                    let dest = z.dir.clone();
                    self.spawn_job(format!("Extracting {name}"), format!("To {}", dest.display()), move |_| {
                        tools::extract_archive_member(&archive_path, &member, is_dir, &dest)
                            .map(|p| format!("Extracted {}", display_name(&p)))
                            .map_err(|e| format!("Extract failed: {e}"))
                    });
                    self.toast(format!("Extracting {} to {}", name, display_name(&z.dir)), false);
                } else {
                    let start_pos = pointer.map(|p| p - drag.grab_offset).unwrap_or(drag.origin_rect.min);
                    let start_rect = Rect::from_min_size(start_pos, drag.origin_rect.size());
                    self.archive_snap_back = Some(SnapBackAnim {
                        path: PathBuf::from(&drag.member),
                        icon_key: format!("ext:{}", drag.name.rsplit('.').next().unwrap_or("")),
                        is_dir: drag.is_dir,
                        name: drag.name,
                        start_rect,
                        target_rect: drag.origin_rect,
                        start_time: Instant::now(),
                        duration: Duration::from_millis(180),
                    });
                    ctx.request_repaint();
                }
            }
            return;
        }

        // Archive snap-back animation
        if let Some(anim) = &self.archive_snap_back {
            let t = (anim.start_time.elapsed().as_secs_f32() / anim.duration.as_secs_f32()).clamp(0.0, 1.0);
            if t < 1.0 {
                let ease = 1.0 - (1.0 - t).powi(3);
                let cur_min = anim.start_rect.min.lerp(anim.target_rect.min, ease);
                let cur_rect = Rect::from_min_size(cur_min, anim.target_rect.size());
                let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("archive_snapback")));

                let alpha = (60.0 * (1.0 - ease)) as u8;
                painter.rect_filled(cur_rect.expand(2.0), CornerRadius::same(th.radius.min(8)), Color32::from_black_alpha(alpha));
                painter.rect_filled(cur_rect, CornerRadius::same(th.radius.min(8)), th.pane);
                painter.rect_stroke(cur_rect, CornerRadius::same(th.radius.min(8)), Stroke::new(1.2_f32, th.accent.gamma_multiply(1.0 - ease)), egui::StrokeKind::Inside);

                let icon_rect = Rect::from_center_size(Pos2::new(cur_rect.left() + 20.0, cur_rect.center().y), Vec2::splat(16.0));
                if anim.is_dir {
                    chickenwm_core::ui_icons::paint_folder(&painter, icon_rect, th.dir);
                } else {
                    paint_icon_painter(&painter, &mut self.icons, &anim.icon_key, false, icon_rect, false, &th);
                }

                painter.text(cur_rect.left_center() + egui::vec2(38.0, 0.0), Align2::LEFT_CENTER, &anim.name, FontId::proportional(th.font), th.fg);
                ctx.request_repaint();
            } else {
                self.archive_snap_back = None;
                ctx.request_repaint();
            }
        }

        let Some(drag) = &self.drag else { return };
        // Left the window while still holding the button: hand over to Windows drag and drop.
        let outside = !has_pointer || pointer.is_some_and(|p| !screen.shrink(2.0).contains(p));
        if outside && any_down {
            let drag = self.drag.take().unwrap();
            if shell::drag_out(self.hwnd, &drag.paths, drag.secondary) {
                self.dirty.store(true, Ordering::Relaxed);
            }
            return;
        }
        let target = pointer.and_then(|p| {
            self.zones.iter().rev().find(|z| {
                z.rect.contains(p)
                    && !drag.paths.iter().any(|s| s == &z.dir || z.dir.starts_with(s) && s.is_dir())
            })
        });
        let kind = target.map(|z| default_drop_kind(&drag.paths, &z.dir, mods));
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("dnd")));
        if let Some(z) = target {
            painter.rect_stroke(z.rect.expand(1.0), CornerRadius::same(th.radius.min(6)), Stroke::new(2.0_f32, th.accent), egui::StrokeKind::Outside);
        }
        if let Some(p) = pointer {
            let tile_pos = p - drag.grab_offset;
            let tile_rect = Rect::from_min_size(tile_pos, drag.origin_rect.size());

            // 1. Floating tile shadow & background
            painter.rect_filled(tile_rect.expand(2.0), CornerRadius::same(th.radius.min(8)), Color32::from_black_alpha(75));
            painter.rect_filled(tile_rect, CornerRadius::same(th.radius.min(8)), th.pane);
            painter.rect_stroke(tile_rect, CornerRadius::same(th.radius.min(8)), Stroke::new(1.5_f32, th.accent), egui::StrokeKind::Inside);

            // 2. Icon inside moving tile
            let icon_size = (tile_rect.height() * 0.46).clamp(24.0, 52.0);
            let icon_y = tile_rect.top() + icon_size * 0.5 + 8.0;
            let icon_rect = Rect::from_center_size(Pos2::new(tile_rect.center().x, icon_y), Vec2::splat(icon_size));
            paint_icon_painter(&painter, &mut self.icons, &drag.icon_key, true, icon_rect, drag.is_dir, &th);

            // 3. Label inside moving tile
            let font_size = (th.font - 1.5).max(10.0);
            let mut job = egui::text::LayoutJob::simple(
                drag.name.clone(),
                FontId::proportional(font_size),
                th.fg,
                (tile_rect.width() - 8.0).max(40.0),
            );
            job.wrap.max_rows = 2;
            job.wrap.overflow_character = Some('…');
            job.halign = egui::Align::Center;
            let galley = ctx.fonts(|f| f.layout_job(job));
            let lx = tile_rect.center().x - galley.size().x / 2.0;
            let ly = (icon_rect.bottom() + 4.0).min(tile_rect.bottom() - 16.0);
            painter.galley(Pos2::new(lx, ly), galley, th.fg);

            // 4. Count badge for multi-item drag
            if drag.paths.len() > 1 {
                let badge_rect = Rect::from_center_size(Pos2::new(tile_rect.right() - 4.0, tile_rect.top() + 4.0), Vec2::splat(18.0));
                painter.rect_filled(badge_rect, CornerRadius::same(9), th.accent);
                painter.text(badge_rect.center(), Align2::CENTER_CENTER, drag.paths.len().to_string(), FontId::proportional(th.font - 3.0), Color32::WHITE);
            }

            // 5. Target / Action hint badge
            let hint = if self.fav_drop_hover {
                Some("📌 Add to Favorites".to_string())
            } else if let Some(z) = target {
                let what = if drag.paths.len() == 1 { display_name(&drag.paths[0]) } else { format!("{} items", drag.paths.len()) };
                match (kind, drag.secondary) {
                    (_, true) => Some(format!("{what} ➡ {} (options)", display_name(&z.dir))),
                    (Some(DropKind::Copy), _) => Some(format!("+ Copy {what} to {}", display_name(&z.dir))),
                    (Some(DropKind::Shortcut), _) => Some(format!("🔗 Shortcut in {}", display_name(&z.dir))),
                    _ => Some(format!("➡ Move {what} to {}", display_name(&z.dir))),
                }
            } else {
                None
            };
            if let Some(text) = hint {
                let h_galley = painter.layout_no_wrap(text, FontId::proportional(th.font - 1.5), th.fg);
                let h_rect = Rect::from_min_size(p + egui::vec2(16.0, 16.0), h_galley.size() + egui::vec2(16.0, 8.0));
                painter.rect(h_rect, CornerRadius::same(6), th.pane, Stroke::new(1.0_f32, th.accent), egui::StrokeKind::Inside);
                painter.galley(h_rect.min + egui::vec2(8.0, 4.0), h_galley, th.fg);
            }
        }
        if released || !any_down {
            let drag = self.drag.take().unwrap();
            if let (Some(z), Some(kind)) = (target.map(|z| z.dir.clone()), kind) {
                if drag.secondary {
                    self.drop_menu = Some(DropMenu { pos: pointer.unwrap_or_default(), sources: drag.paths, dest: z, opened: Instant::now() });
                } else {
                    actions.push(Action::Transfer { sources: drag.paths, dest: z, kind });
                }
            }
            else if !self.fav_drop_hover {
                let start_pos = pointer.map(|p| p - drag.grab_offset).unwrap_or(drag.origin_rect.min);
                let start_rect = Rect::from_min_size(start_pos, drag.origin_rect.size());
                self.snap_back = Some(SnapBackAnim {
                    path: drag.paths[0].clone(),
                    icon_key: drag.icon_key,
                    is_dir: drag.is_dir,
                    name: drag.name,
                    start_rect,
                    target_rect: drag.origin_rect,
                    start_time: Instant::now(),
                    duration: Duration::from_millis(180),
                });
                ctx.request_repaint();
            }
        }

        // Smooth snap-back animation when item is not moved elsewhere
        if let Some(anim) = &self.snap_back {
            let t = (anim.start_time.elapsed().as_secs_f32() / anim.duration.as_secs_f32()).clamp(0.0, 1.0);
            if t < 1.0 {
                let ease = 1.0 - (1.0 - t).powi(3); // smooth cubic ease out
                let cur_min = anim.start_rect.min.lerp(anim.target_rect.min, ease);
                let cur_rect = Rect::from_min_size(cur_min, anim.target_rect.size());
                let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("snapback")));

                // Floating tile shadow & background (fading out)
                let alpha = (60.0 * (1.0 - ease)) as u8;
                painter.rect_filled(cur_rect.expand(2.0), CornerRadius::same(th.radius.min(8)), Color32::from_black_alpha(alpha));
                painter.rect_filled(cur_rect, CornerRadius::same(th.radius.min(8)), th.pane);
                painter.rect_stroke(cur_rect, CornerRadius::same(th.radius.min(8)), Stroke::new(1.2_f32, th.accent.gamma_multiply(1.0 - ease)), egui::StrokeKind::Inside);

                // Icon
                let icon_size = (cur_rect.height() * 0.46).clamp(24.0, 52.0);
                let icon_y = cur_rect.top() + icon_size * 0.5 + 8.0;
                let icon_rect = Rect::from_center_size(Pos2::new(cur_rect.center().x, icon_y), Vec2::splat(icon_size));
                paint_icon_painter(&painter, &mut self.icons, &anim.icon_key, true, icon_rect, anim.is_dir, &th);

                // Label
                let font_size = (th.font - 1.5).max(10.0);
                let mut job = egui::text::LayoutJob::simple(
                    anim.name.clone(),
                    FontId::proportional(font_size),
                    th.fg,
                    (cur_rect.width() - 8.0).max(40.0),
                );
                job.wrap.max_rows = 2;
                job.wrap.overflow_character = Some('…');
                job.halign = egui::Align::Center;
                let galley = ctx.fonts(|f| f.layout_job(job));
                let lx = cur_rect.center().x - galley.size().x / 2.0;
                let ly = (icon_rect.bottom() + 4.0).min(cur_rect.bottom() - 16.0);
                painter.galley(Pos2::new(lx, ly), galley, th.fg);

                ctx.request_repaint();
            } else {
                self.snap_back = None;
                ctx.request_repaint();
            }
        }
    }
}

/// Finder's view switcher: one rounded group with the current view raised. Returns a new pick.
fn view_dropdown(ui: &mut egui::Ui, th: Theme, current: ViewMode) -> Option<ViewMode> {
    let mut picked = None;
    let (icon_fn, title): (fn(&egui::Painter, Rect, Color32), &str) = match current {
        ViewMode::Details => (paint_view_list, "List"),
        ViewMode::Icons => (paint_view_icons, "Icons"),
        ViewMode::Columns => (paint_view_columns, "Columns"),
        ViewMode::Dual => (paint_view_dual, "Dual"),
    };

    let label = format!("     {title}   ");
    let btn = egui::Button::new(egui::RichText::new(label).size(th.font - 0.5).color(th.fg))
        .fill(th.pane)
        .stroke(Stroke::new(1.0_f32, th.hairline()))
        .corner_radius(CornerRadius::same(th.radius.min(6)))
        .min_size(egui::vec2(0.0, 28.0));

    let menu_resp = egui::menu::menu_custom_button(ui, btn, |ui| {
        ui.set_min_width(190.0);
        let items = [
            (ViewMode::Details, "List", "Ctrl+Shift+1", paint_view_list as fn(&egui::Painter, Rect, Color32)),
            (ViewMode::Icons, "Icons", "Ctrl+Shift+2", paint_view_icons),
            (ViewMode::Columns, "Columns", "Ctrl+Shift+3", paint_view_columns),
            (ViewMode::Dual, "Dual", "Ctrl+Shift+4", paint_view_dual),
        ];
        for (vm, name, sc, p_icon) in items {
            let active = vm == current;
            let (r, resp) = ui.allocate_exact_size(egui::vec2(190.0, 24.0), Sense::click());
            if resp.hovered() || active {
                ui.painter().rect_filled(r, CornerRadius::same(4), if active { th.accent.gamma_multiply(0.18) } else { th.fg.gamma_multiply(0.06) });
            }
            let ic_r = Rect::from_min_size(r.min + egui::vec2(8.0, 4.0), egui::vec2(16.0, 16.0));
            p_icon(ui.painter(), ic_r, if active { th.accent } else { th.fg });
            ui.painter().text(r.min + egui::vec2(30.0, 12.0), Align2::LEFT_CENTER, name, FontId::proportional(th.font - 0.5), if active { th.accent } else { th.fg });
            ui.painter().text(r.max - egui::vec2(10.0, 12.0), Align2::RIGHT_CENTER, sc, FontId::proportional(th.font - 2.5), th.dim);
            if resp.clicked() {
                picked = Some(vm);
                ui.close_menu();
            }
        }
    });

    let rect = menu_resp.response.rect;
    let icon_rect = Rect::from_center_size(Pos2::new(rect.left() + 14.0, rect.center().y), Vec2::splat(14.0));
    icon_fn(ui.painter(), icon_rect, if menu_resp.response.hovered() { th.accent } else { th.fg });
    let chev_rect = Rect::from_center_size(Pos2::new(rect.right() - 11.0, rect.center().y), Vec2::new(8.0, 10.0));
    paint_chevron_down(ui.painter(), chev_rect, if menu_resp.response.hovered() { th.accent } else { th.dim });

    picked
}

/// `C:\` for any path on drive C.
fn pane_root(path: &Path) -> PathBuf {
    path.ancestors().last().map(Path::to_path_buf).unwrap_or_else(|| path.to_path_buf())
}

impl FilesApp {
    fn export_folder_dialog(&mut self, ctx: &egui::Context, th: Theme) {
        let Some(prompt) = self.export_folder_prompt.as_mut() else { return };
        let mut cancel = false;
        let mut commit = false;

        let frame = egui::Frame::window(&ctx.style())
            .fill(th.pane)
            .corner_radius(CornerRadius::same(th.radius.max(8)))
            .stroke(Stroke::new(1.0_f32, th.accent))
            .shadow(egui::epaint::Shadow {
                offset: [0, 10],
                blur: 28,
                spread: 0,
                color: Color32::from_black_alpha(170),
            })
            .inner_margin(egui::Margin::same(16));

        egui::Window::new("Export to Folder")
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(frame)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (icon_r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                    chickenwm_core::ui_icons::paint_folder(ui.painter(), icon_r, th.accent);
                    ui.add_space(4.0);
                    ui.heading("Export Archive to Folder");
                });
                ui.add_space(6.0);
                ui.label(egui::RichText::new(format!("Extract all contents of {} into a new folder:", display_name(&prompt.archive))).color(th.dim));
                ui.add_space(12.0);

                ui.label(egui::RichText::new("Folder name:").strong().size(th.font));
                ui.add_space(4.0);

                let id = egui::Id::new("export_folder_name_input");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut prompt.folder_name)
                        .id(id)
                        .desired_width(f32::INFINITY)
                        .margin(egui::Margin::symmetric(8, 6))
                        .font(FontId::proportional(th.font)),
                );

                if prompt.initial_focus {
                    prompt.initial_focus = false;
                    resp.request_focus();
                    let len = prompt.folder_name.chars().count();
                    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(len),
                        )));
                        state.store(ctx, id);
                    }
                }

                if resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    commit = true;
                }

                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let btn_h = 28.0;
                        if ui.add(egui::Button::new(egui::RichText::new("Export").strong().color(Color32::WHITE))
                            .fill(th.accent)
                            .min_size(egui::vec2(85.0, btn_h))).clicked()
                        {
                            commit = true;
                        }
                        if ui.add(egui::Button::new("Cancel").min_size(egui::vec2(75.0, btn_h))).clicked() {
                            cancel = true;
                        }
                    });
                });

                if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });

        if cancel {
            self.export_folder_prompt = None;
        } else if commit {
            if let Some(prompt) = self.export_folder_prompt.take() {
                let name = prompt.folder_name.trim().to_string();
                let stem = if name.is_empty() { "archive" } else { &name };
                let target_dir = unique_path(&prompt.dest_dir, stem, "");
                let archive_path = prompt.archive.clone();
                let arc_name = display_name(&archive_path);
                let dest = target_dir.clone();
                self.spawn_job(format!("Extracting {arc_name}"), format!("To {}", dest.display()), move |progress| {
                    extract_archive(&archive_path, &dest, progress)
                        .map(|_| format!("Extracted {arc_name}"))
                        .map_err(|e| format!("Extract failed: {e}"))
                });
                self.toast(format!("Exporting to {}", display_name(&target_dir)), false);
            }
        }
    }

    fn export_here_dialog(&mut self, ctx: &egui::Context, th: Theme) {
        let Some(prompt) = self.export_here_prompt.as_mut() else { return };
        let mut cancel = false;
        let mut extract_subfolder = false;
        let mut extract_here_anyway = false;

        let frame = egui::Frame::window(&ctx.style())
            .fill(th.pane)
            .corner_radius(CornerRadius::same(th.radius.max(8)))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(249, 226, 175)))
            .shadow(egui::epaint::Shadow {
                offset: [0, 10],
                blur: 28,
                spread: 0,
                color: Color32::from_black_alpha(170),
            })
            .inner_margin(egui::Margin::same(16));

        egui::Window::new("Folder Contains Other Items")
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(frame)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("⚠️").size(18.0));
                    ui.add_space(4.0);
                    ui.heading("Folder Contains Existing Files");
                });
                ui.add_space(6.0);
                let dest_name = display_name(&prompt.dest_dir);
                ui.label(egui::RichText::new(format!(
                    "The folder '{dest_name}' already contains other files or directories. Extracting everything here directly might clutter this folder."
                )).color(th.dim));
                ui.add_space(12.0);

                ui.label(egui::RichText::new("Extract into a new subfolder instead:").strong().size(th.font));
                ui.add_space(4.0);

                let id = egui::Id::new("export_here_folder_name_input");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut prompt.folder_name)
                        .id(id)
                        .desired_width(f32::INFINITY)
                        .margin(egui::Margin::symmetric(8, 6))
                        .font(FontId::proportional(th.font)),
                );

                if prompt.initial_focus {
                    prompt.initial_focus = false;
                    resp.request_focus();
                }

                if resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    extract_subfolder = true;
                }

                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    let btn_h = 28.0;
                    if ui.add(egui::Button::new(egui::RichText::new("Create Folder & Extract").strong().color(Color32::WHITE))
                        .fill(th.accent)
                        .min_size(egui::vec2(160.0, btn_h))).clicked()
                    {
                        extract_subfolder = true;
                    }
                    if ui.add(egui::Button::new("Extract here anyway").min_size(egui::vec2(120.0, btn_h))).clicked() {
                        extract_here_anyway = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new("Cancel").min_size(egui::vec2(60.0, btn_h))).clicked() {
                            cancel = true;
                        }
                    });
                });

                if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });

        if cancel {
            self.export_here_prompt = None;
        } else if extract_subfolder {
            if let Some(prompt) = self.export_here_prompt.take() {
                let name = prompt.folder_name.trim().to_string();
                let stem = if name.is_empty() { "archive" } else { &name };
                let target_dir = unique_path(&prompt.dest_dir, stem, "");
                let archive_path = prompt.archive.clone();
                let arc_name = display_name(&archive_path);
                let dest = target_dir.clone();
                self.spawn_job(format!("Extracting {arc_name}"), format!("To {}", dest.display()), move |progress| {
                    extract_archive(&archive_path, &dest, progress)
                        .map(|_| format!("Extracted {arc_name}"))
                        .map_err(|e| format!("Extract failed: {e}"))
                });
                self.toast(format!("Exporting to {}", display_name(&target_dir)), false);
            }
        } else if extract_here_anyway {
            if let Some(prompt) = self.export_here_prompt.take() {
                let archive_path = prompt.archive.clone();
                let arc_name = display_name(&archive_path);
                let dest = prompt.dest_dir.clone();
                let dest_target = dest.clone();
                self.spawn_job(format!("Extracting {arc_name}"), format!("To {}", dest.display()), move |progress| {
                    extract_archive(&archive_path, &dest_target, progress)
                        .map(|_| format!("Extracted {arc_name}"))
                        .map_err(|e| format!("Extract failed: {e}"))
                });
                self.toast(format!("Extracting to {}", display_name(&dest)), false);
            }
        }
    }

    fn compress_dialog(&mut self, ctx: &egui::Context, th: Theme) {
        let Some(prompt) = self.compress_prompt.as_mut() else { return };
        let mut commit = false;
        let mut cancel = false;

        let frame = egui::Frame::window(&ctx.style())
            .fill(th.pane)
            .corner_radius(CornerRadius::same(th.radius.max(8)))
            .stroke(Stroke::new(1.0_f32, th.border))
            .shadow(egui::epaint::Shadow {
                offset: [0, 8],
                blur: 24,
                spread: 0,
                color: Color32::from_black_alpha(150),
            })
            .inner_margin(egui::Margin::same(16));

        egui::Window::new("Compress to ZIP")
            .collapsible(false)
            .resizable(false)
            .default_width(400.0)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(frame)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (icon_r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                    chickenwm_core::ui_icons::paint_folder(ui.painter(), icon_r, th.accent);
                    ui.add_space(4.0);
                    ui.heading("Compress to ZIP Archive");
                });
                ui.add_space(6.0);
                ui.label(egui::RichText::new(format!("Compressing {} selected item(s)", prompt.paths.len())).color(th.dim));
                ui.add_space(12.0);

                ui.label(egui::RichText::new("Archive filename:").strong().size(th.font));
                ui.add_space(4.0);

                let id = egui::Id::new("compress_name_input");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut prompt.name_buffer)
                        .id(id)
                        .desired_width(f32::INFINITY)
                        .margin(egui::Margin::symmetric(8, 6))
                        .font(FontId::proportional(th.font)),
                );

                if prompt.initial_focus {
                    prompt.initial_focus = false;
                    resp.request_focus();
                    let stem_len = prompt.name_buffer.trim_end_matches(".zip").trim_end_matches(".ZIP").chars().count();
                    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(stem_len),
                        )));
                        state.store(ctx, id);
                    }
                }

                if resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    commit = true;
                }

                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let btn_h = 28.0;
                        if ui.add(egui::Button::new(egui::RichText::new("Compress").strong().color(Color32::WHITE))
                            .fill(th.accent)
                            .min_size(egui::vec2(85.0, btn_h))).clicked()
                        {
                            commit = true;
                        }
                        if ui.add(egui::Button::new("Cancel").min_size(egui::vec2(75.0, btn_h))).clicked() {
                            cancel = true;
                        }
                    });
                });

                if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });

        if cancel {
            self.compress_prompt = None;
        } else if commit {
            if let Some(prompt) = self.compress_prompt.take() {
                let mut name = prompt.name_buffer.trim().to_string();
                if name.is_empty() {
                    name = "archive.zip".to_string();
                }
                if !name.to_ascii_lowercase().ends_with(".zip") {
                    name.push_str(".zip");
                }
                let stem = name.trim_end_matches(".zip").trim_end_matches(".ZIP");
                let zip_path = unique_path(&prompt.dir, stem, "zip");
                self.apply(Action::ExecuteCompress {
                    paths: prompt.paths,
                    zip_path,
                });
            }
        }
    }
}
