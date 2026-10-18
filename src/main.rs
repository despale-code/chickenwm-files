//! Phase 8: ChickenWM Files (`chickenwm-files`), the ChickenWM file explorer.
//! - `app`: window, Details / Icons / Columns / Dual-pane views, tabs, breadcrumbs, sidebar,
//!   context menus, drag and drop (internal, into other apps, from other apps), keyboard (standard
//!   Windows shortcuts plus vim-style j/k/h/l, gg/G, yy, dd, p, cw, /)
//! - `shell`: Windows integration (Recycle Bin, shell copy/move with conflict dialogs, system file
//!   clipboard, OLE drag-out, native context menu, Open with, Properties, shortcuts)
//! - `icons`: shell icons and type names; `model`: directory model, sorting, archives, prefs
//! - Live CSS styling (`~/.config/chickenwm/files.css`) and live folder refresh (file watcher)

// Release builds are windowed apps: no console window when launched from the shell, a
// hotkey or the launcher. Debug builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod icons;
mod model;
mod shell;

use anyhow::Result;
use clap::Parser;
use eframe::egui;
use chickenwm_core::bootstrap_config_and_themes;
use chickenwm_core::css::{load_css_sheet, spawn_config_watcher, ConfigWatchEvent};
use chickenwm_core::failsafe::spawn_emergency_failsafe_thread;
use chickenwm_core::init_tracing;
use chickenwm_core::monitor::{enumerate_monitors, max_native_refresh_rate};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use windows::Win32::Storage::FileSystem::GetLogicalDriveStringsW;

#[derive(Parser, Debug)]
#[command(
    name = "chickenwm-files",
    version,
    about = "ChickenWM Keyboard-First File Explorer (Ranger / Yazi / Total Commander style)"
)]
struct Cli {
    /// Initial directory (or file, to select it) to open. Also accepts explorer.exe-style
    /// arguments such as `/select,C:\path\file.txt`, `/e,C:\dir` or `/root,C:\dir`, since the
    /// shell hands those over when chickenwm-files is the default file manager.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    path: Vec<String>,
}

/// Turns raw launch arguments into `(directory to open, entry to select)`.
fn resolve_launch_target(args: &[String]) -> (PathBuf, Option<PathBuf>) {
    let home = || dirs::home_dir().unwrap_or_else(|| PathBuf::from(r"C:\"));
    let joined = args.join(" ");
    let joined = joined.trim();
    let mut select = false;
    let mut target;
    if joined.starts_with('/') {
        // explorer.exe switch syntax: `/switch,/switch,path` (paths may still contain commas,
        // so everything after the last recognised switch is the target).
        let mut rest = joined;
        loop {
            let (head, tail) = rest.split_once(',').unwrap_or((rest, ""));
            match head.trim().to_ascii_lowercase().as_str() {
                "/select" => select = true,
                "/e" | "/n" | "/root" | "/separate" => {}
                _ => break,
            }
            rest = tail;
            if rest.is_empty() {
                break;
            }
        }
        target = rest.trim().trim_matches('"').trim().to_string();
    } else {
        target = joined.trim_matches('"').trim().to_string();
    }
    // Virtual shell locations (`::{CLSID}`, `shell:Downloads`) have no filesystem path to browse.
    if target.is_empty() || target.starts_with("::") || target.to_ascii_lowercase().starts_with("shell:")
    {
        if !target.is_empty() {
            tracing::warn!("Unsupported shell location {target:?}; opening home directory");
        }
        return (home(), None);
    }
    // A bare drive letter like `D:` means the drive root, not D:'s current directory.
    if target.len() == 2 && target.ends_with(':') {
        target.push('\\');
    }
    let path = PathBuf::from(&target);
    if path.is_dir() && !select {
        (path, None)
    } else if path.exists() {
        match path.parent() {
            Some(parent) => (parent.to_path_buf(), Some(path)),
            None => (path, None),
        }
    } else {
        tracing::warn!("Launch target {target:?} does not exist; opening home directory");
        (home(), None)
    }
}

/// Phase 8.1: Enumerates logical drives (`C:\`, `D:\`, etc.) via Win32 `GetLogicalDriveStringsW`.
pub fn enumerate_logical_drives() -> Vec<PathBuf> {
    let mut buf = [0u16; 256];
    let mut drives = Vec::new();
    unsafe {
        let len = GetLogicalDriveStringsW(Some(&mut buf)) as usize;
        if len > 0 && len < buf.len() {
            for slice in buf[..len].split(|&c| c == 0) {
                if !slice.is_empty() {
                    drives.push(PathBuf::from(String::from_utf16_lossy(slice)));
                }
            }
        }
    }
    if drives.is_empty() {
        drives.push(PathBuf::from(r"C:\"));
    }
    drives
}


fn main() -> Result<()> {
    init_tracing("chickenwm_files");
    let _failsafe = spawn_emergency_failsafe_thread();
    let cli = Cli::parse();

    let (_doc, cfg) = bootstrap_config_and_themes()?;
    let monitors = enumerate_monitors();
    let native_hz = max_native_refresh_rate(&monitors);
    let target_fps = if cfg.misc.fps_limit > 0 {
        cfg.misc.fps_limit
    } else {
        native_hz
    };

    let css = Arc::new(RwLock::new(
        load_css_sheet("files.css").unwrap_or_default(),
    ));
    let css_watcher_ref = Arc::clone(&css);
    let _watcher = spawn_config_watcher(move |ev| {
        if let ConfigWatchEvent::CssChanged { file_name, .. } = ev {
            if file_name.eq_ignore_ascii_case("files.css") {
                if let Ok(new_sheet) = load_css_sheet("files.css") {
                    *css_watcher_ref.write() = new_sheet;
                }
            }
        }
    });

    // `--render-to out.png [--render-size WxH]`: one frame, headless (the path argument swallows
    // every argument, so the render flags are taken out of it first).
    let render = chickenwm_core::headless::RenderRequest::from_args([1240.0, 760.0]);
    let path = chickenwm_core::headless::strip_render_args(&cli.path);
    let (initial_dir, initial_select) = resolve_launch_target(&path);
    if let Some(req) = render {
        return chickenwm_core::headless::render_app(&req, |cc| {
            app::FilesApp::new(cc, initial_dir, initial_select, css, target_fps)
        });
    }

    let native_options = eframe::NativeOptions {
        // glow, not wgpu: egui_kittest (headless feature) also enables eframe's wgpu backend,
        // which eframe would then prefer, and wgpu draws transparent windows black on Windows.
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("ChickenWM Files")
            .with_inner_size([1240.0, 760.0])
            .with_min_inner_size([640.0, 400.0])
            // Accept files dragged in from Explorer and other apps.
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        "ChickenWM Files",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(app::FilesApp::new(cc, initial_dir, initial_select, css, target_fps)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_target_parsing() {
        let dir = std::env::temp_dir().join("chickenwm files, test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let d = dir.to_string_lossy().into_owned();
        let f = file.to_string_lossy().into_owned();

        assert_eq!(resolve_launch_target(&[format!("\"{d}\"")]), (dir.clone(), None));
        assert_eq!(resolve_launch_target(&[f.clone()]), (dir.clone(), Some(file.clone())));
        assert_eq!(
            resolve_launch_target(&[format!("/select,\"{f}\"")]),
            (dir.clone(), Some(file.clone()))
        );
        assert_eq!(resolve_launch_target(&[format!("/e,{d}")]), (dir.clone(), None));
        assert_eq!(resolve_launch_target(&["C:".into()]), (PathBuf::from(r"C:\"), None));
        let home = dirs::home_dir().unwrap();
        assert_eq!(resolve_launch_target(&[]), (home.clone(), None));
        assert_eq!(resolve_launch_target(&["shell:Downloads".into()]), (home.clone(), None));
        assert_eq!(resolve_launch_target(&[r"Z:\nope\missing".into()]), (home, None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
