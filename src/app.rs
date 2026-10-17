//! The chickenwm-files window: tabs, toolbar with breadcrumbs, sidebar, the Details / Icons /
//! Columns / Dual-pane views, context menus, drag and drop, and the status bar.
//!
//! UI code never mutates panes directly while drawing them; it records `Action`s which are
//! applied after the frame's layout, so every view (and every menu) shares one implementation
//! of each command.

mod preview;

use crate::icons::IconCache;
use crate::model::{
    archive_stem, compress_zip, extract_archive, format_bytes, format_time, is_archive, read_directory,
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    Shortcut(Vec<PathBuf>),
    Terminal(PathBuf),
    Pin(PathBuf),
    Unpin(PathBuf),
    Transfer { sources: Vec<PathBuf>, dest: PathBuf, kind: DropKind },
    Sort(SortKey),
    SortDesc(bool),
    SetView(ViewMode),
    ToggleHidden,
    TogglePreview,
    Refresh,
    NewTab,
    CloseTab(usize),
    DuplicateTab(usize),
    CloseOtherTabs(usize),
    SwitchTab(usize),
    Back(Side),
    Forward(Side),
    Up(Side),
    BeginDrag { paths: Vec<PathBuf>, secondary: bool },
    EditAddress,
    FocusSearch,
}

/// Colors and metrics from `files.css`.
#[derive(Clone, Copy, PartialEq)]
struct Theme {
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
    fn from_css(sheet: &CssStyleSheet) -> Self {
        let fg = c(sheet.var_color("--fg-color", rgb(205, 214, 244)));
        let accent = c(sheet.var_color("--accent-color", rgb(51, 204, 255)));
        Self {
            bg: c(sheet.var_color("--bg-color", rgb(24, 24, 37))),
            pane: c(sheet.var_color("--pane-bg", rgb(30, 30, 46))),
            sidebar: c(sheet.var_color("--sidebar-bg", rgb(17, 17, 27))),
            fg,
            dim: fg.gamma_multiply(0.6),
            dir: c(sheet.var_color("--dir-color", rgb(137, 180, 250))),
            exec: c(sheet.var_color("--exec-color", rgb(166, 227, 161))),
            archive: c(sheet.var_color("--archive-color", rgb(249, 226, 175))),
            media: c(sheet.var_color("--media-color", rgb(203, 166, 247))),
            accent,
            selected: c(sheet.var_color(
                "--selected-bg",
                RgbaColor { r: 51, g: 204, b: 255, a: 56 },
            )),
            border: c(sheet.var_color("--border-color", rgb(49, 50, 68))),
            radius: sheet.var_px("--border-radius", 8.0).clamp(0.0, 16.0) as u8,
            row_h: sheet.var_px("--row-height", 28.0).clamp(20.0, 48.0),
            font: sheet.var_px("--font-size", 14.0).clamp(10.0, 22.0),
        }
    }

    fn name_color(&self, e: &Entry) -> Color32 {
        if e.is_dir {
            return self.dir;
        }
        match e.ext.to_ascii_lowercase().as_str() {
            "exe" | "bat" | "cmd" | "ps1" | "msi" | "com" | "lnk" => self.exec,
            "zip" | "7z" | "rar" | "gz" | "tgz" | "tar" | "xz" | "bz2" => self.archive,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "mp3" | "flac"
            | "wav" | "ogg" | "m4a" | "mp4" | "mkv" | "webm" | "mov" | "avi" => self.media,
            _ => self.fg,
        }
    }

    fn apply(&self, ctx: &egui::Context) {
        let [br, bgc, bb, _] = self.bg.to_array();
        let light = (0.2126 * br as f32 + 0.7152 * bgc as f32 + 0.0722 * bb as f32) / 255.0 > 0.5;
        let mut v = if light { egui::Visuals::light() } else { egui::Visuals::dark() };
        v.panel_fill = self.bg;
        v.window_fill = self.pane;
        v.extreme_bg_color = self.sidebar;
        v.faint_bg_color = self.pane;
        v.override_text_color = Some(self.fg);
        v.selection.bg_fill = self.accent.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0_f32, self.accent);
        v.hyperlink_color = self.accent;
        v.window_stroke = Stroke::new(1.0_f32, self.border);
        v.window_corner_radius = CornerRadius::same(self.radius);
        v.menu_corner_radius = CornerRadius::same(self.radius.min(8));
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
        v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, self.border);
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

struct Drag {
    paths: Vec<PathBuf>,
    secondary: bool,
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
    theme: Option<Theme>,
    target_fps: u32,
    icons: IconCache,
    drives: Vec<DriveInfo>,
    drives_at: Instant,
    rename: Option<Rename>,
    address: Option<String>,
    focus_search: bool,
    drag: Option<Drag>,
    zones: Vec<DropZone>,
    drop_menu: Option<DropMenu>,
    jobs: Arc<Jobs>,
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
        let prefs = Prefs::load();
        let mut tab = Pane::new(initial_dir.clone());
        tab.rebuild(&prefs);
        if let Some(sel) = &initial_select {
            tab.select_path(sel);
        }
        let mut second = Pane::new(initial_dir);
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
                if let chickenwm_core::css::ConfigWatchEvent::HyprConfChanged(_) = ev {
                    flag.store(true, Ordering::Relaxed);
                    ctx.request_repaint();
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
            theme: None,
            target_fps: target_fps.max(60),
            drives: Vec::new(),
            drives_at: Instant::now(),
            rename: None,
            address: None,
            focus_search: false,
            drag: None,
            zones: Vec::new(),
            drop_menu: None,
            jobs: Arc::new(Jobs {
                running: AtomicUsize::new(0),
                status: Mutex::new(String::new()),
                progress: Mutex::new(None),
            }),
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
        };
        app.refresh_drives();
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

    /// Runs `work` on a worker thread; the visible folders refresh when it finishes.
    fn spawn_job(
        &self,
        label: String,
        work: impl FnOnce(&dyn Fn(f32)) -> Result<String, String> + Send + 'static,
    ) {
        let jobs = self.jobs.clone();
        let dirty = self.dirty.clone();
        let ctx = self.ctx.clone();
        jobs.running.fetch_add(1, Ordering::SeqCst);
        *jobs.status.lock() = label;
        *jobs.progress.lock() = None;
        std::thread::spawn(move || {
            shell::init_com_thread();
            let report = |p: f32| {
                *jobs.progress.lock() = Some(p);
                ctx.request_repaint();
            };
            let result = work(&report);
            *jobs.status.lock() = match result {
                Ok(msg) => msg,
                Err(msg) => format!("⚠ {msg}"),
            };
            *jobs.progress.lock() = None;
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
        let label = format!("{verb} {what}…");
        self.spawn_job(label, move |_| {
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
                self.spawn_job(format!("Extracting {name}…"), move |progress| {
                    extract_archive(&archive, &dest, progress)
                        .map(|_| format!("Extracted {name}"))
                        .map_err(|e| format!("Extract failed: {e}"))
                });
            }
            Action::Compress(paths) if !paths.is_empty() => {
                let dir = paths[0].parent().map(Path::to_path_buf).unwrap_or_default();
                let stem = if paths.len() == 1 {
                    paths[0].file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                } else {
                    display_name(&dir)
                };
                let zip_path = unique_path(&dir, &stem, "zip");
                let name = display_name(&zip_path);
                self.spawn_job(format!("Compressing to {name}…"), move |progress| {
                    compress_zip(&paths, &zip_path, progress)
                        .map(|_| format!("Created {name}"))
                        .map_err(|e| format!("Compress failed: {e}"))
                });
            }
            Action::Compress(_) => {}
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
                if !self.prefs.pinned.contains(&path) {
                    self.prefs.pinned.push(path);
                    self.prefs.save();
                }
            }
            Action::Unpin(path) => {
                self.prefs.pinned.retain(|p| p != &path);
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
                    DropKind::Copy => self.file_job(FileOp::Copy, sources, Some(dest), false),
                    DropKind::Move => {
                        if sources.iter().all(|s| s.parent() == Some(dest.as_path())) {
                            return;
                        }
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
            Action::BeginDrag { paths, secondary } => {
                if !paths.is_empty() {
                    self.drag = Some(Drag { paths, secondary });
                }
            }
            Action::EditAddress => {
                self.address = Some(self.pane(self.focused()).cwd.display().to_string());
            }
            Action::FocusSearch => self.focus_search = true,
        }
    }

    /// New folder / file: show it selected and start renaming, like Explorer.
    fn after_create(&mut self, path: PathBuf) {
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

        if ctx.wants_keyboard_input() || self.rename.is_some() || self.address.is_some() {
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
                if let Some(e) = &current {
                    actions.push(Action::StartRename(e.path.clone()));
                }
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
            return;
        }
        self.preview = Preview { path: path.clone(), ..Default::default() };
        let Some(entry) = current else { return };
        if !entry.is_dir && is_image(&entry.path) && entry.size < 60 * 1024 * 1024 {
            self.preview.loading_image = true;
            self.preview.kind = preview::Kind::Image;
            let tx = self.image_tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let decoded = image::open(&entry.path).ok().map(|img| {
                    let dims = [img.width() as usize, img.height() as usize];
                    let thumb = img.thumbnail(640, 640).to_rgba8();
                    let size = [thumb.width() as usize, thumb.height() as usize];
                    (egui::ColorImage::from_rgba_unmultiplied(size, thumb.as_raw()), dims)
                });
                let (img, dims) = match decoded {
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
}

fn paint_icon(ui: &egui::Ui, icons: &mut IconCache, key: &str, large: bool, rect: Rect, fallback_dir: bool, theme: &Theme) {
    if let Some(tex) = icons.get(key, large) {
        ui.painter().image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    } else {
        let glyph = if fallback_dir { "📁" } else { "📄" };
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(rect.height() * 0.85), theme.dim);
    }
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
    }
    item(ui, "Delete", "Del", Action::Recycle(t()));
    item(ui, "Delete permanently", "Shift+Del", Action::Delete(t()));
    ui.separator();
    item(ui, "Compress to ZIP file", "", Action::Compress(t()));
    item(ui, "Create shortcut", "", Action::Shortcut(t()));
    if entry.is_dir && single {
        if pinned {
            item(ui, "Unpin from Quick access", "", Action::Unpin(entry.path.clone()));
        } else {
            item(ui, "Pin to Quick access", "", Action::Pin(entry.path.clone()));
        }
        item(ui, "Open in Terminal", "", Action::Terminal(entry.path.clone()));
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
        if ui.add(egui::Button::new("📁  Folder").shortcut_text("Ctrl+Shift+N")).clicked() {
            push(ui, Action::NewFolder(cwd.to_path_buf()));
        }
        if ui.button("📄  Text document").clicked() {
            push(ui, Action::NewTextFile(cwd.to_path_buf()));
        }
    });
    ui.separator();
    if ui.button("Open in Terminal").clicked() {
        push(ui, Action::Terminal(cwd.to_path_buf()));
    }
    if ui.button(if pinned { "Unpin from Quick access" } else { "Pin to Quick access" }).clicked() {
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
            self.actions.push(Action::BeginDrag { paths, secondary });
        }
        if entry.is_dir {
            self.zones.push(DropZone { rect, dir: entry.path.clone() });
        }
        let targets = if pane.selection.contains(&entry.path) { pane.targets() } else { vec![entry.path.clone()] };
        let pinned = self.prefs.pinned.contains(&entry.path);
        let actions = &mut *self.actions;
        resp.context_menu(|ui| item_menu(ui, side, entry, &targets, pinned, dual, actions));
    }

    fn row_background(&self, ui: &egui::Ui, rect: Rect, resp: &egui::Response, selected: bool, cursor: bool) {
        let radius = CornerRadius::same(self.theme.radius.min(6));
        if selected {
            ui.painter().rect_filled(rect, radius, self.theme.selected);
        } else if resp.hovered() && !self.dragging {
            ui.painter().rect_filled(rect, radius, self.theme.fg.gamma_multiply(0.06));
        }
        if cursor && self.focused {
            ui.painter().rect_stroke(rect.shrink(0.5), radius, Stroke::new(1.0_f32, self.theme.accent.gamma_multiply(0.6)), egui::StrokeKind::Inside);
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
        let resp = ui.interact(rect, ui.id().with(("bg", side)), Sense::click());
        if resp.clicked() {
            self.actions.push(Action::ClearSelection(side));
        }
        if resp.secondary_clicked() {
            self.actions.push(Action::ClearSelection(side));
        }
        self.zones.push(DropZone { rect, dir: pane.cwd.clone() });
        let pinned = self.prefs.pinned.contains(&pane.cwd);
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

        // Header with sort buttons.
        let (header, _) = ui.allocate_exact_size(egui::vec2(full.width(), 26.0), Sense::hover());
        let mut x = header.left() + 8.0;
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
        ui.painter().hline(header.x_range(), header.bottom(), Stroke::new(1.0_f32, th.border));

        let list_rect = ui.available_rect_before_wrap();
        self.background(ui, side, pane, list_rect);
        self.empty_message(ui, list_rect, pane);

        let sa = self.scroll_area(side, "details", pane, row_h, 1, memo);
        let out = sa.show_rows(ui, row_h, pane.visible.len(), |ui, range| {
            for pos in range {
                let Some(entry) = pane.entry_at(pos) else { continue };
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click_and_drag());
                let selected = pane.selection.contains(&entry.path);
                self.row_background(ui, rect, &resp, selected, pos == pane.cursor);
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
                if !entry.is_dir {
                    ui.painter().text(Pos2::new(cx + size_w - 6.0, rect.center().y), Align2::RIGHT_CENTER, format_bytes(entry.size), small, th.dim);
                }
                self.interact(&resp, side, pos, entry, pane, rect, dual);
            }
        });
        ScrollMemo { offset: out.state.offset.y, viewport: out.inner_rect.height() }
    }

    /// Icons view: a grid of tiles.
    fn icons_grid(&mut self, ui: &mut egui::Ui, side: Side, pane: &Pane, memo: Option<&ScrollMemo>, cols_out: &mut usize) -> ScrollMemo {
        let th = self.theme;
        let tile = egui::vec2(108.0, 100.0);
        let list_rect = ui.available_rect_before_wrap();
        self.background(ui, side, pane, list_rect);
        self.empty_message(ui, list_rect, pane);
        let cols = ((list_rect.width() - 12.0) / (tile.x + 6.0)).floor().max(1.0) as usize;
        *cols_out = cols;
        let rows = pane.visible.len().div_ceil(cols);
        let sa = self.scroll_area(side, "icons", pane, tile.y + 6.0, cols, memo);
        let out = sa.show_rows(ui, tile.y + 6.0, rows, |ui, range| {
            for row in range {
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    for pos in row * cols..((row + 1) * cols).min(pane.visible.len()) {
                        let Some(entry) = pane.entry_at(pos) else { continue };
                        let (rect, resp) = ui.allocate_exact_size(tile, Sense::click_and_drag());
                        let selected = pane.selection.contains(&entry.path);
                        self.row_background(ui, rect, &resp, selected, pos == pane.cursor);
                        let icon = Rect::from_center_size(Pos2::new(rect.center().x, rect.top() + 30.0), Vec2::splat(40.0));
                        paint_icon(ui, self.icons, &entry.icon_key, true, icon, entry.is_dir, &th);
                        let label_rect = Rect::from_min_max(Pos2::new(rect.left() + 4.0, rect.top() + 56.0), Pos2::new(rect.right() - 4.0, rect.bottom() - 2.0));
                        if !self.rename_field(ui, entry, Rect::from_min_size(label_rect.min, egui::vec2(label_rect.width(), 24.0))) {
                            let galley = ui.painter().layout(
                                shown_name(entry, self.prefs),
                                FontId::proportional(th.font - 1.0),
                                th.name_color(entry),
                                label_rect.width(),
                            );
                            let painter = ui.painter().with_clip_rect(label_rect);
                            let x = label_rect.center().x - galley.size().x / 2.0;
                            painter.galley(Pos2::new(x, label_rect.top()), galley, th.fg);
                        }
                        self.interact(&resp, side, pos, entry, pane, rect, false);
                    }
                });
            }
        });
        ScrollMemo { offset: out.state.offset.y, viewport: out.inner_rect.height() }
    }

    /// A simple column (Columns view): name-only rows; `on_click` decides what a click does.
    fn name_column(&mut self, ui: &mut egui::Ui, salt: &str, entries: &[Entry], highlight: Option<&Path>, open_side: Side) {
        let th = self.theme;
        egui::ScrollArea::vertical().id_salt(salt).auto_shrink([false, false]).show_rows(ui, th.row_h, entries.len(), |ui, range| {
            for e in &entries[range] {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), th.row_h), Sense::click());
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
        if self.drives_at.elapsed() > Duration::from_secs(30) {
            self.refresh_drives();
        }
        self.sync_watcher();
        if self.jobs.running.load(Ordering::Relaxed) > 0 {
            ctx.request_repaint_after(Duration::from_secs_f64(1.0 / self.target_fps as f64));
        }

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
        let mut commit_rename = false;
        self.draw_top(ctx, theme, &mut actions);
        self.draw_status(ctx, theme);
        self.draw_sidebar(ctx, theme, &mut actions);
        self.draw_center(ctx, theme, &mut actions, &mut commit_rename);
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
    fn draw_top(&mut self, ctx: &egui::Context, th: Theme, actions: &mut Vec<Action>) {
        let frame = egui::Frame::NONE.fill(th.sidebar).inner_margin(egui::Margin::symmetric(8, 6));
        egui::TopBottomPanel::top("files_top").frame(frame).show(ctx, |ui| {
            // Tabs
            ui.horizontal(|ui| {
                for i in 0..self.tabs.len() {
                    let active = i == self.active;
                    let title = self.tabs[i].title();
                    let fill = if active { th.pane } else { Color32::TRANSPARENT };
                    let resp = egui::Frame::NONE
                        .fill(fill)
                        .corner_radius(CornerRadius::same(th.radius.min(8)))
                        .inner_margin(egui::Margin::symmetric(10, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                                paint_icon(ui, &mut self.icons, "dir", false, icon_rect, true, &th);
                                let label = ui.add(egui::Label::new(egui::RichText::new(title).color(if active { th.fg } else { th.dim })).sense(Sense::click()).truncate());
                                if self.tabs.len() > 1 && ui.add(egui::Button::new(egui::RichText::new("×").color(th.dim)).frame(false)).on_hover_text("Close tab (Ctrl+W)").clicked() {
                                    actions.push(Action::CloseTab(i));
                                }
                                label
                            })
                            .inner
                        });
                    let label = resp.inner;
                    let rect = resp.response.rect;
                    let tab_resp = ui.interact(rect, ui.id().with(("tab", i)), Sense::click());
                    if label.clicked() || tab_resp.clicked() {
                        actions.push(Action::SwitchTab(i));
                    }
                    if label.middle_clicked() || tab_resp.middle_clicked() {
                        actions.push(Action::CloseTab(i));
                    }
                    self.zones.push(DropZone { rect, dir: self.tabs[i].cwd.clone() });
                    let n = self.tabs.len();
                    tab_resp.union(label).context_menu(|ui| {
                        if ui.button("New tab").clicked() {
                            actions.push(Action::NewTab);
                            ui.close_menu();
                        }
                        if ui.button("Duplicate tab").clicked() {
                            actions.push(Action::DuplicateTab(i));
                            ui.close_menu();
                        }
                        if ui.add_enabled(n > 1, egui::Button::new("Close tab")).clicked() {
                            actions.push(Action::CloseTab(i));
                            ui.close_menu();
                        }
                        if ui.add_enabled(n > 1, egui::Button::new("Close other tabs")).clicked() {
                            actions.push(Action::CloseOtherTabs(i));
                            ui.close_menu();
                        }
                    });
                }
                if ui.add(egui::Button::new("+").frame(false)).on_hover_text("New tab (Ctrl+T)").clicked() {
                    actions.push(Action::NewTab);
                }
            });
            ui.add_space(4.0);

            // Navigation, breadcrumbs, search, view switcher
            ui.horizontal(|ui| {
                let side = self.focused();
                let pane = self.pane(side);
                let nav = |ui: &mut egui::Ui, glyph: &str, tip: &str, enabled: bool| {
                    ui.add_enabled(enabled, egui::Button::new(egui::RichText::new(glyph).size(th.font + 2.0)).min_size(egui::vec2(30.0, 28.0)))
                        .on_hover_text(tip)
                        .clicked()
                };
                if nav(ui, "⬅", "Back (Alt+Left)", !pane.back.is_empty()) {
                    actions.push(Action::Back(side));
                }
                if nav(ui, "➡", "Forward (Alt+Right)", !pane.forward.is_empty()) {
                    actions.push(Action::Forward(side));
                }
                if nav(ui, "⬆", "Up (Alt+Up)", pane.cwd.parent().is_some()) {
                    actions.push(Action::Up(side));
                }
                if nav(ui, "⟳", "Refresh (F5)", true) {
                    actions.push(Action::Refresh);
                }

                let search_w = 220.0;
                let views_w = 330.0;
                let addr_w = (ui.available_width() - search_w - views_w - 24.0).max(160.0);
                self.address_bar(ui, side, addr_w, th, actions);

                // Search
                let pane = self.pane_mut(side);
                let hint = format!("🔍 Search {}", pane.title());
                let resp = ui.add(egui::TextEdit::singleline(&mut pane.filter).hint_text(hint).desired_width(search_w));
                if self.focus_search {
                    resp.request_focus();
                    self.focus_search = false;
                }
                if resp.changed() {
                    let prefs = self.prefs.clone();
                    self.pane_mut(side).rebuild(&prefs);
                }
                if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.pane_mut(side).filter.clear();
                    let prefs = self.prefs.clone();
                    self.pane_mut(side).rebuild(&prefs);
                    resp.surrender_focus();
                }

                // Views + options
                for (v, label, tip) in [
                    (ViewMode::Details, "Details", "Ctrl+Shift+1"),
                    (ViewMode::Icons, "Icons", "Ctrl+Shift+2"),
                    (ViewMode::Columns, "Columns", "Ctrl+Shift+3"),
                    (ViewMode::Dual, "Dual pane", "Ctrl+Shift+4"),
                ] {
                    if ui.selectable_label(self.prefs.view == v, label).on_hover_text(tip).clicked() {
                        actions.push(Action::SetView(v));
                    }
                }
                ui.menu_button("⚙", |ui| {
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
                    // Same light/dark option as every ChickenWM app (master theme provider).
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
                    if ui.button("New folder").clicked() {
                        actions.push(Action::NewFolder(self.pane(side).cwd.clone()));
                        ui.close_menu();
                    }
                    if ui.button("Open in Terminal").clicked() {
                        actions.push(Action::Terminal(self.pane(side).cwd.clone()));
                        ui.close_menu();
                    }
                    ui.separator();
                    ui.label(egui::RichText::new("Keyboard: arrows/j k move · Enter/l open · Backspace/h up · F2 rename · Del recycle · Ctrl+C/X/V · / search").small().color(th.dim));
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
        ui.painter().rect(rect, CornerRadius::same(th.radius.min(8)), th.pane, Stroke::new(1.0_f32, th.border), egui::StrokeKind::Inside);
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
        let frame = egui::Frame::NONE.fill(th.sidebar).inner_margin(egui::Margin::symmetric(10, 4));
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
                    text.push_str(&format!("   ·   {} selected", pane.selection.len()));
                    if sel_size > 0 {
                        text.push_str(&format!("  {}", format_bytes(sel_size)));
                    }
                }
                ui.label(egui::RichText::new(text).color(th.dim));

                let running = self.jobs.running.load(Ordering::Relaxed);
                let status = self.jobs.status.lock().clone();
                if running > 0 {
                    ui.separator();
                    match *self.jobs.progress.lock() {
                        Some(p) => {
                            ui.add(egui::ProgressBar::new(p).desired_width(160.0).text(status));
                        }
                        None => {
                            ui.spinner();
                            ui.label(status);
                        }
                    }
                } else if let Some((msg, at, error)) = &self.toast {
                    if at.elapsed() < Duration::from_secs(4) {
                        ui.separator();
                        ui.label(egui::RichText::new(msg).color(if *error { Color32::from_rgb(243, 139, 168) } else { th.accent }));
                        ctx.request_repaint_after(Duration::from_millis(500));
                    }
                } else if !status.is_empty() {
                    ui.separator();
                    ui.label(egui::RichText::new(status).color(th.dim));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some((free, total)) = shell::disk_space(&pane_root(&self.pane(self.focused()).cwd)) {
                        ui.label(egui::RichText::new(format!("{} free of {}", format_bytes(free), format_bytes(total))).color(th.dim));
                    }
                });
            });
        });
    }

    fn draw_sidebar(&mut self, ctx: &egui::Context, th: Theme, actions: &mut Vec<Action>) {
        let frame = egui::Frame::NONE.fill(th.sidebar).inner_margin(egui::Margin::symmetric(8, 8));
        egui::SidePanel::left("files_sidebar").resizable(true).default_width(200.0).width_range(150.0..=340.0).frame(frame).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let cwd = self.pane(self.focused()).cwd.clone();
                let side = self.focused();
                let section = |ui: &mut egui::Ui, title: &str| {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(title).small().strong().color(th.dim));
                };
                section(ui, "QUICK ACCESS");
                for (name, path) in quick_access() {
                    self.sidebar_item(ui, th, &name, &format!("path:{}", path.display()), &path, cwd == path, side, false, None, actions);
                }
                if !self.prefs.pinned.is_empty() {
                    section(ui, "PINNED");
                    for path in self.prefs.pinned.clone() {
                        self.sidebar_item(ui, th, &display_name(&path), "dir", &path, cwd == path, side, true, None, actions);
                    }
                }
                section(ui, "THIS PC");
                let drives: Vec<(PathBuf, String, Option<(u64, u64)>)> =
                    self.drives.iter().map(|d| (d.root.clone(), d.label.clone(), d.space)).collect();
                for (root, label, space) in drives {
                    let letter = root.display().to_string().trim_end_matches('\\').to_string();
                    let name = format!("{label} ({letter})");
                    let key = format!("path:{}", root.display());
                    self.sidebar_item(ui, th, &name, &key, &root, cwd == root, side, false, space, actions);
                }
            });
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn sidebar_item(&mut self, ui: &mut egui::Ui, th: Theme, name: &str, icon_key: &str, path: &Path, active: bool, side: Side, pinned: bool, space: Option<(u64, u64)>, actions: &mut Vec<Action>) {
        let h = if space.is_some() { th.row_h + 10.0 } else { th.row_h };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), Sense::click());
        let radius = CornerRadius::same(th.radius.min(6));
        if active {
            ui.painter().rect_filled(rect, radius, th.selected);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, radius, th.fg.gamma_multiply(0.06));
        }
        let row_center = Pos2::new(rect.left(), rect.top() + th.row_h / 2.0);
        let icon = Rect::from_center_size(row_center + egui::vec2(16.0, 0.0), Vec2::splat(16.0));
        paint_icon(ui, &mut self.icons, icon_key, false, icon, true, &th);
        ui.painter().with_clip_rect(rect).text(row_center + egui::vec2(32.0, 0.0), Align2::LEFT_CENTER, name, FontId::proportional(th.font - 0.5), th.fg);
        if let Some((free, total)) = space.filter(|(_, t)| *t > 0) {
            let used = 1.0 - free as f32 / total as f32;
            let bar = Rect::from_min_size(Pos2::new(rect.left() + 32.0, rect.bottom() - 9.0), egui::vec2(rect.width() - 42.0, 4.0));
            ui.painter().rect_filled(bar, 2.0_f32, th.border);
            let fill = if used > 0.9 { Color32::from_rgb(243, 139, 168) } else { th.accent };
            ui.painter().rect_filled(Rect::from_min_size(bar.min, egui::vec2(bar.width() * used, 4.0)), 2.0_f32, fill);
        }
        if resp.clicked() {
            actions.push(Action::Navigate(side, path.to_path_buf()));
        }
        if resp.middle_clicked() {
            actions.push(Action::OpenNewTab(path.to_path_buf()));
        }
        if let Some((free, total)) = space {
            resp.clone().on_hover_text(format!("{} free of {}", format_bytes(free), format_bytes(total)));
        }
        self.zones.push(DropZone { rect, dir: path.to_path_buf() });
        let path = path.to_path_buf();
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
            if ui.button("Paste").clicked() {
                actions.push(Action::Paste(path.clone()));
                ui.close_menu();
            }
            if ui.button("Open in Terminal").clicked() {
                actions.push(Action::Terminal(path.clone()));
                ui.close_menu();
            }
            if pinned && ui.button("Unpin from Quick access").clicked() {
                actions.push(Action::Unpin(path.clone()));
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Properties").clicked() {
                actions.push(Action::Properties(vec![path.clone()]));
                ui.close_menu();
            }
        });
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
            let frame = egui::Frame::NONE.fill(th.pane).inner_margin(egui::Margin::same(10)).stroke(Stroke::new(1.0_f32, th.border));
            egui::SidePanel::right("files_preview").resizable(true).default_width(300.0).width_range(200.0..=600.0).frame(frame).show(ctx, |ui| {
                self.draw_preview(ui, th);
            });
        }
        let frame = egui::Frame::NONE.fill(th.bg).inner_margin(egui::Margin::same(8));
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
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
            };
            let pane_frame = |focused: bool| {
                egui::Frame::NONE
                    .fill(th.pane)
                    .corner_radius(CornerRadius::same(th.radius))
                    .stroke(Stroke::new(1.0_f32, if focused { th.accent.gamma_multiply(0.5) } else { th.border }))
                    .inner_margin(egui::Margin::same(6))
            };
            match view {
                ViewMode::Details => {
                    let memo = pane_frame(false).show(ui, |ui| draw.details(ui, Side::Main, &main, false, false, memo_main.as_ref())).inner;
                    self.scroll.insert(Side::Main, memo);
                }
                ViewMode::Icons => {
                    let memo = pane_frame(false).show(ui, |ui| draw.icons_grid(ui, Side::Main, &main, memo_main.as_ref(), &mut icon_cols)).inner;
                    self.scroll.insert(Side::Main, memo);
                }
                ViewMode::Columns => {
                    let memo = ui.columns(3, |cols| {
                        pane_frame(false).show(&mut cols[0], |ui| {
                            ui.label(egui::RichText::new(main.cwd.parent().map(display_name).unwrap_or_default()).small().color(th.dim));
                            draw.name_column(ui, "col_parent", &parent_entries, Some(&main.cwd), Side::Main);
                        });
                        let memo = pane_frame(false).show(&mut cols[1], |ui| draw.details(ui, Side::Main, &main, true, false, memo_main.as_ref())).inner;
                        pane_frame(false).show(&mut cols[2], |ui| {
                            if let Some(dir) = &preview_entry {
                                ui.label(egui::RichText::new(display_name(dir)).small().color(th.dim));
                                if child_entries.is_empty() {
                                    ui.label(egui::RichText::new("This folder is empty").color(th.dim));
                                }
                                draw.name_column(ui, "col_child", &child_entries, None, Side::Main);
                            } else {
                                preview::body(ui, &th, preview, "col_preview");
                            }
                        });                        memo
                    });
                    self.scroll.insert(Side::Main, memo);
                }
                ViewMode::Dual => {
                    let second = second.expect("dual pane");
                    let (m1, m2) = ui.columns(2, |cols| {
                        draw.focused = focus == Side::Main;
                        let m1 = pane_frame(focus == Side::Main).show(&mut cols[0], |ui| {
                            ui.label(egui::RichText::new(main.cwd.display().to_string()).small().color(if focus == Side::Main { th.accent } else { th.dim }));
                            draw.details(ui, Side::Main, &main, true, true, memo_main.as_ref())
                        });
                        draw.focused = focus == Side::Second;
                        let m2 = pane_frame(focus == Side::Second).show(&mut cols[1], |ui| {
                            ui.label(egui::RichText::new(second.cwd.display().to_string()).small().color(if focus == Side::Second { th.accent } else { th.dim }));
                            draw.details(ui, Side::Second, &second, true, true, memo_second.as_ref())
                        });
                        if m1.response.clicked() || m1.response.contains_pointer() && cols[0].input(|i| i.pointer.any_pressed()) {
                            draw.actions.push(Action::Focus(Side::Main));
                        }
                        if m2.response.contains_pointer() && cols[1].input(|i| i.pointer.any_pressed()) {
                            draw.actions.push(Action::Focus(Side::Second));
                        }
                        (m1.inner, m2.inner)
                    });
                    self.scroll.insert(Side::Main, m1);
                    self.scroll.insert(Side::Second, m2);
                }
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
            let n = drag.paths.len();
            let what = if n == 1 { display_name(&drag.paths[0]) } else { format!("{n} items") };
            let text = match (target, kind, drag.secondary) {
                (Some(z), _, true) => format!("{what} ➡ {}  (release for options)", display_name(&z.dir)),
                (Some(z), Some(DropKind::Copy), _) => format!("+ Copy {what} to {}", display_name(&z.dir)),
                (Some(z), Some(DropKind::Shortcut), _) => format!("🔗 Create shortcut in {}", display_name(&z.dir)),
                (Some(z), _, _) => format!("➡ Move {what} to {}", display_name(&z.dir)),
                (None, _, _) => format!("{what}  (drag outside to drop into another app)"),
            };
            let galley = painter.layout_no_wrap(text, FontId::proportional(th.font - 1.0), th.fg);
            let rect = Rect::from_min_size(p + egui::vec2(16.0, 18.0), galley.size() + egui::vec2(16.0, 10.0));
            painter.rect(rect, CornerRadius::same(6), th.pane, Stroke::new(1.0_f32, th.accent.gamma_multiply(0.6)), egui::StrokeKind::Inside);
            painter.galley(rect.min + egui::vec2(8.0, 5.0), galley, th.fg);
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
        }
    }
}

/// `C:\` for any path on drive C.
fn pane_root(path: &Path) -> PathBuf {
    path.ancestors().last().map(Path::to_path_buf).unwrap_or_else(|| path.to_path_buf())
}
