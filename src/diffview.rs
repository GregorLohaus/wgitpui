//! Virtualized diff viewer: unified or side-by-side layout, syntax colors, word-level change
//! highlights, collapsible files, and hunk/line selection for staging, unstaging and discarding.

use crate::font::{MONO, UI};
use crate::attr;
use crate::git::{self, Blame, BlameCommit, FileDiff, Side};
use std::path::PathBuf;
use std::sync::mpsc;
use crate::syntax::{self, Lang, Tok};
use crate::ui::{theme, Color, Cursor, Rect, Scroll, Ui};
use std::collections::{HashMap, HashSet};

pub const DIFF_ROW_H: f32 = 19.0;
const MAX_DIFF_ROWS: usize = 120_000;
const CODE_SIZE: f32 = 12.0;

#[derive(Clone, Copy, PartialEq)]
pub enum DiffMode {
    Unstaged,
    Staged,
    Untracked,
    /// Read-only (commit or comparison).
    Commit,
}

pub enum DRow {
    File(usize),
    Hunk(usize, usize),
    /// Unified: one hunk line.
    Line { f: usize, h: usize, l: usize },
    /// Split: left (old) and right (new) halves.
    Pair { f: usize, h: usize, left: Option<usize>, right: Option<usize> },
    /// Split: a full-width annotation line (e.g. conflict markers).
    Marker { f: usize, h: usize, l: usize },
    Note(String),
}

pub enum DiffAction {
    /// Move the whole hunk (or only its selected lines) into / out of the index.
    Hunk { f: usize, h: usize, lines_only: bool },
    /// Revert the hunk (or selected lines) in the working tree.
    Discard { f: usize, h: usize, lines_only: bool },
    StageFile(String),
    DeleteUntracked(String),
    Blame(String),
    /// Jump to the commit that last touched a line.
    OpenCommit(String),
}

/// Where to blame each side of this diff from (old side = removed / left, new = added / right).
pub struct BlameSrc {
    pub root: PathBuf,
    pub old: Option<Side>,
    pub new: Option<Side>,
}

enum Slot {
    Pending(mpsc::Receiver<Option<Blame>>),
    Ready(Option<Blame>),
}

/// How a file changed, derived from the diff header.
#[derive(Clone)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed(String),
}

impl FileStatus {
    pub fn badge(&self) -> (char, Color) {
        match self {
            FileStatus::Added => ('A', theme::GREEN),
            FileStatus::Deleted => ('D', theme::RED),
            FileStatus::Modified => ('M', theme::YELLOW),
            FileStatus::Renamed(_) => ('R', theme::PURPLE),
        }
    }
}

pub struct DiffView {
    pub files: Vec<FileDiff>,
    pub status: Vec<FileStatus>,
    unified: Vec<DRow>,
    split: Vec<DRow>,
    file_rows_u: Vec<usize>,
    file_rows_s: Vec<usize>,
    /// Per file/hunk/line: (old line number, new line number).
    lnums: Vec<Vec<Vec<(Option<u32>, Option<u32>)>>>,
    /// Removed line ↔ added line it replaced (both directions), for word highlighting.
    partner: HashMap<(usize, usize, usize), usize>,
    words: HashMap<(usize, usize, usize), Vec<(usize, usize)>>,
    langs: Vec<Option<&'static Lang>>,
    pub collapsed: HashSet<usize>,
    pub mode: DiffMode,
    pub key: String,
    pub scroll: Scroll,
    pub hscroll: f32,
    pub selected: HashSet<(usize, usize, usize)>,
    drag: Option<bool>,
    first_row: usize,
    notes: Vec<String>,
    spans: Vec<(usize, usize, Tok)>,
    blame_src: Option<BlameSrc>,
    /// (file, new side?) → blame, loaded lazily for visible files.
    blames: HashMap<(usize, bool), Slot>,
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

pub fn tok_color(t: Tok) -> Color {
    match t {
        Tok::Plain => theme::TEXT.mix(theme::DIM, 0.12),
        Tok::Keyword => theme::PURPLE,
        Tok::Str => theme::GREEN.mix(theme::TEXT, 0.15),
        Tok::Comment => theme::FAINT.mix(theme::DIM, 0.4),
        Tok::Number => theme::ORANGE,
        Tok::Type => theme::YELLOW,
        Tok::Func => theme::ACCENT,
        Tok::Punct => theme::DIM.mix(theme::TEXT, 0.3),
    }
}

fn display(line: &str) -> String {
    line.get(1..).unwrap_or("").replace('\t', "    ")
}

impl DiffView {
    pub fn empty() -> Self {
        Self::new(vec![], DiffMode::Commit, String::new())
    }

    pub fn new(files: Vec<FileDiff>, mode: DiffMode, key: String) -> Self {
        let status = files
            .iter()
            .map(|f| {
                if f.header.iter().any(|l| l.starts_with("new file mode")) {
                    FileStatus::Added
                } else if f.header.iter().any(|l| l.starts_with("deleted file mode")) {
                    FileStatus::Deleted
                } else if let Some(from) = f.header.iter().find_map(|l| l.strip_prefix("rename from ")) {
                    FileStatus::Renamed(from.to_string())
                } else {
                    FileStatus::Modified
                }
            })
            .collect();
        let mut lnums = Vec::with_capacity(files.len());
        let mut partner = HashMap::new();
        for (fi, f) in files.iter().enumerate() {
            let mut fl = Vec::with_capacity(f.hunks.len());
            for (hi, h) in f.hunks.iter().enumerate() {
                let (mut o, mut n) = (h.old_start, h.new_start);
                let nums = h
                    .lines
                    .iter()
                    .map(|line| match line.as_bytes().first() {
                        Some(b'+') => {
                            n += 1;
                            (None, Some(n - 1))
                        }
                        Some(b'-') => {
                            o += 1;
                            (Some(o - 1), None)
                        }
                        Some(b'\\') => (None, None),
                        _ => {
                            o += 1;
                            n += 1;
                            (Some(o - 1), Some(n - 1))
                        }
                    })
                    .collect();
                fl.push(nums);
                for (dels, adds) in change_runs(&h.lines) {
                    for (d, a) in dels.iter().zip(adds.iter()) {
                        partner.insert((fi, hi, *d), *a);
                        partner.insert((fi, hi, *a), *d);
                    }
                }
            }
            lnums.push(fl);
        }
        let langs = files.iter().map(|f| syntax::lang_for(&f.path)).collect();
        let mut v = DiffView {
            files,
            status,
            unified: vec![],
            split: vec![],
            file_rows_u: vec![],
            file_rows_s: vec![],
            lnums,
            partner,
            words: HashMap::new(),
            langs,
            collapsed: HashSet::new(),
            mode,
            key,
            scroll: Scroll::default(),
            hscroll: 0.0,
            selected: HashSet::new(),
            drag: None,
            first_row: 0,
            notes: vec![],
            spans: vec![],
            blame_src: None,
            blames: HashMap::new(),
        };
        v.rebuild();
        v
    }

    /// Enables per-line attribution ("who, which commit, when").
    pub fn with_blame(mut self, src: BlameSrc) -> Self {
        self.blame_src = Some(src);
        self
    }

    fn poll_blames(&mut self) -> bool {
        let mut pending = false;
        for slot in self.blames.values_mut() {
            if let Slot::Pending(rx) = slot {
                match rx.try_recv() {
                    Ok(b) => *slot = Slot::Ready(b),
                    Err(mpsc::TryRecvError::Disconnected) => *slot = Slot::Ready(None),
                    Err(mpsc::TryRecvError::Empty) => pending = true,
                }
            }
        }
        pending
    }

    /// Starts blaming one side of one file in the background (a few at a time).
    fn want(&mut self, f: usize, new_side: bool) {
        if self.blames.contains_key(&(f, new_side)) {
            return;
        }
        let Some(src) = &self.blame_src else { return };
        let side = if new_side { src.new.clone() } else { src.old.clone() };
        let path = if new_side { self.files[f].path.clone() } else { self.files[f].old_path.clone() };
        let Some(side) = side else {
            self.blames.insert((f, new_side), Slot::Ready(None));
            return;
        };
        if self.blames.values().filter(|s| matches!(s, Slot::Pending(_))).count() >= 6 {
            return;
        }
        let root = src.root.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(git::blame_at(&root, &side, &path));
        });
        self.blames.insert((f, new_side), Slot::Pending(rx));
    }

    /// The commit behind one line. `new_side`: None = pick by the line's tag (unified view).
    fn who(&self, f: usize, h: usize, l: usize, new_side: Option<bool>) -> Option<&BlameCommit> {
        let tag = self.files[f].hunks[h].lines[l].as_bytes().first().copied()?;
        if tag == b'\\' {
            return None;
        }
        let new_side = new_side.unwrap_or(tag != b'-');
        let (old, new) = self.lnums[f][h][l];
        let no = if new_side { new } else { old }? as usize;
        match self.blames.get(&(f, new_side))? {
            Slot::Ready(Some(b)) => {
                let i = *b.line_commit.get(no.checked_sub(1)?)? as usize;
                b.commits.get(i)
            }
            _ => None,
        }
    }

    fn row_line(&self, row: usize, split: bool, side: usize) -> Option<(usize, usize, usize, Option<bool>)> {
        match self.rows(split).get(row)? {
            DRow::Line { f, h, l } => Some((*f, *h, *l, None)),
            DRow::Pair { f, h, left, right } => {
                let l = if side == 0 { (*left)? } else { (*right)? };
                Some((*f, *h, l, Some(side == 1)))
            }
            _ => None,
        }
    }

    /// Draws the attribution cell for one row/side; returns the commit if the cell is hovered.
    fn attr_cell(&mut self, ui: &mut Ui, cell: Rect, row: usize, split: bool, side: usize) -> Option<BlameCommit> {
        let (f, h, l, ns) = self.row_line(row, split, side)?;
        let tag = self.files[f].hunks[h].lines[l].as_bytes().first().copied().unwrap_or(b' ');
        if tag == b'\\' {
            return None;
        }
        let new_side = ns.unwrap_or(tag != b'-');
        self.want(f, new_side);
        let Some(c) = self.who(f, h, l, ns).cloned() else {
            if matches!(self.blames.get(&(f, new_side)), Some(Slot::Pending(_)) | None) {
                ui.p.text(cell.x + 8.0, cell.cy(), UI, 11.0, "…", theme::FAINT);
            }
            return None;
        };
        // Only label the first line of a run from the same commit.
        let prev = row.checked_sub(1).and_then(|p| self.row_line(p, split, side)).and_then(|(pf, ph, pl, pns)| {
            (pf == f).then(|| self.who(pf, ph, pl, pns).map(|x| x.hash.clone())).flatten()
        });
        let hov = ui.hovered(cell);
        attr::cell(ui, cell, &c, prev.as_deref() != Some(c.hash.as_str()), hov);
        hov.then_some(c)
    }

    pub fn note(mut self, msg: String) -> Self {
        self.notes.push(msg);
        self.rebuild();
        self
    }

    /// Rebuilds both row layouts (after construction or collapsing a file).
    fn rebuild(&mut self) {
        let (mut u, mut s, mut fu, mut fs) = (vec![], vec![], vec![], vec![]);
        for n in &self.notes {
            u.push(DRow::Note(n.clone()));
            s.push(DRow::Note(n.clone()));
        }
        for (fi, f) in self.files.iter().enumerate() {
            fu.push(u.len());
            fs.push(s.len());
            u.push(DRow::File(fi));
            s.push(DRow::File(fi));
            if self.collapsed.contains(&fi) {
                continue;
            }
            let note = if f.binary {
                Some("Binary file not shown")
            } else if f.hunks.is_empty() {
                Some("No content changes (mode or rename only)")
            } else {
                None
            };
            if let Some(n) = note {
                u.push(DRow::Note(n.into()));
                s.push(DRow::Note(n.into()));
            }
            for (hi, h) in f.hunks.iter().enumerate() {
                u.push(DRow::Hunk(fi, hi));
                s.push(DRow::Hunk(fi, hi));
                for l in 0..h.lines.len() {
                    u.push(DRow::Line { f: fi, h: hi, l });
                }
                // Split: context lines pair with themselves; each run of removals pairs up
                // index-by-index with the run of additions that follows it.
                let lines = &h.lines;
                let mut i = 0;
                while i < lines.len() {
                    match lines[i].as_bytes().first() {
                        Some(b'-') | Some(b'+') => {
                            let mut dels = vec![];
                            while i < lines.len() && lines[i].starts_with('-') {
                                dels.push(i);
                                i += 1;
                            }
                            // Markers between the two runs (conflict views) don't break pairing.
                            while !dels.is_empty() && i < lines.len() && lines[i].starts_with('\\') {
                                i += 1;
                            }
                            let mut adds = vec![];
                            while i < lines.len() && lines[i].starts_with('+') {
                                adds.push(i);
                                i += 1;
                            }
                            for k in 0..dels.len().max(adds.len()) {
                                s.push(DRow::Pair { f: fi, h: hi, left: dels.get(k).copied(), right: adds.get(k).copied() });
                            }
                        }
                        Some(b'\\') => {
                            s.push(DRow::Marker { f: fi, h: hi, l: i });
                            i += 1;
                        }
                        _ => {
                            s.push(DRow::Pair { f: fi, h: hi, left: Some(i), right: Some(i) });
                            i += 1;
                        }
                    }
                }
            }
            if u.len() > MAX_DIFF_ROWS || s.len() > MAX_DIFF_ROWS {
                u.push(DRow::Note("… diff truncated".into()));
                s.push(DRow::Note("… diff truncated".into()));
                break;
            }
        }
        self.unified = u;
        self.split = s;
        self.file_rows_u = fu;
        self.file_rows_s = fs;
    }

    fn rows(&self, split: bool) -> &[DRow] {
        if split { &self.split } else { &self.unified }
    }

    pub fn jump_to_file(&mut self, fi: usize, split: bool) {
        let rows = if split { &self.file_rows_s } else { &self.file_rows_u };
        if let Some(&r) = rows.get(fi) {
            self.scroll.jump(r as f32 * DIFF_ROW_H);
        }
    }

    /// The file whose section is at the top of the viewport.
    pub fn current_file(&self, split: bool) -> Option<usize> {
        let rows = if split { &self.file_rows_s } else { &self.file_rows_u };
        let i = rows.partition_point(|&r| r <= self.first_row);
        i.checked_sub(1)
    }

    pub fn toggle_collapse(&mut self, fi: usize) {
        if !self.collapsed.remove(&fi) {
            self.collapsed.insert(fi);
        }
        self.rebuild();
    }

    pub fn set_all_collapsed(&mut self, collapsed: bool) {
        self.collapsed = if collapsed { (0..self.files.len()).collect() } else { HashSet::new() };
        self.rebuild();
    }

    fn selected_in(&self, f: usize, h: usize) -> usize {
        self.selected.iter().filter(|s| s.0 == f && s.1 == h).count()
    }

    /// Word-level change ranges for one line (computed lazily against its partner line).
    fn word_ranges(&mut self, f: usize, h: usize, l: usize) -> &[(usize, usize)] {
        if !self.words.contains_key(&(f, h, l)) {
            match self.partner.get(&(f, h, l)) {
                Some(&p) => {
                    let lines = &self.files[f].hunks[h].lines;
                    let (d, a) = if lines[l].starts_with('-') { (l, p) } else { (p, l) };
                    let (rd, ra) = syntax::word_diff(&display(&lines[d]), &display(&lines[a]));
                    self.words.insert((f, h, d), rd);
                    self.words.insert((f, h, a), ra);
                }
                None => {
                    self.words.insert((f, h, l), vec![]);
                }
            }
        }
        &self.words[&(f, h, l)]
    }

    /// Draws one line of code: word-change backgrounds, then syntax-colored spans.
    fn code(&mut self, ui: &mut Ui, x: f32, cy: f32, f: usize, h: usize, l: usize) {
        let line = &self.files[f].hunks[h].lines[l];
        let tag = line.as_bytes().first().copied().unwrap_or(b' ');
        if tag == b'\\' {
            ui.p.text(x, cy, MONO, 11.0, line.trim_start_matches('\\').trim(), theme::FAINT);
            return;
        }
        let text = display(line);
        if tag == b'+' || tag == b'-' {
            let word_bg = if tag == b'+' { theme::GREEN.alpha(0.24) } else { theme::RED.alpha(0.28) };
            let ranges = self.word_ranges(f, h, l).to_vec();
            for (s, e) in ranges {
                let x0 = x + ui.p.measure(MONO, CODE_SIZE, &text[..s]);
                let w = ui.p.measure(MONO, CODE_SIZE, &text[s..e]);
                ui.p.rounded(Rect::new(x0 - 1.0, cy - 8.5, w + 2.0, 17.0), 3.0, word_bg);
            }
        }
        let mut spans = std::mem::take(&mut self.spans);
        syntax::highlight(self.langs[f], &text, &mut spans);
        let mut xx = x;
        for &(s, e, t) in &spans {
            xx += ui.p.text(xx, cy, MONO, CODE_SIZE, &text[s..e], tok_color(t));
        }
        self.spans = spans;
    }

    /// Click/drag line selection for staging. `key` is the line under the cursor in `cell`.
    fn select_line(&mut self, ui: &mut Ui, cell: Rect, key: (usize, usize, usize)) {
        if !ui.hovered(cell) {
            return;
        }
        ui.cursor = Cursor::Pointer;
        let sel = self.selected.contains(&key);
        if ui.input.pressed {
            self.drag = Some(!sel);
        }
        if let Some(state) = self.drag {
            if ui.input.down || ui.input.pressed {
                if state {
                    self.selected.insert(key);
                } else {
                    self.selected.remove(&key);
                }
            }
        }
    }

    pub fn draw(&mut self, ui: &mut Ui, r: Rect, split: bool, authors: bool) -> Option<DiffAction> {
        ui.p.rect(r, theme::BG);
        if self.rows(split).is_empty() {
            let msg = match self.mode {
                DiffMode::Commit => "No differences",
                _ => "Select a file to see its changes",
            };
            let w = ui.p.measure(UI, 13.0, msg);
            ui.p.text(r.x + (r.w - w) * 0.5, r.cy(), UI, 13.0, msg, theme::FAINT);
            return None;
        }
        let n_rows = self.rows(split).len();
        let content = n_rows as f32 * DIFF_ROW_H + 24.0;
        if ui.hovered(r) && ui.input.scroll.0 != 0.0 {
            self.hscroll = (self.hscroll - ui.input.scroll.0).clamp(0.0, 4000.0);
        }
        let off = self.scroll.update(ui, r, content);
        let first = (off / DIFF_ROW_H) as usize;
        let last = (first + (r.h / DIFF_ROW_H) as usize + 2).min(n_rows);
        self.first_row = first;
        let selectable = matches!(self.mode, DiffMode::Unstaged | DiffMode::Staged | DiffMode::Untracked);
        let mut action = None;
        let mut toggle = None;

        // Column geometry. The attribution gutter only appears when there's room for it.
        let num_w = 44.0;
        let half = (r.w * 0.5).floor();
        let attributed = authors && self.blame_src.is_some();
        let aw = match (attributed, split) {
            (true, false) if r.w > 640.0 => 196.0,
            (true, true) if half > 470.0 => 136.0,
            _ => 0.0,
        };
        let ux = r.x + aw; // unified: start of the line-number gutter
        let u_text_x = ux + 2.0 * num_w + 22.0;
        let (lx, rx) = (r.x, r.x + half);
        let s_text = |x0: f32| x0 + aw + num_w + 18.0;
        if attributed && self.poll_blames() {
            ui.wake_in(std::time::Duration::from_millis(50));
        }
        let mut hover_who: Option<BlameCommit> = None;

        ui.p.push_clip(r);
        for i in first..last {
            let y = r.y + i as f32 * DIFF_ROW_H - off;
            let row = Rect::new(r.x, y, r.w, DIFF_ROW_H);
            match self.rows(split)[i] {
                DRow::File(fi) => {
                    let hov = ui.hovered(row);
                    ui.p.rect(row, if hov { theme::RAISED } else { theme::PANEL2 });
                    ui.p.rect(Rect::new(r.x, y + DIFF_ROW_H - 1.0, r.w, 1.0), theme::BORDER);
                    let (cx, cy) = (r.x + 14.0, row.cy());
                    if self.collapsed.contains(&fi) {
                        ui.p.line((cx - 2.0, cy - 4.0), (cx + 2.0, cy), 1.5, theme::DIM);
                        ui.p.line((cx + 2.0, cy), (cx - 2.0, cy + 4.0), 1.5, theme::DIM);
                    } else {
                        ui.p.line((cx - 4.0, cy - 2.0), (cx, cy + 2.0), 1.5, theme::DIM);
                        ui.p.line((cx, cy + 2.0), (cx + 4.0, cy - 2.0), 1.5, theme::DIM);
                    }
                    let (letter, color) = self.status[fi].badge();
                    let badge = Rect::new(r.x + 26.0, cy - 7.5, 15.0, 15.0);
                    ui.p.rounded(badge, 3.5, color.alpha(0.16));
                    ui.p.text(badge.x + 3.5, cy, MONO, 10.0, &letter.to_string(), color);
                    let f = &self.files[fi];
                    let mut x = r.x + 50.0;
                    let label = match &self.status[fi] {
                        FileStatus::Renamed(from) => format!("{from} → {}", f.path),
                        _ => f.path.clone(),
                    };
                    let (adds, dels, path, binary) = (f.adds, f.dels, f.path.clone(), f.binary);
                    x += ui.p.text_fit(x, cy, r.w - 220.0, UI, 12.5, &label, theme::TEXT) + 10.0;
                    x += ui.p.text(x, cy, MONO, 11.5, &format!("+{adds}"), theme::GREEN) + 6.0;
                    ui.p.text(x, cy, MONO, 11.5, &format!("-{dels}"), theme::RED);
                    let blame = Rect::new(r.right() - 70.0, y + 1.0, 62.0, DIFF_ROW_H - 2.0);
                    let can_blame = !binary && self.mode != DiffMode::Untracked && !matches!(self.status[fi], FileStatus::Deleted);
                    if can_blame && ui.button(blame, "Blame", false, true) {
                        action = Some(DiffAction::Blame(path));
                    } else if hov && ui.input.pressed && !(can_blame && blame.contains(ui.input.mouse)) {
                        toggle = Some(fi);
                    }
                    if hov {
                        ui.cursor = Cursor::Pointer;
                    }
                }
                DRow::Hunk(fi, hi) => {
                    ui.p.rect(row, theme::ACCENT.alpha(0.07));
                    let header = self.files[fi].hunks[hi].header.clone();
                    let n = self.selected_in(fi, hi);
                    let path = self.files[fi].path.clone();
                    // (label, is_discard, primary)
                    let buttons: Vec<(String, bool, bool)> = match self.mode {
                        DiffMode::Unstaged if n == 0 => {
                            vec![("Discard hunk".into(), true, false), ("Stage hunk".into(), false, false)]
                        }
                        DiffMode::Unstaged => vec![
                            (format!("Discard {n} line{}", plural(n)), true, false),
                            (format!("Stage {n} line{}", plural(n)), false, true),
                        ],
                        DiffMode::Staged if n == 0 => vec![("Unstage hunk".into(), false, false)],
                        DiffMode::Staged => vec![(format!("Unstage {n} line{}", plural(n)), false, true)],
                        DiffMode::Untracked if n == 0 => vec![("Delete file".into(), true, false), ("Stage file".into(), false, false)],
                        DiffMode::Untracked => vec![
                            (format!("Discard {n} line{}", plural(n)), true, false),
                            (format!("Stage {n} line{}", plural(n)), false, true),
                        ],
                        DiffMode::Commit => vec![],
                    };
                    let widths: Vec<f32> = buttons.iter().map(|b| ui.p.measure(UI, 12.5, &b.0) + 22.0).collect();
                    let total: f32 = widths.iter().sum::<f32>() + 6.0 * buttons.len() as f32;
                    ui.p.text_fit(r.x + 12.0, row.cy(), r.w - total - 30.0, MONO, 11.5, &header, theme::DIM);
                    let mut bx = r.right() - 8.0 - total + 6.0;
                    for ((label, discard, primary), w) in buttons.into_iter().zip(widths) {
                        let b = Rect::new(bx, y + 1.0, w, DIFF_ROW_H - 2.0);
                        let hit = if discard { ui.danger_button(b, &label) } else { ui.button(b, &label, primary, true) };
                        if hit {
                            action = Some(match (self.mode, discard) {
                                (DiffMode::Untracked, true) if n == 0 => DiffAction::DeleteUntracked(path.clone()),
                                (DiffMode::Untracked, false) if n == 0 => DiffAction::StageFile(path.clone()),
                                (_, true) => DiffAction::Discard { f: fi, h: hi, lines_only: n > 0 },
                                (_, false) => DiffAction::Hunk { f: fi, h: hi, lines_only: n > 0 },
                            });
                        }
                        bx += w + 6.0;
                    }
                }
                DRow::Line { f, h, l } => {
                    let tag = self.files[f].hunks[h].lines[l].as_bytes().first().copied().unwrap_or(b' ');
                    let bg = match tag {
                        b'+' => theme::GREEN.alpha(0.08),
                        b'-' => theme::RED.alpha(0.09),
                        _ => Color([0.0; 4]),
                    };
                    ui.p.rect(row, bg);
                    if self.selected.contains(&(f, h, l)) {
                        ui.p.rect(row, theme::ACCENT.alpha(0.20));
                        ui.p.rect(Rect::new(r.x, y, 3.0, DIFF_ROW_H), theme::ACCENT);
                    }
                    let (old, new) = self.lnums[f][h][l];
                    if aw > 0.0 {
                        let cell = Rect::new(r.x, y, aw, DIFF_ROW_H);
                        if let Some(c) = self.attr_cell(ui, cell, i, false, 0) {
                            hover_who = Some(c);
                        }
                    }
                    num(ui, ux + num_w - 6.0, row.cy(), old);
                    num(ui, ux + 2.0 * num_w - 6.0, row.cy(), new);
                    let sign_color = match tag {
                        b'+' => theme::GREEN,
                        b'-' => theme::RED,
                        _ => theme::FAINT,
                    };
                    if tag == b'+' || tag == b'-' {
                        ui.p.text(ux + 2.0 * num_w + 6.0, row.cy(), MONO, CODE_SIZE, &(tag as char).to_string(), sign_color);
                        if selectable {
                            self.select_line(ui, Rect::new(ux, y, r.w - aw, DIFF_ROW_H), (f, h, l));
                        }
                    }
                }
                DRow::Pair { f, h, left, right } => {
                    for (side, idx) in [(0, left), (1, right)] {
                        let x0 = if side == 0 { lx } else { rx };
                        let cell = Rect::new(x0, y, half, DIFF_ROW_H);
                        let Some(l) = idx else {
                            ui.p.rect(cell, theme::PANEL.alpha(0.55));
                            continue;
                        };
                        let tag = self.files[f].hunks[h].lines[l].as_bytes().first().copied().unwrap_or(b' ');
                        let changed = tag == b'+' || tag == b'-';
                        if changed {
                            ui.p.rect(cell, if tag == b'+' { theme::GREEN.alpha(0.08) } else { theme::RED.alpha(0.09) });
                        }
                        if self.selected.contains(&(f, h, l)) {
                            ui.p.rect(cell, theme::ACCENT.alpha(0.20));
                            ui.p.rect(Rect::new(x0, y, 3.0, DIFF_ROW_H), theme::ACCENT);
                        }
                        let (old, new) = self.lnums[f][h][l];
                        if aw > 0.0 {
                            let ac = Rect::new(x0, y, aw, DIFF_ROW_H);
                            if let Some(c) = self.attr_cell(ui, ac, i, true, side) {
                                hover_who = Some(c);
                            }
                        }
                        num(ui, x0 + aw + num_w - 6.0, row.cy(), if side == 0 { old } else { new });
                        if changed && selectable {
                            self.select_line(ui, Rect::new(x0 + aw, y, half - aw, DIFF_ROW_H), (f, h, l));
                        }
                    }
                    ui.p.rect(Rect::new(rx - 0.5, y, 1.0, DIFF_ROW_H), theme::BORDER);
                }
                DRow::Marker { f, h, l } => {
                    ui.p.rect(row, theme::PANEL2.alpha(0.7));
                    let text = self.files[f].hunks[h].lines[l].trim_start_matches('\\').trim().to_string();
                    ui.p.text_fit(r.x + 16.0, row.cy(), r.w - 32.0, MONO, 11.0, &text, theme::DIM);
                }
                DRow::Note(ref s) => {
                    let s = s.clone();
                    ui.p.text(r.x + 16.0, row.cy(), UI, 12.0, &s, theme::FAINT);
                }
            }
        }
        ui.p.pop_clip();

        // Code text in its own clip(s), so horizontal scrolling never bleeds into the gutters.
        let columns: Vec<(Rect, Option<usize>)> = if split {
            vec![
                (Rect::new(s_text(lx) - 4.0, r.y, (half - aw - num_w - 14.0).max(0.0), r.h), Some(0)),
                (Rect::new(s_text(rx) - 4.0, r.y, (half - aw - num_w - 14.0).max(0.0), r.h), Some(1)),
            ]
        } else {
            vec![(Rect::new(u_text_x - 4.0, r.y, (r.right() - u_text_x + 4.0).max(0.0), r.h), None)]
        };
        for (clip, side) in columns {
            ui.p.push_clip(clip);
            for i in first..last {
                let y = r.y + i as f32 * DIFF_ROW_H - off;
                let cy = y + DIFF_ROW_H * 0.5;
                let item = match (&self.rows(split)[i], side) {
                    (DRow::Line { f, h, l }, None) => Some((*f, *h, *l)),
                    (DRow::Pair { f, h, left, .. }, Some(0)) => left.map(|l| (*f, *h, l)),
                    (DRow::Pair { f, h, right, .. }, Some(_)) => right.map(|l| (*f, *h, l)),
                    _ => None,
                };
                if let Some((f, h, l)) = item {
                    let x = clip.x + 4.0 - self.hscroll;
                    self.code(ui, x, cy, f, h, l);
                }
            }
            ui.p.pop_clip();
        }

        if let Some(c) = hover_who {
            ui.cursor = Cursor::Pointer;
            attr::tooltip(ui, ui.input.mouse, &c);
            if ui.input.pressed && !attr::uncommitted(&c) {
                action = Some(DiffAction::OpenCommit(c.hash.clone()));
            }
        }
        if !ui.input.down {
            self.drag = None;
        }
        if let Some(fi) = toggle {
            self.toggle_collapse(fi);
        }
        action
    }
}

fn num(ui: &mut Ui, right: f32, cy: f32, n: Option<u32>) {
    if let Some(n) = n {
        let s = n.to_string();
        let w = ui.p.measure(MONO, 11.0, &s);
        ui.p.text(right - w, cy, MONO, 11.0, &s, theme::FAINT);
    }
}

/// Runs of consecutive removals followed by additions within a hunk: (removed idx, added idx).
fn change_runs(lines: &[String]) -> Vec<(Vec<usize>, Vec<usize>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with('-') || lines[i].starts_with('+') {
            let mut dels = vec![];
            while i < lines.len() && lines[i].starts_with('-') {
                dels.push(i);
                i += 1;
            }
            while !dels.is_empty() && i < lines.len() && lines[i].starts_with('\\') {
                i += 1;
            }
            let mut adds = vec![];
            while i < lines.len() && lines[i].starts_with('+') {
                adds.push(i);
                i += 1;
            }
            out.push((dels, adds));
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Hunk;

    #[test]
    fn untracked_lines_select_and_offer_line_staging() {
        let fd = FileDiff {
            header: vec!["diff --git a/u b/u".into(), "new file mode 100644".into(), "--- /dev/null".into(), "+++ b/u".into()],
            path: "u".into(),
            hunks: vec![Hunk { header: "@@ -0,0 +1,2 @@".into(), old_start: 0, new_start: 1, lines: vec!["+p".into(), "+q".into()] }],
            adds: 2,
            ..Default::default()
        };
        let mut v = DiffView::new(vec![fd], DiffMode::Untracked, "u".into());
        let mut ui = Ui::new(crate::font::Fonts::load().unwrap());
        let r = Rect::new(0.0, 0.0, 900.0, 400.0);
        // Rows: file, hunk, "+p", "+q". Click "+q".
        ui.begin(900.0, 400.0, 1.0);
        ui.input.mouse = (400.0, 3.5 * DIFF_ROW_H);
        ui.input.pressed = true;
        ui.input.down = true;
        assert!(v.draw(&mut ui, r, false, false).is_none());
        ui.input.end_frame();
        assert!(v.selected.contains(&(0, 0, 1)));
        // The hunk's primary button now stages just that line.
        ui.input.down = false;
        ui.begin(900.0, 400.0, 1.0);
        ui.input.mouse = (900.0 - 30.0, 1.5 * DIFF_ROW_H);
        ui.input.pressed = true;
        let a = v.draw(&mut ui, r, false, false);
        assert!(matches!(a, Some(DiffAction::Hunk { f: 0, h: 0, lines_only: true })));
    }
}
