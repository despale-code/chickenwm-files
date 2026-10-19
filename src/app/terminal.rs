//! Embedded quick terminal drawer for ChickenWM Files (`Ctrl + ` `).
//!
//! Provides a slide-up command terminal inside the file explorer initialized to the
//! current browsing directory. Supports executing system commands, streaming output,
//! command history (Up/Down), `cd` synchronization with the file manager pane,
//! `cls` / `clear`, and external terminal launch.

use eframe::egui::{self, Color32, CornerRadius, FontId, Stroke};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

pub enum TerminalMsg {
    Line { text: String, is_err: bool },
    Done(Option<i32>),
    #[allow(dead_code)]
    Nav(PathBuf),
}

#[derive(Clone, Debug)]
pub struct TerminalLine {
    pub text: String,
    pub is_err: bool,
    pub is_prompt: bool,
}

pub struct EmbeddedTerminal {
    pub open: bool,
    pub height: f32,
    pub input: String,
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    pub lines: Vec<TerminalLine>,
    pub running: bool,
    pub cwd: PathBuf,
    pub request_focus: bool,
    rx: Option<Receiver<TerminalMsg>>,
    cancel_tx: Option<Sender<()>>,
}

impl EmbeddedTerminal {
    pub fn new(cwd: PathBuf) -> Self {
        let mut t = Self {
            open: false,
            height: 220.0,
            input: String::new(),
            history: Vec::new(),
            history_idx: None,
            lines: Vec::new(),
            running: false,
            cwd: cwd.clone(),
            request_focus: false,
            rx: None,
            cancel_tx: None,
        };
        t.lines.push(TerminalLine {
            text: format!("ChickenWM Terminal — {}", cwd.display()),
            is_err: false,
            is_prompt: false,
        });
        t.lines.push(TerminalLine {
            text: "Type shell commands, 'cd <dir>' to navigate, or 'cls' to clear. (Ctrl+` to hide)".to_string(),
            is_err: false,
            is_prompt: false,
        });
        t
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.request_focus = true;
        }
    }

    pub fn set_cwd(&mut self, dir: PathBuf) {
        if !self.running && dir.is_dir() {
            self.cwd = dir;
        }
    }

    /// Polls messages from any active background command execution.
    /// If the command changed directory (e.g. `cd`), returns `Some(new_path)`
    /// so the file explorer can immediately navigate there.
    pub fn poll(&mut self) -> Option<PathBuf> {
        let mut nav_target = None;
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    TerminalMsg::Line { text, is_err } => {
                        self.lines.push(TerminalLine { text, is_err, is_prompt: false });
                        if self.lines.len() > 1200 {
                            self.lines.remove(0);
                        }
                    }
                    TerminalMsg::Done(code) => {
                        self.running = false;
                        let status_str = match code {
                            Some(0) => "Command finished successfully (exit 0)".to_string(),
                            Some(c) => format!("Command exited with code {c}"),
                            None => "Command terminated".to_string(),
                        };
                        self.lines.push(TerminalLine {
                            text: status_str,
                            is_err: code.unwrap_or(0) != 0,
                            is_prompt: false,
                        });
                    }
                    TerminalMsg::Nav(path) => {
                        self.cwd = path.clone();
                        nav_target = Some(path);
                    }
                }
            }
        }
        if !self.running {
            self.rx = None;
            self.cancel_tx = None;
        }
        nav_target
    }

    /// Executes the current command line in `self.input`.
    pub fn submit(&mut self, ctx: &egui::Context) -> Option<PathBuf> {
        let raw = self.input.trim().to_string();
        if raw.is_empty() {
            return None;
        }
        self.input.clear();
        self.history_idx = None;
        if self.history.last().map(|s| s.as_str()) != Some(&raw) {
            self.history.push(raw.clone());
        }

        // Print prompt line
        self.lines.push(TerminalLine {
            text: format!("{} > {}", self.cwd.display(), raw),
            is_err: false,
            is_prompt: true,
        });

        // Built-in command: cls / clear
        if raw.eq_ignore_ascii_case("cls") || raw.eq_ignore_ascii_case("clear") {
            self.lines.clear();
            return None;
        }

        // Built-in command: exit
        if raw.eq_ignore_ascii_case("exit") {
            self.open = false;
            return None;
        }

        // Built-in command: cd
        if raw.eq_ignore_ascii_case("cd") {
            self.lines.push(TerminalLine {
                text: self.cwd.display().to_string(),
                is_err: false,
                is_prompt: false,
            });
            return None;
        }

        if let Some(stripped) = raw.strip_prefix("cd ").or_else(|| raw.strip_prefix("cd\\")) {
            let target_str = stripped.trim().trim_matches('"').trim();
            let new_dir = if target_str == ".." {
                self.cwd.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| self.cwd.clone())
            } else if target_str == "\\" || target_str == "/" {
                self.cwd.components().next().map(|c| PathBuf::from(c.as_os_str())).unwrap_or_else(|| self.cwd.clone())
            } else {
                let candidate = PathBuf::from(target_str);
                if candidate.is_absolute() {
                    candidate
                } else {
                    self.cwd.join(candidate)
                }
            };

            match std::fs::canonicalize(&new_dir) {
                Ok(resolved) if resolved.is_dir() => {
                    let cleaned = PathBuf::from(resolved.to_string_lossy().trim_start_matches(r"\\?\"));
                    self.cwd = cleaned.clone();
                    self.lines.push(TerminalLine {
                        text: format!("Changed directory to {}", cleaned.display()),
                        is_err: false,
                        is_prompt: false,
                    });
                    return Some(cleaned);
                }
                _ if new_dir.is_dir() => {
                    self.cwd = new_dir.clone();
                    return Some(new_dir);
                }
                _ => {
                    self.lines.push(TerminalLine {
                        text: format!("Directory not found: {target_str}"),
                        is_err: true,
                        is_prompt: false,
                    });
                    return None;
                }
            }
        }

        // Run system command asynchronously via cmd.exe
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.running = true;

        let cwd = self.cwd.clone();
        let cmd_str = raw.clone();
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            let mut child = match Command::new("cmd.exe")
                .args(["/d", "/c", &cmd_str])
                .current_dir(&cwd)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    let _ = tx.send(TerminalMsg::Line {
                        text: format!("Failed to spawn process: {e}"),
                        is_err: true,
                    });
                    let _ = tx.send(TerminalMsg::Done(Some(1)));
                    ctx.request_repaint();
                    return;
                }
            };

            let stdout = child.stdout.take();
            let stderr = child.stderr.take();

            let tx_out = tx.clone();
            let ctx_out = ctx.clone();
            let stdout_handle = std::thread::spawn(move || {
                if let Some(out) = stdout {
                    let reader = BufReader::new(out);
                    for line in reader.lines().map_while(Result::ok) {
                        let _ = tx_out.send(TerminalMsg::Line { text: line, is_err: false });
                        ctx_out.request_repaint();
                    }
                }
            });

            let tx_err = tx.clone();
            let ctx_err = ctx.clone();
            let stderr_handle = std::thread::spawn(move || {
                if let Some(err) = stderr {
                    let reader = BufReader::new(err);
                    for line in reader.lines().map_while(Result::ok) {
                        let _ = tx_err.send(TerminalMsg::Line { text: line, is_err: true });
                        ctx_err.request_repaint();
                    }
                }
            });

            let _ = stdout_handle.join();
            let _ = stderr_handle.join();

            let exit_code = child.wait().ok().and_then(|st| st.code());
            let _ = tx.send(TerminalMsg::Done(exit_code));
            ctx.request_repaint();
        });

        None
    }

    /// Renders the slide-up terminal drawer. Returns `Some(PathBuf)` if `cd` was executed.
    pub(super) fn draw(&mut self, ui: &mut egui::Ui, th: &super::Theme) -> (Option<PathBuf>, bool) {
        if !self.open {
            return (None, false);
        }

        let mut nav_target = None;
        let mut close_requested = false;
        let drawer_height = self.height.clamp(140.0, 500.0);

        let drawer_frame = egui::Frame::NONE
            .fill(th.sidebar)
            .stroke(Stroke::new(1.0_f32, th.border))
            .corner_radius(CornerRadius::same(th.radius.min(6)))
            .inner_margin(egui::Margin::symmetric(10, 8));

        ui.add_space(4.0);
        drawer_frame.show(ui, |ui| {
            ui.set_height(drawer_height);

            // Header bar
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("  Terminal").strong().size(th.font - 0.5).color(th.accent));
                ui.label(
                    egui::RichText::new(format!("({})", self.cwd.display()))
                        .size(th.font - 2.0)
                        .color(th.dim)
                        .monospace(),
                );

                if self.running {
                    ui.spinner();
                    ui.label(egui::RichText::new("running…").size(th.font - 2.0).color(th.accent));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(egui::RichText::new("×").size(th.font + 2.0)).on_hover_text("Close (Ctrl+`)").clicked() {
                        close_requested = true;
                    }
                    if ui.button(egui::RichText::new("Clear").size(th.font - 2.0)).clicked() {
                        self.lines.clear();
                    }
                    if ui.button(egui::RichText::new("External ↗").size(th.font - 2.0)).on_hover_text("Open in Windows Terminal (wt.exe)").clicked() {
                        let _ = Command::new("wt.exe").arg("-d").arg(&self.cwd).spawn()
                            .or_else(|_| Command::new("cmd.exe").arg("/c").arg("start").arg("cmd.exe").current_dir(&self.cwd).spawn());
                    }
                });
            });

            ui.add_space(2.0);
            ui.separator();

            // Terminal buffer area
            let avail_h = (ui.available_height() - 32.0).max(60.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .max_height(avail_h)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for line in &self.lines {
                        let color = if line.is_err {
                            Color32::from_rgb(243, 139, 168) // crimson
                        } else if line.is_prompt {
                            th.accent
                        } else {
                            th.fg
                        };
                        ui.label(
                            egui::RichText::new(&line.text)
                                .font(FontId::monospace(th.font - 1.5))
                                .color(color),
                        );
                    }
                });

            ui.add_space(4.0);

            // Command input row
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(">").monospace().strong().color(th.accent));

                let edit = egui::TextEdit::singleline(&mut self.input)
                    .font(FontId::monospace(th.font - 1.0))
                    .hint_text("Type a command… (Enter to run, Up/Down for history)")
                    .desired_width(ui.available_width() - 8.0);

                let resp = ui.add(edit);
                if self.request_focus {
                    resp.request_focus();
                    self.request_focus = false;
                }

                // Keyboard handling inside terminal edit
                if resp.has_focus() {
                    let (enter, up, down, esc) = ui.input(|i| {
                        (
                            i.key_pressed(egui::Key::Enter),
                            i.key_pressed(egui::Key::ArrowUp),
                            i.key_pressed(egui::Key::ArrowDown),
                            i.key_pressed(egui::Key::Escape),
                        )
                    });

                    if esc {
                        close_requested = true;
                    } else if enter {
                        let ctx = ui.ctx().clone();
                        if let Some(target) = self.submit(&ctx) {
                            nav_target = Some(target);
                        }
                    } else if up && !self.history.is_empty() {
                        let next_idx = match self.history_idx {
                            Some(0) => 0,
                            Some(idx) => idx - 1,
                            None => self.history.len() - 1,
                        };
                        self.history_idx = Some(next_idx);
                        self.input = self.history[next_idx].clone();
                    } else if down && !self.history.is_empty() {
                        match self.history_idx {
                            Some(idx) if idx + 1 < self.history.len() => {
                                let next_idx = idx + 1;
                                self.history_idx = Some(next_idx);
                                self.input = self.history[next_idx].clone();
                            }
                            Some(_) => {
                                self.history_idx = None;
                                self.input.clear();
                            }
                            None => {}
                        }
                    }
                }
            });
        });

        if close_requested {
            self.open = false;
        }

        (nav_target, close_requested)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_terminal_cd_and_clear() {
        let temp_dir = std::env::temp_dir();
        let mut term = EmbeddedTerminal::new(temp_dir.clone());
        assert_eq!(term.cwd, temp_dir);
        assert!(!term.lines.is_empty());

        // Test clear
        term.input = "clear".to_string();
        let ctx = egui::Context::default();
        let res = term.submit(&ctx);
        assert_eq!(res, None);
        assert!(term.lines.is_empty());
        assert_eq!(term.history.len(), 1);
        assert_eq!(term.history[0], "clear");

        // Test cls
        term.lines.push(TerminalLine {
            text: "test".to_string(),
            is_err: false,
            is_prompt: false,
        });
        term.input = "cls".to_string();
        let res = term.submit(&ctx);
        assert_eq!(res, None);
        assert!(term.lines.is_empty());

        // Test cd
        let parent = temp_dir.parent().unwrap_or(&temp_dir).to_path_buf();
        term.input = format!("cd \"{}\"", parent.display());
        let res = term.submit(&ctx);
        assert_eq!(res, Some(parent.clone()));
        assert_eq!(term.cwd, parent);
    }

    #[test]
    fn test_history_traversal() {
        let mut term = EmbeddedTerminal::new(std::env::temp_dir());
        term.history.push("cmd1".to_string());
        term.history.push("cmd2".to_string());
        term.history.push("cmd3".to_string());

        // Up arrow when at None goes to last
        let next_idx = match term.history_idx {
            Some(0) => 0,
            Some(idx) => idx - 1,
            None => term.history.len() - 1,
        };
        term.history_idx = Some(next_idx);
        term.input = term.history[next_idx].clone();
        assert_eq!(term.input, "cmd3");

        // Up arrow again goes to idx 1
        let next_idx = match term.history_idx {
            Some(0) => 0,
            Some(idx) => idx - 1,
            None => term.history.len() - 1,
        };
        term.history_idx = Some(next_idx);
        term.input = term.history[next_idx].clone();
        assert_eq!(term.input, "cmd2");
    }
}

