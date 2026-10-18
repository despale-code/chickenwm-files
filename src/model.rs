//! Directory model (entries, sorting, filtering, selection, history) and persisted preferences.

use flate2::read::GzDecoder;
use chickenwm_core::config::config_dir;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub ext: String,
    pub modified: Option<SystemTime>,
    pub hidden: bool,
    pub icon_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewMode {
    Details,
    Icons,
    Columns,
    Dual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortKey {
    Name,
    Modified,
    Type,
    Size,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub view: ViewMode,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub show_hidden: bool,
    pub show_preview: bool,
    pub show_extensions: bool,
    /// Old separate "Pinned" list; folded into `favorites` on first run.
    pub pinned: Vec<PathBuf>,
    /// The sidebar's Favorites, in order. `None` until first set up (then seeded with the
    /// standard folders), so a user who removes everything keeps an empty list.
    pub favorites: Option<Vec<Favorite>>,
}

/// A sidebar favorite: a folder, optionally shown under a name of the user's choosing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Favorite {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Favorite {
    pub fn new(path: PathBuf) -> Self {
        Self { path, name: None }
    }

    pub fn label(&self) -> String {
        if let Some(n) = self.name.as_deref().filter(|n| !n.trim().is_empty()) {
            return n.to_string();
        }
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            view: ViewMode::Details,
            sort: SortKey::Name,
            sort_desc: false,
            show_hidden: false,
            show_preview: true,
            show_extensions: true,
            pinned: Vec::new(),
            favorites: None,
        }
    }
}

impl Prefs {
    /// First run with Favorites: seed them with `defaults` plus anything previously pinned.
    /// Returns true when the list was created (so it can be saved).
    pub fn ensure_favorites(&mut self, defaults: Vec<Favorite>) -> bool {
        if self.favorites.is_some() {
            return false;
        }
        let mut list = defaults;
        for p in std::mem::take(&mut self.pinned) {
            if !list.iter().any(|f| f.path == p) {
                list.push(Favorite::new(p));
            }
        }
        self.favorites = Some(list);
        true
    }

    pub fn favorites(&self) -> &[Favorite] {
        self.favorites.as_deref().unwrap_or(&[])
    }

    pub fn is_favorite(&self, path: &Path) -> bool {
        self.favorites().iter().any(|f| f.path == path)
    }

    pub fn add_favorite(&mut self, path: PathBuf) {
        let list = self.favorites.get_or_insert_with(Vec::new);
        if !list.iter().any(|f| f.path == path) {
            list.push(Favorite::new(path));
        }
    }

    pub fn remove_favorite(&mut self, path: &Path) {
        if let Some(list) = self.favorites.as_mut() {
            list.retain(|f| f.path != path);
        }
    }

    /// Moves the favorite at `from` so it ends up before the item now at `to` (`to == len`
    /// moves it to the end).
    pub fn move_favorite(&mut self, from: usize, to: usize) {
        let Some(list) = self.favorites.as_mut() else { return };
        if from >= list.len() || to > list.len() || to == from || to == from + 1 {
            return;
        }
        let item = list.remove(from);
        let to = if to > from { to - 1 } else { to };
        list.insert(to, item);
    }

    pub fn rename_favorite(&mut self, path: &Path, name: Option<String>) {
        if let Some(f) = self.favorites.as_mut().and_then(|l| l.iter_mut().find(|f| f.path == path)) {
            f.name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
        }
    }
    fn path() -> PathBuf {
        config_dir().join("files.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), text);
        }
    }
}

pub fn read_directory(dir: &Path) -> std::io::Result<Vec<Entry>> {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    let mut list = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        let attrs = meta.as_ref().map_or(0, |m| m.file_attributes());
        let ext = if is_dir {
            String::new()
        } else {
            path.extension().and_then(|e| e.to_str()).unwrap_or("").to_string()
        };
        list.push(Entry {
            icon_key: crate::icons::icon_key(&path, is_dir),
            size: if is_dir { 0 } else { meta.as_ref().map_or(0, |m| m.len()) },
            modified: meta.as_ref().and_then(|m| m.modified().ok()),
            hidden: attrs & (HIDDEN | SYSTEM) != 0 || name.starts_with('.') && attrs & HIDDEN != 0,
            name,
            path,
            is_dir,
            ext,
        });
    }
    Ok(list)
}

/// One browsing location: a tab, or the right side of the dual-pane view.
#[derive(Debug, Clone)]
pub struct Pane {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    /// Indices into `entries` after hiding, filtering and sorting.
    pub visible: Vec<usize>,
    /// Keyboard cursor, an index into `visible`.
    pub cursor: usize,
    /// Start of a Shift range selection, an index into `visible`.
    pub anchor: usize,
    pub selection: HashSet<PathBuf>,
    pub filter: String,
    pub back: Vec<PathBuf>,
    pub forward: Vec<PathBuf>,
    pub error: Option<String>,
    pub scroll_to_cursor: bool,
    /// Entry the cursor should land on at the next `rebuild`.
    pending_cursor: Option<PathBuf>,
}

impl Pane {
    pub fn new(dir: PathBuf) -> Self {
        let mut pane = Self {
            cwd: dir,
            entries: Vec::new(),
            visible: Vec::new(),
            cursor: 0,
            anchor: 0,
            selection: HashSet::new(),
            filter: String::new(),
            back: Vec::new(),
            forward: Vec::new(),
            error: None,
            scroll_to_cursor: false,
            pending_cursor: None,
        };
        pane.reload();
        pane
    }

    pub fn title(&self) -> String {
        self.cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.cwd.display().to_string())
    }

    /// Re-reads the directory, keeping cursor and selection where possible.
    pub fn reload(&mut self) {
        let cursor_path = self.current().map(|e| e.path.clone());
        match read_directory(&self.cwd) {
            Ok(list) => {
                self.entries = list;
                self.error = None;
            }
            Err(e) => {
                self.entries.clear();
                self.error = Some(e.to_string());
            }
        }
        let existing: HashSet<&PathBuf> = self.entries.iter().map(|e| &e.path).collect();
        self.selection.retain(|p| existing.contains(p));
        self.pending_cursor = cursor_path;
    }

    /// Recomputes `visible` (call after reload or when sort/filter/hidden prefs change).
    pub fn rebuild(&mut self, prefs: &Prefs) {
        let cursor_path = self
            .pending_cursor
            .take()
            .or_else(|| self.current().map(|e| e.path.clone()));
        let mut idx: Vec<usize> = (0..self.entries.len())
            .filter(|&i| prefs.show_hidden || !self.entries[i].hidden)
            .collect();
        let q = self.filter.trim();
        if q.is_empty() {
            let entries = &self.entries;
            idx.sort_by(|&a, &b| {
                let (a, b) = (&entries[a], &entries[b]);
                let ord = match prefs.sort {
                    SortKey::Name => natural_cmp(&a.name, &b.name),
                    SortKey::Modified => a.modified.cmp(&b.modified),
                    SortKey::Type => a.ext.to_ascii_lowercase().cmp(&b.ext.to_ascii_lowercase()),
                    SortKey::Size => a.size.cmp(&b.size),
                }
                .then_with(|| natural_cmp(&a.name, &b.name));
                // Folders always first, like Explorer.
                b.is_dir.cmp(&a.is_dir).then(if prefs.sort_desc { ord.reverse() } else { ord })
            });
        } else {
            let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
            let pat = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
            let mut buf = Vec::new();
            let mut scored: Vec<(u32, usize)> = idx
                .into_iter()
                .filter_map(|i| {
                    buf.clear();
                    let hay = Utf32Str::new(&self.entries[i].name, &mut buf);
                    pat.score(hay, &mut matcher).map(|s| (s, i))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            idx = scored.into_iter().map(|(_, i)| i).collect();
        }
        self.visible = idx;
        self.cursor = cursor_path
            .and_then(|p| self.visible.iter().position(|&i| self.entries[i].path == p))
            .unwrap_or(0)
            .min(self.visible.len().saturating_sub(1));
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(*self.visible.get(self.cursor)?)
    }

    pub fn entry_at(&self, pos: usize) -> Option<&Entry> {
        self.entries.get(*self.visible.get(pos)?)
    }

    /// The selected paths, or the cursor item when nothing is selected.
    pub fn targets(&self) -> Vec<PathBuf> {
        if self.selection.is_empty() {
            self.current().map(|e| vec![e.path.clone()]).unwrap_or_default()
        } else {
            // Keep on-screen order.
            self.visible
                .iter()
                .map(|&i| &self.entries[i].path)
                .filter(|p| self.selection.contains(*p))
                .cloned()
                .collect()
        }
    }

    pub fn navigate(&mut self, dir: PathBuf, prefs: &Prefs) {
        if dir == self.cwd {
            return;
        }
        let old = std::mem::replace(&mut self.cwd, dir);
        self.back.push(old.clone());
        self.forward.clear();
        self.enter(Some(old), prefs);
    }

    pub fn go_back(&mut self, prefs: &Prefs) {
        if let Some(dir) = self.back.pop() {
            let old = std::mem::replace(&mut self.cwd, dir);
            self.forward.push(old.clone());
            self.enter(Some(old), prefs);
        }
    }

    pub fn go_forward(&mut self, prefs: &Prefs) {
        if let Some(dir) = self.forward.pop() {
            let old = std::mem::replace(&mut self.cwd, dir);
            self.back.push(old.clone());
            self.enter(Some(old), prefs);
        }
    }

    pub fn go_up(&mut self, prefs: &Prefs) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(parent, prefs);
        }
    }

    /// Loads the new `cwd`; when coming back up from a child folder, lands on that folder.
    fn enter(&mut self, came_from: Option<PathBuf>, prefs: &Prefs) {
        self.filter.clear();
        self.selection.clear();
        self.cursor = 0;
        self.reload();
        self.pending_cursor = came_from.filter(|p| p.parent() == Some(self.cwd.as_path()));
        self.rebuild(prefs);
        self.anchor = self.cursor;
        self.scroll_to_cursor = true;
    }

    pub fn select_path(&mut self, path: &Path) {
        if let Some(pos) = self.visible.iter().position(|&i| self.entries[i].path == path) {
            self.cursor = pos;
            self.anchor = pos;
            self.selection = HashSet::from([path.to_path_buf()]);
            self.scroll_to_cursor = true;
        }
    }

    /// Click semantics shared by every view: plain, Ctrl (toggle) and Shift (range).
    pub fn click(&mut self, pos: usize, ctrl: bool, shift: bool) {
        let Some(path) = self.entry_at(pos).map(|e| e.path.clone()) else { return };
        if shift {
            let (a, b) = (self.anchor.min(pos), self.anchor.max(pos));
            if !ctrl {
                self.selection.clear();
            }
            for p in a..=b {
                if let Some(e) = self.entry_at(p) {
                    self.selection.insert(e.path.clone());
                }
            }
        } else if ctrl {
            if !self.selection.remove(&path) {
                self.selection.insert(path);
            }
            self.anchor = pos;
        } else {
            self.selection = HashSet::from([path]);
            self.anchor = pos;
        }
        self.cursor = pos;
    }

    /// Keyboard cursor movement; Shift extends the selection like Explorer.
    pub fn move_cursor(&mut self, delta: isize, shift: bool) {
        if self.visible.is_empty() {
            return;
        }
        let max = self.visible.len() as isize - 1;
        let pos = (self.cursor as isize + delta).clamp(0, max) as usize;
        if shift {
            self.click(pos, false, true);
        } else {
            self.cursor = pos;
            self.anchor = pos;
            self.selection.clear();
            if let Some(e) = self.entry_at(pos) {
                self.selection.insert(e.path.clone());
            }
        }
        self.scroll_to_cursor = true;
    }

    pub fn select_all(&mut self) {
        self.selection = self.visible.iter().map(|&i| self.entries[i].path.clone()).collect();
    }
}

/// Windows-style "natural" order: `file2` before `file10`, case-insensitive.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    bi.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", (b / KB).ceil())
    } else {
        format!("{bytes} B")
    }
}

/// Local "2026-10-17 11:42" for the Date modified column.
pub fn format_time(t: SystemTime) -> String {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let Ok(d) = t.duration_since(SystemTime::UNIX_EPOCH) else {
        return String::new();
    };
    let ticks = d.as_nanos() as u64 / 100 + 116_444_736_000_000_000;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err()
            || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err()
        {
            return String::new();
        }
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    )
}

/// `dir/name`, or `dir/name (2)`, `(3)`, ... if taken.
pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let make = |n: u32| {
        let base = if n == 1 { stem.to_string() } else { format!("{stem} ({n})") };
        dir.join(if ext.is_empty() { base } else { format!("{base}.{ext}") })
    };
    (1..).map(make).find(|p| !p.exists()).unwrap()
}

/// Windows' own bsdtar (reads 7z / rar), not a GNU tar that may come first on PATH.
pub fn system_tar() -> PathBuf {
    let sys = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let tar = sys.join(r"System32\tar.exe");
    if tar.exists() { tar } else { PathBuf::from("tar.exe") }
}

pub fn is_archive(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    [".zip", ".tar.gz", ".tgz", ".tar", ".7z", ".rar"].iter().any(|s| l.ends_with(s))
}

pub fn archive_stem(name: &str) -> &str {
    let l = name.to_ascii_lowercase();
    for suffix in [".tar.gz", ".tgz", ".zip", ".tar", ".7z", ".rar"] {
        if l.ends_with(suffix) {
            return &name[..name.len() - suffix.len()];
        }
    }
    name
}

/// Extracts `.zip` / `.tar` / `.tar.gz` into `dest`, reporting progress in 0..=1.
pub fn extract_archive(archive: &Path, dest: &Path, progress: &dyn Fn(f32)) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    let lower = archive.to_string_lossy().to_ascii_lowercase();
    if lower.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
        let len = zip.len().max(1);
        for i in 0..zip.len() {
            let mut zf = zip.by_index(i)?;
            // `enclosed_name` rejects `..` / absolute paths (zip-slip).
            let Some(rel) = zf.enclosed_name() else { continue };
            let out = dest.join(rel);
            if zf.is_dir() {
                std::fs::create_dir_all(&out)?;
            } else {
                if let Some(p) = out.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::io::copy(&mut zf, &mut File::create(&out)?)?;
            }
            progress((i + 1) as f32 / len as f32);
        }
    } else if lower.ends_with(".7z") || lower.ends_with(".rar") {
        // Windows' bsdtar (tar.exe) reads 7z and rar.
        use std::os::windows::process::CommandExt;
        let status = std::process::Command::new(system_tar())
            .arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(dest)
            .creation_flags(0x0800_0000)
            .status()?;
        anyhow::ensure!(status.success(), "tar.exe couldn't extract this archive");
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        tar::Archive::new(GzDecoder::new(File::open(archive)?)).unpack(dest)?;
    } else {
        tar::Archive::new(File::open(archive)?).unpack(dest)?;
    }
    Ok(())
}

/// Zips `sources` (files and folders, recursively) into `zip_path`.
pub fn compress_zip(sources: &[PathBuf], zip_path: &Path, progress: &dyn Fn(f32)) -> anyhow::Result<()> {
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for src in sources {
        let base = src.parent().unwrap_or(src);
        collect_files(src, base, &mut files);
    }
    let mut zip = zip::ZipWriter::new(File::create(zip_path)?);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let total = files.len().max(1);
    let mut buf = Vec::new();
    for (i, (path, name)) in files.iter().enumerate() {
        if path.is_dir() {
            zip.add_directory(name.as_str(), opts)?;
        } else {
            zip.start_file(name.as_str(), opts)?;
            buf.clear();
            File::open(path)?.read_to_end(&mut buf)?;
            zip.write_all(&buf)?;
        }
        progress((i + 1) as f32 / total as f32);
    }
    zip.finish()?;
    Ok(())
}

fn collect_files(path: &Path, base: &Path, out: &mut Vec<(PathBuf, String)>) {
    let name = path
        .strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    if path.is_dir() {
        out.push((path.to_path_buf(), format!("{name}/")));
        if let Ok(rd) = std::fs::read_dir(path) {
            for e in rd.flatten() {
                collect_files(&e.path(), base, out);
            }
        }
    } else {
        out.push((path.to_path_buf(), name));
    }
}

/// Text / directory preview for the preview pane (images are decoded separately).
pub fn text_preview(entry: &Entry) -> String {
    if entry.is_dir {
        return match read_directory(&entry.path) {
            Ok(children) => {
                let mut s = format!("{} items\n\n", children.len());
                for ch in children.iter().take(60) {
                    s.push_str(if ch.is_dir { "📁 " } else { "    " });
                    s.push_str(&ch.name);
                    s.push('\n');
                }
                s
            }
            Err(e) => format!("Can't open folder: {e}"),
        };
    }
    if is_archive(&entry.name) {
        return format!("Archive · {}\n\nRight-click → Extract to open it.", format_bytes(entry.size));
    }
    let Ok(mut f) = File::open(&entry.path) else {
        return "Can't read this file.".to_string();
    };
    let mut buf = vec![0u8; 16 * 1024];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    if buf.iter().take(1024).any(|&b| b == 0) {
        format!("Binary file · {}", format_bytes(entry.size))
    } else {
        String::from_utf8_lossy(&buf).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fav_paths(p: &Prefs) -> Vec<String> {
        p.favorites().iter().map(|f| f.path.display().to_string()).collect()
    }

    #[test]
    fn favorites_seed_once_and_absorb_old_pins() {
        let mut p = Prefs { pinned: vec![PathBuf::from("B"), PathBuf::from("X")], ..Prefs::default() };
        assert!(p.ensure_favorites(vec![Favorite::new("A".into()), Favorite::new("B".into())]));
        assert_eq!(fav_paths(&p), ["A", "B", "X"]);
        assert!(p.pinned.is_empty());
        // An emptied list stays empty: it's the user's choice, not "not set up".
        p.remove_favorite(Path::new("A"));
        p.remove_favorite(Path::new("B"));
        p.remove_favorite(Path::new("X"));
        assert!(!p.ensure_favorites(vec![Favorite::new("A".into())]));
        assert!(p.favorites().is_empty());
    }

    #[test]
    fn favorites_reorder_rename_and_dedupe() {
        let mut p = Prefs::default();
        for n in ["A", "B", "C", "D"] {
            p.add_favorite(n.into());
        }
        p.add_favorite("B".into());
        assert_eq!(fav_paths(&p), ["A", "B", "C", "D"]);
        p.move_favorite(0, 3); // A before D
        assert_eq!(fav_paths(&p), ["B", "C", "A", "D"]);
        p.move_favorite(3, 0); // D to the top
        assert_eq!(fav_paths(&p), ["D", "B", "C", "A"]);
        p.move_favorite(1, 4); // B to the end
        assert_eq!(fav_paths(&p), ["D", "C", "A", "B"]);
        p.move_favorite(2, 3); // no-op: already there
        assert_eq!(fav_paths(&p), ["D", "C", "A", "B"]);
        p.rename_favorite(Path::new("C"), Some("  Work ".into()));
        assert_eq!(p.favorites()[1].label(), "Work");
        p.rename_favorite(Path::new("C"), Some("   ".into()));
        assert_eq!(p.favorites()[1].label(), "C");
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["file10", "File2", "file1", "a", "file02b"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["a", "file1", "File2", "file02b", "file10"]);
    }

    #[test]
    fn unique_names_and_archives() {
        let dir = std::env::temp_dir().join("chickenwm-files-unique-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("New folder")).unwrap();
        assert_eq!(unique_path(&dir, "New folder", ""), dir.join("New folder (2)"));
        assert_eq!(unique_path(&dir, "notes", "txt"), dir.join("notes.txt"));
        assert_eq!(archive_stem("backup.tar.gz"), "backup");
        assert!(is_archive("X.ZIP"));

        // Round-trip a folder through zip.
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub").join("a.txt"), b"hello").unwrap();
        let zip_path = dir.join("out.zip");
        compress_zip(&[src.clone()], &zip_path, &|_| {}).unwrap();
        let out = dir.join("out");
        extract_archive(&zip_path, &out, &|_| {}).unwrap();
        assert_eq!(std::fs::read(out.join("src").join("sub").join("a.txt")).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selection_semantics() {
        let dir = std::env::temp_dir().join("chickenwm-files-sel-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["a", "b", "c", "d"] {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        let prefs = Prefs::default();
        let mut pane = Pane::new(dir.clone());
        pane.rebuild(&prefs);
        pane.click(1, false, false);
        pane.click(3, false, true);
        assert_eq!(pane.targets(), vec![dir.join("b"), dir.join("c"), dir.join("d")]);
        pane.click(2, true, false);
        assert_eq!(pane.targets(), vec![dir.join("b"), dir.join("d")]);
        pane.filter = "d".into();
        pane.rebuild(&prefs);
        assert_eq!(pane.visible.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
