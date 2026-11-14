//! Changed files as a folder tree, one component for the working copy's unstaged and staged
//! lists and for a commit's (stash's, comparison's) changes. Folders come first and open or
//! close (click, or h / l); a folder holding just one folder shows as one row ("src/app").
//! Every row shows how much changes under it: a file its status and lines added / removed, a
//! folder the sum.

use crate::font::{MONO, UI};
use crate::ui::{theme, Color, Cursor, Rect, Scroll, Ui};
use std::collections::{BTreeMap, HashSet};

pub const ROW_H: f32 = 24.0;

/// The order the tree lists files in (so a diff can follow it): folder by folder, a folder's
/// folders before its files, each by name.
pub fn tree_order(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.split('/').peekable(), b.split('/').peekable());
    loop {
        match (x.next(), y.next()) {
            (Some(p), Some(q)) => {
                let (p_dir, q_dir) = (x.peek().is_some(), y.peek().is_some());
                if p_dir != q_dir {
                    return q_dir.cmp(&p_dir);
                }
                if p != q {
                    return p.cmp(q);
                }
            }
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    /// Where a renamed file came from.
    pub from: Option<String>,
    /// Status letter and its color.
    pub badge: (char, Color),
    pub adds: u32,
    pub dels: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Dir,
    /// The entry's index.
    File(usize),
}

#[derive(Clone, Debug)]
pub struct Row {
    pub depth: u16,
    /// A file's name; a folder's (or a chain of folders that only hold each other: "src/app").
    pub name: String,
    /// The file's path; the folder's, ending in '/'.
    pub path: String,
    pub kind: Kind,
    /// Files under it (a file: 1), and their lines added / removed.
    pub files: usize,
    pub adds: u32,
    pub dels: u32,
}

impl Row {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
}

/// What a click on the tree did.
#[derive(Debug, PartialEq)]
pub enum Click {
    /// A file: (path, double click).
    File(String, bool),
    /// A folder opened or closed.
    Dir(String),
    /// One of the hovered row's buttons: its label, and the files it's for (a folder's: all of
    /// them).
    Button(&'static str, Vec<String>),
}

/// How the tree draws this frame.
pub struct Look<'a> {
    /// The file whose diff is showing.
    pub selected: Option<&'a str>,
    /// The keyboard's row (file or folder), ringed.
    pub cursor: Option<&'a str>,
    /// Buttons on the hovered row: (label, dangerous).
    pub buttons: &'a [(&'static str, bool)],
    /// Said when there are no files.
    pub empty: &'a str,
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    files: Vec<usize>,
}

#[derive(Default)]
pub struct FileTree {
    entries: Vec<Entry>,
    /// Folders (paths ending in '/') that are closed; they stay closed as the files change.
    closed: HashSet<String>,
    rows: Vec<Row>,
    /// The entries top to bottom, as if every folder were open.
    order: Vec<usize>,
    pub scroll: Scroll,
    view_h: f32,
}

impl FileTree {
    pub fn set(&mut self, entries: Vec<Entry>) {
        self.entries = entries;
        self.rebuild();
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Height that shows every row.
    pub fn height(&self) -> f32 {
        self.rows.len().max(1) as f32 * ROW_H + 4.0
    }

    pub fn row_of(&self, path: &str) -> Option<usize> {
        self.rows.iter().position(|r| r.path == path)
    }

    /// The row for `path` or, while it's in a closed folder (or its folder has gone), the
    /// closest folder row above it.
    pub fn row_near(&self, path: &str) -> Option<usize> {
        self.row_of(path).or_else(|| {
            let dirs = self.rows.iter().enumerate().filter(|(_, r)| r.is_dir() && path.starts_with(r.path.as_str()) && path != r.path);
            dirs.max_by_key(|(_, r)| r.path.len()).map(|(i, _)| i)
        })
    }

    /// The first file, top to bottom (open folder or not).
    pub fn first_file(&self) -> Option<&str> {
        self.order.first().map(|&i| self.entries[i].path.as_str())
    }

    pub fn is_open(&self, dir: &str) -> bool {
        !self.closed.contains(dir)
    }

    pub fn set_open(&mut self, dir: &str, open: bool) {
        let changed = if open { self.closed.remove(dir) } else { self.closed.insert(dir.to_string()) };
        if changed {
            self.rebuild();
        }
    }

    /// Opens every folder.
    pub fn open_all(&mut self) {
        if !self.closed.is_empty() {
            self.closed.clear();
            self.rebuild();
        }
    }

    /// Every file under a folder, open or not (a file: itself).
    pub fn files_under(&self, path: &str) -> Vec<String> {
        match path.ends_with('/') {
            true => self.entries.iter().filter(|e| e.path.starts_with(path)).map(|e| e.path.clone()).collect(),
            false => vec![path.to_string()],
        }
    }

    /// The folder row a row sits in.
    pub fn parent(&self, row: usize) -> Option<usize> {
        let depth = self.rows.get(row)?.depth;
        (0..row).rev().find(|&i| self.rows[i].depth < depth)
    }

    /// Scrolls a row into view.
    pub fn scroll_to(&mut self, row: usize) {
        let y = row as f32 * ROW_H;
        self.scroll.reveal(y, y + ROW_H + 4.0, self.view_h.max(ROW_H));
    }

    fn rebuild(&mut self) {
        let mut root = Node::default();
        for (i, e) in self.entries.iter().enumerate() {
            let mut node = &mut root;
            let parts: Vec<&str> = e.path.split('/').collect();
            for d in &parts[..parts.len() - 1] {
                node = node.dirs.entry(d.to_string()).or_default();
            }
            node.files.push(i);
        }
        let (mut rows, mut order) = (Vec::new(), Vec::new());
        self.walk(&root, "", 0, true, &mut rows, &mut order);
        self.rows = rows;
        self.order = order;
    }

    /// Rows for what's `shown` (inside open folders); every file goes into `order`.
    fn walk(&self, node: &Node, prefix: &str, depth: u16, shown: bool, rows: &mut Vec<Row>, order: &mut Vec<usize>) {
        for (name, mut dir) in &node.dirs {
            let mut name = name.clone();
            // A folder that only holds one folder shares its row.
            while dir.files.is_empty() && dir.dirs.len() == 1 {
                let (sub, next) = dir.dirs.iter().next().unwrap();
                name = format!("{name}/{sub}");
                dir = next;
            }
            let path = format!("{prefix}{name}/");
            if shown {
                let (files, adds, dels) = self.totals(dir);
                rows.push(Row { depth, name, path: path.clone(), kind: Kind::Dir, files, adds, dels });
            }
            self.walk(dir, &path, depth + 1, shown && self.is_open(&path), rows, order);
        }
        let mut files = node.files.clone();
        files.sort_by(|a, b| self.entries[*a].path.cmp(&self.entries[*b].path));
        for i in files {
            order.push(i);
            if shown {
                let e = &self.entries[i];
                let name = e.path.rsplit('/').next().unwrap_or(&e.path).to_string();
                rows.push(Row { depth, name, path: e.path.clone(), kind: Kind::File(i), files: 1, adds: e.adds, dels: e.dels });
            }
        }
    }

    fn totals(&self, node: &Node) -> (usize, u32, u32) {
        let mut t = node.files.iter().fold((node.files.len(), 0, 0), |(n, a, d), &i| (n, a + self.entries[i].adds, d + self.entries[i].dels));
        for d in node.dirs.values() {
            let (n, a, dd) = self.totals(d);
            t = (t.0 + n, t.1 + a, t.2 + dd);
        }
        t
    }

    pub fn draw(&mut self, ui: &mut Ui, r: Rect, look: &Look) -> Option<Click> {
        self.view_h = r.h;
        let off = self.scroll.update(ui, r, self.rows.len() as f32 * ROW_H + 4.0);
        ui.p.push_clip(r);
        if self.rows.is_empty() {
            ui.p.text(r.x + 16.0, r.y + ROW_H * 0.5, UI, 12.0, look.empty, theme::FAINT);
        }
        let first = (off / ROW_H) as usize;
        let last = (first + (r.h / ROW_H) as usize + 2).min(self.rows.len());
        let mut click = None;
        for i in first..last {
            let row = Rect::new(r.x + 6.0, r.y + i as f32 * ROW_H - off, r.w - 12.0, ROW_H);
            let t = &self.rows[i];
            let hov = ui.hovered(row);
            let selected = !t.is_dir() && look.selected == Some(t.path.as_str());
            if selected || hov {
                ui.p.rounded(row, 5.0, if selected { theme::SELECT } else { theme::HOVER });
            }
            if look.cursor == Some(t.path.as_str()) {
                ui.p.rounded_stroke(row, 5.0, 1.5, theme::ACCENT);
            }
            let x = row.x + 8.0 + t.depth as f32 * 14.0;
            let cy = row.cy();
            // The hovered row's buttons take the right end; otherwise how much changes.
            let mut right = row.right() - 4.0;
            let mut pressed_button = None;
            if hov {
                for &(label, danger) in look.buttons.iter().rev() {
                    let w = ui.p.measure(UI, 12.5, label) + 16.0;
                    let b = Rect::new(right - w, row.y + 2.0, w, ROW_H - 4.0);
                    if if danger { ui.danger_button(b, label) } else { ui.button(b, label, false, true) } {
                        pressed_button = Some(label);
                    }
                    right = b.x - 4.0;
                }
                ui.cursor = Cursor::Pointer;
            } else {
                right = Self::change_bar(ui, right, cy, t.adds, t.dels) - 8.0;
            }
            match t.kind {
                Kind::Dir => {
                    let c = theme::FAINT;
                    if self.is_open(&t.path) {
                        ui.p.line((x, cy - 2.0), (x + 4.0, cy + 2.0), 1.5, c);
                        ui.p.line((x + 4.0, cy + 2.0), (x + 8.0, cy - 2.0), 1.5, c);
                    } else {
                        ui.p.line((x + 2.0, cy - 4.0), (x + 6.0, cy), 1.5, c);
                        ui.p.line((x + 6.0, cy), (x + 2.0, cy + 4.0), 1.5, c);
                    }
                    let nw = ui.p.text_fit(x + 16.0, cy, right - x - 16.0, UI, 12.5, &t.name, theme::DIM.mix(theme::TEXT, 0.4));
                    if right - (x + 16.0 + nw) > 30.0 {
                        ui.p.text(x + 22.0 + nw, cy, UI, 11.0, &t.files.to_string(), theme::FAINT);
                    }
                }
                Kind::File(e) => {
                    let e = &self.entries[e];
                    let (letter, color) = e.badge;
                    let badge = Rect::new(x, cy - 8.0, 16.0, 16.0);
                    ui.p.rounded(badge, 4.0, color.alpha(0.16));
                    let lw = ui.p.measure(MONO, 10.5, &letter.to_string());
                    ui.p.text(badge.x + (16.0 - lw) * 0.5, cy, MONO, 10.5, &letter.to_string(), color);
                    let nw = ui.p.text_fit(x + 24.0, cy, right - x - 24.0, UI, 12.5, &t.name, theme::TEXT);
                    if let Some(from) = &e.from {
                        let room = right - (x + 30.0 + nw);
                        if room > 30.0 {
                            ui.p.text_fit(x + 30.0 + nw, cy, room, UI, 11.5, &format!("← {from}"), theme::FAINT);
                        }
                    }
                }
            }
            if let Some(label) = pressed_button {
                click = Some(Click::Button(label, self.files_under(&t.path)));
            } else if hov && ui.input.pressed {
                click = match t.kind {
                    // The second click of a double click leaves the folder as the first left it.
                    Kind::Dir if ui.input.double => None,
                    Kind::Dir => Some(Click::Dir(t.path.clone())),
                    Kind::File(_) => Some(Click::File(t.path.clone(), ui.input.double)),
                };
            }
        }
        ui.p.pop_clip();
        if let Some(Click::Dir(d)) = &click {
            let open = !self.is_open(d);
            self.set_open(&d.clone(), open);
        }
        click
    }

    /// "+12 −3" and a bar split by how much is added and removed, ending at `right`; returns
    /// where it starts.
    fn change_bar(ui: &mut Ui, right: f32, cy: f32, adds: u32, dels: u32) -> f32 {
        let dw = ui.p.measure(MONO, 11.0, &format!("−{dels}"));
        let aw = ui.p.measure(MONO, 11.0, &format!("+{adds}"));
        let mut x = right - dw;
        ui.p.text(x, cy, MONO, 11.0, &format!("−{dels}"), if dels > 0 { theme::RED.alpha(0.85) } else { theme::FAINT });
        x -= aw + 6.0;
        ui.p.text(x, cy, MONO, 11.0, &format!("+{adds}"), if adds > 0 { theme::GREEN.alpha(0.85) } else { theme::FAINT });
        let bar = Rect::new(x - 8.0 - 30.0, cy - 2.5, 30.0, 5.0);
        ui.p.rounded(bar, 2.5, theme::RAISED);
        let total = (adds + dels) as f32;
        if total > 0.0 {
            let gw = bar.w * adds as f32 / total;
            if adds > 0 {
                ui.p.rounded(Rect::new(bar.x, bar.y, gw.max(3.0), bar.h), 2.5, theme::GREEN.alpha(0.85));
            }
            if dels > 0 {
                ui.p.rounded(Rect::new(bar.x + gw, bar.y, (bar.w - gw).max(3.0), bar.h), 2.5, theme::RED.alpha(0.85));
            }
        }
        bar.x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, adds: u32) -> Entry {
        Entry { path: path.into(), from: None, badge: ('M', theme::YELLOW), adds, dels: 1 }
    }

    #[test]
    fn folders_first_chains_merged_totals_and_closing() {
        let mut t = FileTree::default();
        t.set(vec![e("src/app/a.rs", 1), e("src/app/b.rs", 2), e("src/ui.rs", 3), e("README.md", 4), e("docs/x/y/z.md", 5)]);
        let shown = |t: &FileTree| t.rows().iter().map(|r| format!("{}{}", "  ".repeat(r.depth as usize), r.name)).collect::<Vec<_>>();
        assert_eq!(shown(&t), ["docs/x/y", "  z.md", "src", "  app", "    a.rs", "    b.rs", "  ui.rs", "README.md"]);
        // tree_order sorts paths the way the rows list them.
        let mut paths = ["README.md", "src/ui.rs", "docs/x/y/z.md", "src/app/b.rs", "src/app/a.rs"];
        paths.sort_by(|a, b| tree_order(a, b));
        let files: Vec<&str> = t.rows().iter().filter(|r| !r.is_dir()).map(|r| r.path.as_str()).collect();
        assert_eq!(paths, files[..]);
        let src = &t.rows()[t.row_of("src/").unwrap()];
        assert_eq!((src.files, src.adds, src.dels), (3, 6, 3));
        assert_eq!(t.files_under("src/"), ["src/app/a.rs", "src/app/b.rs", "src/ui.rs"]);
        // Closing hides what's inside, and stays closed as the files change.
        t.set_open("src/app/", false);
        assert_eq!(shown(&t)[3..6], ["  app", "  ui.rs", "README.md"]);
        t.set(vec![e("src/app/a.rs", 1), e("src/app/c.rs", 1), e("src/ui.rs", 3)]);
        assert!(!t.is_open("src/app/") && t.row_of("src/app/c.rs").is_none());
        assert_eq!(t.parent(t.row_of("src/app/").unwrap()), t.row_of("src/"));
        // A file in a closed folder is near that folder's row.
        assert_eq!(t.row_near("src/app/c.rs"), t.row_of("src/app/"));
        assert_eq!(t.first_file(), Some("src/app/a.rs"), "closed folders still count");
        t.set_open("src/app/", true);
        assert!(t.row_of("src/app/c.rs").is_some());
        assert_eq!(t.row_near("src/app/c.rs"), t.row_of("src/app/c.rs"));
    }
}
