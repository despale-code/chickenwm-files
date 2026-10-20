//! Shell icons and file type names ("Text Document", "PNG File") for the file lists.
//!
//! Lookups run on a COM worker thread and come back as RGBA, which the UI turns into egui
//! textures. Ordinary files are looked up by extension (`SHGFI_USEFILEATTRIBUTES`, no disk
//! access, one icon per extension); executables, shortcuts and drives by their actual path,
//! since each has its own icon.
//!
//! When an icon theme is active (`theme { icon_theme = ... }` in chickenwm.conf), its PNGs win
//! over the shell icons; anything the theme doesn't cover falls back to the shell.
use chickenwm_core::handles::SafeHicon;
use chickenwm_core::icon_theme::{IconRequest, IconTheme, SPECIAL_FOLDERS};
use std::collections::HashMap;
use std::mem::size_of;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use windows::core::HSTRING;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_FLAGS, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SMALLICON,
    SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, HICON, ICONINFO};

/// Icon lookup key: `dir`, `ext:.png`, or `path:C:\...` for items with their own icon.
pub fn icon_key(path: &Path, is_dir: bool) -> String {
    if is_dir {
        // Drive roots have their own icons; ordinary folders share one.
        return if path.parent().is_none() {
            format!("path:{}", path.display())
        } else {
            "dir".to_string()
        };
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(ext.as_str(), "exe" | "lnk" | "ico" | "url" | "cpl" | "msc" | "appref-ms" | "scr") {
        format!("path:{}", path.display())
    } else {
        format!("ext:.{ext}")
    }
}

struct Decoded {
    key: String,
    large: bool,
    rgba: Option<(usize, usize, Vec<u8>)>,
    type_name: Option<String>,
    /// Icon theme the lookup was made for; results for a previous theme are dropped.
    theme: String,
}

/// Special folder name (downloads, ...) for a path, if it is one.
fn special_folder(path: &Path) -> Option<&'static str> {
    let dirs_by_name = [
        (SPECIAL_FOLDERS[0], dirs::home_dir()),
        (SPECIAL_FOLDERS[1], dirs::desktop_dir()),
        (SPECIAL_FOLDERS[2], dirs::download_dir()),
        (SPECIAL_FOLDERS[3], dirs::document_dir()),
        (SPECIAL_FOLDERS[4], dirs::picture_dir()),
        (SPECIAL_FOLDERS[5], dirs::audio_dir()),
        (SPECIAL_FOLDERS[6], dirs::video_dir()),
    ];
    dirs_by_name
        .into_iter()
        .find(|(_, d)| d.as_deref() == Some(path))
        .map(|(n, _)| n)
}

/// What the icon theme should be asked for, given a cache key.
fn theme_request(key: &str) -> Option<IconRequest> {
    if key == "dir" {
        return Some(IconRequest::Folder);
    }
    if let Some(ext) = key.strip_prefix("ext:.") {
        return Some(IconRequest::File(ext.to_string()));
    }
    let path = Path::new(key.strip_prefix("path:")?);
    if path.parent().is_none() {
        return Some(IconRequest::Drive);
    }
    if path.is_dir() {
        return Some(match special_folder(path) {
            Some(s) => IconRequest::SpecialFolder(s.to_string()),
            None => IconRequest::Folder,
        });
    }
    // Programs and shortcuts keep their own icon unless the theme has pp-<name>.png.
    let stem = path.file_stem()?.to_string_lossy().to_ascii_lowercase();
    Some(IconRequest::App(stem))
}

fn load_theme_image(path: &Path, large: bool) -> Option<(usize, usize, Vec<u8>)> {
    // Rendered at 16 / 40 logical px; keep 2x for high-DPI.
    let side = if large { 80 } else { 32 };
    chickenwm_core::icon_theme::load_icon_image(path, side)
}

fn lookup(key: &str, large: bool, theme: &IconTheme) -> Decoded {
    let themed = theme_request(key)
        .and_then(|req| theme.resolve(&req))
        .and_then(|p| load_theme_image(&p, large));
    let mut decoded = shell_lookup(key, large, themed.is_some());
    decoded.theme = theme.name.clone();
    if themed.is_some() {
        decoded.rgba = themed;
    }
    decoded
}

/// Shell icon + type name; with `name_only`, skips extracting the icon.
fn shell_lookup(key: &str, large: bool, name_only: bool) -> Decoded {
    let (target, attrs, use_attrs) = if key == "dir" {
        ("folder".to_string(), FILE_ATTRIBUTE_DIRECTORY, true)
    } else if let Some(ext) = key.strip_prefix("ext:") {
        (format!("file{ext}"), FILE_ATTRIBUTE_NORMAL, true)
    } else {
        (key.trim_start_matches("path:").to_string(), FILE_ATTRIBUTE_NORMAL, false)
    };
    let mut flags: SHGFI_FLAGS = if name_only { SHGFI_TYPENAME } else { SHGFI_ICON | SHGFI_TYPENAME };
    flags |= if large { SHGFI_LARGEICON } else { SHGFI_SMALLICON };
    if use_attrs {
        flags |= SHGFI_USEFILEATTRIBUTES;
    }
    let mut sfi = SHFILEINFOW::default();
    let (rgba, type_name) = unsafe {
        let res = SHGetFileInfoW(
            &HSTRING::from(target),
            attrs,
            Some(&mut sfi),
            size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        if res == 0 {
            (None, None)
        } else {
            let rgba = if sfi.hIcon.is_invalid() {
                None
            } else {
                let _guard = SafeHicon::new(sfi.hIcon);
                hicon_to_rgba(sfi.hIcon)
            };
            let len = sfi.szTypeName.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&sfi.szTypeName[..len]);
            (rgba, (!name.is_empty()).then_some(name))
        }
    };
    Decoded {
        key: key.to_string(),
        large,
        rgba,
        type_name,
        theme: String::new(),
    }
}

fn read_bitmap_bgra(hbm: HBITMAP, width: i32, height: i32) -> Option<Vec<u8>> {
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (width * height * 4) as usize];
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let lines = GetDIBits(
            dc,
            hbm,
            0,
            height as u32,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        (lines > 0).then_some(buf)
    }
}

unsafe fn hicon_to_rgba(hicon: HICON) -> Option<(usize, usize, Vec<u8>)> {
    let mut info = ICONINFO::default();
    unsafe { GetIconInfo(hicon, &mut info) }.ok()?;
    let decoded = (|| {
        if info.hbmColor.is_invalid() {
            return None;
        }
        let mut bm = BITMAP::default();
        let got = unsafe {
            GetObjectW(
                info.hbmColor,
                size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut BITMAP as *mut core::ffi::c_void),
            )
        };
        if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        let mut px = read_bitmap_bgra(info.hbmColor, w, h)?;
        // Icons without an alpha channel keep transparency in the AND mask.
        if px.chunks_exact(4).all(|p| p[3] == 0) {
            let mask = read_bitmap_bgra(info.hbmMask, w, h)?;
            for (p, m) in px.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                p[3] = if m[0] == 0 { 255 } else { 0 };
            }
        }
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2);
        }
        Some((w as usize, h as usize, px))
    })();
    unsafe {
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor);
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(info.hbmMask);
        }
    }
    decoded
}

/// UI-side cache: request icons by key, get textures once the worker delivers them.
pub struct IconCache {
    requests: Sender<(String, bool, IconTheme)>,
    theme: IconTheme,
    results: Receiver<Decoded>,
    textures: HashMap<(String, bool), Option<egui::TextureHandle>>,
    type_names: HashMap<String, String>,
}

impl IconCache {
    pub fn new(ctx: egui::Context) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<(String, bool, IconTheme)>();
        let (res_tx, res_rx) = mpsc::channel::<Decoded>();
        std::thread::Builder::new()
            .name("chickenwm-files-icons".into())
            .spawn(move || {
                crate::shell::init_com_thread();
                while let Ok((key, large, theme)) = req_rx.recv() {
                    if res_tx.send(lookup(&key, large, &theme)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn icon thread");
        Self {
            requests: req_tx,
            theme: IconTheme::current(),
            results: res_rx,
            textures: HashMap::new(),
            type_names: HashMap::new(),
        }
    }

    /// Switches icon theme; every icon is looked up again.
    pub fn set_theme(&mut self, theme: IconTheme) {
        if theme != self.theme {
            self.theme = theme;
            self.textures.clear();
        }
    }

    /// Re-reads icons (e.g. the built-in theme was regenerated for a new palette).
    pub fn reload(&mut self) {
        self.textures.clear();
    }

    /// Uploads icons that arrived since the last frame.
    pub fn absorb(&mut self, ctx: &egui::Context) {
        for d in self.results.try_iter() {
            if d.theme != self.theme.name {
                continue;
            }
            if let Some(name) = d.type_name {
                self.type_names.insert(d.key.clone(), name);
            }
            let tex = d.rgba.map(|(w, h, px)| {
                let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &px);
                ctx.load_texture(format!("icon:{}:{}", d.key, d.large), img, egui::TextureOptions::LINEAR)
            });
            self.textures.insert((d.key, d.large), tex);
        }
    }

    pub fn get(&mut self, key: &str, large: bool) -> Option<egui::TextureHandle> {
        let k = (key.to_string(), large);
        match self.textures.get(&k) {
            Some(t) => t.clone(),
            None => {
                // Placeholder until the worker answers, so each key is requested once.
                self.textures.insert(k.clone(), None);
                let _ = self.requests.send((k.0, k.1, self.theme.clone()));
                None
            }
        }
    }

    /// Shell type name for a key ("File folder", "PNG File"), once its icon has been looked up.
    pub fn type_name(&self, key: &str) -> Option<&str> {
        self.type_names.get(key).map(String::as_str)
    }
}
