//! Conflict resolver: one view of the file, with each conflict shown in place as the lines from
//! both sides. Checked lines are kept, unchecked lines are dropped; the view reads like the
//! resulting file, and "Preview result" shows exactly what will be saved.

use crate::attr;
use crate::diffview::tok_color;
use crate::git::{self, Blame, BlameCommit};
use crate::font::{MONO, UI};
use crate::syntax::{self, Lang};
use crate::ui::{theme, Color, Cursor, Key, Rect, Scroll, TextEdit, Ui};

pub const OURS: Color = theme::ACCENT;
pub const THEIRS: Color = theme::ORANGE;
const EDITED: Color = theme::GREEN;
const LINE_H: f32 = 20.0;
const HEADER_H: f32 = 30.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Choice {
    /// Keep the checked lines.
    Picked,
    /// Keep nothing from this conflict.
    Neither,
    /// Hand-written result.
    Edited,
}

pub struct Block {
    pub ours: Vec<String>,
    pub base: Option<Vec<String>>,
    pub theirs: Vec<String>,
    markers: [String; 4], // <<<<<<<, |||||||, =======, >>>>>>>
    pub pick_ours: Vec<bool>,
    pub pick_theirs: Vec<bool>,
    pub theirs_first: bool,
    pub choice: Option<Choice>,
    pub edited: Option<String>,
    /// Who last changed each line on each side (filled by `attribute`).
    pub who_ours: Vec<Option<BlameCommit>>,
    pub who_theirs: Vec<Option<BlameCommit>>,
}

impl Block {
    fn set(&mut self, ours: bool, theirs: bool, theirs_first: bool) {
        self.pick_ours.iter_mut().for_each(|p| *p = ours);
        self.pick_theirs.iter_mut().for_each(|p| *p = theirs);
        self.theirs_first = theirs_first;
        self.edited = None;
        self.choice = if ours && !self.ours.is_empty() || theirs && !self.theirs.is_empty() {
            Some(Choice::Picked)
        } else {
            Some(Choice::Neither)
        };
    }

    pub fn take_ours(&mut self) {
        self.set(true, false, false);
    }

    pub fn take_theirs(&mut self) {
        self.set(false, true, false);
    }

    /// Both sides; calling it again while both are kept flips the order.
    pub fn take_both(&mut self) {
        let flip = self.choice == Some(Choice::Picked) && self.pick_ours.iter().all(|p| *p) && self.pick_theirs.iter().all(|p| *p);
        let theirs_first = if flip { !self.theirs_first } else { false };
        self.set(true, true, theirs_first);
    }

    pub fn take_neither(&mut self) {
        self.set(false, false, false);
    }

    /// Toggles one line. With nothing checked the conflict is unresolved again.
    pub fn toggle(&mut self, theirs: bool, i: usize) {
        if self.choice == Some(Choice::Edited) || self.choice == Some(Choice::Neither) {
            self.pick_ours.iter_mut().for_each(|p| *p = false);
            self.pick_theirs.iter_mut().for_each(|p| *p = false);
            self.edited = None;
        }
        let v = if theirs { &mut self.pick_theirs } else { &mut self.pick_ours };
        if let Some(p) = v.get_mut(i) {
            *p = !*p;
        }
        let any = self.pick_ours.iter().chain(&self.pick_theirs).any(|p| *p);
        self.choice = any.then_some(Choice::Picked);
    }

    pub fn reset(&mut self) {
        self.pick_ours.iter_mut().for_each(|p| *p = false);
        self.pick_theirs.iter_mut().for_each(|p| *p = false);
        self.theirs_first = false;
        self.choice = None;
        self.edited = None;
    }

    pub fn resolved(&self) -> bool {
        self.choice.is_some()
    }

    /// Lines in display (and result) order: (theirs?, index).
    fn order(&self) -> Vec<(bool, usize)> {
        let o = (0..self.ours.len()).map(|i| (false, i));
        let t = (0..self.theirs.len()).map(|i| (true, i));
        if self.theirs_first { t.chain(o).collect() } else { o.chain(t).collect() }
    }

    fn picked(&self, theirs: bool, i: usize) -> bool {
        if self.choice != Some(Choice::Picked) {
            return false;
        }
        if theirs { self.pick_theirs[i] } else { self.pick_ours[i] }
    }

    fn line(&self, theirs: bool, i: usize) -> &str {
        if theirs { &self.theirs[i] } else { &self.ours[i] }
    }

    pub fn result(&self) -> Option<Vec<String>> {
        match self.choice? {
            Choice::Edited => Some(self.edited.as_deref().unwrap_or("").split('\n').map(String::from).collect()),
            Choice::Neither => Some(vec![]),
            Choice::Picked => Some(self.order().into_iter().filter(|&(t, i)| self.picked(t, i)).map(|(t, i)| self.line(t, i).to_string()).collect()),
        }
    }

    fn describe(&self) -> String {
        let (o, t) = (self.pick_ours.iter().filter(|p| **p).count(), self.pick_theirs.iter().filter(|p| **p).count());
        match self.choice {
            None => "unresolved".into(),
            Some(Choice::Neither) => "neither side".into(),
            Some(Choice::Edited) => "edited".into(),
            Some(Choice::Picked) if o == self.ours.len() && t == 0 => "yours".into(),
            Some(Choice::Picked) if t == self.theirs.len() && o == 0 => "incoming".into(),
            Some(Choice::Picked) if o == self.ours.len() && t == self.theirs.len() => {
                if self.theirs_first { "both · incoming first".into() } else { "both · yours first".into() }
            }
            Some(Choice::Picked) => format!("{} line{}", o + t, if o + t == 1 { "" } else { "s" }),
        }
    }
}

pub enum Seg {
    Common(Vec<String>),
    Conflict(Block),
}

pub enum ResolveAction {
    Back,
    Save,
    OpenExternal,
    Reload,
}

/// One row of the file view.
#[derive(Clone, Copy)]
enum Row {
    Common { seg: usize, line: usize, no: usize },
    Header(usize),
    Line { b: usize, theirs: bool, i: usize, no: Option<usize> },
    Edited { b: usize, i: usize, no: usize },
    Editor(usize),
    /// Preview mode: a conflict that still needs a decision.
    Pending(usize),
}

pub struct ConflictDoc {
    pub path: String,
    pub segs: Vec<Seg>,
    pub original: Vec<u8>,
    crlf: bool,
    final_newline: bool,
    lang: Option<&'static Lang>,
    pub preview: bool,
    pub scroll: Scroll,
    pub focus: usize,
    editing: Option<(usize, TextEdit)>,
    scroll_to: Option<usize>,
    spans: Vec<(usize, usize, syntax::Tok)>,
}

fn is_marker(line: &str, c: char) -> bool {
    let n = line.chars().take_while(|&x| x == c).count();
    n == 7 && line[7..].chars().next().is_none_or(|x| x == ' ')
}

impl ConflictDoc {
    /// Returns None when the file has no (well-formed) conflict markers.
    pub fn parse(path: &str, bytes: Vec<u8>) -> Option<ConflictDoc> {
        let text = String::from_utf8(bytes.clone()).ok()?;
        let crlf = text.contains("\r\n");
        let final_newline = text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(&text);
        let mut segs = Vec::new();
        let mut common = Vec::new();
        let mut block: Option<Block> = None;
        let mut state = 0; // 0 out, 1 ours, 2 base, 3 theirs
        for raw in body.split('\n') {
            let line = raw.strip_suffix('\r').unwrap_or(raw).to_string();
            match state {
                0 if is_marker(&line, '<') => {
                    if !common.is_empty() {
                        segs.push(Seg::Common(std::mem::take(&mut common)));
                    }
                    block = Some(Block {
                        ours: vec![],
                        base: None,
                        theirs: vec![],
                        markers: [line, String::new(), String::new(), String::new()],
                        pick_ours: vec![],
                        pick_theirs: vec![],
                        theirs_first: false,
                        choice: None,
                        edited: None,
                        who_ours: vec![],
                        who_theirs: vec![],
                    });
                    state = 1;
                }
                1 if is_marker(&line, '|') => {
                    let b = block.as_mut()?;
                    b.base = Some(vec![]);
                    b.markers[1] = line;
                    state = 2;
                }
                1 | 2 if is_marker(&line, '=') => {
                    block.as_mut()?.markers[2] = line;
                    state = 3;
                }
                3 if is_marker(&line, '>') => {
                    let mut b = block.take()?;
                    b.markers[3] = line;
                    b.pick_ours = vec![false; b.ours.len()];
                    b.pick_theirs = vec![false; b.theirs.len()];
                    segs.push(Seg::Conflict(b));
                    state = 0;
                }
                0 => common.push(line),
                1 => block.as_mut()?.ours.push(line),
                2 => block.as_mut()?.base.as_mut()?.push(line),
                _ => block.as_mut()?.theirs.push(line),
            }
        }
        if state != 0 {
            return None;
        }
        if !common.is_empty() {
            segs.push(Seg::Common(common));
        }
        if !segs.iter().any(|s| matches!(s, Seg::Conflict(_))) {
            return None;
        }
        Some(ConflictDoc {
            path: path.to_string(),
            segs,
            original: bytes,
            crlf,
            final_newline,
            lang: syntax::lang_for(path),
            preview: false,
            scroll: Scroll::default(),
            focus: 0,
            editing: None,
            scroll_to: Some(0),
            spans: vec![],
        })
    }

    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.segs.iter().filter_map(|s| if let Seg::Conflict(b) = s { Some(b) } else { None })
    }

    fn block_mut(&mut self, i: usize) -> Option<&mut Block> {
        self.segs.iter_mut().filter_map(|s| if let Seg::Conflict(b) = s { Some(b) } else { None }).nth(i)
    }

    fn block(&self, i: usize) -> Option<&Block> {
        self.blocks().nth(i)
    }

    pub fn total(&self) -> usize {
        self.blocks().count()
    }

    pub fn resolved(&self) -> usize {
        self.blocks().filter(|b| b.resolved()).count()
    }

    /// Branch / commit name from the first incoming (`>>>>>>>`) marker.
    /// Attributes conflict lines using blames of the two versions being merged. Each side of a
    /// conflict is a verbatim, in-order run of lines from its version, so it's located by content.
    pub fn attribute(&mut self, ours: Option<Blame>, theirs: Option<Blame>) {
        fn locate(hay: &[String], needle: &[String], from: usize) -> Option<usize> {
            let clean = |s: &str| s.trim_end_matches('\r').to_string();
            if needle.is_empty() || hay.len() < needle.len() {
                return None;
            }
            (from..=hay.len() - needle.len()).find(|&i| needle.iter().enumerate().all(|(k, l)| clean(&hay[i + k]) == *l))
        }
        let (mut co, mut ct) = (0, 0);
        for s in self.segs.iter_mut() {
            let Seg::Conflict(b) = s else { continue };
            b.who_ours = vec![None; b.ours.len()];
            b.who_theirs = vec![None; b.theirs.len()];
            for (bl, lines, who, cursor) in [(&ours, &b.ours, &mut b.who_ours, &mut co), (&theirs, &b.theirs, &mut b.who_theirs, &mut ct)] {
                let Some(bl) = bl else { continue };
                if let Some(pos) = locate(&bl.lines, lines, *cursor) {
                    for (k, w) in who.iter_mut().enumerate() {
                        *w = bl.line_commit.get(pos + k).and_then(|&c| bl.commits.get(c as usize)).cloned();
                    }
                    *cursor = pos + lines.len();
                }
            }
        }
    }

    pub fn incoming_label(&self) -> String {
        self.blocks().next().map(|b| b.markers[3].get(7..).unwrap_or("").trim().to_string()).unwrap_or_default()
    }

    /// The file contents with every resolved block replaced by its result.
    pub fn render(&self) -> String {
        let mut out: Vec<String> = Vec::new();
        for s in &self.segs {
            match s {
                Seg::Common(l) => out.extend(l.iter().cloned()),
                Seg::Conflict(b) => match b.result() {
                    Some(r) => out.extend(r),
                    None => {
                        out.push(b.markers[0].clone());
                        out.extend(b.ours.iter().cloned());
                        if let Some(base) = &b.base {
                            out.push(b.markers[1].clone());
                            out.extend(base.iter().cloned());
                        }
                        out.push(b.markers[2].clone());
                        out.extend(b.theirs.iter().cloned());
                        out.push(b.markers[3].clone());
                    }
                },
            }
        }
        let nl = if self.crlf { "\r\n" } else { "\n" };
        let mut s = out.join(nl);
        if self.final_newline {
            s.push_str(nl);
        }
        s
    }

    pub fn take_all(&mut self, theirs: bool) {
        for i in 0..self.total() {
            if let Some(b) = self.block_mut(i) {
                if theirs { b.take_theirs() } else { b.take_ours() }
            }
        }
    }

    pub fn debug_choose(&mut self, choices: &str) {
        for (i, c) in choices.split(',').enumerate() {
            if let Some(b) = self.block_mut(i) {
                match c {
                    "o" => b.take_ours(),
                    "t" => b.take_theirs(),
                    "b" => b.take_both(),
                    "n" => b.take_neither(),
                    _ => {}
                }
            }
        }
        self.preview = choices.split(',').any(|c| c == "v");
    }

    fn goto(&mut self, i: usize) {
        self.focus = i.min(self.total().saturating_sub(1));
        self.scroll_to = Some(self.focus);
    }

    /// After deciding the focused conflict, move on to the next unresolved one.
    fn advance(&mut self) {
        let n = self.total();
        if let Some(next) = (1..=n).map(|k| (self.focus + k) % n).find(|&i| self.block(i).is_some_and(|b| !b.resolved())) {
            self.goto(next);
        }
    }

    fn hunk(&mut self, i: usize, what: char) {
        if let Some(b) = self.block_mut(i) {
            match what {
                'o' => b.take_ours(),
                't' => b.take_theirs(),
                'b' => b.take_both(),
                'n' => b.take_neither(),
                _ => b.reset(),
            }
        }
        self.focus = i;
        if what != 'b' && what != 'r' {
            self.advance();
        }
    }

    fn start_edit(&mut self, i: usize) {
        let Some(b) = self.block(i) else { return };
        let text = match b.result() {
            Some(r) if !r.is_empty() => r.join("\n"),
            _ => b.ours.iter().chain(&b.theirs).cloned().collect::<Vec<_>>().join("\n"),
        };
        let mut ed = TextEdit::default();
        ed.set(&text);
        self.editing = Some((i, ed));
        self.focus = i;
    }

    fn finish_edit(&mut self) {
        if let Some((i, ed)) = self.editing.take() {
            if let Some(b) = self.block_mut(i) {
                b.edited = Some(ed.text);
                b.choice = Some(Choice::Edited);
            }
            self.focus = i;
            self.advance();
        }
    }

    /// Rows of the file view, numbered like the resulting file.
    fn rows(&self) -> Vec<(Row, f32)> {
        let mut rows = Vec::new();
        let mut no = 0usize;
        let mut bi = 0;
        for (si, s) in self.segs.iter().enumerate() {
            match s {
                Seg::Common(lines) => {
                    for l in 0..lines.len() {
                        no += 1;
                        rows.push((Row::Common { seg: si, line: l, no }, LINE_H));
                    }
                }
                Seg::Conflict(b) => {
                    let editing = self.editing.as_ref().is_some_and(|e| e.0 == bi);
                    if self.preview && !editing {
                        match b.choice {
                            None => rows.push((Row::Pending(bi), LINE_H + 4.0)),
                            Some(Choice::Edited) => {
                                for i in 0..b.result().map_or(0, |r| r.len()) {
                                    no += 1;
                                    rows.push((Row::Edited { b: bi, i, no }, LINE_H));
                                }
                            }
                            _ => {
                                for (t, i) in b.order() {
                                    if b.picked(t, i) {
                                        no += 1;
                                        rows.push((Row::Line { b: bi, theirs: t, i, no: Some(no) }, LINE_H));
                                    }
                                }
                            }
                        }
                    } else {
                        rows.push((Row::Header(bi), HEADER_H));
                        if editing {
                            let n = self.editing.as_ref().map_or(1, |e| e.1.text.split('\n').count());
                            rows.push((Row::Editor(bi), n.max(3) as f32 * 18.0 + 60.0));
                        } else if b.choice == Some(Choice::Edited) {
                            for i in 0..b.result().map_or(0, |r| r.len()) {
                                no += 1;
                                rows.push((Row::Edited { b: bi, i, no }, LINE_H));
                            }
                        } else {
                            for (t, i) in b.order() {
                                let n = if b.picked(t, i) {
                                    no += 1;
                                    Some(no)
                                } else {
                                    None
                                };
                                rows.push((Row::Line { b: bi, theirs: t, i, no: n }, LINE_H));
                            }
                        }
                    }
                    bi += 1;
                }
            }
        }
        rows
    }

    /// Syntax-colored code; `dim` renders dropped lines.
    fn code(&mut self, ui: &mut Ui, x: f32, cy: f32, text: &str, dim: bool) -> f32 {
        let text = text.replace('\t', "    ");
        let mut spans = std::mem::take(&mut self.spans);
        syntax::highlight(self.lang, &text, &mut spans);
        let mut xx = x;
        for &(s, e, t) in &spans {
            let c = if dim { theme::FAINT.alpha(0.8) } else { tok_color(t) };
            xx += ui.p.text(xx, cy, MONO, 12.0, &text[s..e], c);
        }
        self.spans = spans;
        xx - x
    }

    /// Draws the resolver into `r`. `labels` are (ours, theirs) human names for the two sides.
    pub fn draw(&mut self, ui: &mut Ui, r: Rect, labels: (&str, &str), op: &str) -> Option<ResolveAction> {
        let mut action = None;
        let total = self.total();
        self.focus = self.focus.min(total.saturating_sub(1));

        // ---- keyboard -------------------------------------------------------------------
        if let Some((bi, mut ed)) = self.editing.take() {
            let input = &mut ui.input;
            if input.take_key(Key::Escape, false).is_some() {
                // cancelled
            } else if input.take_key(Key::Enter, true).is_some() {
                self.editing = Some((bi, ed));
                self.finish_edit();
            } else {
                ed.handle(&mut ui.input, true);
                self.editing = Some((bi, ed));
            }
        } else {
            let input = &mut ui.input;
            let f = self.focus;
            if input.take_key(Key::Char('1'), false).is_some() {
                self.hunk(f, 'o');
            } else if input.take_key(Key::Char('2'), false).is_some() {
                self.hunk(f, 't');
            } else if input.take_key(Key::Char('3'), false).is_some() {
                self.hunk(f, 'b');
            } else if input.take_key(Key::Char('0'), false).is_some() {
                self.hunk(f, 'n');
            } else if input.take_key(Key::Char('e'), false).is_some() {
                self.start_edit(f);
            } else if input.take_key(Key::Char('v'), false).is_some() {
                self.preview = !self.preview;
            } else if input.take_key(Key::Char('n'), false).is_some() || input.take_key(Key::Down, false).is_some() {
                self.goto(f + 1);
            } else if input.take_key(Key::Char('p'), false).is_some() || input.take_key(Key::Up, false).is_some() {
                self.goto(f.saturating_sub(1));
            } else if input.take_key(Key::Char('s'), true).is_some() && self.resolved() == total {
                action = Some(ResolveAction::Save);
            } else if input.take_key(Key::Escape, false).is_some() {
                action = Some(ResolveAction::Back);
            }
        }
        let done = self.resolved();

        // ---- header ---------------------------------------------------------------------
        let mut area = r;
        let head = area.take_top(48.0);
        ui.p.rect(head, theme::PANEL);
        let cy = head.cy();
        if ui.button(Rect::new(head.x + 12.0, cy - 13.0, 64.0, 26.0), "← Back", false, true) {
            action = Some(ResolveAction::Back);
        }
        let mut x = head.x + 88.0;
        x += ui.pill(x, cy, op, theme::RED, theme::RED.alpha(0.14), false) + 10.0;
        ui.p.text_fit(x, cy, head.right() - x - 230.0, UI, 14.0, &self.path, theme::TEXT);
        if ui.button(Rect::new(head.right() - 82.0, cy - 13.0, 70.0, 26.0), "Reload", false, true) {
            action = Some(ResolveAction::Reload);
        }
        if ui.button(Rect::new(head.right() - 206.0, cy - 13.0, 116.0, 26.0), "Open in editor", false, true) {
            action = Some(ResolveAction::OpenExternal);
        }

        // Progress, legend, bulk actions, preview toggle.
        let bar = area.take_top(44.0);
        ui.p.rect(bar, theme::PANEL);
        ui.p.rect(Rect::new(bar.x, bar.bottom() - 1.0, bar.w, 1.0), theme::BORDER);
        let cy = bar.cy();
        let label = format!("{done} of {total} conflict{} resolved", if total == 1 { "" } else { "s" });
        let lw = ui.p.text(bar.x + 16.0, cy - 7.0, UI, 12.5, &label, if done == total { EDITED } else { theme::TEXT });
        let track = Rect::new(bar.x + 16.0, cy + 7.0, lw.max(120.0), 4.0);
        ui.p.rounded(track, 2.0, theme::RAISED);
        if total > 0 {
            ui.p.rounded(Rect::new(track.x, track.y, track.w * done as f32 / total as f32, track.h), 2.0, EDITED);
        }
        let mut lx = track.right() + 24.0;
        for (name, color) in [(labels.0, OURS), (labels.1, THEIRS)] {
            ui.p.rounded(Rect::new(lx, cy - 7.0, 3.0, 14.0), 1.5, color);
            lx += ui.p.text_fit(lx + 9.0, cy, 240.0, UI, 12.0, name, theme::DIM) + 26.0;
        }
        let mut rx = bar.right() - 12.0;
        for (l, theirs) in [("All incoming", true), ("All yours", false)] {
            let w = ui.p.measure(UI, 12.5, l) + 20.0;
            rx -= w;
            if ui.button(Rect::new(rx, cy - 12.0, w, 24.0), l, false, true) {
                self.take_all(theirs);
            }
            rx -= 6.0;
        }
        let pw = ui.p.measure(UI, 11.5, "Preview result") + 30.0;
        rx -= pw + 6.0;
        if rx > lx {
            let (_, t) = ui.toggle_chip(rx, cy, "Preview result", self.preview);
            if t {
                self.preview = !self.preview;
            }
        }

        // Footer: hints + save.
        let foot = Rect::new(area.x, area.bottom() - 52.0, area.w, 52.0);
        area.h -= 52.0;
        ui.p.rect(foot, theme::PANEL);
        ui.p.rect(Rect::new(foot.x, foot.y, foot.w, 1.0), theme::BORDER);
        let hint = if self.editing.is_some() {
            "⌘↵ apply edit  ·  esc cancel"
        } else {
            "click lines to keep / drop them  ·  1 yours  ·  2 incoming  ·  3 both  ·  0 neither  ·  e edit  ·  n / p next / previous  ·  v preview"
        };
        ui.p.text_fit(foot.x + 16.0, foot.cy(), foot.w - 260.0, UI, 11.5, hint, theme::FAINT);
        let all = done == total;
        let save_label = if all { "Save & mark resolved" } else { "Resolve every conflict to save" };
        let sw = ui.p.measure(UI, 12.5, save_label) + 32.0;
        if ui.button(Rect::new(foot.right() - sw - 12.0, foot.cy() - 15.0, sw, 30.0), save_label, true, all) {
            action = Some(ResolveAction::Save);
        }

        // ---- the file -------------------------------------------------------------------
        ui.p.rect(area, theme::BG);
        let rows = self.rows();
        let mut ys = Vec::with_capacity(rows.len());
        let mut total_h = 8.0;
        for (_, h) in &rows {
            ys.push(total_h);
            total_h += h;
        }
        if let Some(target) = self.scroll_to.take() {
            let k = rows.iter().position(|(row, _)| match row {
                Row::Header(b) | Row::Pending(b) | Row::Editor(b) => *b == target,
                Row::Line { b, .. } | Row::Edited { b, .. } => *b == target,
                _ => false,
            });
            if let Some(k) = k {
                self.scroll.jump((ys[k] - area.h * 0.3).max(0.0));
            }
        }
        let off = self.scroll.update(ui, area, total_h + 40.0);
        let gutter = 52.0;
        let check_x = area.x + gutter + 10.0;
        let text_x = area.x + gutter + 30.0;
        let mut toggle: Option<(usize, bool, usize)> = None;
        let mut hunk: Option<(usize, char)> = None;
        let mut focus_click: Option<usize> = None;
        let mut editor_cmd: Option<bool> = None;
        let mut tooltip: Option<BlameCommit> = None;

        ui.p.push_clip(area);
        for (k, (row, h)) in rows.iter().enumerate() {
            let y = area.y + ys[k] - off;
            if y + h < area.y || y > area.bottom() {
                continue;
            }
            let rr = Rect::new(area.x, y, area.w, *h);
            let cy = rr.cy();
            let num = |ui: &mut Ui, n: usize| {
                let s = n.to_string();
                let w = ui.p.measure(MONO, 11.0, &s);
                ui.p.text(area.x + gutter - 8.0 - w, cy, MONO, 11.0, &s, theme::FAINT);
            };
            match *row {
                Row::Common { seg, line, no } => {
                    num(ui, no);
                    if let Seg::Common(lines) = &self.segs[seg] {
                        let t = lines[line].clone();
                        self.code(ui, text_x, cy, &t, false);
                    }
                }
                Row::Header(bi) => {
                    let Some(b) = self.block(bi) else { continue };
                    let focused = bi == self.focus;
                    let resolved = b.resolved();
                    let accent = if resolved { EDITED } else { THEIRS };
                    ui.p.rect(rr, accent.alpha(if focused { 0.12 } else { 0.07 }));
                    ui.p.rect(Rect::new(area.x, y, area.w, 1.0), accent.alpha(0.5));
                    ui.p.rect(Rect::new(area.x, y, 3.0, *h), accent);
                    let mut x = area.x + 16.0;
                    x += ui.p.text(x, cy, UI, 12.0, &format!("Conflict {} of {total}", bi + 1), theme::TEXT) + 10.0;
                    let status = if resolved { format!("✓ {}", b.describe()) } else { "choose what to keep".into() };
                    ui.pill(x, cy, &status, accent, accent.alpha(0.16), false);
                    // Hunk buttons, right-aligned.
                    let editing = self.editing.as_ref().is_some_and(|e| e.0 == bi);
                    if !editing {
                        let mut bx = area.right() - 12.0;
                        let both_label = if b.theirs_first { "Both ↑" } else { "Both" };
                        let all_o = b.choice == Some(Choice::Picked) && b.pick_ours.iter().all(|p| *p) && !b.pick_theirs.iter().any(|p| *p);
                        let all_t = b.choice == Some(Choice::Picked) && b.pick_theirs.iter().all(|p| *p) && !b.pick_ours.iter().any(|p| *p);
                        let both = b.choice == Some(Choice::Picked) && b.pick_ours.iter().all(|p| *p) && b.pick_theirs.iter().all(|p| *p);
                        let buttons: [(&str, char, Option<Color>, bool); 6] = [
                            ("Reset", 'r', None, false),
                            ("Edit", 'e', None, b.choice == Some(Choice::Edited)),
                            ("Neither", 'n', None, b.choice == Some(Choice::Neither)),
                            (both_label, 'b', None, both),
                            ("Incoming", 't', Some(THEIRS), all_t),
                            ("Yours", 'o', Some(OURS), all_o),
                        ];
                        for (label, what, dot, active) in buttons {
                            if what == 'r' && !resolved {
                                continue;
                            }
                            let w = ui.p.measure(UI, 12.0, label) + if dot.is_some() { 30.0 } else { 18.0 };
                            bx -= w;
                            let br = Rect::new(bx, y + 4.0, w, h - 8.0);
                            bx -= 5.0;
                            if ui.button(br, if dot.is_some() { "" } else { label }, active, true) {
                                hunk = Some((bi, what));
                            }
                            if let Some(c) = dot {
                                ui.p.circle((br.x + 12.0, br.cy()), 3.5, if active { theme::BG } else { c });
                                ui.p.text(br.x + 21.0, br.cy(), UI, 12.5, label, if active { theme::BG } else { theme::TEXT });
                            }
                        }
                    }
                    if ui.hovered(rr) && ui.input.pressed {
                        focus_click = Some(bi);
                    }
                }
                Row::Line { b: bi, theirs, i, no } => {
                    let Some(b) = self.block(bi) else { continue };
                    let color = if theirs { THEIRS } else { OURS };
                    let undecided = !b.resolved();
                    let kept = no.is_some();
                    let side_first = b.order().iter().position(|&(t, _)| t == theirs) == b.order().iter().position(|&p| p == (theirs, i));
                    let whos = if theirs { &b.who_theirs } else { &b.who_ours };
                    let who = whos.get(i).cloned().flatten();
                    let run_start = side_first || whos.get(i.wrapping_sub(1)).cloned().flatten().map(|c| c.hash) != who.as_ref().map(|c| c.hash.clone());
                    let text = b.line(theirs, i).to_string();
                    let hov = ui.hovered(rr);
                    let alpha = if kept { 0.13 } else if undecided { 0.07 } else { 0.03 };
                    ui.p.rect(rr, color.alpha(if hov { alpha + 0.06 } else { alpha }));
                    ui.p.rect(Rect::new(area.x, y, 3.0, *h), color.alpha(if kept || undecided { 1.0 } else { 0.35 }));
                    if let Some(n) = no {
                        num(ui, n);
                    }
                    if !self.preview {
                        // Keep/drop checkbox.
                        let cb = (check_x, cy);
                        if kept {
                            ui.p.circle(cb, 6.0, color);
                            ui.p.line((cb.0 - 2.7, cb.1), (cb.0 - 0.6, cb.1 + 2.2), 1.5, theme::BG);
                            ui.p.line((cb.0 - 0.6, cb.1 + 2.2), (cb.0 + 3.0, cb.1 - 2.4), 1.5, theme::BG);
                        } else {
                            ui.p.ring(cb, 6.0, 1.3, if hov { color } else { color.alpha(0.6) });
                        }
                    }
                    let dropped = !kept && !undecided;
                    let w = self.code(ui, text_x, cy, &text, dropped);
                    if dropped && w > 0.0 {
                        ui.p.rect(Rect::new(text_x, cy, w, 1.2), theme::FAINT.alpha(0.9));
                    }
                    // Right edge: side tag on the first line of each side, and who / when / which
                    // commit on the first line of every run of lines from the same commit.
                    if !self.preview && (side_first || (run_start && who.is_some())) {
                        let mut rx = area.right() - 14.0;
                        let mut label_rect: Option<Rect> = None;
                        if let Some(c) = &who {
                            let text = if attr::uncommitted(c) {
                                "uncommitted".to_string()
                            } else {
                                format!("{}  ·  {}  ·  {}", c.author, git::short_age(c.time), &c.hash[..7.min(c.hash.len())])
                            };
                            let tw = ui.p.measure(UI, 10.5, &text).min(area.w * 0.35);
                            rx -= tw;
                            ui.p.text_fit(rx, cy, tw, UI, 10.5, &text, theme::DIM);
                            let dot = if attr::uncommitted(c) { theme::YELLOW } else { attr::age_color(c.time) };
                            ui.p.circle((rx - 9.0, cy), 3.0, dot);
                            label_rect = Some(Rect::new(rx - 16.0, y, tw + 18.0, *h));
                            rx -= 22.0;
                        }
                        if side_first {
                            let tag = if theirs { "incoming" } else { "yours" };
                            let tw = ui.p.measure(UI, 10.5, tag);
                            ui.p.text(rx - tw, cy, UI, 10.5, tag, color.alpha(0.85));
                        }
                        if let (Some(lr), Some(c)) = (label_rect, &who) {
                            if ui.hovered(lr) {
                                tooltip = Some(c.clone());
                            }
                        }
                    }
                    if hov {
                        ui.cursor = Cursor::Pointer;
                        if ui.input.pressed {
                            if self.preview {
                                focus_click = Some(bi);
                            } else {
                                toggle = Some((bi, theirs, i));
                            }
                        }
                    }
                }
                Row::Edited { b: bi, i, no } => {
                    let line = self.block(bi).and_then(|b| b.result()).and_then(|r| r.get(i).cloned()).unwrap_or_default();
                    ui.p.rect(rr, EDITED.alpha(0.10));
                    ui.p.rect(Rect::new(area.x, y, 3.0, *h), EDITED);
                    num(ui, no);
                    self.code(ui, text_x, cy, &line, false);
                    if ui.hovered(rr) && ui.input.pressed {
                        focus_click = Some(bi);
                    }
                }
                Row::Pending(bi) => {
                    let pr = Rect::new(area.x + 8.0, y + 2.0, area.w - 16.0, h - 4.0);
                    let hov = ui.hovered(pr);
                    ui.p.rounded(pr, 5.0, THEIRS.alpha(if hov { 0.2 } else { 0.12 }));
                    let msg = format!("Conflict {} not decided yet — would be saved with conflict markers (click to choose)", bi + 1);
                    ui.p.text_fit(pr.x + 12.0, pr.cy(), pr.w - 24.0, UI, 12.0, &msg, THEIRS);
                    if hov {
                        ui.cursor = Cursor::Pointer;
                        if ui.input.pressed {
                            focus_click = Some(bi);
                            self.preview = false;
                        }
                    }
                }
                Row::Editor(_) => {
                    let er = Rect::new(text_x - 8.0, y + 6.0, area.right() - text_x - 4.0, h - 50.0);
                    if let Some((_, ed)) = &self.editing {
                        ed.draw(ui, er, "Write what this part of the file should contain…", true, true);
                    }
                    let by = er.bottom() + 8.0;
                    if ui.button(Rect::new(er.right() - 120.0, by, 120.0, 28.0), "Apply  ⌘↵", true, true) {
                        editor_cmd = Some(true);
                    }
                    if ui.button(Rect::new(er.right() - 210.0, by, 82.0, 28.0), "Cancel", false, true) {
                        editor_cmd = Some(false);
                    }
                }
            }
        }
        ui.p.pop_clip();

        if let Some(c) = tooltip {
            attr::tooltip(ui, ui.input.mouse, &c);
        }

        // ---- apply interactions ---------------------------------------------------------
        if let Some((bi, theirs, i)) = toggle {
            if let Some(b) = self.block_mut(bi) {
                b.toggle(theirs, i);
            }
            self.focus = bi;
        }
        if let Some((bi, what)) = hunk {
            if what == 'e' {
                self.start_edit(bi);
            } else {
                self.hunk(bi, what);
            }
        }
        if let Some(bi) = focus_click {
            self.goto(bi);
        }
        match editor_cmd {
            Some(true) => self.finish_edit(),
            Some(false) => self.editing = None,
            None => {}
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "fn a() {}\n<<<<<<< HEAD\nlet x = 1;\nlet y = 2;\n||||||| base\nlet x = 0;\n=======\nlet x = 3;\n>>>>>>> feature\nfn b() {}\n";

    fn doc() -> ConflictDoc {
        ConflictDoc::parse("a.rs", SAMPLE.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn roundtrips_unresolved() {
        let d = doc();
        assert_eq!(d.total(), 1);
        assert_eq!(d.resolved(), 0);
        assert_eq!(d.render(), SAMPLE);
    }

    #[test]
    fn hunk_choices_render() {
        let mut d = doc();
        d.take_all(true);
        assert_eq!(d.render(), "fn a() {}\nlet x = 3;\nfn b() {}\n");
        d.hunk(0, 'b');
        assert_eq!(d.render(), "fn a() {}\nlet x = 1;\nlet y = 2;\nlet x = 3;\nfn b() {}\n");
        d.hunk(0, 'b'); // again: flips the order
        assert_eq!(d.render(), "fn a() {}\nlet x = 3;\nlet x = 1;\nlet y = 2;\nfn b() {}\n");
        d.hunk(0, 'n');
        assert_eq!(d.render(), "fn a() {}\nfn b() {}\n");
        assert_eq!(d.resolved(), 1);
    }

    #[test]
    fn picking_lines_is_the_result_and_unpicking_all_is_unresolved() {
        let mut d = doc();
        let b = d.block_mut(0).unwrap();
        b.toggle(false, 1); // "let y = 2;"
        b.toggle(true, 0); // "let x = 3;"
        assert_eq!(d.render(), "fn a() {}\nlet y = 2;\nlet x = 3;\nfn b() {}\n");
        // Display rows number exactly the kept lines, in result order.
        let numbered: Vec<usize> = d.rows().iter().filter_map(|(r, _)| match r {
            Row::Common { no, .. } => Some(*no),
            Row::Line { no: Some(n), .. } => Some(*n),
            _ => None,
        }).collect();
        assert_eq!(numbered, [1, 2, 3, 4]);
        let b = d.block_mut(0).unwrap();
        b.toggle(false, 1);
        b.toggle(true, 0);
        assert_eq!(d.resolved(), 0, "unchecking everything must not count as resolved");
        assert_eq!(d.render(), SAMPLE);
    }

    #[test]
    fn preview_rows_match_the_saved_file() {
        let mut d = doc();
        d.preview = true;
        d.hunk(0, 't');
        let text: Vec<String> = d
            .rows()
            .iter()
            .filter_map(|(r, _)| match *r {
                Row::Common { seg, line, .. } => match &d.segs[seg] {
                    Seg::Common(l) => Some(l[line].clone()),
                    _ => None,
                },
                Row::Line { b, theirs, i, .. } => Some(d.block(b).unwrap().line(theirs, i).to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(text.join("\n") + "\n", d.render());
    }

    #[test]
    fn attributes_conflict_lines_by_content() {
        let mut d = doc();
        let mk = |hash: &str| BlameCommit { hash: hash.repeat(40)[..40].to_string(), author: hash.into(), time: 0, summary: String::new() };
        // "HEAD" version of the file: line 2-3 are the ours block (written by "b").
        let ours = Blame {
            commits: vec![mk("a"), mk("b")],
            line_commit: vec![0, 1, 1, 0],
            lines: ["fn a() {}", "let x = 1;", "let y = 2;", "fn b() {}"].map(String::from).to_vec(),
        };
        let theirs = Blame {
            commits: vec![mk("a"), mk("c")],
            line_commit: vec![0, 1, 0],
            lines: ["fn a() {}", "let x = 3;", "fn b() {}"].map(String::from).to_vec(),
        };
        d.attribute(Some(ours), Some(theirs));
        let b = d.block(0).unwrap();
        assert_eq!(b.who_ours.iter().map(|w| w.as_ref().unwrap().author.as_str()).collect::<Vec<_>>(), ["b", "b"]);
        assert_eq!(b.who_theirs[0].as_ref().unwrap().author, "c");
    }

    #[test]
    fn keeps_crlf_and_rejects_clean_files() {
        let crlf = SAMPLE.replace('\n', "\r\n");
        let mut d = ConflictDoc::parse("a.rs", crlf.clone().into_bytes()).unwrap();
        assert_eq!(d.render(), crlf);
        d.take_all(false);
        assert_eq!(d.render(), "fn a() {}\r\nlet x = 1;\r\nlet y = 2;\r\nfn b() {}\r\n");
        assert!(ConflictDoc::parse("a.rs", b"no conflicts\n".to_vec()).is_none());
        assert!(ConflictDoc::parse("a.md", b"title\n========\n".to_vec()).is_none());
    }
}
