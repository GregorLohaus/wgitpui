//! Tabs: several repositories open at once, each a full `GitApp` with its own state.
//! The open tabs and recently used repositories are remembered between launches. Opening a
//! repository never holds up the window: worker threads read it (at startup the visible tab's
//! first), and its tab fills in when the read comes back.

use crate::app::{self, GitApp, Opened};
use crate::chrome;
use crate::config::{self, Config};
use crate::font::UI;
use crate::git::Repo;
use crate::ui::{bind, theme, Color, Cursor, Rect, Ui};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
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

/// Threads reading repositories for tabs, at most.
const READERS: usize = 2;

/// A repository for a reader to read, and whether to say so if that fails (a tab restored
/// from the last session whose repository is gone just closes).
struct Job {
    path: PathBuf,
    report: bool,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    readers: usize,
}

/// Reads tabs' repositories on worker threads, in the order queued (a tab that's switched to
/// meanwhile goes to the front). Readers start when there's work and stop when there's none.
struct Loader {
    queue: Arc<Mutex<Queue>>,
    tx: mpsc::Sender<(Job, Result<Opened, String>)>,
    rx: mpsc::Receiver<(Job, Result<Opened, String>)>,
}

impl Loader {
    fn new() -> Loader {
        let (tx, rx) = mpsc::channel();
        Loader { queue: Arc::default(), tx, rx }
    }

    fn lock(queue: &Mutex<Queue>) -> std::sync::MutexGuard<'_, Queue> {
        queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues a repository (once), starting a reader if fewer than `READERS` are at work.
    fn read(&self, path: PathBuf, report: bool) {
        let mut q = Self::lock(&self.queue);
        if q.jobs.iter().any(|j| j.path == path) {
            return;
        }
        q.jobs.push_back(Job { path, report });
        if q.readers >= READERS {
            return;
        }
        q.readers += 1;
        let (queue, tx) = (self.queue.clone(), self.tx.clone());
        std::thread::spawn(move || loop {
            let job = {
                let mut q = Self::lock(&queue);
                match q.jobs.pop_front() {
                    Some(j) => j,
                    None => {
                        q.readers -= 1;
                        return;
                    }
                }
            };
            // A read that panics fails like one git refuses, instead of leaving its tab waiting.
            let read = std::panic::catch_unwind(|| app::read_repo(&job.path)).unwrap_or_else(|_| Err("reading it failed".into()));
            if tx.send((job, read)).is_err() {
                return;
            }
        });
    }

    /// Reads this one next, if it's still waiting.
    fn hurry(&self, path: &Path) {
        let mut q = Self::lock(&self.queue);
        if let Some(j) = q.jobs.iter().position(|j| j.path == path).and_then(|i| q.jobs.remove(i)) {
            q.jobs.push_front(j);
        }
    }

    fn forget(&self, path: &Path) {
        Self::lock(&self.queue).jobs.retain(|j| j.path != path);
    }
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
    /// The settings file (only read and written when `persist`; else the defaults).
    config: Config,
    /// Tabs' repositories, being read.
    loader: Loader,
}

impl Workspace {
    /// Restores the previous session, then opens `arg` (if any) in a tab and focuses it. The
    /// repositories are read in the background: this returns right away.
    pub fn new(arg: Option<PathBuf>) -> Self {
        let (tabs, active, recent) = load_session();
        let mut ws = Self::restore(tabs, active, recent, true);
        ws.config = config::load();
        let open = match arg {
            Some(p) => Some(p),
            None if ws.tabs.is_empty() => std::env::current_dir().ok().filter(|cwd| Repo::open(cwd).is_ok()),
            None => None,
        };
        if let Some(i) = open.and_then(|p| ws.add_tab(&p, true, false)) {
            ws.active = i;
        }
        ws.start_loading();
        ws.save();
        ws
    }

    /// A saved session's tabs, each still to read its repository (`start_loading`).
    fn restore(tabs: Vec<PathBuf>, active: usize, recent: Vec<PathBuf>, persist: bool) -> Self {
        let mut ws = Self::empty(persist);
        ws.recent = recent;
        for p in tabs {
            if ws.tabs.iter().any(|t| t.root().as_ref() == Some(&p)) {
                continue;
            }
            ws.touch_recent(&p);
            let mut app = GitApp::pending(p);
            app.in_tab = true;
            ws.tabs.push(app);
        }
        ws.active = active.min(ws.tabs.len().saturating_sub(1));
        ws
    }

    /// Starts reading the restored tabs' repositories: the visible one first, then outwards.
    fn start_loading(&mut self) {
        let mut waiting: Vec<usize> = (0..self.tabs.len()).filter(|&i| self.tabs[i].pending_path().is_some()).collect();
        waiting.sort_by_key(|&i| i.abs_diff(self.active));
        for i in waiting {
            if let Some(p) = self.tabs[i].pending_path() {
                self.loader.read(p.to_path_buf(), false);
            }
        }
    }

    /// Shows the tabs whose repositories have been read.
    fn take_loaded(&mut self, ui: &mut Ui) {
        while let Ok((job, read)) = self.loader.rx.try_recv() {
            self.loaded(job, read);
        }
        if self.tabs.iter().any(|t| t.pending_path().is_some()) {
            ui.wake_in(Duration::from_millis(30));
        }
    }

    /// Waits until every tab has its repository (screenshots, tests).
    fn finish_loading(&mut self) {
        while self.tabs.iter().any(|t| t.pending_path().is_some()) {
            match self.loader.rx.recv_timeout(Duration::from_secs(60)) {
                Ok((job, read)) => self.loaded(job, read),
                Err(_) => break,
            }
        }
    }

    /// A repository read for a tab: shown, or (couldn't be read) the tab closes.
    fn loaded(&mut self, job: Job, read: Result<Opened, String>) {
        let Some(i) = self.tabs.iter().position(|t| t.pending_path() == Some(job.path.as_path())) else { return };
        match read {
            Ok(o) => self.tabs[i].opened(o),
            Err(e) => {
                self.close(i);
                if job.report {
                    self.error = Some(format!("Couldn't open {} ({})", tilde(&job.path), e.lines().next().unwrap_or("")));
                }
            }
        }
    }

    /// Opens exactly these repositories without touching the saved session (screenshots, tests).
    pub fn for_paths(paths: &[PathBuf]) -> Self {
        let mut ws = Self::empty(false);
        for p in paths {
            ws.open_tab(p);
        }
        ws.finish_loading();
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
            config: Config::default(),
            loader: Loader::new(),
        }
    }

    pub fn active_mut(&mut self) -> Option<&mut GitApp> {
        self.tabs.get_mut(self.active)
    }

    pub fn title(&self) -> String {
        self.tabs.get(self.active).map_or_else(|| "wgitpui".into(), GitApp::title)
    }

    pub fn on_focus(&mut self) {
        // Settings edited by hand apply when you come back to the window.
        if self.persist {
            self.config = config::load();
        }
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

    /// A tab for the repository at `path` (or the one already open there): after the active
    /// tab, or at the end. It reads the repository in the background.
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
        self.loader.read(root.clone(), true);
        let mut app = GitApp::pending(root);
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
            if let Some(p) = self.tabs[i].pending_path() {
                self.loader.hurry(p);
            }
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
        if let Some(p) = self.tabs[i].pending_path() {
            self.loader.forget(p);
        }
        self.tabs.remove(i);
        let was_active = i == self.active;
        if self.active > i || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        // The tab that comes into view catches up (closing one behind it changes nothing here).
        if was_active {
            if let Some(t) = self.tabs.get_mut(self.active) {
                t.on_focus();
            }
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
        self.take_loaded(ui);
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
            app.key_bar = self.config.key_bar;
            let blocked = ui.blocked;
            app.frame_in(ui, body);
            ui.blocked = blocked;
            // The tab's key bar button (or palette) switched it: that's a setting for every tab.
            if app.key_bar != self.config.key_bar {
                self.config.key_bar = app.key_bar;
                if self.persist {
                    config::save("key_bar", if app.key_bar { "true" } else { "false" });
                }
            }
            let requests = std::mem::take(&mut self.tabs[self.active].open_requests);
            let mut closing = Vec::new();
            let mut quiet = Vec::new();
            for (i, t) in self.tabs.iter_mut().enumerate() {
                closing.append(&mut t.close_requests);
                if i != self.active {
                    quiet.append(&mut t.open_requests);
                }
            }
            for p in requests {
                self.open_tab(&p);
            }
            // Work that finished in a background tab (a worktree it was adding) opens without
            // taking focus.
            for p in quiet {
                self.add_tab(&p, true, true);
                self.save();
            }
            // Worktrees that were removed: their tabs go too.
            for p in closing {
                if let Some(i) = self.tabs.iter().position(|t| t.root().is_some_and(|r| crate::git::same_path(&r, &p) || r == p)) {
                    self.close(i);
                }
            }
        }
        ui.blocked = false;
        if self.add_menu.is_some() {
            self.draw_add_menu(ui);
        }
        self.draw_error(ui);
    }

    /// The tab shortcuts, offered like every other key (`Ui::key`).
    fn keys(&mut self, ui: &mut Ui) {
        let n = self.tabs.len();
        if ui.key(bind(&["<m-t>"], "New tab (recent repositories)")) {
            self.add_menu = Some(self.plus_at);
            self.add_menu_fresh = true;
        }
        if ui.key(bind(&["<m-o>"], "Open a repository…")) {
            self.pick_folder();
        }
        if ui.key(bind(&["<m-w>"], "Close this tab").when(n > 0)) {
            self.close(self.active);
        }
        let digits = &["<m-1>", "<m-2>", "<m-3>", "<m-4>", "<m-5>", "<m-6>", "<m-7>", "<m-8>", "<m-9>"];
        if let Some(d) = ui.key_index(bind(digits, "Go to tab 1 … 8 (9: the last one)").when(n > 0)) {
            // The last digit is always the last tab, like browsers.
            let i = if d == 8 { n - 1 } else { d };
            self.activate(i.min(n - 1));
        }
        if ui.key(bind(&["<c-tab>", "<m-}>", "<m-]>"], "Next tab").when(n > 1)) {
            self.activate((self.active + 1) % n);
        }
        if ui.key(bind(&["<c-s-tab>", "<m-{>", "<m-[>"], "Previous tab").when(n > 1)) {
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
        // "+" goes straight to the folder picker (⌘T still shows recent repositories).
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
            self.pick_folder();
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
        if ui.key(bind(&["<esc>"], "Close").bar("close")) {
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

pub fn tilde(p: &Path) -> String {
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
    Some(config::dir()?.join("session"))
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
    use crate::ui::Key;
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
        ui.input.push_key(KeyPress { key: Key::Char(c), mods });
        frame(ws, ui);
    }

    #[test]
    fn key_bar_shows_what_has_focus_runs_clicks_and_hides() {
        let dir = repo("keybar");
        std::fs::write(dir.join("f"), "changed").unwrap();
        let mut ws = Workspace::for_paths(&[dir.clone()]);
        let mut ui = Ui::new(crate::font::Fonts::load().unwrap());
        frame(&mut ws, &mut ui);
        let click = |ws: &mut Workspace, ui: &mut Ui, x: f32| {
            ui.input.mouse = (x, 900.0 - 13.0);
            ui.input.pressed = true;
            ui.input.down = true;
            frame(ws, ui);
            ui.input.down = false;
            frame(ws, ui);
        };
        // The files have focus: the bar starts with "space  stage", and clicking it stages.
        assert!(ws.config.key_bar);
        click(&mut ws, &mut ui, 30.0);
        let staged = crate::git::run(&dir, &["diff", "--cached", "--name-only"], None).unwrap();
        assert_eq!(staged.trim(), "f");
        // × (far right) hides it; the workspace takes that as the setting.
        click(&mut ws, &mut ui, 1400.0 - 17.0);
        assert!(!ws.tabs[0].key_bar && !ws.config.key_bar);
        let _ = std::fs::remove_dir_all(&dir);
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
    fn background_tabs_open_new_tabs_without_taking_focus() {
        let (a, b, c) = (repo("qa"), repo("qb"), repo("qc"));
        let mut ws = Workspace::for_paths(&[a.clone(), b.clone()]);
        let mut ui = Ui::new(crate::font::Fonts::load().unwrap());
        frame(&mut ws, &mut ui);
        assert_eq!(ws.tabs[ws.active].root(), Some(b.clone()));
        // Tab a, in the background, finished adding a worktree: it asks for a tab.
        ws.tabs[0].open_requests.push(c.clone());
        frame(&mut ws, &mut ui);
        assert_eq!(ws.tabs.len(), 3);
        assert_eq!(ws.tabs[ws.active].root(), Some(b.clone()), "focus stays where it was");
        assert!(ws.tabs.iter().any(|t| t.root() == Some(c.clone())));
        for d in [&a, &b, &c] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn restored_tabs_read_their_repositories_in_the_background() {
        let (a, b) = (repo("ra"), repo("rb"));
        std::fs::write(b.join("f"), "changed").unwrap();
        let gone = std::env::temp_dir().join(format!("wgitpui-ws-gone-{}", std::process::id()));
        // No git at all yet: every tab is waiting, and they're all part of the session.
        let mut ws = Workspace::restore(vec![a.clone(), gone.clone(), b.clone()], 2, vec![], false);
        assert!(ws.tabs.iter().all(|t| t.pending_path().is_some()));
        assert_eq!(ws.tabs.iter().filter_map(GitApp::root).collect::<Vec<_>>(), [a.clone(), gone.clone(), b.clone()]);
        assert_eq!(ws.tabs[2].tab_info().1, "opening…");
        ws.start_loading();
        let mut ui = Ui::new(crate::font::Fonts::load().unwrap());
        let t = std::time::Instant::now();
        while ws.tabs.iter().any(|t| t.pending_path().is_some()) && t.elapsed() < Duration::from_secs(20) {
            frame(&mut ws, &mut ui);
            std::thread::sleep(Duration::from_millis(10));
        }
        // The missing one is gone; the others show their repositories, the same tab active.
        assert_eq!(ws.tabs.iter().filter_map(GitApp::root).collect::<Vec<_>>(), [a.clone(), b.clone()]);
        assert_eq!(ws.active, 1);
        assert!(ws.tabs.iter().all(|t| t.pending_path().is_none()));
        let (name, branch, changes, _) = ws.tabs[1].tab_info();
        assert_eq!((branch.as_str(), changes), ("main", 1), "{name}");
        ws.activate(0);
        frame(&mut ws, &mut ui);
        assert!(ws.title().starts_with("wgitpui-ws-ra"), "{}", ws.title());
        for d in [&a, &b] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn opening_a_tab_reads_its_repository_in_the_background() {
        let (a, b) = (repo("oa"), repo("ob"));
        let mut ws = Workspace::for_paths(&[a.clone()]);
        let mut ui = Ui::new(crate::font::Fonts::load().unwrap());
        frame(&mut ws, &mut ui);
        // The tab is there (and focused) at once; its repository comes after.
        ws.open_tab(&b);
        assert_eq!((ws.tabs.len(), ws.active), (2, 1));
        assert_eq!(ws.tabs[1].pending_path(), Some(b.as_path()));
        let t = std::time::Instant::now();
        while ws.tabs[1].pending_path().is_some() && t.elapsed() < Duration::from_secs(20) {
            frame(&mut ws, &mut ui);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(ws.tabs[1].tab_info().1, "main");
        // Opening it again while it's open (or still being read) switches to it.
        ws.open_tab(&a);
        ws.open_tab(&b);
        assert_eq!((ws.tabs.len(), ws.active), (2, 1));
        for d in [&a, &b] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn session_roundtrip() {
        let tabs = vec![PathBuf::from("/x/a"), PathBuf::from("/x/b c")];
        let recent = vec![PathBuf::from("/x/b c"), PathBuf::from("/y")];
        let (t, a, r) = parse_session(&format_session(&tabs, 1, &recent));
        assert_eq!((t, a, r), (tabs, 1, recent));
    }
}
