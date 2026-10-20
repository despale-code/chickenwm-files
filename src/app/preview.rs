//! Preview content: images, archives, PDFs, audio/video thumbnails, highlighted code and text,
//! drawn in a card that fills the space it is given, with everything centered.

use super::{Preview, Theme};
use crate::model::{format_bytes, is_archive, Entry};
use eframe::egui::{self, text::LayoutJob, Align2, Color32, CornerRadius, FontId, Rect, Sense, Stroke, TextFormat, Vec2};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// What the preview holds, decided when the selection changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Kind {
    #[default]
    Empty,
    Image,
    /// Source code, with the language label shown in the header.
    Code(&'static str),
    /// Prose / plain text, wrapped.
    Text,
    /// Folder or archive listing.
    Folder,
    /// PDF document preview.
    Pdf,
    /// Audio or video file preview.
    Media,
    /// A one-line note (binary file, unreadable…), shown centered.
    Message,
}

pub(super) fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

pub(super) fn is_audio_video(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" | "opus" | "mp4" | "mkv" | "webm" | "mov" | "avi" | "wmv"
    )
}

/// Language label and comment syntax for a file extension.
fn language(ext: &str) -> Option<(&'static str, Comments)> {
    const C: Comments = Comments { line: &["//"], block: Some(("/*", "*/")) };
    const HASH: Comments = Comments { line: &["#"], block: None };
    const NONE: Comments = Comments { line: &[], block: None };
    Some(match ext {
        "rs" => ("Rust", C),
        "js" | "mjs" | "cjs" | "jsx" => ("JavaScript", C),
        "ts" | "tsx" => ("TypeScript", C),
        "c" | "h" => ("C", C),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => ("C++", C),
        "cs" => ("C#", C),
        "java" => ("Java", C),
        "kt" | "kts" => ("Kotlin", C),
        "go" => ("Go", C),
        "swift" => ("Swift", C),
        "dart" => ("Dart", C),
        "zig" => ("Zig", C),
        "glsl" | "hlsl" | "wgsl" | "vert" | "frag" => ("Shader", C),
        "css" | "scss" | "less" => ("CSS", Comments { line: &[], block: Some(("/*", "*/")) }),
        "json" | "jsonc" => ("JSON", C),
        "py" | "pyw" => ("Python", HASH),
        "rb" => ("Ruby", HASH),
        "sh" | "bash" | "zsh" | "fish" => ("Shell", HASH),
        "ps1" | "psm1" => ("PowerShell", Comments { line: &["#"], block: Some(("<#", "#>")) }),
        "toml" => ("TOML", HASH),
        "yaml" | "yml" => ("YAML", HASH),
        "conf" | "cfg" => ("Config", HASH),
        "ini" => ("INI", Comments { line: &[";", "#"], block: None }),
        "sql" => ("SQL", Comments { line: &["--"], block: Some(("/*", "*/")) }),
        "lua" => ("Lua", Comments { line: &["--"], block: None }),
        "html" | "htm" | "xml" | "svg" | "xaml" | "vue" => ("Markup", Comments { line: &[], block: Some(("<!--", "-->")) }),
        "bat" | "cmd" => ("Batch", Comments { line: &["::", "REM ", "rem "], block: None }),
        "diff" | "patch" => ("Diff", NONE),
        _ => return None,
    })
}

#[derive(Clone, Copy)]
struct Comments {
    line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
}

pub(super) fn classify(entry: &Entry, text: &str) -> Kind {
    if entry.is_dir || is_archive(&entry.name) || super::tools::is_browsable_archive(&entry.name) {
        return Kind::Folder;
    }
    if is_pdf(&entry.path) {
        return Kind::Pdf;
    }
    if is_audio_video(&entry.path) {
        return Kind::Media;
    }
    let ext = entry.path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    // Short notes from `text_preview` (binary, unreadable) have no real content.
    if text.starts_with("Binary file · ") || text == "Can't read this file." {
        return Kind::Message;
    }
    if let Some((name, _)) = language(&ext) {
        return Kind::Code(name);
    }
    if text.trim().is_empty() {
        return Kind::Message;
    }
    Kind::Text
}

/// Builds a folder-style listing for an archive (.zip, .tar, .7z, .rar, etc.).
pub(super) fn archive_preview_text(entry: &Entry, cancel: &AtomicBool) -> String {
    if cancel.load(Ordering::Relaxed) {
        return String::new();
    }
    match super::tools::list_archive(&entry.path) {
        Ok(items) => {
            let mut s = format!("Archive · {} items ({})\n\n", items.len(), format_bytes(entry.size));
            for item in items.iter().take(80) {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                if item.is_dir {
                    s.push_str("📁 ");
                    s.push_str(&item.path);
                } else {
                    s.push_str("    ");
                    s.push_str(&item.path);
                    if let Some(sz) = item.size {
                        s.push_str(&format!("  ({})", format_bytes(sz)));
                    }
                }
                s.push('\n');
            }
            s
        }
        Err(_) => format!("Archive · {}\n\nRight-click → Extract to open it.", format_bytes(entry.size)),
    }
}

/// Extracts page count, metadata, and readable text snippets from a PDF file.
pub(super) fn pdf_preview_text(entry: &Entry, cancel: &AtomicBool) -> String {
    let Ok(mut f) = std::fs::File::open(&entry.path) else {
        return format!("PDF Document · {}", format_bytes(entry.size));
    };
    let mut buf = vec![0u8; 256 * 1024];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    if cancel.load(Ordering::Relaxed) {
        return String::new();
    }
    let raw = String::from_utf8_lossy(&buf);
    let pages = raw.matches("/Type /Page").count().saturating_sub(raw.matches("/Type /Pages").count()).max(1);
    let mut lines = vec![format!("PDF Document · ~{pages} page(s) · {}", format_bytes(entry.size))];
    for tag in ["/Title", "/Author", "/Subject"] {
        if let Some(pos) = raw.find(tag) {
            let after = &raw[pos + tag.len()..];
            if let Some(open) = after.find('(') {
                if let Some(close) = after[open + 1..].find(')') {
                    let val = after[open + 1..open + 1 + close].trim();
                    if !val.is_empty() && val.chars().all(|c| !c.is_control()) {
                        lines.push(format!("{}: {val}", &tag[1..]));
                    }
                }
            }
        }
    }
    lines.push(String::new());
    // Extract readable ASCII text literals inside BT ... ET blocks
    let mut snippets = Vec::new();
    let mut rest = raw.as_ref();
    while let Some(bt) = rest.find("BT") {
        let after_bt = &rest[bt + 2..];
        let Some(et) = after_bt.find("ET") else { break };
        let block = &after_bt[..et];
        let mut b_rest = block;
        while let Some(o) = b_rest.find('(') {
            let after_o = &b_rest[o + 1..];
            let Some(c) = after_o.find(')') else { break };
            let lit = after_o[..c].trim();
            if lit.len() >= 3 && lit.chars().all(|ch| ch.is_ascii_graphic() || ch == ' ') {
                snippets.push(lit.to_owned());
            }
            b_rest = &after_o[c + 1..];
        }
        if snippets.len() >= 24 {
            break;
        }
        rest = &after_bt[et + 2..];
    }
    if !snippets.is_empty() {
        lines.push(snippets.join(" "));
    }
    lines.join("\n")
}

/// Extracts metadata summary for audio/video files.
pub(super) fn media_preview_text(entry: &Entry) -> String {
    let ext = entry.ext.to_ascii_uppercase();
    let is_audio = matches!(
        entry.ext.to_ascii_lowercase().as_str(),
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" | "opus"
    );
    let kind = if is_audio { "Audio" } else { "Video" };
    format!("{kind} ({ext}) · {}", format_bytes(entry.size))
}

/// Extracts an embedded JPEG/PNG thumbnail from a media or PDF file, or synthesizes a clean
/// visual thumbnail card if none is embedded.
pub(super) fn extract_or_synthesize_thumbnail(
    entry: &Entry,
    cancel: &AtomicBool,
) -> Option<(egui::ColorImage, [usize; 2])> {
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    if let Ok(mut f) = std::fs::File::open(&entry.path) {
        let mut buf = vec![0u8; 2 * 1024 * 1024];
        let n = f.read(&mut buf).unwrap_or(0);
        buf.truncate(n);
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        // Look for embedded PNG (`\x89PNG\r\n\x1a\n` .. `IEND`)
        if let Some(start) = buf.windows(8).position(|w| w == b"\x89PNG\r\n\x1a\n") {
            if let Some(end_rel) = buf[start..].windows(4).position(|w| w == b"IEND") {
                let end = (start + end_rel + 8).min(buf.len());
                if let Ok(img) = image::load_from_memory(&buf[start..end]) {
                    let dims = [img.width() as usize, img.height() as usize];
                    let thumb = img.thumbnail(480, 480).to_rgba8();
                    let size = [thumb.width() as usize, thumb.height() as usize];
                    return Some((egui::ColorImage::from_rgba_unmultiplied(size, thumb.as_raw()), dims));
                }
            }
        }
        // Look for embedded JPEG (`\xFF\xD8\xFF` .. `\xFF\xD9`)
        let mut search = 0;
        while search + 4 < buf.len() {
            let Some(rel) = buf[search..].windows(3).position(|w| w == [0xFF, 0xD8, 0xFF]) else {
                break;
            };
            let start = search + rel;
            if let Some(end_rel) = buf[start + 3..].windows(2).position(|w| w == [0xFF, 0xD9]) {
                let end = start + 3 + end_rel + 2;
                if end - start > 512 {
                    if let Ok(img) = image::load_from_memory(&buf[start..end]) {
                        let dims = [img.width() as usize, img.height() as usize];
                        let thumb = img.thumbnail(480, 480).to_rgba8();
                        let size = [thumb.width() as usize, thumb.height() as usize];
                        return Some((egui::ColorImage::from_rgba_unmultiplied(size, thumb.as_raw()), dims));
                    }
                }
            }
            search = start + 3;
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    // Synthesize a clean thumbnail card if no embedded cover art/thumbnail exists
    let (w, h) = (240usize, 180usize);
    let mut pixels = vec![Color32::from_rgb(36, 40, 56); w * h];
    let is_pdf_file = is_pdf(&entry.path);
    let accent = if is_pdf_file {
        [225u8, 85, 95]
    } else {
        [110u8, 155, 245]
    };
    for y in 0..h {
        for x in 0..w {
            let border = x < 2 || x + 2 >= w || y < 2 || y + 2 >= h;
            let bar = !is_pdf_file && (y > h / 2 - 24 && y < h / 2 + 24) && ((x / 8) % 2 == 0)
                && {
                    let wave = (((x as f32 * 0.17).sin().abs() * 20.0) as usize).max(4);
                    y.abs_diff(h / 2) <= wave
                };
            let page_line = is_pdf_file && x >= 44 && x < w - 44 && y >= 28 && y < h - 28 && (y % 14 < 3);
            if border || bar || page_line {
                pixels[y * w + x] = Color32::from_rgb(accent[0], accent[1], accent[2]);
            }
        }
    }
    Some((egui::ColorImage { size: [w, h], pixels }, [w, h]))
}

/// Draws the preview card over all of the space left in `ui`.
pub(super) fn body(ui: &mut egui::Ui, th: &Theme, preview: &Preview, salt: &str) {
    let rect = ui.available_rect_before_wrap();
    let rect = Rect::from_min_size(rect.min, Vec2::new(rect.width(), rect.height().max(80.0)));
    ui.allocate_rect(rect, Sense::hover());
    let radius = CornerRadius::same(th.radius.clamp(6, 14));
    ui.painter().rect(rect, radius, th.bg.gamma_multiply(0.35), Stroke::new(1.0_f32, th.hairline()), egui::StrokeKind::Inside);
    let inner = rect.shrink(12.0);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Center)));
    let ui = &mut child;

    if preview.loading_image && preview.image.is_none() && preview.text.is_empty() {
        centered(ui, inner, |ui| {
            ui.spinner();
        });
        return;
    }
    if let Some((tex, dims)) = &preview.image {
        if matches!(preview.kind, Kind::Pdf | Kind::Media) && !preview.text.is_empty() {
            let size = tex.size_vec2();
            let scale = (inner.width() / size.x).min(140.0 / size.y).min(1.5);
            let img_size = size * scale;
            let (img_rect, _) = ui.allocate_exact_size(img_size, Sense::hover());
            egui::Image::new(tex).corner_radius(CornerRadius::same(6)).paint_at(ui, img_rect);
            ui.add_space(8.0);
            egui::ScrollArea::vertical().id_salt(salt).auto_shrink([false, false]).show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(preview.text.trim_end())
                            .size(th.font - 0.5)
                            .color(th.fg)
                            .line_height(Some(th.font * 1.4)),
                    )
                    .wrap(),
                );
            });
            return;
        }
        // Fit inside the card (small images may grow up to 2×), centered, caption below.
        let caption_h = 22.0;
        let size = tex.size_vec2();
        let room = Vec2::new(inner.width(), (inner.height() - caption_h).max(16.0));
        let scale = (room.x / size.x).min(room.y / size.y).min(2.0);
        let img_size = size * scale;
        let top = inner.top() + (room.y - img_size.y) / 2.0;
        let img_rect = Rect::from_min_size(egui::pos2(inner.center().x - img_size.x / 2.0, top), img_size);
        // Checkerboard behind transparent pixels.
        checkerboard(ui.painter(), img_rect, th);
        egui::Image::new(tex).corner_radius(CornerRadius::same(4)).paint_at(ui, img_rect);
        ui.painter().text(
            egui::pos2(inner.center().x, img_rect.bottom() + caption_h / 2.0 + 2.0),
            Align2::CENTER_CENTER,
            format!("{} × {} px", dims[0], dims[1]),
            FontId::proportional(th.font - 2.0),
            th.dim,
        );
        return;
    }
    match preview.kind {
        Kind::Empty => {}
        Kind::Image => centered(ui, inner, |ui| {
            ui.label(egui::RichText::new("Can't decode this image").color(th.dim));
        }),
        Kind::Message => centered(ui, inner, |ui| {
            ui.label(egui::RichText::new(preview.text.trim()).color(th.dim));
        }),
        Kind::Folder => folder(ui, th, &preview.text, salt),
        Kind::Pdf | Kind::Media | Kind::Text => {
            let label = match preview.kind {
                Kind::Pdf => "PDF",
                Kind::Media => "Media",
                _ => "Text",
            };
            header(ui, th, label, line_count(&preview.text));
            egui::ScrollArea::vertical().id_salt(salt).auto_shrink([false, false]).show(ui, |ui| {
                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(preview.text.trim_end()).size(th.font - 0.5).color(th.fg).line_height(Some(th.font * 1.45)))
                            .halign(egui::Align::Center)
                            .wrap(),
                    );
                });
            });
        }
        Kind::Code(lang) => {
            header(ui, th, lang, line_count(&preview.text));
            let ext = preview
                .path
                .as_ref()
                .and_then(|p| p.extension())
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let comments = language(&ext).map(|l| l.1).unwrap_or(Comments { line: &[], block: None });
            let job = highlight(&preview.text, comments, th);
            egui::ScrollArea::both().id_salt(salt).auto_shrink([false, false]).show(ui, |ui| {
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.add(egui::Label::new(job).extend());
                });
            });
        }
    }
}

fn line_count(text: &str) -> usize {
    text.lines().count().max(1)
}

/// Small centered header: language chip and line count.
fn header(ui: &mut egui::Ui, th: &Theme, label: &str, lines: usize) {
    let font = FontId::proportional(th.font - 2.5);
    let chip = ui.painter().layout_no_wrap(label.to_string(), font.clone(), th.fg);
    let count = format!("{lines} line{}", if lines == 1 { "" } else { "s" });
    let count = ui.painter().layout_no_wrap(count, font, th.dim);
    let (pad, gap) = (Vec2::new(9.0, 3.0), 8.0);
    let chip_size = chip.size() + pad * 2.0;
    let total = chip_size.x + gap + count.size().x;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), chip_size.y), Sense::hover());
    let x = rect.center().x - total / 2.0;
    let chip_rect = Rect::from_min_size(egui::pos2(x, rect.top()), chip_size);
    let painter = ui.painter();
    painter.rect_filled(chip_rect, CornerRadius::same(255), th.selected);
    painter.galley(chip_rect.min + pad, chip, th.fg);
    painter.galley(egui::pos2(chip_rect.right() + gap, rect.center().y - count.size().y / 2.0), count, th.dim);
    ui.add_space(8.0);
}
fn centered(ui: &mut egui::Ui, rect: Rect, add: impl FnOnce(&mut egui::Ui)) {
    let mut c = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::centered_and_justified(egui::Direction::TopDown)));
    add(&mut c);
}

fn checkerboard(painter: &egui::Painter, rect: Rect, th: &Theme) {
    let cell = 8.0;
    let light = th.pane;
    let dark = th.pane.lerp_to_gamma(th.fg, 0.06);
    painter.rect_filled(rect, 4.0, light);
    let painter = painter.with_clip_rect(rect);
    let (cols, rows) = ((rect.width() / cell).ceil() as i32, (rect.height() / cell).ceil() as i32);
    if cols * rows > 40_000 {
        return;
    }
    for y in 0..rows {
        for x in (y % 2..cols).step_by(2) {
            let r = Rect::from_min_size(rect.min + Vec2::new(x as f32 * cell, y as f32 * cell), Vec2::splat(cell));
            painter.rect_filled(r, 0.0, dark);
        }
    }
}

/// Folder listing from `text_preview` ("N items", then one name per line, folders marked 📁).
fn folder(ui: &mut egui::Ui, th: &Theme, text: &str, salt: &str) {
    let mut lines = text.lines();
    let count = lines.next().unwrap_or_default();
    ui.label(egui::RichText::new(count).size(th.font - 1.0).color(th.dim));
    ui.add_space(6.0);
    let items: Vec<&str> = lines.filter(|l| !l.trim().is_empty()).collect();
    if items.is_empty() {
        let rect = ui.available_rect_before_wrap();
        centered(ui, rect, |ui| {
            ui.label(egui::RichText::new("This folder is empty").color(th.dim));
        });
        return;
    }
    egui::ScrollArea::vertical().id_salt(salt).auto_shrink([false, false]).show(ui, |ui| {
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            for item in items {
                let (is_dir, name) = match item.strip_prefix("📁 ") {
                    Some(n) => (true, n),
                    None => (false, item.trim_start()),
                };
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(if is_dir { "▸" } else { "·" }).color(if is_dir { th.dir } else { th.dim }));
                    ui.label(egui::RichText::new(name).size(th.font - 1.0).color(if is_dir { th.dir } else { th.fg }));
                });
            }
        });
    });
}

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "case", "catch", "class", "const", "continue", "crate", "def", "default", "defer",
    "del", "do", "elif", "else", "enum", "export", "extends", "extern", "false", "final", "finally", "fn", "for",
    "from", "func", "function", "go", "if", "impl", "import", "in", "interface", "is", "lambda", "let", "loop",
    "match", "mod", "module", "mut", "namespace", "new", "nil", "None", "not", "null", "or", "and", "package",
    "pass", "private", "protected", "pub", "public", "raise", "ref", "return", "self", "Self", "static", "struct",
    "super", "switch", "this", "throw", "trait", "true", "True", "False", "try", "type", "typeof", "unsafe", "use",
    "using", "var", "void", "where", "while", "with", "yield", "fun", "val", "local", "then", "end", "elseif",
    "select", "SELECT", "FROM", "WHERE", "INSERT", "UPDATE", "DELETE", "JOIN", "param", "foreach", "echo",
];

/// Lightweight syntax highlighting with a line-number gutter.
fn highlight(text: &str, comments: Comments, th: &Theme) -> LayoutJob {
    let size = th.font - 1.5;
    let font = FontId::monospace(size);
    let fmt = |color: Color32| TextFormat { font_id: font.clone(), color, line_height: Some(size * 1.55), ..Default::default() };
    let (kw, string, number, comment, ty, gutter) = (th.media, th.exec, th.archive, th.dim, th.dir, th.dim.gamma_multiply(0.7));
    let mut job = LayoutJob { wrap: egui::text::TextWrapping { max_width: f32::INFINITY, ..Default::default() }, ..Default::default() };
    let total = line_count(text);
    let digits = total.to_string().len();
    let mut in_block = false;
    for (n, line) in text.lines().enumerate() {
        job.append(&format!("{:>digits$}   ", n + 1), 0.0, fmt(gutter));
        let mut rest = line;
        while !rest.is_empty() {
            if in_block {
                let end = comments.block.map(|b| b.1).unwrap_or("");
                match rest.find(end) {
                    Some(i) if !end.is_empty() => {
                        job.append(&rest[..i + end.len()], 0.0, fmt(comment));
                        rest = &rest[i + end.len()..];
                        in_block = false;
                    }
                    _ => {
                        job.append(rest, 0.0, fmt(comment));
                        rest = "";
                    }
                }
                continue;
            }
            if comments.line.iter().any(|m| rest.starts_with(m)) {
                job.append(rest, 0.0, fmt(comment));
                break;
            }
            if let Some((start, _)) = comments.block.filter(|b| rest.starts_with(b.0)) {
                job.append(start, 0.0, fmt(comment));
                rest = &rest[start.len()..];
                in_block = true;
                continue;
            }
            let ch = rest.chars().next().unwrap();
            if ch == '"' || ch == '\'' || ch == '`' {
                // String literal up to the matching quote (backslash escapes skipped).
                let mut end = rest.len();
                let mut escaped = false;
                for (i, c) in rest.char_indices().skip(1) {
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == ch {
                        end = i + c.len_utf8();
                        break;
                    }
                }
                // A lone apostrophe (Rust lifetimes, prose) isn't a string.
                if ch == '\'' && end == rest.len() {
                    job.append("'", 0.0, fmt(th.fg));
                    rest = &rest[1..];
                    continue;
                }
                job.append(&rest[..end], 0.0, fmt(string));
                rest = &rest[end..];
                continue;
            }
            if ch.is_ascii_digit() {
                let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_')).unwrap_or(rest.len());
                job.append(&rest[..end], 0.0, fmt(number));
                rest = &rest[end..];
                continue;
            }
            if ch.is_alphabetic() || ch == '_' || ch == '$' || ch == '@' {
                let end = rest
                    .char_indices()
                    .skip(1)
                    .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len());
                let word = &rest[..end];
                let color = if KEYWORDS.contains(&word) {
                    kw
                } else if word.chars().next().is_some_and(|c| c.is_uppercase()) {
                    ty
                } else if rest[end..].starts_with('(') {
                    th.accent.lerp_to_gamma(th.fg, 0.35)
                } else {
                    th.fg
                };
                job.append(word, 0.0, fmt(color));
                rest = &rest[end..];
                continue;
            }
            let end = ch.len_utf8();
            job.append(&rest[..end], 0.0, fmt(th.fg.gamma_multiply(0.85)));
            rest = &rest[end..];
        }
        job.append("\n", 0.0, fmt(th.fg));
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_and_kinds() {
        assert_eq!(language("rs").map(|l| l.0), Some("Rust"));
        assert!(language("txt").is_none());
    }
}
