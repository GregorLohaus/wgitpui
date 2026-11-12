//! File contents at any revision with an optional blame gutter (age heatmap), and the
//! repository tree used by the time machine.

use crate::font::{MONO, UI};
use crate::git::{self, Blame, Repo};
use crate::ui::{theme, Color, Cursor, Rect, Scroll, Ui};
use std::collections::HashSet;

const ROW_H: f32 = 19.0;
const MAX_LINES: usize = 200_000;
const UNCOMMITTED: &str = "0000000000000000000000000000000000000000";

pub enum FileAction {
    Back,
    ToggleBlame,
    OpenCommit(String),
}

pub struct FileView {
    /// None = working tree.
    pub rev: Option<String>,
    pub path: String,
    pub show_blame: bool,
    lines: Vec<String>,
    blame: Option<Blame>,
    /// Per blame commit: 0 = oldest in this file, 1 = newest.
    heat: Vec<f32>,
    /// Per line: id of the contiguous run of lines from the same commit.
    chunk: Vec<u32>,
    error: Option<String>,
    pub scroll: Scroll,
    hscroll: f32,
}

impl FileView {
    pub fn load(repo: &Repo, rev: Option<String>, path: String, show_blame: bool) -> FileView {
        let mut v = FileView {
            rev,
            path,
            show_blame,
            lines: vec![],
            blame: None,
            heat: vec![],
            chunk: vec![],
            error: None,
            scroll: Scroll::default(),
            hscroll: 0.0,
        };
        v.reload(repo);
        v
    }

    /// Re-reads the file at `self.rev`, keeping scroll position (used while scrubbing history).
    pub fn reload(&mut self, repo: &Repo) {
        self.error = None;
        self.blame = None;
        match repo.read_file(self.rev.as_deref(), &self.path) {
            Ok(text) if text.contains('\0') => {
                self.lines.clear();
                self.error = Some("Binary file".into());
            }
            Ok(text) => self.lines = text.lines().take(MAX_LINES).map(|l| l.replace('\t', "    ")).collect(),
            Err(_) => {
                self.lines.clear();
                self.error = Some("This file doesn't exist at this revision".into());
            }
        }
        if self.show_blame && self.error.is_none() {
            match repo.blame(self.rev.as_deref(), &self.path) {
                Ok(b) => self.set_blame(b),
                Err(e) => self.error = Some(e),
            }
        }
    }

    fn set_blame(&mut self, b: Blame) {
        let mut times: Vec<i64> = b.commits.iter().map(|c| c.time).collect();
        times.sort_unstable();
        times.dedup();
        let rank = |t: i64| {
            let i = times.partition_point(|&x| x < t);
            if times.len() <= 1 { 1.0 } else { i as f32 / (times.len() - 1) as f32 }
        };
        self.heat = b.commits.iter().map(|c| rank(c.time)).collect();
        let mut id = 0;
        self.chunk = b
            .line_commit
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                if i > 0 && b.line_commit[i - 1] != c {
                    id += 1;
                }
                id
            })
            .collect();
        self.blame = Some(b);
    }

    pub fn toggle_blame(&mut self, repo: &Repo) {
        self.show_blame = !self.show_blame;
        if self.show_blame && self.blame.is_none() {
            self.reload(repo);
        }
    }

    fn heat_color(t: f32) -> Color {
        theme::ACCENT.mix(theme::ORANGE, t).alpha(0.25 + 0.75 * t)
    }

    pub fn draw(&mut self, ui: &mut Ui, r: Rect, show_back: bool) -> Option<FileAction> {
        let mut action = None;
        let mut area = r;
        let head = area.take_top(40.0);
        ui.p.rect(head, theme::PANEL);
        let mut x = head.x + 10.0;
        if show_back {
            if ui.button(Rect::new(x, head.cy() - 13.0, 64.0, 26.0), "← Back", false, true) {
                action = Some(FileAction::Back);
            }
            x += 74.0;
        }
        let rev_label = match &self.rev {
            Some(r) => r[..8.min(r.len())].to_string(),
            None => "working tree".to_string(),
        };
        let bw = 74.0;
        let rw = ui.p.measure(MONO, 11.5, &rev_label) + 14.0;
        let name_w = head.right() - x - bw - rw - 30.0;
        x += ui.p.text_fit(x, head.cy(), name_w, UI, 13.0, &self.path, theme::TEXT) + 10.0;
        let rr = Rect::new(x, head.cy() - 10.0, rw, 20.0);
        ui.p.rounded(rr, 5.0, theme::RAISED);
        ui.p.text(rr.x + 7.0, rr.cy(), MONO, 11.5, &rev_label, theme::ACCENT);
        if ui.button(Rect::new(head.right() - bw - 10.0, head.cy() - 13.0, bw, 26.0), "Blame", self.show_blame, true) {
            action = Some(FileAction::ToggleBlame);
        }
        ui.p.rect(Rect::new(head.x, head.bottom() - 1.0, head.w, 1.0), theme::BORDER);

        ui.p.rect(area, theme::BG);
        if let Some(e) = &self.error {
            let w = ui.p.measure(UI, 13.0, e);
            ui.p.text(area.x + (area.w - w) * 0.5, area.y + 60.0, UI, 13.0, e, theme::FAINT);
            return action;
        }
        if ui.hovered(area) && ui.input.scroll.0 != 0.0 {
            self.hscroll = (self.hscroll - ui.input.scroll.0).clamp(0.0, 4000.0);
        }
        let off = self.scroll.update(ui, area, self.lines.len() as f32 * ROW_H + 24.0);
        let first = (off / ROW_H) as usize;
        let last = (first + (area.h / ROW_H) as usize + 2).min(self.lines.len());
        let blame = self.blame.as_ref().filter(|_| self.show_blame);
        let blame_w = if blame.is_some() { (area.w * 0.38).clamp(180.0, 300.0) } else { 0.0 };
        let num_w = 50.0;
        let text_x = area.x + blame_w + num_w + 12.0;

        // Hovered chunk (for highlight + click-through to the commit).
        let mut hover_chunk = None;
        if blame.is_some() && ui.hovered(area) && ui.input.mouse.0 < area.x + blame_w {
            let li = ((ui.input.mouse.1 - area.y + off) / ROW_H) as usize;
            hover_chunk = self.chunk.get(li).copied();
        }

        ui.p.push_clip(area);
        for i in first..last {
            let y = area.y + i as f32 * ROW_H - off;
            let row = Rect::new(area.x, y, area.w, ROW_H);
            if let Some(b) = blame {
                let ci = b.line_commit.get(i).copied().unwrap_or(0) as usize;
                let c = &b.commits[ci];
                let uncommitted = c.hash == UNCOMMITTED;
                let heat = if uncommitted { theme::YELLOW } else { Self::heat_color(self.heat[ci]) };
                let ch = self.chunk.get(i).copied();
                let chunk_start = i == 0 || self.chunk.get(i - 1) != self.chunk.get(i);
                if ch.is_some() && ch == hover_chunk {
                    ui.p.rect(row, theme::ACCENT.alpha(0.08));
                }
                let gutter = Rect::new(area.x, y, blame_w, ROW_H);
                ui.p.rect(gutter, theme::PANEL.alpha(0.6));
                ui.p.rect(Rect::new(area.x, y, 3.0, ROW_H), heat);
                if chunk_start {
                    if i > 0 {
                        ui.p.rect(Rect::new(area.x, y, blame_w, 1.0), theme::BORDER);
                    }
                    let age = if uncommitted { "now".to_string() } else { git::rel_time(c.time) };
                    let aw = ui.p.measure(UI, 11.0, &age);
                    let summary = if uncommitted { "Not committed yet" } else { &c.summary };
                    ui.p.text_fit(area.x + 10.0, row.cy(), blame_w - aw - 24.0, UI, 12.0, summary, theme::TEXT.mix(theme::DIM, 0.3));
                    ui.p.text(area.x + blame_w - aw - 8.0, row.cy(), UI, 11.0, &age, heat.alpha(1.0));
                } else if i > 0 && self.chunk.get(i - 1) == self.chunk.get(i) && !chunk_start {
                    // Second line of a chunk: show the author.
                    let second = i >= 1 && (i == 1 || self.chunk.get(i - 2) != self.chunk.get(i));
                    if second && !uncommitted {
                        ui.p.text_fit(area.x + 10.0, row.cy(), blame_w - 20.0, UI, 11.0, &c.author, theme::FAINT);
                    }
                }
                if hover_chunk.is_some() && ch == hover_chunk && ui.input.pressed && !uncommitted {
                    action = Some(FileAction::OpenCommit(c.hash.clone()));
                }
            }
            let n = (i + 1).to_string();
            let nw = ui.p.measure(MONO, 11.0, &n);
            ui.p.text(area.x + blame_w + num_w - nw, row.cy(), MONO, 11.0, &n, theme::FAINT);
        }
        if hover_chunk.is_some() {
            ui.cursor = Cursor::Pointer;
        }
        ui.p.pop_clip();

        ui.p.push_clip(Rect::new(text_x, area.y, (area.right() - text_x).max(0.0), area.h));
        for i in first..last {
            let y = area.y + i as f32 * ROW_H - off;
            ui.p.text(text_x - self.hscroll, y + ROW_H * 0.5, MONO, 12.0, &self.lines[i], theme::TEXT.mix(theme::DIM, 0.15));
        }
        ui.p.pop_clip();
        action
    }
}

// ---------------------------------------------------------------------------------------------
// Tree (time machine)
// ---------------------------------------------------------------------------------------------

struct TRow {
    depth: u16,
    name: String,
    path: String,
    dir: bool,
}

pub enum TreeAction {
    Open(String),
}

#[derive(Default)]
pub struct TreeView {
    pub rev: String,
    files: Vec<String>,
    changed: HashSet<String>,
    changed_dirs: HashSet<String>,
    expanded: HashSet<String>,
    rows: Vec<TRow>,
    pub scroll: Scroll,
    pub error: Option<String>,
}

impl TreeView {
    /// Loads the tree at `rev`, highlighting `changed` paths and expanding their folders.
    pub fn load(&mut self, repo: &Repo, rev: &str, changed: Vec<String>) {
        if self.rev == rev {
            return;
        }
        self.rev = rev.to_string();
        match repo.ls_tree(rev) {
            Ok(mut f) => {
                f.sort_by(|a, b| a.split('/').cmp(b.split('/')));
                self.files = f;
                self.error = None;
            }
            Err(e) => {
                self.files.clear();
                self.error = Some(e);
            }
        }
        self.changed_dirs.clear();
        for p in &changed {
            let mut acc = String::new();
            for part in p.split('/').collect::<Vec<_>>().split_last().map_or(&[][..], |(_, d)| d) {
                acc.push_str(part);
                acc.push('/');
                self.changed_dirs.insert(acc.clone());
                self.expanded.insert(acc.clone());
            }
        }
        self.changed = changed.into_iter().collect();
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        let mut emitted: HashSet<String> = HashSet::new();
        for f in &self.files {
            let parts: Vec<&str> = f.split('/').collect();
            let mut prefix = String::new();
            let mut visible = true;
            for (d, part) in parts[..parts.len() - 1].iter().enumerate() {
                prefix.push_str(part);
                prefix.push('/');
                if !emitted.contains(&prefix) {
                    emitted.insert(prefix.clone());
                    rows.push(TRow { depth: d as u16, name: part.to_string(), path: prefix.clone(), dir: true });
                }
                if !self.expanded.contains(&prefix) {
                    visible = false;
                    break;
                }
            }
            if visible {
                rows.push(TRow {
                    depth: (parts.len() - 1) as u16,
                    name: parts[parts.len() - 1].to_string(),
                    path: f.clone(),
                    dir: false,
                });
            }
        }
        self.rows = rows;
    }

    pub fn draw(&mut self, ui: &mut Ui, r: Rect) -> Option<TreeAction> {
        ui.p.rect(r, theme::BG);
        if let Some(e) = &self.error {
            ui.p.text(r.x + 16.0, r.y + 24.0, UI, 12.5, e, theme::RED);
            return None;
        }
        let rh = 22.0;
        let off = self.scroll.update(ui, r, self.rows.len() as f32 * rh + 12.0);
        let first = (off / rh) as usize;
        let last = (first + (r.h / rh) as usize + 2).min(self.rows.len());
        let mut action = None;
        let mut toggle = None;
        ui.p.push_clip(r);
        for i in first..last {
            let row = Rect::new(r.x + 6.0, r.y + 4.0 + i as f32 * rh - off, r.w - 12.0, rh);
            let t = &self.rows[i];
            let hov = ui.hovered(row);
            if hov {
                ui.p.rounded(row, 5.0, theme::HOVER);
                ui.cursor = Cursor::Pointer;
            }
            let x = row.x + 10.0 + t.depth as f32 * 14.0;
            let cy = row.cy();
            if t.dir {
                let open = self.expanded.contains(&t.path);
                let c = theme::FAINT;
                if open {
                    ui.p.line((x, cy - 2.0), (x + 4.0, cy + 2.0), 1.5, c);
                    ui.p.line((x + 4.0, cy + 2.0), (x + 8.0, cy - 2.0), 1.5, c);
                } else {
                    ui.p.line((x + 2.0, cy - 4.0), (x + 6.0, cy), 1.5, c);
                    ui.p.line((x + 6.0, cy), (x + 2.0, cy + 4.0), 1.5, c);
                }
                let changed = self.changed_dirs.contains(&t.path);
                let w = ui.p.text(x + 16.0, cy, UI, 12.5, &t.name, if changed { theme::TEXT } else { theme::DIM });
                if changed {
                    ui.p.circle((x + 22.0 + w, cy), 2.5, theme::YELLOW);
                }
                if hov && ui.input.pressed {
                    toggle = Some(t.path.clone());
                }
            } else {
                let changed = self.changed.contains(&t.path);
                let color = if changed { theme::YELLOW } else { theme::TEXT.mix(theme::DIM, 0.25) };
                ui.p.text_fit(x + 16.0, cy, row.right() - x - 24.0, UI, 12.5, &t.name, color);
                if hov && ui.input.pressed {
                    action = Some(TreeAction::Open(t.path.clone()));
                }
            }
        }
        ui.p.pop_clip();
        if let Some(p) = toggle {
            if !self.expanded.remove(&p) {
                self.expanded.insert(p);
            }
            self.rebuild();
        }
        action
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}
