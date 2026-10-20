//! Power tools for the file explorer:
//! - batch rename (find / replace, optional regex, `{n}` numbering) with a live preview,
//! - folder sizes on demand and a disk usage view,
//! - browsing archives (.zip, .tar, .tar.gz, .7z, .rar) as folders,
//! - git status badges in the details view,
//! - "go anywhere" (Ctrl+P) across favorites, recent and nearby folders,
//! - undo (Ctrl+Z) for renames, moves, copies and new files / folders.

use super::{config_dir, display_name, quick_access, Action, FilesApp, Theme};
use crate::app::{paint_nav_back, paint_archive_icon, paint_icon_painter, ArchiveDrag, ExportFolderPrompt, ExportHerePrompt};
use crate::model::{archive_stem, extract_archive};
use crate::model::format_bytes;
use crate::shell::{self, FileOp};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// =============================================================================================
// State
// =============================================================================================

#[derive(Default)]
pub(super) struct Tools {
    pub batch: Option<BatchRename>,
    pub usage: Option<DiskUsage>,
    pub archive: Option<ArchiveView>,
    pub goto: Option<GoAnywhere>,
    /// Most recent last.
    pub undo: Vec<UndoOp>,
    /// Folder sizes asked for from the context menu; `None` while counting.
    pub folder_sizes: Arc<Mutex<HashMap<PathBuf, Option<u64>>>>,
    /// Per-folder cached size and timestamp so unchanged folders don't recount from scratch.
    pub folder_size_cache: Arc<Mutex<HashMap<PathBuf, (u64, Instant)>>>,
    /// Active cancellation flags per folder currently being scanned in the background.
    pub folder_size_stops: Arc<Mutex<HashMap<PathBuf, Arc<AtomicBool>>>>,
    pub git: Git,
    recent: Recent,
    last_cwd: Option<PathBuf>,
}

// =============================================================================================
// Undo
// =============================================================================================

#[derive(Debug, Clone)]
pub(super) enum UndoOp {
    /// (from, to) pairs, as renamed.
    Rename(Vec<(PathBuf, PathBuf)>),
    /// Moved: (original path, where it went).
    Move(Vec<(PathBuf, PathBuf)>),
    /// Files / folders this app created (copies, new items): undo recycles them.
    Created(Vec<PathBuf>),
}

impl UndoOp {
    fn describe(&self) -> String {
        let n = |v: usize| if v == 1 { "1 item".to_owned() } else { format!("{v} items") };
        match self {
            UndoOp::Rename(v) => format!("rename of {}", n(v.len())),
            UndoOp::Move(v) => format!("move of {}", n(v.len())),
            UndoOp::Created(v) => format!("creation of {}", n(v.len())),
        }
    }
}

const UNDO_MAX: usize = 50;

impl FilesApp {
    pub(super) fn push_undo(&mut self, op: UndoOp) {
        self.tools.undo.push(op);
        if self.tools.undo.len() > UNDO_MAX {
            self.tools.undo.remove(0);
        }
    }

    /// Remembers a copy / move about to run so it can be undone. Items whose target name is
    /// already taken (the shell would rename or ask) aren't recorded.
    pub(super) fn record_transfer(&mut self, op: FileOp, sources: &[PathBuf], dest: &Path) {
        let pairs: Vec<(PathBuf, PathBuf)> = sources
            .iter()
            .filter_map(|s| Some((s.clone(), dest.join(s.file_name()?))))
            .filter(|(s, d)| s != d && !d.exists())
            .collect();
        if pairs.is_empty() {
            return;
        }
        match op {
            FileOp::Copy => self.push_undo(UndoOp::Created(pairs.into_iter().map(|(_, d)| d).collect())),
            FileOp::Move => self.push_undo(UndoOp::Move(pairs)),
            FileOp::Recycle | FileOp::Delete => {}
        }
    }

    pub(super) fn undo(&mut self) {
        let Some(op) = self.tools.undo.pop() else {
            self.toast("Nothing to undo", false);
            return;
        };
        let what = op.describe();
        match op {
            UndoOp::Rename(pairs) => {
                let reversed: Vec<(PathBuf, PathBuf)> = pairs
                    .into_iter()
                    .rev()
                    .map(|(from, to)| (to, from))
                    .collect();
                match rename_all(&reversed) {
                    Ok(()) => self.toast(format!("Undid {what}"), false),
                    Err(e) => self.toast(format!("Undo {what} failed: {e}"), true),
                }
                self.dirty.store(true, Ordering::Relaxed);
            }
            UndoOp::Move(pairs) => {
                // Grouped by original folder, moved back through the shell (progress, undo-safe).
                let mut by_dir: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
                for (orig, now) in pairs {
                    if let (true, Some(dir)) = (now.exists(), orig.parent()) {
                        by_dir.entry(dir.to_path_buf()).or_default().push(now);
                    }
                }
                for (dir, items) in by_dir {
                    self.file_job(FileOp::Move, items, Some(dir), false);
                }
                self.toast(format!("Undoing {what}…"), false);
            }
            UndoOp::Created(paths) => {
                let existing: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
                if !existing.is_empty() {
                    self.file_job(FileOp::Recycle, existing, None, false);
                }
                self.toast(format!("Undid {what} (moved to the Recycle Bin)"), false);
            }
        }
    }
}

// =============================================================================================
// Batch rename
// =============================================================================================

pub(super) struct BatchRename {
    paths: Vec<PathBuf>,
    find: String,
    replace: String,
    regex: bool,
    case_sensitive: bool,
    /// Apply to the extension too (off: only the name before the last dot changes).
    include_ext: bool,
    start: u32,
    focus: bool,
    /// Most recently applied `(old_path, new_path)` pairs so the user can undo right in the dialog.
    last_applied: Option<Vec<(PathBuf, PathBuf)>>,
}

impl BatchRename {
    pub fn new(mut paths: Vec<PathBuf>) -> Self {
        paths.sort_by(|a, b| crate::model::natural_cmp(&display_name(a), &display_name(b)));
        Self {
            paths,
            find: String::new(),
            replace: "{name}".into(),
            regex: false,
            case_sensitive: false,
            include_ext: false,
            start: 1,
            focus: true,
            last_applied: None,
        }
    }
}

/// Splits `name.ext` (a leading dot is part of the name).
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

/// Fills `{name}` (the name after find/replace), `{ext}`, `{n}` / `{n:3}` (counter, padded).
fn expand_template(template: &str, name: &str, ext: &str, n: u32) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let token = &rest[start + 1..start + end];
        match token {
            "name" => out.push_str(name),
            "ext" => out.push_str(ext),
            "n" => out.push_str(&n.to_string()),
            t if t.starts_with("n:") => {
                let width: usize = t[2..].parse().unwrap_or(1);
                out.push_str(&format!("{n:0width$}"));
            }
            other => {
                out.push('{');
                out.push_str(other);
                out.push('}');
            }
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

/// Checks proposed names: empty / reserved characters, and duplicates (case-insensitive, like
/// NTFS) are reported per row.
fn validate(new_names: Vec<String>) -> Vec<Result<String, String>> {
    let lower: Vec<String> = new_names.iter().map(|s| s.trim().to_lowercase()).collect();
    new_names
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            let n = n.trim().to_owned();
            if n.is_empty() || n == "." || n == ".." {
                Err("empty name".to_owned())
            } else if n.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
                Err("can't contain \\ / : * ? \" < > |".to_owned())
            } else if lower.iter().enumerate().any(|(j, l)| j != i && *l == lower[i]) {
                Err("duplicate name".to_owned())
            } else {
                Ok(n)
            }
        })
        .collect()
}

/// Pre-apply collision check against existing files on disk outside the batch.
pub(super) fn check_disk_collisions(paths: &[PathBuf], rows: &mut [Result<String, String>]) {
    let batch_sources: std::collections::HashSet<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().to_lowercase())
        .collect();
    for (src, row) in paths.iter().zip(rows.iter_mut()) {
        let Ok(new_name) = row else { continue };
        let target = src.with_file_name(new_name.as_str());
        let target_lower = target.to_string_lossy().to_lowercase();
        if target_lower != src.to_string_lossy().to_lowercase()
            && target.exists()
            && !batch_sources.contains(&target_lower)
        {
            *row = Err("already exists on disk".to_owned());
        }
    }
}

/// Plain mode. With `find` empty, each name becomes `template` (`{name}`, `{ext}`, `{n}`);
/// otherwise every occurrence of `find` in the name is replaced by `template`. The extension is
/// kept unless `include_ext` (or the template places `{ext}` itself).
pub(super) fn plan_rename(
    names: &[String],
    find: &str,
    template: &str,
    case_sensitive: bool,
    include_ext: bool,
    start: u32,
) -> Result<Vec<Result<String, String>>, String> {
    let re = if find.is_empty() {
        None
    } else {
        Some(regex::RegexBuilder::new(&regex::escape(find)).case_insensitive(!case_sensitive).build().map_err(|e| e.to_string())?)
    };
    let new_names = names
        .iter()
        .enumerate()
        .map(|(i, full)| {
            let (stem, ext) = if include_ext { (full.as_str(), "") } else { split_ext(full) };
            let rep = expand_template(template, stem, ext, start + i as u32);
            let (new, places_ext) = match &re {
                None => (rep, template.contains("{ext}")),
                Some(re) => (re.replace_all(stem, regex::NoExpand(&rep)).into_owned(), false),
            };
            if !include_ext && !ext.is_empty() && !places_ext { format!("{new}.{ext}") } else { new }
        })
        .collect();
    Ok(validate(new_names))
}

/// Regex mode: `find` → `template` with `$1` groups and `{n}` numbering, on each name.
pub(super) fn plan_rename_regex(
    names: &[String],
    find: &str,
    template: &str,
    case_sensitive: bool,
    include_ext: bool,
    start: u32,
) -> Result<Vec<Result<String, String>>, String> {
    let re = regex::RegexBuilder::new(find).case_insensitive(!case_sensitive).build().map_err(|e| e.to_string())?;
    let new_names = names
        .iter()
        .enumerate()
        .map(|(i, full)| {
            let (stem, ext) = if include_ext { (full.as_str(), "") } else { split_ext(full) };
            let numbered = expand_template(template, stem, ext, start + i as u32);
            let new = re.replace_all(stem, numbered.as_str()).into_owned();
            if !include_ext && !ext.is_empty() { format!("{new}.{ext}") } else { new }
        })
        .collect();
    Ok(validate(new_names))
}

impl FilesApp {
    fn batch_rename_window(&mut self, ctx: &egui::Context, th: Theme) {
        let Some(b) = self.tools.batch.as_mut() else { return };
        let names: Vec<String> = b.paths.iter().map(|p| display_name(p)).collect();
        let mut plan = if b.regex && !b.find.is_empty() {
            plan_rename_regex(&names, &b.find, &b.replace, b.case_sensitive, b.include_ext, b.start)
        } else {
            plan_rename(&names, &b.find, &b.replace, b.case_sensitive, b.include_ext, b.start)
        };
        if let Ok(rows) = &mut plan {
            check_disk_collisions(&b.paths, rows);
        }
        let mut open = true;
        let mut apply = false;
        let mut undo_now = false;
        let has_last_applied = b.last_applied.is_some();
        egui::Window::new(format!("Rename {} items", b.paths.len()))
            .open(&mut open)
            .collapsible(false)
            .default_size([620.0, 460.0])
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Grid::new("br-fields").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Find");
                    let r = ui.add(egui::TextEdit::singleline(&mut b.find).hint_text("text to replace (empty: whole name)").desired_width(f32::INFINITY));
                    if std::mem::take(&mut b.focus) {
                        r.request_focus();
                    }
                    ui.end_row();
                    ui.label("Replace with");
                    ui.add(egui::TextEdit::singleline(&mut b.replace).desired_width(f32::INFINITY));
                    ui.end_row();
                });
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut b.regex, "Regular expression");
                    ui.checkbox(&mut b.case_sensitive, "Match case");
                    ui.checkbox(&mut b.include_ext, "Include extension");
                    ui.label("Number from");
                    ui.add(egui::DragValue::new(&mut b.start).range(0..=1_000_000));
                });
                ui.label(egui::RichText::new("{name} = the name   {ext} = extension   {n} / {n:3} = counter   $1 = regex group").small().color(th.dim));
                ui.separator();
                let rows = match &plan {
                    Ok(rows) => rows.clone(),
                    Err(e) => {
                        ui.colored_label(Color32::from_rgb(220, 90, 80), format!("Regex error: {e}"));
                        Vec::new()
                    }
                };
                egui::ScrollArea::vertical().max_height(ui.available_height() - 40.0).show(ui, |ui| {
                    egui::Grid::new("br-preview").num_columns(3).striped(true).spacing([10.0, 4.0]).show(ui, |ui| {
                        for (old, new) in names.iter().zip(&rows) {
                            ui.label(old);
                            ui.label("→");
                            match new {
                                Ok(n) if n == old => ui.label(egui::RichText::new(n).color(th.dim)),
                                Ok(n) => ui.label(egui::RichText::new(n).color(th.accent)),
                                Err(e) => ui.colored_label(Color32::from_rgb(220, 90, 80), e),
                            };
                            ui.end_row();
                        }
                    });
                });
                ui.separator();
                let ok = !rows.is_empty() && rows.iter().all(Result::is_ok);
                let changes = names.iter().zip(&rows).filter(|(o, n)| n.as_ref().is_ok_and(|n| n != *o)).count();
                ui.horizontal(|ui| {
                    if ui.add_enabled(ok && changes > 0, egui::Button::new(format!("Rename {changes}"))).clicked() {
                        apply = true;
                    }
                    if has_last_applied && ui.button("Undo last rename (Ctrl+Z)").clicked() {
                        undo_now = true;
                    }
                    if !ok && !rows.is_empty() {
                        ui.colored_label(Color32::from_rgb(220, 90, 80), "Fix the names marked in red (duplicates / collisions) first");
                    }
                });
            });
        if undo_now {
            if let Some(b) = self.tools.batch.as_mut() {
                if let Some(pairs) = b.last_applied.take() {
                    let reversed: Vec<(PathBuf, PathBuf)> = pairs
                        .iter()
                        .rev()
                        .map(|(from, to)| (to.clone(), from.clone()))
                        .collect();
                    match rename_all(&reversed) {
                        Ok(()) => {
                            for p in &mut b.paths {
                                if let Some((orig, _)) = pairs.iter().find(|(_, new_p)| new_p == p) {
                                    *p = orig.clone();
                                }
                            }
                            if matches!(self.tools.undo.last(), Some(UndoOp::Rename(_))) {
                                self.tools.undo.pop();
                            }
                            self.toast("Undid batch rename", false);
                        }
                        Err(e) => self.toast(format!("Undo failed: {e}"), true),
                    }
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
            return;
        }
        if apply {
            if let Ok(rows) = plan {
                let Some(b_ref) = self.tools.batch.as_ref() else { return };
                let pairs: Vec<(PathBuf, PathBuf)> = b_ref
                    .paths
                    .iter()
                    .zip(rows)
                    .filter_map(|(p, r)| r.ok().map(|n| (p.clone(), p.with_file_name(n))))
                    .filter(|(a, b)| a != b)
                    .collect();
                match rename_all(&pairs) {
                    Ok(()) => {
                        let n = pairs.len();
                        if let Some(b_mut) = self.tools.batch.as_mut() {
                            for p in &mut b_mut.paths {
                                if let Some((_, new_p)) = pairs.iter().find(|(old_p, _)| old_p == p) {
                                    *p = new_p.clone();
                                }
                            }
                            b_mut.last_applied = Some(pairs.clone());
                        }
                        self.push_undo(UndoOp::Rename(pairs));
                        self.toast(format!("Renamed {n} item(s) — Ctrl+Z or Undo button to revert"), false);
                    }
                    Err(e) => self.toast(format!("Rename failed: {e}"), true),
                }
                self.dirty.store(true, Ordering::Relaxed);
            }
            return;
        }
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.tools.batch = None;
        }
    }
}

/// Renames in two steps (through temporary names) so swaps like a→b, b→a work. Rolls back on
/// failure.
fn rename_all(pairs: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let sources: std::collections::HashSet<String> = pairs.iter().map(|(a, _)| a.to_string_lossy().to_lowercase()).collect();
    for (_, to) in pairs {
        if to.exists() && !sources.contains(&to.to_string_lossy().to_lowercase()) {
            return Err(format!("\"{}\" already exists", display_name(to)));
        }
    }
    let tag = std::process::id();
    let temps: Vec<PathBuf> = pairs.iter().enumerate().map(|(i, (a, _))| a.with_file_name(format!(".chickenwm-rename-{tag}-{i}"))).collect();
    for (i, (a, _)) in pairs.iter().enumerate() {
        if let Err(e) = std::fs::rename(a, &temps[i]) {
            for j in (0..i).rev() {
                let _ = std::fs::rename(&temps[j], &pairs[j].0);
            }
            return Err(format!("{}: {e}", display_name(a)));
        }
    }
    for (i, (_, b)) in pairs.iter().enumerate() {
        if let Err(e) = std::fs::rename(&temps[i], b) {
            // Put everything not yet placed back.
            for j in i..pairs.len() {
                let _ = std::fs::rename(&temps[j], &pairs[j].0);
            }
            for j in (0..i).rev() {
                let _ = std::fs::rename(&pairs[j].1, &pairs[j].0);
            }
            return Err(format!("{}: {e}", display_name(b)));
        }
    }
    Ok(())
}

// =============================================================================================
// Folder sizes & disk usage
// =============================================================================================

/// Total size of a folder tree (junctions / symlinks aren't followed). `stop` aborts early.
pub(super) fn tree_size(path: &Path, counted: &AtomicU64, stop: &AtomicBool) -> u64 {
    if stop.load(Ordering::Relaxed) {
        return 0;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if !meta.is_dir() {
        counted.fetch_add(1, Ordering::Relaxed);
        return meta.len();
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & REPARSE_POINT != 0 {
            return 0;
        }
    }
    std::fs::read_dir(path).into_iter().flatten().flatten().map(|e| tree_size(&e.path(), counted, stop)).sum()
}

const FOLDER_SIZE_CACHE_TTL: Duration = Duration::from_secs(120);

impl FilesApp {
    pub(super) fn compute_folder_sizes(&mut self, dirs: Vec<PathBuf>) {
        let sizes = Arc::clone(&self.tools.folder_sizes);
        let cache = Arc::clone(&self.tools.folder_size_cache);
        let stops = Arc::clone(&self.tools.folder_size_stops);
        let ctx = self.ctx.clone();
        for dir in dirs.into_iter().filter(|d| d.is_dir()) {
            // Check per-folder cache first
            if let Some(&(cached_bytes, at)) = cache.lock().get(&dir) {
                if at.elapsed() < FOLDER_SIZE_CACHE_TTL {
                    sizes.lock().insert(dir, Some(cached_bytes));
                    continue;
                }
            }
            // Cancel any previous in-flight scan for this folder
            if let Some(prev) = stops.lock().remove(&dir) {
                prev.store(true, Ordering::Relaxed);
            }
            let stop = Arc::new(AtomicBool::new(false));
            stops.lock().insert(dir.clone(), Arc::clone(&stop));
            sizes.lock().insert(dir.clone(), None);
            let (sizes, cache, stops, ctx) = (
                Arc::clone(&sizes),
                Arc::clone(&cache),
                Arc::clone(&stops),
                ctx.clone(),
            );
            std::thread::spawn(move || {
                let total = tree_size(&dir, &AtomicU64::new(0), &stop);
                let cancelled = stop.load(Ordering::Relaxed);
                stops.lock().remove(&dir);
                if cancelled {
                    sizes.lock().remove(&dir);
                } else {
                    cache.lock().insert(dir.clone(), (total, Instant::now()));
                    sizes.lock().insert(dir, Some(total));
                }
                ctx.request_repaint();
            });
        }
    }

    /// Cancels all in-flight background folder size scans.
    pub(super) fn cancel_folder_sizes(&mut self) {
        let mut stops = self.tools.folder_size_stops.lock();
        if stops.is_empty() {
            return;
        }
        let mut sizes = self.tools.folder_sizes.lock();
        for (dir, stop) in stops.drain() {
            stop.store(true, Ordering::Relaxed);
            if sizes.get(&dir) == Some(&None) {
                sizes.remove(&dir);
            }
        }
    }
}

pub(super) struct DiskUsage {
    root: PathBuf,
    items: Arc<Mutex<Vec<(String, PathBuf, u64, bool)>>>,
    counted: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    started: Instant,
}

impl Drop for DiskUsage {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl DiskUsage {
    pub fn start(root: PathBuf, ctx: &egui::Context) -> Self {
        let items = Arc::new(Mutex::new(Vec::new()));
        let counted = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (it, c, d, s, dir, ctx) = (items.clone(), counted.clone(), done.clone(), stop.clone(), root.clone(), ctx.clone());
        std::thread::spawn(move || {
            let children: Vec<std::fs::DirEntry> = std::fs::read_dir(&dir).into_iter().flatten().flatten().collect();
            for e in children {
                if s.load(Ordering::Relaxed) {
                    return;
                }
                let path = e.path();
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                let size = tree_size(&path, &c, &s);
                let mut v = it.lock();
                v.push((e.file_name().to_string_lossy().into_owned(), path, size, is_dir));
                v.sort_by(|a, b| b.2.cmp(&a.2));
                drop(v);
                ctx.request_repaint();
            }
            d.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        });
        Self { root, items, counted, done, stop, started: Instant::now() }
    }
}

impl FilesApp {
    fn disk_usage_window(&mut self, ctx: &egui::Context, th: Theme) {
        let Some(du) = self.tools.usage.as_ref() else { return };
        let items = du.items.lock().clone();
        let done = du.done.load(Ordering::Relaxed);
        let root = du.root.clone();
        let total: u64 = items.iter().map(|i| i.2).sum();
        let mut open = true;
        let mut drill: Option<PathBuf> = None;
        let mut reveal: Option<PathBuf> = None;
        egui::Window::new(format!("Disk usage — {}", display_name(&root)))
            .open(&mut open)
            .collapsible(false)
            .default_size([560.0, 520.0])
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(parent) = root.parent() {
                        if ui.button("⬆ Up").clicked() {
                            drill = Some(parent.to_path_buf());
                        }
                    }
                    ui.label(egui::RichText::new(root.display().to_string()).color(th.dim));
                });
                ui.label(if done {
                    format!("{} in {} items ({:.1}s)", format_bytes(total), items.len(), du.started.elapsed().as_secs_f32())
                } else {
                    format!("Counting… {} so far ({} files)", format_bytes(total), du.counted.load(Ordering::Relaxed))
                });
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let w = ui.available_width();
                    for (name, path, size, is_dir) in &items {
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 24.0), Sense::click());
                        let frac = if total > 0 { *size as f32 / total as f32 } else { 0.0 };
                        let bar = Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height()));
                        ui.painter().rect_filled(bar, 4.0, th.accent.gamma_multiply(0.22));
                        if resp.hovered() {
                            ui.painter().rect_stroke(rect, 4.0, Stroke::new(1.0_f32, th.border), egui::StrokeKind::Inside);
                        }
                        let font = FontId::proportional(th.font - 1.0);
                        let label = if *is_dir { format!("{name}/") } else { name.clone() };
                        ui.painter().with_clip_rect(rect.shrink2(egui::vec2(130.0, 0.0)).translate(egui::vec2(-124.0, 0.0)))
                            .text(rect.left_center() + egui::vec2(6.0, 0.0), Align2::LEFT_CENTER, label, font.clone(), if *is_dir { th.dir } else { th.fg });
                        ui.painter().text(rect.right_center() - egui::vec2(6.0, 0.0), Align2::RIGHT_CENTER, format!("{}  {:>4.1}%", format_bytes(*size), frac * 100.0), font, th.dim);
                        let resp = resp.on_hover_text(if *is_dir { "Double-click to look inside" } else { "Double-click to show it" });
                        if resp.double_clicked() {
                            if *is_dir {
                                drill = Some(path.clone());
                            } else {
                                reveal = Some(path.clone());
                            }
                        }
                    }
                });
            });
        if let Some(dir) = drill {
            self.tools.usage = Some(DiskUsage::start(dir, ctx));
        } else if !open {
            self.tools.usage = None;
        }
        if let Some(path) = reveal {
            if let Some(dir) = path.parent() {
                let side = self.focused();
                let prefs = self.prefs.clone();
                let pane = self.pane_mut(side);
                pane.navigate(dir.to_path_buf(), &prefs);
                pane.select_path(&path);
            }
        }
        if !done {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

// =============================================================================================
// Archives as folders
// =============================================================================================

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ArchiveItem {
    /// Path inside the archive, `/`-separated, no trailing slash.
    pub path: String,
    pub size: Option<u64>,
    pub is_dir: bool,
}

pub(super) fn is_browsable_archive(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    [".zip", ".tar", ".tar.gz", ".tgz", ".7z", ".rar"].iter().any(|s| l.ends_with(s))
}

/// Uses Windows' bsdtar (`tar.exe`), which reads 7z and rar too.
fn tar_exe() -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new(crate::model::system_tar());
    c.creation_flags(0x0800_0000);
    c
}

pub(super) fn list_archive(archive: &Path) -> anyhow::Result<Vec<ArchiveItem>> {
    let lower = archive.to_string_lossy().to_ascii_lowercase();
    let mut items = Vec::new();
    if lower.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)?;
        for i in 0..zip.len() {
            let f = zip.by_index_raw(i)?;
            let is_dir = f.is_dir();
            let path = f.name()?.replace('\\', "/").trim_end_matches('/').to_owned();
            items.push(ArchiveItem { path, size: (!is_dir).then(|| f.size()), is_dir });
        }
    } else {
        let out = tar_exe().arg("-tf").arg(archive).output()?;
        if !out.status.success() {
            anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        for line in String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()) {
            let is_dir = line.ends_with('/');
            items.push(ArchiveItem { path: line.replace('\\', "/").trim_end_matches('/').to_owned(), size: None, is_dir });
        }
    }
    Ok(items)
}

/// The direct children of `dir` (`""` = archive root): folders implied by deeper paths count.
pub(super) fn archive_children(items: &[ArchiveItem], dir: &str) -> Vec<ArchiveItem> {
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let mut out: Vec<ArchiveItem> = Vec::new();
    for it in items {
        let Some(rest) = it.path.strip_prefix(&prefix) else { continue };
        if rest.is_empty() {
            continue;
        }
        match rest.split_once('/') {
            Some((first, _)) => {
                let path = format!("{prefix}{first}");
                if !out.iter().any(|o| o.path == path) {
                    out.push(ArchiveItem { path, size: None, is_dir: true });
                }
            }
            None => {
                if let Some(o) = out.iter_mut().find(|o| o.path == it.path) {
                    o.is_dir |= it.is_dir;
                } else {
                    out.push(it.clone());
                }
            }
        }
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| crate::model::natural_cmp(&a.path, &b.path)));
    out
}

/// Extracts one member (file or folder) into `dest`, returning where it landed.
pub(super) fn extract_archive_member(archive: &Path, member: &str, is_dir: bool, dest: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dest)?;
    let lower = archive.to_string_lossy().to_ascii_lowercase();
    let base_name = member.rsplit('/').next().unwrap_or(member);
    let out_path = dest.join(base_name);

    if lower.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)?;
        if is_dir {
            std::fs::create_dir_all(&out_path)?;
            let prefix = format!("{}/", member.trim_end_matches('/'));
            for i in 0..zip.len() {
                let mut f = zip.by_index(i)?;
                let name = f.name()?.replace('\\', "/");
                if name.starts_with(&prefix) {
                    let rel = name.strip_prefix(&prefix).unwrap_or(&name);
                    if rel.is_empty() {
                        continue;
                    }
                    let target = out_path.join(rel);
                    if f.is_dir() {
                        std::fs::create_dir_all(&target)?;
                    } else {
                        if let Some(p) = target.parent() {
                            std::fs::create_dir_all(p)?;
                        }
                        std::io::copy(&mut f, &mut std::fs::File::create(&target)?)?;
                    }
                }
            }
            Ok(out_path)
        } else {
            let idx = (0..zip.len())
                .find(|&i| {
                    zip.by_index_raw(i)
                        .is_ok_and(|f| f.name().is_ok_and(|n| n.replace('\\', "/").trim_end_matches('/') == member))
                })
                .ok_or_else(|| anyhow::anyhow!("{member} not found"))?;
            let mut f = zip.by_index(idx)?;
            std::io::copy(&mut f, &mut std::fs::File::create(&out_path)?)?;
            Ok(out_path)
        }
    } else {
        let staging = dest.join(format!(".x-{}", std::process::id()));
        std::fs::create_dir_all(&staging)?;
        let status = tar_exe().arg("-xf").arg(archive).arg("-C").arg(&staging).arg(member).status()?;
        if !status.success() {
            anyhow::bail!("tar couldn't extract {member}");
        }
        let member_path = staging.join(member.replace('/', "\\"));
        if member_path.exists() {
            if out_path.exists() {
                let _ = std::fs::remove_dir_all(&out_path);
                let _ = std::fs::remove_file(&out_path);
            }
            std::fs::rename(&member_path, &out_path)?;
        }
        let _ = std::fs::remove_dir_all(&staging);
        Ok(out_path)
    }
}



pub(super) struct ArchiveView {
    pub archive: PathBuf,
    pub items: Result<Vec<ArchiveItem>, String>,
    pub dir: String,
    pub selected: Option<String>,
    pub filter: String,
}

impl ArchiveView {
    pub fn open(archive: PathBuf) -> Self {
        let items = list_archive(&archive).map_err(|e| e.to_string());
        Self { archive, items, dir: String::new(), selected: None, filter: String::new() }
    }
}

impl FilesApp {
    /// Renders the Zip / Archive screen as a dedicated first-class screen filling the main area.
    pub(super) fn draw_archive_screen(&mut self, ui: &mut egui::Ui, th: Theme, _actions: &mut Vec<Action>) {
        let Some(av) = self.tools.archive.as_mut() else { return };
        let mut exit_archive = false;
        let mut open_member: Option<String> = None;
        let mut extract_member_to: Option<(String, bool, PathBuf)> = None;
        let mut trigger_export_folder = false;
        let mut trigger_export_here = false;
        let mut start_archive_drag: Option<(String, String, bool, Option<u64>, Rect, Vec2)> = None;

        // Archive Top Bar
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;

            // Back button
            let back_tip = if av.dir.is_empty() { "Exit Archive (Esc)" } else { "Up one level" };
            let (back_rect, back_resp) = ui.allocate_exact_size(Vec2::new(26.0, 26.0), Sense::click());
            if back_resp.hovered() {
                ui.painter().rect_filled(back_rect, CornerRadius::same(5), th.fg.gamma_multiply(0.08));
            }
            paint_nav_back(ui.painter(), back_rect, if back_resp.hovered() { th.fg } else { th.dim });
            if back_resp.on_hover_text(back_tip).clicked() {
                if av.dir.is_empty() {
                    exit_archive = true;
                } else {
                    av.dir = av.dir.rsplit_once('/').map(|(p, _)| p.to_owned()).unwrap_or_default();
                    av.selected = None;
                }
            }

            // Archive Chip
            let chip_frame = egui::Frame::NONE
                .fill(th.sidebar)
                .stroke(Stroke::new(1.0_f32, th.hairline()))
                .corner_radius(CornerRadius::same(th.radius.min(6)))
                .inner_margin(egui::Margin::symmetric(8, 4));
            chip_frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let (icon_r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                    paint_archive_icon(ui.painter(), icon_r, th.accent);
                    ui.label(egui::RichText::new(display_name(&av.archive)).strong().size(th.font));
                });
            });

            // Breadcrumb navigation within the archive
            ui.separator();
            if ui.selectable_label(av.dir.is_empty(), "Root").clicked() {
                av.dir.clear();
                av.selected = None;
            }
            if !av.dir.is_empty() {
                let mut accum = String::new();
                for seg in av.dir.split('/') {
                    if !accum.is_empty() {
                        accum.push('/');
                    }
                    accum.push_str(seg);
                    ui.label(egui::RichText::new("›").color(th.dim));
                    let is_last = accum == av.dir;
                    let seg_str = accum.clone();
                    if ui.selectable_label(is_last, seg).clicked() && !is_last {
                        av.dir = seg_str;
                        av.selected = None;
                        break;
                    }
                }
            }

            // Right-aligned actions: Export to folder, Export here, Exit
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Exit button
                let close_btn = egui::Button::new(egui::RichText::new("✕ Close").size(th.font - 1.0))
                    .min_size(Vec2::new(0.0, 26.0))
                    .corner_radius(CornerRadius::same(th.radius.min(6)));
                if ui.add(close_btn).on_hover_text("Close Archive (Esc)").clicked() {
                    exit_archive = true;
                }

                // Export to folder button
                let exp_folder_btn = egui::Button::new(egui::RichText::new("📂 Export to folder...").strong().color(Color32::WHITE).size(th.font - 0.5))
                    .fill(th.accent)
                    .corner_radius(CornerRadius::same(th.radius.min(6)))
                    .min_size(Vec2::new(0.0, 26.0));
                if ui.add(exp_folder_btn).on_hover_text("Extract all files into a named folder").clicked() {
                    trigger_export_folder = true;
                }

                // Export here button
                let exp_here_btn = egui::Button::new(egui::RichText::new("📥 Export here").color(th.fg).size(th.font - 0.5))
                    .fill(th.sidebar)
                    .stroke(Stroke::new(1.0_f32, th.hairline()))
                    .corner_radius(CornerRadius::same(th.radius.min(6)))
                    .min_size(Vec2::new(0.0, 26.0));
                if ui.add(exp_here_btn).on_hover_text("Extract archive contents directly into current folder").clicked() {
                    trigger_export_here = true;
                }

                // Filter / Search input
                ui.add(
                    egui::TextEdit::singleline(&mut av.filter)
                        .hint_text("Search archive...")
                        .desired_width(140.0)
                        .margin(egui::Margin::symmetric(6, 4))
                        .font(FontId::proportional(th.font - 1.0)),
                );
            });
        });

        ui.add_space(4.0);
        let divider_y = ui.cursor().top();
        ui.painter().hline(ui.available_rect_before_wrap().x_range(), divider_y, Stroke::new(1.0_f32, th.hairline()));
        ui.add_space(6.0);

        // Header columns
        ui.horizontal(|ui| {
            ui.add_space(42.0);
            ui.label(egui::RichText::new("Name").small().color(th.dim));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(20.0);
                ui.label(egui::RichText::new("Size").small().color(th.dim));
            });
        });
        ui.separator();

        match &av.items {
            Err(e) => {
                ui.vertical_centered(|ui| {
                    ui.add_space(30.0);
                    ui.colored_label(Color32::from_rgb(220, 90, 80), format!("Can't read this archive: {e}"));
                });
            }
            Ok(items) => {
                let mut children = archive_children(items, &av.dir);
                if !av.filter.is_empty() {
                    let needle = av.filter.to_ascii_lowercase();
                    children.retain(|c| c.path.to_ascii_lowercase().contains(&needle));
                }

                if children.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label(egui::RichText::new("This folder in the archive is empty").color(th.dim));
                    });
                } else {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let w = ui.available_width();
                            for c in &children {
                                let name = c.path.rsplit('/').next().unwrap_or(&c.path);
                                let is_dragged = self.archive_drag.as_ref().is_some_and(|d| d.member == c.path);
                                let is_snapping = self.archive_snap_back.as_ref().is_some_and(|s| s.path.to_string_lossy() == c.path);

                                let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, th.row_h), Sense::click_and_drag());
                                if is_dragged || is_snapping {
                                    ui.painter().rect_stroke(rect, CornerRadius::same(th.radius.min(6)), Stroke::new(1.0_f32, th.hairline().gamma_multiply(0.25)), egui::StrokeKind::Inside);
                                } else {
                                    let selected = av.selected.as_deref() == Some(c.path.as_str());
                                    if selected {
                                        ui.painter().rect_filled(rect, CornerRadius::same(th.radius.min(6)), th.selected);
                                        ui.painter().rect_stroke(rect.shrink(0.5), CornerRadius::same(th.radius.min(6)), Stroke::new(1.0_f32, th.accent.gamma_multiply(0.4)), egui::StrokeKind::Inside);
                                    } else if resp.hovered() {
                                        ui.painter().rect_filled(rect, CornerRadius::same(th.radius.min(6)), th.fg.gamma_multiply(0.06));
                                    }

                                    let icon_rect = Rect::from_center_size(Pos2::new(rect.left() + 20.0, rect.center().y), Vec2::splat(16.0));
                                    if c.is_dir {
                                        chickenwm_core::ui_icons::paint_folder(ui.painter(), icon_rect, th.dir);
                                    } else {
                                        let ext = name.rsplit('.').next().unwrap_or("");
                                        let key = format!("ext:{ext}");
                                        paint_icon_painter(ui.painter(), &mut self.icons, &key, false, icon_rect, false, &th);
                                    }

                                    let font = FontId::proportional(th.font);
                                    ui.painter().text(
                                        rect.left_center() + egui::vec2(38.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        name,
                                        font.clone(),
                                        if c.is_dir { th.dir } else { th.fg },
                                    );

                                    if let Some(s) = c.size {
                                        ui.painter().text(
                                            rect.right_center() - egui::vec2(16.0, 0.0),
                                            Align2::RIGHT_CENTER,
                                            format_bytes(s),
                                            FontId::proportional(th.font - 1.5),
                                            th.dim,
                                        );
                                    } else if c.is_dir {
                                        ui.painter().text(
                                            rect.right_center() - egui::vec2(16.0, 0.0),
                                            Align2::RIGHT_CENTER,
                                            "—",
                                            FontId::proportional(th.font - 1.5),
                                            th.dim.gamma_multiply(0.5),
                                        );
                                    }

                                    if resp.clicked() {
                                        av.selected = Some(c.path.clone());
                                    }
                                    if resp.double_clicked() {
                                        if c.is_dir {
                                            av.dir = c.path.clone();
                                            av.selected = None;
                                        } else {
                                            open_member = Some(c.path.clone());
                                        }
                                    }
                                    if resp.drag_started() {
                                        let grab_offset = resp.interact_pointer_pos().map(|p| p - rect.min).unwrap_or(rect.size() * 0.5);
                                        start_archive_drag = Some((c.path.clone(), name.to_string(), c.is_dir, c.size, rect, grab_offset));
                                    }

                                    let archive_parent = av.archive.parent().map(Path::to_path_buf).unwrap_or_default();
                                    resp.context_menu(|ui| {
                                        if !c.is_dir {
                                            if ui.button("Open").clicked() {
                                                open_member = Some(c.path.clone());
                                                ui.close_menu();
                                            }
                                        }
                                        if ui.button("Extract next to archive").clicked() {
                                            extract_member_to = Some((c.path.clone(), c.is_dir, archive_parent));
                                            ui.close_menu();
                                        }
                                    });
                                }
                            }
                        });
                }
            }
        }

        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            if !av.dir.is_empty() {
                av.dir = av.dir.rsplit_once('/').map(|(p, _)| p.to_owned()).unwrap_or_default();
                av.selected = None;
            } else {
                exit_archive = true;
            }
        }

        let archive = av.archive.clone();

        if exit_archive {
            self.tools.archive = None;
        }

        if trigger_export_folder {
            let parent = archive.parent().map(Path::to_path_buf).unwrap_or_default();
            let name = display_name(&archive);
            let stem = archive_stem(&name).to_string();
            self.export_folder_prompt = Some(ExportFolderPrompt {
                archive: archive.clone(),
                dest_dir: parent,
                folder_name: stem,
                initial_focus: true,
            });
        }

        if trigger_export_here {
            let parent = archive.parent().map(Path::to_path_buf).unwrap_or_default();
            let has_other_items = std::fs::read_dir(&parent).map(|rd| {
                rd.filter_map(|e| e.ok()).any(|e| e.path() != archive)
            }).unwrap_or(false);

            if has_other_items {
                let name = display_name(&archive);
                let stem = archive_stem(&name).to_string();
                self.export_here_prompt = Some(ExportHerePrompt {
                    archive: archive.clone(),
                    dest_dir: parent,
                    folder_name: stem,
                    initial_focus: true,
                });
            } else {
                let name = display_name(&archive);
                let archive_path = archive.clone();
                let dest = parent.clone();
                self.spawn_job(format!("Extracting {name}"), format!("To {}", dest.display()), move |progress| {
                    extract_archive(&archive_path, &dest, progress)
                        .map(|_| format!("Extracted {name}"))
                        .map_err(|e| format!("Extract failed: {e}"))
                });
            }
        }

        if let Some((member, name, is_dir, size, origin_rect, grab_offset)) = start_archive_drag {
            self.archive_drag = Some(ArchiveDrag {
                archive: archive.clone(),
                member,
                name,
                is_dir,
                size,
                origin_rect,
                grab_offset,
            });
            self.archive_snap_back = None;
        }

        if let Some(member) = open_member {
            let tmp = std::env::temp_dir().join("chickenwm-files").join(format!("{:x}", fxhash(&archive.to_string_lossy())));
            match extract_archive_member(&archive, &member, false, &tmp) {
                Ok(path) => shell::shell_open(self.hwnd, &path, None),
                Err(e) => self.toast(format!("Couldn't open {member}: {e}"), true),
            }
        }

        if let Some((member, is_dir, dest)) = extract_member_to {
            let name = member.rsplit('/').next().unwrap_or(&member).to_string();
            let archive_path = archive.clone();
            let dest_target = dest.clone();
            self.spawn_job(format!("Extracting {name}"), format!("To {}", dest.display()), move |_| {
                extract_archive_member(&archive_path, &member, is_dir, &dest_target)
                    .map(|p| format!("Extracted {}", display_name(&p)))
                    .map_err(|e| format!("Extract failed: {e}"))
            });
            self.toast(format!("Extracting {name} to {}", display_name(&dest)), false);
        }
    }
}

fn fxhash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

// =============================================================================================
// Git status badges
// =============================================================================================

#[derive(Default)]
pub(super) struct Git {
    /// The folder the badges are for.
    pub cwd: Option<PathBuf>,
    /// Absolute path → status letter (M, A, D, R, ?, U, and • for folders containing changes).
    pub status: Arc<Mutex<HashMap<PathBuf, char>>>,
    pub branch: Arc<Mutex<Option<String>>>,
    pub summary: Arc<Mutex<Option<String>>>,
    pub fetched: Option<Instant>,
    pub busy: Arc<AtomicBool>,
}

/// Parses `git status --porcelain=v1 -z` into (relative path, status letter).
pub(super) fn parse_porcelain(out: &str) -> Vec<(String, char)> {
    let mut v = Vec::new();
    let mut fields = out.split('\0');
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }
        let (x, y) = (entry.as_bytes()[0] as char, entry.as_bytes()[1] as char);
        let path = entry[3..].to_owned();
        let letter = match (x, y) {
            ('?', '?') => '?',
            ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D') => 'U',
            ('R', _) | (_, 'R') => {
                let _ = fields.next(); // the original name follows a rename
                'R'
            }
            ('A', _) => 'A',
            ('D', _) | (_, 'D') => 'D',
            _ => 'M',
        };
        v.push((path.trim_end_matches('/').to_owned(), letter));
    }
    v
}

impl FilesApp {
    /// Refreshes the git badges for `cwd` (in the background) when it changed or every few
    /// seconds while files change.
    pub(super) fn refresh_git(&mut self, cwd: &Path, force: bool) {
        let git = &mut self.tools.git;
        let same = git.cwd.as_deref() == Some(cwd);
        if same && !force && git.fetched.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        if git.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        if !same {
            git.status.lock().clear();
            *git.branch.lock() = None;
            *git.summary.lock() = None;
        }
        git.cwd = Some(cwd.to_path_buf());
        git.fetched = Some(Instant::now());
        let (status, branch, summary, busy, ctx, cwd) = (
            git.status.clone(),
            git.branch.clone(),
            git.summary.clone(),
            git.busy.clone(),
            self.ctx.clone(),
            cwd.to_path_buf(),
        );
        std::thread::spawn(move || {
            use std::os::windows::process::CommandExt;
            let run = |args: &[&str]| {
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&cwd)
                    .args(args)
                    .creation_flags(0x0800_0000)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
            };
            let mut map = HashMap::new();
            let mut branch_name = None;
            let mut mod_count = 0;
            let mut untracked_count = 0;
            if let Some(top) = run(&["rev-parse", "--show-toplevel"]) {
                branch_name = run(&["rev-parse", "--abbrev-ref", "HEAD"])
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
                let top = PathBuf::from(String::from_utf8_lossy(&top.stdout).trim().replace('/', "\\"));
                if let Some(out) = run(&["status", "--porcelain=v1", "-z", "--untracked-files=normal"]) {
                    for (rel, letter) in parse_porcelain(&String::from_utf8_lossy(&out.stdout)) {
                        match letter {
                            '?' => untracked_count += 1,
                            '•' => {}
                            _ => mod_count += 1,
                        }
                        let abs = top.join(rel.replace('/', "\\"));
                        // Folders on the way show that something inside changed.
                        let mut p = abs.parent();
                        while let Some(dir) = p {
                            if !dir.starts_with(&top) || dir == top {
                                break;
                            }
                            map.entry(dir.to_path_buf()).or_insert('•');
                            p = dir.parent();
                        }
                        map.insert(abs, letter);
                    }
                }
            }
            *status.lock() = map;
            *branch.lock() = branch_name;
            let summary_str = if mod_count > 0 || untracked_count > 0 {
                let mut parts = Vec::new();
                if mod_count > 0 {
                    parts.push(format!("{mod_count} modified"));
                }
                if untracked_count > 0 {
                    parts.push(format!("{untracked_count} untracked"));
                }
                Some(parts.join(", "))
            } else {
                None
            };
            *summary.lock() = summary_str;
            busy.store(false, Ordering::SeqCst);
            ctx.request_repaint();
        });
    }
}

pub(super) fn git_badge_color(letter: char, th: &Theme) -> Color32 {
    match letter {
        'A' | '?' => Color32::from_rgb(110, 180, 100),
        'D' | 'U' => Color32::from_rgb(210, 90, 80),
        'R' => th.accent,
        '•' => th.dim,
        _ => Color32::from_rgb(210, 160, 70),
    }
}

// =============================================================================================
// Go anywhere (Ctrl+P)
// =============================================================================================

#[derive(Default)]
struct Recent {
    dirs: Vec<PathBuf>,
    frecency: Option<chickenwm_core::scoring::FrecencyStore>,
    loaded: bool,
}

fn recent_path() -> PathBuf {
    config_dir().join("files-recent.json")
}

fn frecency_path() -> PathBuf {
    config_dir().join("files-frecency.json")
}

impl Recent {
    fn ensure_loaded(&mut self) {
        if !self.loaded {
            self.loaded = true;
            self.dirs = std::fs::read_to_string(recent_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        }
        if self.frecency.is_none() {
            self.frecency = Some(chickenwm_core::scoring::FrecencyStore::load_from(frecency_path()));
        }
    }

    fn visit(&mut self, dir: &Path) {
        self.ensure_loaded();
        self.dirs.retain(|d| d != dir);
        self.dirs.insert(0, dir.to_path_buf());
        self.dirs.truncate(300);
        if let Ok(s) = serde_json::to_string(&self.dirs) {
            let _ = std::fs::write(recent_path(), s);
        }
        if let Some(f) = self.frecency.as_mut() {
            f.record(&dir.to_string_lossy());
        }
    }

    fn frecency_score(&self, dir: &Path) -> i64 {
        self.frecency
            .as_ref()
            .map(|f| i64::from(f.score(&dir.to_string_lossy()).min(400)))
            .unwrap_or(0)
    }
}

pub(super) struct GoAnywhere {
    query: String,
    candidates: Vec<(String, PathBuf)>,
    selected: usize,
    focus: bool,
}

pub(super) use chickenwm_core::scoring::fuzzy_score;

impl FilesApp {
    pub(super) fn open_go_anywhere(&mut self) {
        self.tools.recent.ensure_loaded();
        let mut cands: Vec<(String, PathBuf)> = Vec::new();
        let add = |label: String, p: PathBuf, cands: &mut Vec<(String, PathBuf)>| {
            if !cands.iter().any(|(_, q)| *q == p) {
                cands.push((label, p));
            }
        };
        for f in self.prefs.favorites() {
            add(format!("★ {}", f.label()), f.path.clone(), &mut cands);
        }
        for d in &self.tools.recent.dirs {
            if d.is_dir() {
                add(d.display().to_string(), d.clone(), &mut cands);
            }
        }
        for (name, p) in quick_access() {
            add(name, p, &mut cands);
        }
        let cwd = self.pane(self.focused()).cwd.clone();
        for e in std::fs::read_dir(&cwd).into_iter().flatten().flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                add(e.path().display().to_string(), e.path(), &mut cands);
            }
        }
        for d in &self.drives {
            add(format!("{} ({})", d.label, d.root.display()), d.root.clone(), &mut cands);
        }
        self.tools.goto = Some(GoAnywhere { query: String::new(), candidates: cands, selected: 0, focus: true });
    }

    fn go_anywhere_window(&mut self, ctx: &egui::Context, th: Theme) {
        self.tools.recent.ensure_loaded();
        let Some(g) = self.tools.goto.as_mut() else { return };
        let mut matches: Vec<(i64, usize)> = g
            .candidates
            .iter()
            .enumerate()
            .filter_map(|(i, (label, path))| {
                let s = fuzzy_score(&g.query, &label.to_string()).max(fuzzy_score(&g.query, &path.display().to_string()))?;
                let frec = self.tools.recent.frecency_score(path) / 8;
                Some((s + frec, i))
            })
            .collect();
        // Stable: equal scores keep favorites / recent order.
        matches.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        matches.truncate(14);
        g.selected = g.selected.min(matches.len().saturating_sub(1));
        let (down, up, enter, esc) = ctx.input(|i| {
            (i.key_pressed(egui::Key::ArrowDown), i.key_pressed(egui::Key::ArrowUp), i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape))
        });
        if down {
            g.selected = (g.selected + 1).min(matches.len().saturating_sub(1));
        }
        if up {
            g.selected = g.selected.saturating_sub(1);
        }
        let mut go: Option<PathBuf> = None;
        egui::Window::new("Go to folder")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .fixed_size([560.0, 0.0])
            .anchor(Align2::CENTER_TOP, [0.0, 70.0])
            .show(ctx, |ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut g.query).hint_text("Go to folder… (favorites, recent, nearby, drives)").desired_width(f32::INFINITY));
                if std::mem::take(&mut g.focus) || !r.has_focus() {
                    r.request_focus();
                }
                if r.changed() {
                    g.selected = 0;
                }
                ui.add_space(4.0);
                for (row, (_, i)) in matches.iter().enumerate() {
                    let (label, path) = &g.candidates[*i];
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), Sense::click());
                    if row == g.selected {
                        ui.painter().rect_filled(rect, 6.0, th.selected);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, 6.0, th.accent.gamma_multiply(0.1));
                    }
                    let clip = ui.painter().with_clip_rect(rect.shrink(4.0));
                    clip.text(Pos2::new(rect.left() + 10.0, rect.top() + 11.0), Align2::LEFT_CENTER, label, FontId::proportional(th.font), th.fg);
                    clip.text(Pos2::new(rect.left() + 10.0, rect.bottom() - 9.0), Align2::LEFT_CENTER, path.display().to_string(), FontId::proportional(th.font - 3.0), th.dim);
                    if resp.clicked() {
                        go = Some(path.clone());
                    }
                }
                if matches.is_empty() {
                    ui.label(egui::RichText::new("No matching folders").color(th.dim));
                }
            });
        if enter {
            if let Some((_, i)) = matches.get(g.selected) {
                go = Some(g.candidates[*i].1.clone());
            }
        }
        if let Some(dir) = go {
            self.tools.goto = None;
            let side = self.focused();
            self.apply(Action::Navigate(side, dir));
        } else if esc {
            self.tools.goto = None;
        }
    }
}

// =============================================================================================
// Per-frame hook
// =============================================================================================

impl FilesApp {
    /// Draws whichever tool windows are open and keeps recent folders / git badges current.
    pub(super) fn draw_tools(&mut self, ctx: &egui::Context, th: Theme) {
        let cwd = self.pane(self.focused()).cwd.clone();
        // Headless-render hook: CHICKENWM_FILES_TEST=goto|usage|batch|archive:<path> opens that tool.
        static TEST: std::sync::Once = std::sync::Once::new();
        TEST.call_once(|| match std::env::var("CHICKENWM_FILES_TEST").unwrap_or_default().as_str() {
            "goto" => self.open_go_anywhere(),
            "usage" => self.tools.usage = Some(DiskUsage::start(cwd.clone(), ctx)),
            "batch" => {
                let pane = self.pane(self.focused());
                let files: Vec<PathBuf> = (0..6).filter_map(|i| pane.entry_at(i).map(|e| e.path.clone())).collect();
                let mut b = BatchRename::new(files);
                b.find = "chickenwm-".into();
                b.replace = "cw-{n:2}-".into();
                self.tools.batch = Some(b);
            }
            s if s.starts_with("archive:") => self.tools.archive = Some(ArchiveView::open(PathBuf::from(&s[8..]))),
            _ => {}
        });
        if self.tools.last_cwd.as_ref() != Some(&cwd) {
            self.tools.last_cwd = Some(cwd.clone());
            self.tools.recent.visit(&cwd);
            self.refresh_git(&cwd, true);
        } else if self.prefs.view != crate::model::ViewMode::Icons {
            self.refresh_git(&cwd, false);
        }
        self.batch_rename_window(ctx, th);
        self.disk_usage_window(ctx, th);
        self.go_anywhere_window(ctx, th);
    }

    /// Whether a tool window is taking the keyboard (the file list shouldn't react).
    pub(super) fn tool_window_open(&self) -> bool {
        self.tools.batch.is_some() || self.tools.goto.is_some() || self.tools.archive.is_some() || self.tools.usage.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn ok(v: Vec<Result<String, String>>) -> Vec<String> {
        v.into_iter().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn rename_plain_and_template() {
        let n = names(&["IMG_001.jpg", "IMG_002.jpg", "notes.txt"]);
        let r = plan_rename(&n, "IMG_", "photo-", false, false, 1).unwrap();
        assert_eq!(ok(r), ["photo-001.jpg", "photo-002.jpg", "notes.txt"]);
        let r = plan_rename(&n, "", "trip {n:2}", false, false, 1).unwrap();
        assert_eq!(ok(r), ["trip 01.jpg", "trip 02.jpg", "trip 03.txt"]);
        let r = plan_rename(&n, "", "{name}", false, false, 1).unwrap();
        assert_eq!(ok(r), ["IMG_001.jpg", "IMG_002.jpg", "notes.txt"]);
        // Duplicates and bad characters are reported per row.
        let r = plan_rename(&n, "", "same", false, false, 1).unwrap();
        assert_eq!(r[0], Err("duplicate name".into()));
        assert_eq!(r[2], Ok("same.txt".into()));
        let r = plan_rename(&n, "", "a:b", false, false, 1).unwrap();
        assert!(r[0].is_err());
    }

    #[test]
    fn rename_regex() {
        let n = names(&["2024-05-01 report.pdf", "2023-12-31 summary.pdf"]);
        let r = plan_rename_regex(&n, r"(\d+)-(\d+)-(\d+) (.*)", "$4 ($3.$2.$1)", false, false, 1).unwrap();
        assert_eq!(ok(r), ["report (01.05.2024).pdf", "summary (31.12.2023).pdf"]);
        assert!(plan_rename_regex(&n, "(", "x", false, false, 1).is_err());
    }

    #[test]
    fn rename_all_swaps_and_rolls_back() {
        let dir = std::env::temp_dir().join(format!("cwm-files-br-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), "A").unwrap();
        std::fs::write(dir.join("b"), "B").unwrap();
        rename_all(&[(dir.join("a"), dir.join("b")), (dir.join("b"), dir.join("a"))]).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "B");
        std::fs::write(dir.join("c"), "C").unwrap();
        assert!(rename_all(&[(dir.join("a"), dir.join("c"))]).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "B");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn porcelain() {
        let out = " M src/main.rs\0?? new.txt\0R  b.rs\0a.rs\0A  added/x.rs\0";
        assert_eq!(
            parse_porcelain(out),
            vec![("src/main.rs".into(), 'M'), ("new.txt".into(), '?'), ("b.rs".into(), 'R'), ("added/x.rs".into(), 'A')]
        );
    }

    #[test]
    fn archive_tree() {
        let items = vec![
            ArchiveItem { path: "a/b/c.txt".into(), size: Some(3), is_dir: false },
            ArchiveItem { path: "a/d.txt".into(), size: Some(1), is_dir: false },
            ArchiveItem { path: "top.txt".into(), size: Some(2), is_dir: false },
            ArchiveItem { path: "empty".into(), size: None, is_dir: true },
        ];
        let root: Vec<String> = archive_children(&items, "").into_iter().map(|i| i.path).collect();
        assert_eq!(root, ["a", "empty", "top.txt"]);
        let a: Vec<(String, bool)> = archive_children(&items, "a").into_iter().map(|i| (i.path, i.is_dir)).collect();
        assert_eq!(a, [("a/b".into(), true), ("a/d.txt".into(), false)]);
    }

    #[test]
    fn fuzzy() {
        assert!(fuzzy_score("dl", r"C:\Users\me\Downloads").is_some());
        assert!(fuzzy_score("xyz", r"C:\Users").is_none());
        let a = fuzzy_score("down", r"C:\Users\me\Downloads").unwrap();
        let b = fuzzy_score("down", r"C:\Users\me\docs\own").unwrap();
        assert!(a > b);
    }

    #[test]
    fn templates() {
        assert_eq!(expand_template("{name}-{n:3}.{ext}", "x", "png", 7), "x-007.png");
        assert_eq!(expand_template("{other} {", "x", "", 1), "{other} {");
    }
}

/// Per-frame copies of the folder sizes and git badges, for the details view.
pub(super) struct RowExtras {
    pub sizes: HashMap<PathBuf, Option<u64>>,
    pub git: HashMap<PathBuf, char>,
}

#[cfg(test)]
mod archive_tests {
    use super::*;

    /// Builds a 7z with Windows' bsdtar, then lists it and extracts one member.
    #[test]
    fn seven_zip_round_trip() {
        let dir = std::env::temp_dir().join(format!("cwm-files-7z-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("in/sub")).unwrap();
        std::fs::write(dir.join("in/sub/b.txt"), "yo").unwrap();
        let ok = std::process::Command::new(crate::model::system_tar())
            .current_dir(&dir)
            .args(["--format", "7zip", "-cf", "t.7z", "in"])
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return; // no bsdtar on this machine
        }
        let items = list_archive(&dir.join("t.7z")).unwrap();
        assert!(items.iter().any(|i| i.path == "in/sub/b.txt" && !i.is_dir));
        let out = extract_archive_member(&dir.join("t.7z"), "in/sub/b.txt", false, &dir.join("out")).unwrap();
        assert_eq!(std::fs::read_to_string(out).unwrap(), "yo");
        let _ = std::fs::remove_dir_all(dir);
    }
}
