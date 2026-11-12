//! Tabs: several repositories open at once, each a full `GitApp` with its own state.
//! The open tabs and recently used repositories are remembered between launches.

use crate::app::GitApp;
use crate::chrome;
use crate::font::UI;
use crate::git::Repo;
use crate::ui::{theme, Color, Cursor, Key, Rect, Ui};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

pub const TAB_H: f32 = 36.0;
/// Tab pill height (centered on the same line as the traffic lights).
const PILL_H: f32 = 26.0;
const MAX_RECENT: usize = 15;

struct TabDrag {
    index: usize,
    start_x: f32,
    active: bool,
}

pub struct Workspace {
    tabs: Vec<GitApp>,
    active: usize,
    recent: Vec<PathBuf>,
    persist: bool,
    picker: Option<mpsc::Receiver<Option<PathBuf>>>,
    /// Anchor of the "+" dropdown, when open.
    add_menu: Option<(f32, f32)>,
    add_menu_fresh: bool,
    plus_at: (f32, f32),
    drag: Option<TabDrag>,
    error: Option<String>,
}

impl Workspace {
    /// Restores the previous session, then opens `arg` (if any) in a tab and focuses it.
    pub fn new(arg: Option<PathBuf>) -> Self {
        let mut ws = Self::empty(true);
        let (tabs, active, recent) = load_session();
        ws.recent = recent;
        for p in tabs {
            ws.add_tab(&p, false, true);
        }
        ws.active = active.min(ws.tabs.len().saturating_sub(1));
        match arg {
            Some(p) => ws.open_tab(&p),
            None if ws.tabs.is_empty() => {
                if let Ok(cwd) = std::env::current_dir() {
                    if Repo::open(&cwd).is_ok() {
                        ws.open_tab(&cwd);
                    }
                }
            }
            None => {}
        }
        ws.save();
        ws
    }

    /// Opens exactly these repositories without touching the saved session (screenshots, tests).
    pub fn for_paths(paths: &[PathBuf]) -> Self {
        let mut ws = Self::empty(false);
        for p in paths {
            ws.open_tab(p);
        }
        ws
    }

    fn empty(persist: bool) -> Self {
        Workspace {
            tabs: vec![],
            active: 0,
            recent: vec![],
            persist,
            picker: None,
            add_menu: None,
            add_menu_fresh: false,
            plus_at: (0.0, TAB_H),
            drag: None,
            error: None,
        }
    }

    pub fn active_mut(&mut self) -> Option<&mut GitApp> {
        self.tabs.get_mut(self.active)
    }

    pub fn title(&self) -> String {
        self.tabs.get(self.active).map_or_else(|| "wgitpui".into(), GitApp::title)
    }

    pub fn on_focus(&mut self) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.on_focus();
        }
    }

    /// Opens a repository in a new tab, or switches to it if it's already open.
    pub fn open_tab(&mut self, path: &Path) {
        if let Some(i) = self.add_tab(path, true, false) {
            self.activate(i);
        }
        self.save();
    }

    fn add_tab(&mut self, path: &Path, report: bool, append: bool) -> Option<usize> {
        let repo = match Repo::open(path) {
            Ok(r) => r,
            Err(e) => {
                if report {
                    self.error = Some(format!("{} isn't a git repository ({})", path.display(), e.lines().next().unwrap_or("")));
                }
                return None;
            }
        };
        let root = repo.root.clone();
        self.touch_recent(&root);
        if let Some(i) = self.tabs.iter().position(|t| t.root().as_ref() == Some(&root)) {
            return Some(i);
        }
        let mut app = GitApp::new(Some(root));
        app.in_tab = true;
        let at = if self.tabs.is_empty() || append { self.tabs.len() } else { self.active + 1 };
        self.tabs.insert(at, app);
        if at <= self.active && self.tabs.len() > 1 {
            self.active += 1;
        }
        Some(at)
    }

    fn touch_recent(&mut self, root: &Path) {
        self.recent.retain(|r| r != root);
        self.recent.insert(0, root.to_path_buf());
        self.recent.truncate(MAX_RECENT);
    }

    fn activate(&mut self, i: usize) {
        if i < self.tabs.len() {
            if i != self.active {
                self.active = i;
                self.tabs[i].on_focus();
            }
            self.save();
        }
    }

    fn close(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        self.tabs.remove(i);
        if self.active > i || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.on_focus();
        }
        self.save();
    }

    fn pick_folder(&mut self) {
        if self.picker.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(pick_folder());
        });
        self.picker = Some(rx);
    }

    fn save(&self) {
        if !self.persist {
            return;
        }
        let tabs: Vec<PathBuf> = self.tabs.iter().filter_map(GitApp::root).collect();
        save_session(&tabs, self.active, &self.recent);
    }

    // ---- frame ------------------------------------------------------------------------

    pub fn frame(&mut self, ui: &mut Ui) {
        if let Some(rx) = &self.picker {
            match rx.try_recv() {
                Ok(Some(p)) => {
                    self.picker = None;
                    self.open_tab(&p);
                }
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => self.picker = None,
                Err(mpsc::TryRecvError::Empty) => ui.wake_in(Duration::from_millis(200)),
            }
        }
        self.keys(ui);

        let s = ui.screen;
        let strip = Rect::new(s.x, s.y, s.w, TAB_H);
        let body = Rect::new(s.x, s.y + TAB_H, s.w, s.h - TAB_H);
        if self.add_menu.is_some() {
            ui.blocked = true;
        }
        self.tab_strip(ui, strip);
        for (i, t) in self.tabs.iter_mut().enumerate() {
            if i != self.active {
                t.background(ui);
            }
        }
        if self.tabs.is_empty() {
            self.welcome(ui, body);
        } else {
            let recent = self.recent.clone();
            let app = &mut self.tabs[self.active];
            app.recent = recent;
            let blocked = ui.blocked;
            app.frame_in(ui, body);
            ui.blocked = blocked;
            let requests = std::mem::take(&mut self.tabs[self.active].open_requests);
            for p in requests {
                self.open_tab(&p);
            }
        }
        ui.blocked = false;
        if self.add_menu.is_some() {
            self.draw_add_menu(ui);
        }
        self.draw_error(ui);
    }

    fn keys(&mut self, ui: &mut Ui) {
        let input = &mut ui.input;
        let mut take = |pred: &dyn Fn(Key, crate::ui::Mods) -> bool| -> bool {
            match input.keys.iter().position(|k| pred(k.key, k.mods)) {
                Some(i) => {
                    input.keys.remove(i);
                    true
                }
                None => false,
            }
        };
        let n = self.tabs.len();
        if take(&|k, m| k == Key::Char('t') && m.primary()) {
            self.add_menu = Some(self.plus_at);
            self.add_menu_fresh = true;
        }
        if take(&|k, m| k == Key::Char('o') && m.primary()) {
            self.pick_folder();
        }
        if take(&|k, m| k == Key::Char('w') && m.primary()) && n > 0 {
            self.close(self.active);
        }
        for d in 1..=9u8 {
            let c = (b'0' + d) as char;
            if take(&|k, m| k == Key::Char(c) && m.primary()) && n > 0 {
                // ⌘9 is always the last tab, like browsers.
                let i = if d == 9 { n - 1 } else { d as usize - 1 };
                self.activate(i.min(n - 1));
            }
        }
        let next = take(&|k, m| (k == Key::Tab && m.ctrl && !m.shift) || (m.primary() && m.shift && matches!(k, Key::Char(']') | Key::Char('}'))));
        let prev = take(&|k, m| (k == Key::Tab && m.ctrl && m.shift) || (m.primary() && m.shift && matches!(k, Key::Char('[') | Key::Char('{'))));
        if n > 1 && next {
            self.activate((self.active + 1) % n);
        }
        if n > 1 && prev {
            self.activate((self.active + n - 1) % n);
        }
    }

    fn tab_strip(&mut self, ui: &mut Ui, r: Rect) {
        ui.p.rect(r, theme::BG);
        ui.p.rect(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), theme::BORDER);
        let n = self.tabs.len();
        // Clear breathing room after the traffic lights.
        let x0 = r.x + if chrome::LEFT_INSET > 0.0 { chrome::LEFT_INSET + 16.0 } else { 10.0 };
        let plus_w = 30.0;
        let avail = r.right() - x0 - plus_w - 16.0;
        let tw = if n == 0 { 0.0 } else { (avail / n as f32).clamp(72.0, 240.0) };
        let mut hot: Vec<Rect> = Vec::new();
        let (mut activate, mut close) = (None, None);

        ui.p.push_clip(r);
        for i in 0..n {
            let tr = Rect::new(x0 + i as f32 * tw, r.cy() - PILL_H * 0.5, tw - 6.0, PILL_H);
            hot.push(tr);
            let active = i == self.active;
            let hov = ui.hovered(tr);
            if active {
                ui.p.rounded(tr, 7.0, theme::RAISED);
                ui.p.rounded_stroke(tr, 7.0, 1.0, theme::BORDER.mix(theme::TEXT, 0.12));
            } else if hov {
                ui.p.rounded(tr, 7.0, theme::HOVER.alpha(0.8));
            }
            let (name, branch, changes, busy) = self.tabs[i].tab_info();
            let cy = tr.cy();
            let mut x = tr.x + 11.0;
            let dot: Option<Color> = if busy {
                Some(theme::RED)
            } else if changes > 0 {
                Some(theme::YELLOW)
            } else {
                None
            };
            if let Some(c) = dot {
                ui.p.circle((x + 3.0, cy), 3.5, c);
                x += 12.0;
            }
            let close_w = if hov || active { 20.0 } else { 0.0 };
            let room = tr.right() - x - close_w - 6.0;
            let nw = ui.p.text_fit(x, cy, room, UI, 12.5, &name, if active { theme::TEXT } else { theme::DIM });
            if room - nw > 40.0 && !branch.is_empty() {
                ui.p.text_fit(x + nw + 8.0, cy, room - nw - 8.0, UI, 11.0, &branch, theme::FAINT);
            }
            if close_w > 0.0 {
                let cr = Rect::new(tr.right() - 21.0, cy - 8.0, 16.0, 16.0);
                let chov = ui.hovered(cr);
                if chov {
                    ui.p.rounded(cr, 4.0, theme::HOVER.mix(theme::TEXT, 0.08));
                    ui.cursor = Cursor::Pointer;
                }
                let (cx, ccy) = (cr.x + 8.0, cr.cy());
                let c = if chov { theme::TEXT } else { theme::FAINT };
                ui.p.line((cx - 3.0, ccy - 3.0), (cx + 3.0, ccy + 3.0), 1.3, c);
                ui.p.line((cx - 3.0, ccy + 3.0), (cx + 3.0, ccy - 3.0), 1.3, c);
                if chov && ui.input.pressed {
                    close = Some(i);
                    continue;
                }
            }
            if hov && ui.input.pressed {
                activate = Some(i);
                self.drag = Some(TabDrag { index: i, start_x: ui.input.mouse.0, active: false });
            }
        }
        // "+" opens the new-tab menu.
        let plus = Rect::new(x0 + n as f32 * tw, r.cy() - 12.0, 24.0, 24.0);
        self.plus_at = (plus.x, r.bottom() + 4.0);
        hot.push(plus);
        let phov = ui.hovered(plus);
        if phov {
            ui.p.rounded(plus, 7.0, theme::HOVER);
            ui.cursor = Cursor::Pointer;
        }
        let (px, py) = (plus.x + plus.w * 0.5, plus.cy());
        let pc = if phov { theme::TEXT } else { theme::DIM };
        ui.p.line((px - 4.5, py), (px + 4.5, py), 1.5, pc);
        ui.p.line((px, py - 4.5), (px, py + 4.5), 1.5, pc);
        if phov && ui.input.pressed {
            self.add_menu = Some(self.plus_at);
            self.add_menu_fresh = true;
        }
        ui.p.pop_clip();

        // Dragging a tab reorders it.
        if let Some(d) = &mut self.drag {
            if ui.input.down {
                let mx = ui.input.mouse.0;
                if (mx - d.start_x).abs() > 5.0 {
                    d.active = true;
                }
                if d.active && n > 1 {
                    ui.cursor = Cursor::Grabbing;
                    let target = (((mx - x0) / tw).floor().max(0.0) as usize).min(n - 1);
                    if target != d.index {
                        let t = self.tabs.remove(d.index);
                        self.tabs.insert(target, t);
                        self.active = target;
                        d.index = target;
                    }
                }
            } else {
                let moved = d.active;
                self.drag = None;
                if moved {
                    self.save();
                }
            }
        }
        if let Some(i) = close {
            self.close(i);
        } else if let Some(i) = activate {
            self.activate(i);
        }
        // Empty strip space behaves like a title bar.
        if ui.input.pressed && ui.hovered(r) && !hot.iter().any(|h| h.contains(ui.input.mouse)) {
            if ui.input.double {
                ui.toggle_maximize = true;
            } else {
                ui.drag_window = true;
            }
        }
    }

    /// Recent repositories that aren't open in a tab.
    fn closed_recent(&self) -> Vec<PathBuf> {
        let open: Vec<PathBuf> = self.tabs.iter().filter_map(GitApp::root).collect();
        self.recent.iter().filter(|r| !open.contains(r) && r.exists()).cloned().collect()
    }

    fn draw_add_menu(&mut self, ui: &mut Ui) {
        if ui.input.take_key(Key::Escape, false).is_some() {
            self.add_menu = None;
            return;
        }
        let Some(at) = self.add_menu else { return };
        let recent = self.closed_recent();
        let row_h = 32.0;
        let w = 380.0;
        let h = 12.0 + row_h + if recent.is_empty() { 0.0 } else { 30.0 + recent.len().min(10) as f32 * row_h } + 8.0;
        let s = ui.screen;
        let r = Rect::new(at.0.min(s.right() - w - 8.0).max(8.0), at.1, w, h);
        let fresh = std::mem::replace(&mut self.add_menu_fresh, false);
        if !fresh && (ui.input.pressed || ui.input.right_pressed) && !r.contains(ui.input.mouse) {
            self.add_menu = None;
            return;
        }
        ui.p.rounded(Rect::new(r.x, r.y + 6.0, r.w, r.h), 12.0, Color::hex(0x000000).alpha(0.45));
        ui.p.rounded(r, 10.0, theme::PANEL2);
        ui.p.rounded_stroke(r, 10.0, 1.0, theme::BORDER.mix(theme::TEXT, 0.1));
        let mut y = r.y + 6.0;
        let mut chosen: Option<Option<PathBuf>> = None;
        let row = |ui: &mut Ui, y: f32, label: &str, detail: &str| -> bool {
            let rr = Rect::new(r.x + 5.0, y, r.w - 10.0, row_h);
            let hov = rr.contains(ui.input.mouse);
            if hov {
                ui.p.rounded(rr, 6.0, theme::SELECT);
                ui.cursor = Cursor::Pointer;
            }
            let lw = ui.p.text_fit(rr.x + 12.0, rr.cy(), rr.w * 0.5, UI, 13.0, label, theme::TEXT);
            ui.p.text_fit(rr.x + 24.0 + lw, rr.cy(), rr.w - lw - 40.0, UI, 11.0, detail, theme::FAINT);
            hov && ui.input.pressed
        };
        let key = if cfg!(target_os = "macos") { "⌘O" } else { "Ctrl+O" };
        if row(ui, y, "Open folder…", key) {
            chosen = Some(None);
        }
        y += row_h;
        if !recent.is_empty() {
            ui.p.text(r.x + 16.0, y + 17.0, UI, 10.5, "RECENT", theme::FAINT);
            y += 30.0;
            for p in recent.iter().take(10) {
                let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into());
                if row(ui, y, &name, &tilde(p)) {
                    chosen = Some(Some(p.clone()));
                }
                y += row_h;
            }
        }
        match chosen {
            Some(None) => {
                self.add_menu = None;
                self.pick_folder();
            }
            Some(Some(p)) => {
                self.add_menu = None;
                self.open_tab(&p);
            }
            None => {}
        }
    }

    fn welcome(&mut self, ui: &mut Ui, r: Rect) {
        ui.p.rect(r, theme::BG);
        let recent = self.closed_recent();
        let w = 460.0f32.min(r.w - 40.0);
        let x = r.x + (r.w - w) * 0.5;
        let mut y = r.y + (r.h * 0.22).max(40.0);
        let title = "Open a repository";
        let tw = ui.p.measure(UI, 22.0, title);
        ui.p.text(x + (w - tw) * 0.5, y, UI, 22.0, title, theme::TEXT);
        y += 34.0;
        let sub = "Choose a folder, drop one onto this window, or pick a recent repository.";
        let sw = ui.p.measure(UI, 12.5, sub).min(w);
        ui.p.text_fit(x + (w - sw) * 0.5, y, w, UI, 12.5, sub, theme::DIM);
        y += 34.0;
        let bw = 180.0;
        if ui.button(Rect::new(x + (w - bw) * 0.5, y, bw, 34.0), "Choose folder…", true, self.picker.is_none()) {
            self.pick_folder();
        }
        y += 60.0;
        if !recent.is_empty() {
            ui.p.text(x, y, UI, 10.5, "RECENT", theme::FAINT);
            y += 14.0;
            for p in recent.iter().take(10) {
                let rr = Rect::new(x - 8.0, y, w + 16.0, 34.0);
                let hov = ui.hovered(rr);
                if hov {
                    ui.p.rounded(rr, 7.0, theme::HOVER);
                    ui.cursor = Cursor::Pointer;
                }
                let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into());
                let nw = ui.p.text_fit(x + 4.0, rr.cy(), w * 0.45, UI, 13.0, &name, theme::TEXT);
                ui.p.text_fit(x + nw + 16.0, rr.cy(), w - nw - 20.0, UI, 11.5, &tilde(p), theme::FAINT);
                if hov && ui.input.pressed {
                    let p = p.clone();
                    self.open_tab(&p);
                    return;
                }
                y += 34.0;
            }
        }
    }

    fn draw_error(&mut self, ui: &mut Ui) {
        let Some(e) = &self.error else { return };
        let s = ui.screen;
        let w = (ui.p.measure(UI, 12.5, e) + 70.0).min(s.w * 0.6);
        let r = Rect::new(s.right() - w - 20.0, s.bottom() - 58.0, w, 38.0);
        ui.p.rounded(r, 10.0, theme::RAISED);
        ui.p.rounded_stroke(r, 10.0, 1.0, theme::RED.alpha(0.4));
        ui.p.circle((r.x + 16.0, r.cy()), 4.0, theme::RED);
        ui.p.text_fit(r.x + 28.0, r.cy(), r.w - 60.0, UI, 12.5, e, theme::TEXT);
        if ui.input.pressed {
            self.error = None;
        }
    }
}

fn tilde(p: &Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

/// Native folder chooser through the platform's own tooling (blocking; run off the UI thread).
fn pick_folder() -> Option<PathBuf> {
    use std::process::Command;
    let out = if cfg!(target_os = "macos") {
        Command::new("osascript").args(["-e", "POSIX path of (choose folder with prompt \"Open a git repository\")"]).output()
    } else if cfg!(windows) {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.FolderBrowserDialog; if ($d.ShowDialog() -eq 'OK') { $d.SelectedPath }",
            ])
            .output()
    } else {
        Command::new("zenity")
            .args(["--file-selection", "--directory", "--title=Open a git repository"])
            .output()
            .or_else(|_| Command::new("kdialog").args(["--getexistingdirectory", "."]).output())
    };
    let out = out.ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

// ---- session file ---------------------------------------------------------------------------

fn session_path() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?
    };
    Some(base.join("wgitpui").join("session"))
}

fn load_session() -> (Vec<PathBuf>, usize, Vec<PathBuf>) {
    let text = session_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    parse_session(&text)
}

fn parse_session(text: &str) -> (Vec<PathBuf>, usize, Vec<PathBuf>) {
    let (mut tabs, mut active, mut recent) = (vec![], 0, vec![]);
    for line in text.lines() {
        match line.split_once('\t') {
            Some(("tab", p)) => tabs.push(PathBuf::from(p)),
            Some(("recent", p)) => recent.push(PathBuf::from(p)),
            Some(("active", n)) => active = n.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    (tabs, active, recent)
}

fn format_session(tabs: &[PathBuf], active: usize, recent: &[PathBuf]) -> String {
    let mut s = format!("active\t{active}\n");
    for t in tabs {
        s.push_str(&format!("tab\t{}\n", t.display()));
    }
    for r in recent {
        s.push_str(&format!("recent\t{}\n", r.display()));
    }
    s
}

fn save_session(tabs: &[PathBuf], active: usize, recent: &[PathBuf]) {
    if let Some(p) = session_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, format_session(tabs, active, recent));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{KeyPress, Mods};

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wgitpui-ws-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@example.com"], &["config", "user.name", "T"]] {
            crate::git::run(&dir, args, None).unwrap();
        }
        std::fs::write(dir.join("f"), name).unwrap();
        crate::git::run(&dir, &["add", "."], None).unwrap();
        crate::git::run(&dir, &["commit", "-qm", name], None).unwrap();
        Repo::open(&dir).unwrap().root
    }

    fn frame(ws: &mut Workspace, ui: &mut Ui) {
        ui.begin(1400.0, 900.0, 1.0);
        ws.frame(ui);
        ui.input.end_frame();
    }

    fn press(ws: &mut Workspace, ui: &mut Ui, c: char) {
        let mods = Mods { logo: cfg!(target_os = "macos"), ctrl: !cfg!(target_os = "macos"), ..Default::default() };
        ui.input.keys.push(KeyPress { key: Key::Char(c), mods });
        frame(ws, ui);
    }

    #[test]
    fn tabs_open_switch_dedupe_and_close() {
        let (a, b) = (repo("a"), repo("b"));
        let mut ws = Workspace::for_paths(&[a.clone(), b.clone()]);
        let mut ui = Ui::new(crate::font::Fonts::load().unwrap());
        frame(&mut ws, &mut ui);
        assert_eq!(ws.tabs.len(), 2);
        assert_eq!(ws.tabs[ws.active].root(), Some(b.clone()), "newest tab is active");

        press(&mut ws, &mut ui, '1');
        assert_eq!(ws.tabs[ws.active].root(), Some(a.clone()));

        // Opening an already-open repo (even via a subdirectory) switches instead of duplicating.
        std::fs::create_dir_all(b.join("sub")).unwrap();
        ws.open_tab(&b.join("sub"));
        assert_eq!(ws.tabs.len(), 2);
        assert_eq!(ws.tabs[ws.active].root(), Some(b.clone()));

        // Not a repository: error, no tab.
        ws.open_tab(&std::env::temp_dir());
        assert_eq!(ws.tabs.len(), 2);
        assert!(ws.error.is_some());

        press(&mut ws, &mut ui, 'w');
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.tabs[ws.active].root(), Some(a.clone()));
        press(&mut ws, &mut ui, 'w');
        assert!(ws.tabs.is_empty());
        frame(&mut ws, &mut ui); // welcome screen renders without tabs
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn session_roundtrip() {
        let tabs = vec![PathBuf::from("/x/a"), PathBuf::from("/x/b c")];
        let recent = vec![PathBuf::from("/x/b c"), PathBuf::from("/y")];
        let (t, a, r) = parse_session(&format_session(&tabs, 1, &recent));
        assert_eq!((t, a, r), (tabs, 1, recent));
    }
}
