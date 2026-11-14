//! lazygit-style keyboard control: which pane has focus, what each command of the keymap
//! (`keys.rs`) does there, keyboard-driven menus, and the ring that shows the focused pane.

use super::*;
use crate::ui::{bind, Binding};

/// Panes left to right, as laid out (wide mode hides the first two).
const ORDER: [Pane; 5] = [Pane::Branches, Pane::Commits, Pane::Files, Pane::Stash, Pane::Main];

impl GitApp {
    /// The focused pane's keys (keys.rs), offered in order: the pane's own, then the global
    /// ones it doesn't take over. What `applies` says doesn't work right now isn't offered.
    /// After a key runs, what's on offer is worked out again (another pane, a menu…), so the
    /// key bar always shows what the next key will do.
    pub(super) fn pane_keys(&mut self, ui: &mut Ui) {
        let mark = ui.keymap.now.len();
        'offer: loop {
            ui.keymap.now.truncate(mark);
            self.sync_changes_tree();
            for b in keys::offers(self.pane) {
                if !ui.key(bind(b.keys, b.help).bar(b.bar).when(self.applies(b.cmd))) {
                    continue;
                }
                self.kb = true;
                match b.cmd {
                    Cmd::Help => self.help_menu(&ui.keymap.last),
                    cmd => self.run_cmd(cmd),
                }
                // A menu, the palette or the commit message took over: theirs are the keys now.
                if self.menu.is_some() || self.focus != Focus::None {
                    ui.keymap.now.truncate(mark);
                    match (self.menu.is_some(), self.focus) {
                        (true, _) => self.menu_keys(ui),
                        (_, Focus::Palette) => self.palette_keys(ui),
                        _ => self.message_keys(ui),
                    }
                    return;
                }
                continue 'offer;
            }
            return;
        }
    }

    /// lazygit's commit message box: enter on the summary line commits, tab goes to the
    /// description and back. Everything else is the text field's (shift / ⌥-enter: a new line).
    pub(super) fn message_keys(&mut self, ui: &mut Ui) {
        let on_summary = !self.msg.text[..self.msg.cursor.min(self.msg.text.len())].contains('\n');
        let enter = ui.key(bind(&["<enter>"], "Commit").bar("commit").when(on_summary));
        let command_enter = ui.key(bind(&["<m-enter>"], "Commit").bar("commit"));
        let tab = match on_summary {
            true => ui.key(bind(&["<tab>"], "Write a description").bar("description")),
            false => ui.key(bind(&["<tab>"], "Back to the summary").bar("summary")),
        };
        let leave = ui.key(bind(&["<esc>"], "Leave the message (it stays)").bar("leave"));
        ui.close_keys(false);
        if leave {
            self.focus = Focus::None;
        } else if enter || command_enter {
            self.commit();
        } else if tab {
            let t = &mut self.msg;
            if on_summary {
                if !t.text.contains('\n') {
                    t.text.push_str("\n\n");
                }
                // The description starts after the blank line that follows the summary.
                let first = t.text.find('\n').unwrap_or(t.text.len());
                let body = t.text[first..].find(|c| c != '\n').map_or(t.text.len(), |i| first + i);
                t.cursor = if t.text[first..].starts_with("\n\n") { body.max(first + 2) } else { body };
            } else {
                t.cursor = t.text.find('\n').unwrap_or(t.text.len());
            }
        }
    }

    /// The palette's keys; typing goes to its text field.
    pub(super) fn palette_keys(&mut self, ui: &mut Ui) {
        let down = ui.presses(bind(&["<down>"], "Next").bar("move"));
        let up = ui.presses(bind(&["<up>"], "Previous").bar("move"));
        let run = ui.key(bind(&["<enter>"], "Run it").bar("run"));
        let close = ui.key(bind(&["<esc>"], "Close").bar("close"));
        ui.close_keys(false);
        if close {
            self.focus = Focus::None;
            return;
        }
        let n = self.palette.items.len();
        self.palette.sel = (self.palette.sel + down).saturating_sub(up).min(n.saturating_sub(1));
        if let Some(item) = self.palette.items.get(self.palette.sel).filter(|_| run) {
            let a = item.act.clone();
            self.focus = Focus::None;
            self.act(a);
        }
    }

    /// A menu's keys: moving, picking, closing, and an item's own key (`s` / `m` / `h` in the
    /// reset menu, every key in the `?` list). Nothing else gets a key while a menu is open.
    pub(super) fn menu_keys(&mut self, ui: &mut Ui) {
        let next = ui.presses(bind(&["j", "<down>", "<tab>"], "Next item").bar("move"));
        let prev = ui.presses(bind(&["k", "<up>", "<s-tab>"], "Previous item").bar("move"));
        let pick = ui.key(bind(&["<enter>"], "Pick it").bar("pick"));
        let close = ui.key(bind(&["<esc>"], "Close").bar("close"));
        let own: Vec<(usize, &'static [&'static str])> = self.menu.iter().flat_map(|m| m.items.iter().enumerate().filter(|(_, it)| it.act.is_some() && !it.keys.is_empty()).map(|(i, it)| (i, it.keys))).collect();
        let hot = own.into_iter().find(|&(_, keys)| ui.key(bind(keys, ""))).map(|(i, _)| i);
        ui.close_keys(true);
        let Some(m) = &mut self.menu else { return };
        let live: Vec<usize> = (0..m.items.len()).filter(|&i| m.items[i].act.is_some()).collect();
        if close || live.is_empty() {
            self.menu = None;
            return;
        }
        for _ in 0..next {
            let at = m.sel.and_then(|s| live.iter().position(|&i| i == s));
            m.sel = Some(live[at.map_or(0, |p| (p + 1) % live.len())]);
        }
        for _ in 0..prev {
            let at = m.sel.and_then(|s| live.iter().position(|&i| i == s));
            m.sel = Some(live[at.map_or(live.len() - 1, |p| (p + live.len() - 1) % live.len())]);
        }
        m.reveal_sel();
        if let Some(i) = hot.or(m.sel.filter(|_| pick)) {
            let a = m.items[i].act.clone();
            self.menu = None;
            if let Some(a) = a {
                self.act(a);
            }
        }
    }

    /// Opens a menu from the keyboard: next to the focused pane, first item highlighted.
    pub(super) fn kb_menu(&mut self, title: String, note: Option<(String, Color)>, items: Vec<MenuItem>) {
        if items.iter().all(|i| i.act.is_none()) {
            return;
        }
        let r = self.pane_rect(self.pane).unwrap_or_default();
        let at = (r.x + (r.w * 0.2).min(120.0), r.y + 48.0);
        self.menu = Some(Menu { fresh: false, ..Menu::new(at, title, note, items) });
        self.kb_select_first();
    }

    /// Highlights the first item of the open menu (menus opened by keys start there).
    fn kb_select_first(&mut self) {
        if let Some(m) = &mut self.menu {
            m.sel = m.items.iter().position(|i| i.act.is_some());
        }
    }

    pub(super) fn pane_rect(&self, p: Pane) -> Option<Rect> {
        self.pane_rects.iter().find(|(q, _)| *q == p).map(|(_, r)| *r)
    }

    /// A thin ring around the focused pane while the keyboard is driving.
    pub(super) fn draw_focus(&self, ui: &mut Ui) {
        if !self.kb || self.focus != Focus::None {
            return;
        }
        if let Some(r) = self.pane_rect(self.pane) {
            ui.p.rounded_stroke(r.shrink(1.5), 6.0, 1.5, theme::ACCENT.alpha(0.7));
        }
    }

    /// A click focuses the pane under it (without the ring: that's for the keyboard).
    pub(super) fn click_focus(&mut self, ui: &Ui) {
        if !(ui.input.pressed || ui.input.right_pressed) || ui.blocked || self.menu.is_some() || self.focus == Focus::Palette {
            return;
        }
        if let Some(&(p, _)) = self.pane_rects.iter().rev().find(|(_, r)| r.contains(ui.input.mouse)) {
            if p != Pane::Main && self.pane == Pane::Main {
                self.diff.cursor_clear();
            }
            self.pane = p;
            self.kb = false;
        }
    }

    // ---- the key bar ------------------------------------------------------------------------

    /// What the key bar shows, straight from the bindings offered this frame (so it's what
    /// works right now, wherever the keys are: a pane, a menu, the palette, the commit message,
    /// the resolver): (keys, label, the key a click presses). Neighbours with one label share an
    /// entry ("h l  hunks"). And whether `?` (every key) is on offer.
    pub(super) fn key_bar_entries(offered: &[Binding]) -> (Vec<(String, &'static str, Option<&'static str>)>, bool) {
        let reachable = crate::ui::reachable(offered);
        let mut out: Vec<(Vec<&'static str>, &'static str)> = Vec::new();
        for b in reachable.iter().filter(|b| !b.bar.is_empty()) {
            match out.last_mut() {
                Some(last) if last.1 == b.bar => last.0.push(b.keys[0]),
                _ => out.push((vec![b.keys[0]], b.bar)),
            }
        }
        let help = reachable.iter().any(|b| b.keys.contains(&"?"));
        let entries = out.into_iter().map(|(keys, label)| (keys::show_keys(&keys), label, (keys.len() == 1).then(|| keys[0]))).collect();
        (entries, help)
    }

    /// Whether a pane's key does something right now: it's only offered (and so only listed
    /// and only run) when it does. Cheap, as it's asked every frame.
    fn applies(&self, cmd: Cmd) -> bool {
        let wip = self.showing_wip();
        let commit = self.sel_row().is_some();
        match (self.pane, cmd) {
            // The file lists: what the row under the cursor is (a folder, a conflict, a file).
            (Pane::Files | Pane::Stash, Cmd::ToggleFolder | Cmd::FolderRight) => self.tree_at().is_some_and(|(_, p)| p.ends_with('/')),
            (Pane::Files | Pane::Stash, Cmd::FolderLeft) => self.folder_left().is_some(),
            (Pane::Files, Cmd::Resolve) => self.tree_at().is_some_and(|(id, _)| id == TreeId::Conflicts),
            (Pane::Files | Pane::Stash, Cmd::Enter) => self.tree_at().is_some_and(|(id, p)| !p.ends_with('/') && id != TreeId::Conflicts),
            (Pane::Files, Cmd::ToggleStaged) => wip && self.tree_at().is_some(),
            // Discarding a folder is for the unstaged list's.
            (Pane::Files, Cmd::DiscardMenu) => wip && self.tree_at().is_some_and(|(id, p)| !(p.ends_with('/') && id == TreeId::Staged)),
            (Pane::Files, Cmd::ToggleAll | Cmd::Stash | Cmd::StashMenu) => wip && !self.changes.is_empty(),
            (Pane::Files, Cmd::Amend | Cmd::ResetMenu) => wip && !self.changes.is_empty(),
            (Pane::Files, Cmd::OpenFile | Cmd::CopyMenu | Cmd::CopyPath) => self.cursor_file().is_some(),
            (Pane::Main, Cmd::SelectLine | Cmd::SelectHunk | Cmd::Stage | Cmd::Discard | Cmd::RangeDown | Cmd::RangeUp) => self.diff.selectable(),
            (Pane::Main, Cmd::SwitchSide) => wip && self.diff.mode != DiffMode::Untracked,
            (Pane::Commits, Cmd::PasteCommits | Cmd::ClearCopied) => !self.copied.is_empty(),
            (Pane::Commits, Cmd::Enter) => true,
            (Pane::Commits, Cmd::Checkout) => commit && self.sel_row() != self.head_row,
            (Pane::Commits, Cmd::NewBranch | Cmd::NewTag | Cmd::ResetMenu | Cmd::Revert | Cmd::CopyCommit | Cmd::CopyMenu | Cmd::Amend) => commit,
            (Pane::Commits, Cmd::MoveDown | Cmd::MoveUp | Cmd::Squash | Cmd::Fixup | Cmd::Drop | Cmd::Reword) => commit,
            // The sidebar: the working copy and worktree rows take enter / space (and d, a
            // worktree); everything else wants a branch, remote branch or tag.
            (Pane::Branches, Cmd::CheckoutPrev | Cmd::CheckoutByName) => true,
            (Pane::Branches, Cmd::Enter) => self.side_cursor.is_some(),
            (Pane::Branches, Cmd::Checkout) => match &self.side_cursor {
                Some(SideKey::Worktree(_)) => true,
                Some(SideKey::Ref(_)) => self.cursor_ref().is_some_and(|r| !r.is_head),
                _ => false,
            },
            (Pane::Branches, Cmd::Delete) => matches!(self.side_cursor, Some(SideKey::Worktree(_))) || self.cursor_ref().is_some_and(|r| !r.is_head),
            (Pane::Branches, Cmd::Merge | Cmd::Rebase) => self.cursor_ref().is_some_and(|r| !r.is_head),
            (Pane::Branches, Cmd::FastForward | Cmd::Upstream | Cmd::Rename) => self.cursor_ref().is_some_and(|r| r.kind == RefKind::Local),
            (Pane::Branches, Cmd::NewBranch | Cmd::NewTag | Cmd::ResetMenu | Cmd::CopyMenu) => self.cursor_ref().is_some(),
            (_, Cmd::StashList) => !self.stashes.is_empty(),
            (_, Cmd::OpMenu) => self.op.is_some(),
            _ => true,
        }
    }

    /// The keys for what has focus, along the bottom: exactly the bindings on offer this
    /// frame. Clicking an entry with one key presses it; `?` lists them all; × hides the bar
    /// (a setting).
    pub(super) fn draw_key_bar(&mut self, ui: &mut Ui, r: Rect) {
        ui.p.rect(r, theme::PANEL2);
        ui.p.rect(Rect::new(r.x, r.y, r.w, 1.0), theme::BORDER);
        let (entries, more) = Self::key_bar_entries(&ui.keymap.now);
        let mut press = None;
        let cap = |ui: &mut Ui, x: f32, keys: &str| {
            let kw = ui.p.measure(MONO, 11.0, keys) + 10.0;
            let kr = Rect::new(x, r.cy() - 8.5, kw, 17.0);
            ui.p.rounded_stroke(kr, 4.0, 1.0, theme::BORDER.mix(theme::TEXT, 0.2));
            ui.p.text(kr.x + 5.0, kr.cy(), MONO, 11.0, keys, theme::TEXT.mix(theme::DIM, 0.35));
            kw
        };
        let hide = Rect::new(r.right() - 28.0, r.y + 4.0, 22.0, r.h - 8.0);
        let hide_hov = ui.hovered(hide);
        if hide_hov {
            ui.p.rounded(hide, 4.0, theme::HOVER);
            ui.cursor = Cursor::Pointer;
        }
        let (cx, cy, c) = (hide.x + hide.w * 0.5, hide.cy(), if hide_hov { theme::TEXT } else { theme::FAINT });
        ui.p.line((cx - 3.5, cy - 3.5), (cx + 3.5, cy + 3.5), 1.4, c);
        ui.p.line((cx - 3.5, cy + 3.5), (cx + 3.5, cy - 3.5), 1.4, c);
        let mut limit = hide.x - 8.0;
        if more {
            let w = ui.p.measure(MONO, 11.0, "?") + 10.0 + 6.0 + ui.p.measure(UI, 11.5, "all keys");
            let ar = Rect::new(limit - w - 8.0, r.y + 3.0, w + 8.0, r.h - 6.0);
            if ui.hovered(ar) {
                ui.p.rounded(ar, 4.0, theme::HOVER);
                ui.cursor = Cursor::Pointer;
                if ui.input.pressed {
                    press = Some("?");
                }
            }
            let kw = cap(ui, ar.x + 4.0, "?");
            ui.p.text(ar.x + 4.0 + kw + 6.0, r.cy(), UI, 11.5, "all keys", theme::DIM);
            limit = ar.x - 10.0;
        }
        let mut x = r.x + 12.0;
        for (keys, what, key) in &entries {
            let w = ui.p.measure(MONO, 11.0, keys) + 10.0 + 6.0 + ui.p.measure(UI, 11.5, what);
            if x + w > limit {
                break;
            }
            let er = Rect::new(x - 4.0, r.y + 3.0, w + 8.0, r.h - 6.0);
            if key.is_some() && ui.hovered(er) {
                ui.p.rounded(er, 4.0, theme::HOVER);
                ui.cursor = Cursor::Pointer;
                if ui.input.pressed {
                    press = *key;
                }
            }
            let kw = cap(ui, x, keys);
            ui.p.text(x + kw + 6.0, r.cy(), UI, 11.5, what, theme::DIM);
            x += w + 16.0;
        }
        if hide_hov && ui.input.pressed {
            self.key_bar = false;
            self.toast_ok("Key bar hidden — ⌘K “Show the key bar” brings it back (or key_bar in the config file)");
            return;
        }
        // A click is that key, pressed next frame: whatever offers it then runs.
        if let Some(k) = press {
            self.act(Act::Press(k));
        }
    }

    // ---- commands ---------------------------------------------------------------------------

    pub(super) fn run_cmd(&mut self, cmd: Cmd) {
        if self.repo.is_none() {
            return;
        }
        // The files pane also lists a commit's files; these keys only make sense for yours.
        let wip_only = matches!(cmd, Cmd::ToggleStaged | Cmd::ToggleAll | Cmd::DiscardMenu | Cmd::ResetMenu | Cmd::Amend | Cmd::Stash | Cmd::StashMenu);
        if wip_only && self.pane == Pane::Files && !self.showing_wip() {
            self.toast_err("That's for the working copy — 2 goes there");
            return;
        }
        match cmd {
            Cmd::Down => self.step(1),
            Cmd::Up => self.step(-1),
            Cmd::PageDown => self.page(1),
            Cmd::PageUp => self.page(-1),
            Cmd::Top => self.edge(false),
            Cmd::Bottom => self.edge(true),
            Cmd::PaneLeft => self.move_pane(-1, false),
            Cmd::PaneRight => self.move_pane(1, false),
            Cmd::NextPane => self.move_pane(1, true),
            Cmd::PrevPane => self.move_pane(-1, true),
            Cmd::Jump(n) => self.jump(n),
            Cmd::Enter => self.enter(),
            Cmd::Back => self.back(),
            Cmd::NextTab => self.switch_tab(true),
            Cmd::PrevTab => self.switch_tab(false),
            Cmd::ScrollMainDown => {
                let d = self.diff.scroll_page();
                self.diff.scroll_by(d);
            }
            Cmd::ScrollMainUp => {
                let d = self.diff.scroll_page();
                self.diff.scroll_by(-d);
            }
            Cmd::ScrollLeft => self.diff.hscroll = (self.diff.hscroll - 80.0).max(0.0),
            Cmd::ScrollRight => self.diff.hscroll = (self.diff.hscroll + 80.0).min(4000.0),
            Cmd::Push => self.push_key(),
            Cmd::Pull => self.pull_key(),
            Cmd::Refresh => self.act(Act::Refresh),
            Cmd::Undo => self.undo(),
            Cmd::Redo => self.toast_err("No redo here — z undoes"),
            // Needs the keys on offer: pane_keys opens it.
            Cmd::Help => {}
            Cmd::Search => {
                self.focus = Focus::Palette;
                self.palette.reset("");
            }
            Cmd::RecentRepos => {
                self.focus = Focus::Palette;
                self.palette.reset("open ");
            }
            Cmd::OpMenu => self.op_menu(),
            Cmd::Wide(on) => self.wide = on,
            Cmd::ToggleWide => self.wide = !self.wide,
            Cmd::Context(more) => self.step_context(more),
            Cmd::Whitespace => self.act(Act::ToggleWhitespace),
            Cmd::Split => self.act(Act::ToggleSplit),
            Cmd::Commit => {
                // The message box is in the working copy view: nothing may cover it.
                self.compare = None;
                if self.file_view.as_ref().is_some_and(|f| f.rev.is_none()) {
                    self.file_view = None;
                }
                self.select(Sel::Wip);
                self.set_pane(Pane::Files);
                self.focus = Focus::Message;
            }
            // Files.
            Cmd::ToggleFolder => {
                if let Some((id, p)) = self.tree_at() {
                    if let Some(t) = self.tree_mut(id) {
                        let open = !t.is_open(&p);
                        t.set_open(&p, open);
                    }
                    self.tree_go((id, p));
                }
            }
            Cmd::FolderLeft => {
                if let Some((id, p, close)) = self.folder_left() {
                    if close {
                        if let Some(t) = self.tree_mut(id) {
                            t.set_open(&p, false);
                        }
                    }
                    self.tree_go((id, p));
                }
            }
            Cmd::FolderRight => {
                let Some((id, p)) = self.tree_at().filter(|(_, p)| p.ends_with('/')) else { return };
                let Some(t) = self.tree_mut(id) else { return };
                if t.is_open(&p) {
                    // Into it: its first row.
                    let first = t.row_of(&p).and_then(|r| t.rows().get(r + 1)).map(|r| r.path.clone());
                    if let Some(c) = first {
                        self.tree_go((id, c));
                    }
                } else {
                    t.set_open(&p, true);
                    self.tree_go((id, p));
                }
            }
            Cmd::Resolve => {
                if let Some((TreeId::Conflicts, p)) = self.tree_at() {
                    self.act(Act::OpenResolver(p));
                }
            }
            Cmd::ToggleStaged => self.toggle_staged(),
            Cmd::ToggleAll => {
                let unstaged = self.changes.iter().any(|c| c.unstaged() && !c.conflicted());
                self.act(if unstaged { Act::StageAll } else { Act::UnstageAll });
            }
            Cmd::DiscardMenu => self.discard_menu(),
            Cmd::ResetMenu => self.reset_menu(),
            Cmd::Amend => {
                let head = self.head_row.map(|i| self.commits[i].hash.clone());
                match self.sel_row() {
                    Some(i) if self.pane == Pane::Commits && Some(&self.commits[i].hash) != head.as_ref() => {
                        self.toast_err("Only the latest commit can be amended — stage, commit, then fixup (f)")
                    }
                    _ => self.act(Act::Amend),
                }
            }
            Cmd::OpenFile => self.open_file(),
            Cmd::CopyMenu => self.copy_menu(),
            Cmd::CopyPath => {
                if let Some(p) = self.cursor_file() {
                    self.act(Act::CopyText(p));
                }
            }
            Cmd::Fetch => self.act(Act::Fetch),
            Cmd::Stash => {
                if self.changes.is_empty() {
                    self.toast_err("Nothing to stash");
                } else {
                    self.act(Act::PromptStash(StashKind::Changes));
                }
            }
            Cmd::StashMenu => self.act(Act::StashMenu),
            Cmd::StashList => self.act(Act::StashList),
            Cmd::Apply => {
                if let Some(h) = self.stash_view.clone() {
                    self.act(Act::ApplyStash(h));
                }
            }
            Cmd::Pop => {
                if let Some(h) = self.stash_view.clone() {
                    self.act(Act::PopStash(h));
                }
            }
            // The diff.
            Cmd::NextHunk => self.diff.cursor_hunk(1),
            Cmd::PrevHunk => self.diff.cursor_hunk(-1),
            Cmd::SelectLine => self.diff.toggle_line(),
            Cmd::SelectHunk => self.diff.toggle_hunk(),
            Cmd::RangeDown => self.diff.extend(1),
            Cmd::RangeUp => self.diff.extend(-1),
            Cmd::Stage => self.stage_selection(false),
            Cmd::Discard => self.stage_selection(true),
            Cmd::SwitchSide => self.switch_side(),
            // Commits and branches.
            Cmd::Checkout => self.checkout_key(),
            Cmd::NewBranch if self.pane == Pane::Stash => {
                if let Some(h) = self.stash_view.clone() {
                    self.act(Act::PromptStashBranch(h));
                }
            }
            Cmd::NewBranch => {
                if let Some(h) = self.target_commit().or_else(|| self.repo.as_ref()?.rev_parse("HEAD")) {
                    self.act(Act::PromptBranchAt(h));
                }
            }
            Cmd::NewTag => {
                if let Some(h) = self.target_commit().or_else(|| self.repo.as_ref()?.rev_parse("HEAD")) {
                    self.act(Act::PromptTagAt(h));
                }
            }
            Cmd::Revert => {
                if let Some(h) = self.selected_commit() {
                    self.act(Act::Revert(h));
                }
            }
            Cmd::CopyCommit => self.copy_commit(),
            Cmd::PasteCommits => self.paste_commits(),
            Cmd::ClearCopied => {
                self.copied.clear();
                self.toast_ok("Cleared the copied commits");
            }
            Cmd::MoveDown => self.act(Act::MoveCommit(false)),
            Cmd::MoveUp => self.act(Act::MoveCommit(true)),
            Cmd::Squash => self.act(Act::SquashWithMessage),
            Cmd::Fixup => self.act(Act::SquashIntoParent),
            Cmd::Drop if self.pane == Pane::Stash => {
                let st = self.stash_view.as_ref().and_then(|h| self.stashes.iter().find(|s| &s.hash == h)).cloned();
                if let Some(st) = st {
                    let mut it = danger(format!("Drop {}", st.name), Act::DropStash(st.hash.clone()));
                    it.detail = "undoable".into();
                    self.kb_menu(st.parts().1.to_string(), None, vec![it]);
                }
            }
            Cmd::Drop => self.act(Act::DropCommit),
            Cmd::Reword => {
                if let Some(h) = self.selected_commit() {
                    self.act(Act::PromptReword(h));
                }
            }
            Cmd::Delete => self.delete_menu(),
            Cmd::Merge => self.merge_menu(),
            Cmd::Rebase => match self.cursor_ref() {
                Some(r) if !r.is_head => self.act(Act::RebaseOnto { branch: None, onto: r.name }),
                _ => self.toast_err(format!("Pick the branch to rebase {} onto", self.head_name)),
            },
            Cmd::FastForward => self.fast_forward_key(),
            Cmd::Upstream => match self.cursor_ref() {
                Some(r) if r.kind == RefKind::Local => self.act(Act::PromptUpstream(r.name)),
                _ => self.toast_err("Pick a local branch to set its upstream"),
            },
            Cmd::Rename if self.pane == Pane::Stash => {
                if let Some(h) = self.stash_view.clone() {
                    self.act(Act::PromptStashRename(h));
                }
            }
            Cmd::Rename => match self.cursor_ref() {
                Some(r) if r.kind == RefKind::Local => self.act(Act::PromptRename(r.name)),
                _ => self.toast_err("Pick a local branch to rename"),
            },
            Cmd::CheckoutPrev => self.act(Act::CheckoutPrev),
            Cmd::CheckoutByName => {
                self.focus = Focus::Palette;
                self.palette.reset("checkout ");
            }
            Cmd::PushTag => match self.cursor_ref() {
                Some(r) if r.kind == RefKind::Tag => {
                    let remotes = self.repo.as_ref().map(|r| r.remotes()).unwrap_or_default();
                    let items = remotes
                        .into_iter()
                        .map(|(name, url)| {
                            let mut it = item(format!("Push tag to {name}"), Act::PushTag { tag: Some(r.name.clone()), remote: name });
                            it.detail = short_url(&url);
                            it
                        })
                        .collect();
                    self.kb_menu(format!("Push tag {}", r.name), None, items);
                }
                _ => self.push_key(),
            },
        }
    }

    // ---- panes ------------------------------------------------------------------------------

    fn set_pane(&mut self, p: Pane) {
        let p = match p {
            Pane::Files | Pane::Stash if self.stash_view.is_some() => Pane::Stash,
            Pane::Stash => Pane::Files,
            p => p,
        };
        if self.pane == Pane::Main && p != Pane::Main {
            self.diff.cursor_clear();
        }
        self.pane = p;
        match p {
            Pane::Main => {
                if self.pane_rect(Pane::Main).is_none() || self.diff.files.is_empty() {
                    self.pane = Pane::Files;
                    return;
                }
                match self.tree_file_index().filter(|_| !self.showing_wip() && self.diff.cursor.is_none()) {
                    Some(fi) => self.diff.cursor_in_file(fi),
                    None => self.diff.cursor_start(),
                }
            }
            // A commit's files: the cursor starts where the diff is.
            Pane::Files | Pane::Stash if !self.showing_wip() => {
                self.sync_changes_tree();
                self.tree_cursor = self.diff_row();
            }
            Pane::Files => self.tree_cursor = self.tree_at(),
            Pane::Branches if self.side_cursor.is_none() => {
                let head = self.refs.iter().find(|r| r.is_head && r.kind == RefKind::Local).map(|r| SideKey::Ref(ref_full(r)));
                self.side_cursor = head.or(Some(SideKey::Wip));
                self.reveal_side();
            }
            _ => {}
        }
    }

    /// h / l move to the neighbouring pane; tab / shift-tab go round.
    fn move_pane(&mut self, d: i64, wrap: bool) {
        let order: Vec<Pane> = ORDER.iter().copied().filter(|p| self.pane_rect(*p).is_some()).collect();
        let Some(i) = order.iter().position(|p| *p == self.pane) else {
            if let Some(p) = order.first() {
                self.set_pane(*p);
            }
            return;
        };
        let n = order.len() as i64;
        let j = if wrap { (i as i64 + d).rem_euclid(n) } else { (i as i64 + d).clamp(0, n - 1) };
        self.set_pane(order[j as usize]);
    }

    /// 1 2 3 4: the panels left to right — sidebar, commits, files, diff. The two that wide
    /// mode hides bring the normal layout back.
    fn jump(&mut self, n: u8) {
        if matches!(n, 1 | 2) {
            self.wide = false;
        }
        match n {
            1 => self.set_pane(Pane::Branches),
            2 => self.set_pane(Pane::Commits),
            3 => self.set_pane(Pane::Files),
            4 => self.set_pane(Pane::Main),
            _ => {}
        }
    }

    fn enter(&mut self) {
        match self.pane {
            Pane::Branches => match self.side_cursor.clone() {
                Some(SideKey::Wip) => {
                    if self.sel != Sel::Wip {
                        self.select(Sel::Wip);
                    }
                    self.set_pane(Pane::Files);
                }
                Some(SideKey::Worktree(p)) => self.act(Act::OpenWorktree(p.display().to_string())),
                Some(SideKey::Ref(_)) => {
                    self.side_preview();
                    self.set_pane(Pane::Commits);
                }
                None => {}
            },
            Pane::Commits => self.set_pane(Pane::Files),
            Pane::Files | Pane::Stash => {
                if let Some(row) = self.tree_at() {
                    self.tree_go(row);
                }
                self.set_pane(Pane::Main);
            }
            Pane::Main => {}
        }
    }

    fn back(&mut self) {
        match self.pane {
            // A selection goes first, then the diff.
            Pane::Main if self.diff.selectable() && !self.diff.selected.is_empty() => {
                self.diff.selected.clear();
                return;
            }
            Pane::Main => {
                self.set_pane(Pane::Files);
                return;
            }
            Pane::Stash => {
                self.close_stash();
                return;
            }
            Pane::Files if !self.showing_wip() && self.compare.is_none() => {
                self.set_pane(Pane::Commits);
                return;
            }
            _ => {}
        }
        if self.file_view.is_some() {
            self.file_view = None;
        } else if self.compare.is_some() {
            self.act(Act::CompareClose);
        } else if self.tab == Tab::Files {
            self.tab = Tab::Changes;
        } else if self.wide {
            self.wide = false;
        }
    }

    /// ] / [: the next section of the branch list, or the commit's other tab.
    fn switch_tab(&mut self, next: bool) {
        match self.pane {
            Pane::Branches => {
                let items = self.side_items();
                let at = self.side_cursor.as_ref().and_then(|c| items.iter().position(|(k, _)| k == c)).unwrap_or(0);
                let sec = items.get(at).map_or(0, |(_, s)| *s);
                let target = if next {
                    items.iter().find(|(_, s)| *s > sec)
                } else {
                    // The first row of the section before this one.
                    let prev = items.iter().rev().find(|(_, s)| *s < sec).map(|(_, s)| *s);
                    prev.and_then(|p| items.iter().find(|(_, s)| *s == p))
                };
                if let Some((k, _)) = target {
                    self.side_cursor = Some(k.clone());
                    self.side_preview();
                }
            }
            _ if self.sel_row().is_some() && self.compare.is_none() && self.stash_view.is_none() => {
                if self.tab == Tab::Files {
                    self.tab = Tab::Changes;
                    self.file_view = None;
                } else {
                    self.act(Act::BrowseFiles);
                }
            }
            _ => {}
        }
    }

    fn step(&mut self, d: i64) {
        match self.pane {
            Pane::Branches => self.side_step(d),
            Pane::Commits => self.graph_step(d),
            Pane::Files | Pane::Stash => self.file_step(d),
            Pane::Main => self.diff.cursor_step(d),
        }
    }

    fn page(&mut self, d: i64) {
        match self.pane {
            Pane::Branches => self.side_step(d * 10),
            Pane::Commits => self.graph_step(d * ((self.graph_view_h / ROW_H) as i64 - 2).max(1)),
            Pane::Files | Pane::Stash => self.file_step(d * 10),
            Pane::Main => self.diff.cursor_page(d),
        }
    }

    fn edge(&mut self, last: bool) {
        let d = if last { i64::MAX / 4 } else { -(i64::MAX / 4) };
        match self.pane {
            Pane::Main => self.diff.cursor_edge(last),
            _ => self.step(d),
        }
    }

    // ---- commits ----------------------------------------------------------------------------

    fn graph_step(&mut self, d: i64) {
        let row = match self.sel {
            Sel::Wip => 0,
            Sel::Commit(_) => match self.sel_row() {
                Some(i) => i + 1,
                None => return,
            },
        };
        let n = self.commits.len() as i64 + 1;
        let t = (row as i64 + d).clamp(0, n - 1) as usize;
        if t != row {
            self.select(if t == 0 { Sel::Wip } else { Sel::Commit(t - 1) });
            self.reveal_row(t);
        }
    }

    /// The commit the focused pane points at: the selected one, or the branch under the cursor.
    fn target_commit(&self) -> Option<String> {
        match self.pane {
            Pane::Branches => self.cursor_ref().map(|r| r.target),
            _ => self.selected_commit(),
        }
    }

    fn selected_commit(&self) -> Option<String> {
        self.sel_row().map(|i| self.commits[i].hash.clone())
    }

    fn copy_commit(&mut self) {
        let Some(h) = self.selected_commit() else { return };
        if let Some(i) = self.copied.iter().position(|c| *c == h) {
            self.copied.remove(i);
        } else {
            self.copied.push(h);
        }
        let n = self.copied.len();
        let head = self.head_name.clone();
        self.toast_ok(match n {
            0 => "No commits copied".to_string(),
            n => format!("{n} commit{} copied · V pastes onto {head}", if n == 1 { "" } else { "s" }),
        });
    }

    fn paste_commits(&mut self) {
        if self.copied.is_empty() {
            self.toast_err("Nothing copied — C copies the selected commit");
            return;
        }
        // Oldest first, the order they were made in.
        let mut v: Vec<(usize, String)> = self.copied.iter().map(|h| (self.hash_index.get(h).copied().unwrap_or(0), h.clone())).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        self.act(Act::CherryPickMany(v.into_iter().map(|(_, h)| h).collect()));
    }

    fn checkout_key(&mut self) {
        match self.pane {
            Pane::Commits => {
                let Some(i) = self.sel_row() else { return };
                if Some(i) == self.head_row {
                    return;
                }
                let c = &self.commits[i];
                let mut it = item(format!("Check out {} (detached HEAD)", c.short()), Act::CheckoutDetached(c.hash.clone()));
                it.detail = "no branch moves".into();
                let title = c.subject.clone();
                self.kb_menu(title, None, vec![it]);
            }
            Pane::Branches => match self.side_cursor.clone() {
                Some(SideKey::Worktree(p)) => self.act(Act::OpenWorktree(p.display().to_string())),
                Some(SideKey::Ref(_)) => {
                    let Some(r) = self.cursor_ref() else { return };
                    match r.kind {
                        RefKind::Local if !r.is_head => self.act(Act::Checkout(r.name)),
                        RefKind::Local => {}
                        RefKind::Remote => self.act(Act::Checkout(branch_short(&r).to_string())),
                        RefKind::Tag => {
                            let it = item(format!("Check out {} (detached HEAD)", r.name), Act::CheckoutDetached(r.target.clone()));
                            self.kb_menu(r.name.clone(), None, vec![it]);
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// g: reset the current branch to the selected commit or branch; D (files): reset options
    /// for the working copy.
    fn reset_menu(&mut self) {
        let head = self.head_name.clone();
        if self.pane == Pane::Files && self.showing_wip() {
            let mut items = vec![];
            if self.changes.iter().any(|c| c.unstaged()) {
                let mut it = danger("Discard all unstaged changes", Act::DiscardAll);
                it.detail = "undoable".into();
                items.push(it);
            }
            if self.changes.iter().any(|c| c.staged()) {
                items.push(item("Unstage everything", Act::UnstageAll));
            }
            if let Some(h) = self.repo.as_ref().and_then(|r| r.rev_parse("HEAD")) {
                let mut it = danger("Hard reset to HEAD", Act::Reset(h, "hard"));
                it.detail = "discards staged and unstaged · undoable".into();
                it.keys = &["h"];
                items.push(it);
            }
            self.kb_menu("Reset working copy".into(), None, items);
            return;
        }
        let (target, label) = match self.pane {
            Pane::Branches => match self.cursor_ref() {
                Some(r) => (r.target.clone(), r.name.clone()),
                None => return,
            },
            _ => match self.sel_row() {
                Some(i) => (self.commits[i].hash.clone(), self.commits[i].short().to_string()),
                None => return,
            },
        };
        let mut items = Vec::new();
        for (key, mode, text, detail) in [
            (&["s"], "soft", "Soft reset", "keep changes staged"),
            (&["m"], "mixed", "Mixed reset", "keep changes unstaged"),
            (&["h"], "hard", "Hard reset", "discard changes · undoable"),
        ] {
            let mut it = if mode == "hard" { danger(text, Act::Reset(target.clone(), mode)) } else { item(text, Act::Reset(target.clone(), mode)) };
            it.detail = detail.into();
            it.keys = key;
            items.push(it);
        }
        self.kb_menu(format!("Reset {head} to {label}"), None, items);
    }

    fn copy_menu(&mut self) {
        let mut items = Vec::new();
        let mut add = |label: &str, text: String| {
            if !text.is_empty() {
                let mut it = item(label, Act::CopyText(text.clone()));
                it.detail = text.lines().next().unwrap_or("").chars().take(48).collect();
                items.push(it);
            }
        };
        let title = match self.pane {
            Pane::Branches => {
                let Some(r) = self.cursor_ref() else { return };
                add("Name", r.name.clone());
                if r.kind == RefKind::Remote {
                    add("Branch name", branch_short(&r).to_string());
                }
                add("Commit hash", r.target.clone());
                r.name
            }
            Pane::Commits => {
                let Some(i) = self.sel_row() else { return };
                let c = self.commits[i].clone();
                let message = self.repo.as_ref().and_then(|r| r.git(&["log", "-1", "--format=%B", &c.hash]).ok()).unwrap_or_default();
                add("Commit hash", c.hash.clone());
                add("Short hash", c.short().to_string());
                add("Subject", c.subject.clone());
                add("Message", message.trim_end().to_string());
                add("Author", c.author.clone());
                c.subject
            }
            _ => {
                let Some(p) = self.cursor_file() else { return };
                let abs = self.repo.as_ref().map(|r| r.root.join(&p).display().to_string()).unwrap_or_default();
                add("File name", p.rsplit('/').next().unwrap_or(&p).to_string());
                add("Path", p.clone());
                add("Absolute path", abs);
                p
            }
        };
        self.kb_menu(format!("Copy · {title}"), None, items);
    }

    fn op_menu(&mut self) {
        let Some(op) = self.op else {
            self.toast_err("No merge or rebase in progress — M merges a branch, r rebases onto it");
            return;
        };
        let mut items = vec![item(format!("Continue the {}", op.name().to_lowercase()), Act::OpContinue)];
        if let Some(c) = self.changes.iter().find(|c| c.conflicted()) {
            items.push(item("Resolve conflicts", Act::OpenResolver(c.path.clone())));
        }
        items.push(danger(format!("Abort the {}", op.name().to_lowercase()), Act::OpAbort));
        self.kb_menu(format!("{} in progress", op.name()), None, items);
    }

    /// `?`: every key on offer (the last frame's complete list); picking one, or pressing its
    /// key, presses it.
    fn help_menu(&mut self, offered: &[Binding]) {
        let mut seen: Vec<&str> = Vec::new();
        let items = crate::ui::reachable(offered)
            .into_iter()
            .filter(|b| !b.help.is_empty() && !b.keys.contains(&"?"))
            .filter(|b| !seen.contains(&b.help) && {
                seen.push(b.help);
                true
            })
            .map(|b| {
                let mut it = item(b.help, Act::Press(b.keys[0]));
                it.keys = b.keys;
                it
            })
            .collect();
        let note = Some(("j / k to move · enter or the key runs it · esc closes".to_string(), theme::DIM));
        self.kb_menu(format!("Keys · {}", self.pane.name()), note, items);
    }

    fn push_key(&mut self) {
        let Some(repo) = &self.repo else { return };
        let branch = self.head_name.clone();
        if !self.local_names.contains(&branch) {
            self.toast_err("Detached HEAD — check out a branch to push");
            return;
        }
        // Straight to the upstream when that's a plain push (lazygit's P); otherwise the picker.
        let up = repo.upstream();
        let remote = up.as_ref().and_then(|u| repo.remotes().into_iter().map(|(n, _)| n).find(|n| *u == format!("{n}/{branch}")));
        match (remote, up) {
            (Some(remote), Some(u)) if repo.count_range("HEAD", &u) == 0 => self.act(Act::PushTo { remote, force: false }),
            _ => {
                self.open_push_menu(self.remote_btns[0]);
                self.kb_select_first();
            }
        }
    }

    fn pull_key(&mut self) {
        let Some(repo) = &self.repo else { return };
        if !self.local_names.contains(&self.head_name) {
            self.toast_err("Detached HEAD — check out a branch to pull into");
            return;
        }
        // A fast-forward from the upstream goes straight ahead; anything else picks first.
        if let (_, Some((remote, branch))) = self.remotes_upstream_first() {
            if repo.count_range(&format!("{remote}/{branch}"), "HEAD") == 0 {
                self.act(Act::PullFrom { remote, branch, mode: "ff" });
                return;
            }
        }
        self.open_pull_menu(self.remote_btns[1]);
        self.kb_select_first();
    }

    fn step_context(&mut self, more: bool) {
        const STEPS: [u32; 4] = [3, 10, 25, u32::MAX];
        let i = STEPS.iter().position(|&c| c == self.opts.context).unwrap_or(0);
        let j = if more { (i + 1).min(STEPS.len() - 1) } else { i.saturating_sub(1) };
        if j != i {
            self.opts.context = STEPS[j];
            self.reload_diff();
        }
    }

    // ---- branches (the sidebar) -------------------------------------------------------------

    /// The sidebar's rows in the order they're drawn, with a section number for ] / [.
    fn side_items(&self) -> Vec<(SideKey, u8)> {
        let mut v = vec![(SideKey::Wip, 0)];
        for (w, busy) in self.sidebar_worktrees() {
            if busy.is_none() {
                v.push((SideKey::Worktree(w.path.clone()), 0));
            }
        }
        let hidden = |r: &Ref| self.hidden.contains(&ref_full(r));
        let sections: [(u8, Box<dyn Fn(&Ref) -> bool>); 4] = [
            (1, Box::new(|r: &Ref| r.kind == RefKind::Local && !hidden(r))),
            (2, Box::new(|r: &Ref| r.kind == RefKind::Remote && !hidden(r))),
            (3, Box::new(|r: &Ref| hidden(r))),
            (4, Box::new(|r: &Ref| r.kind == RefKind::Tag && !hidden(r))),
        ];
        for (sec, keep) in &sections {
            v.extend(self.refs.iter().filter(|r| keep(r)).map(|r| (SideKey::Ref(ref_full(r)), *sec)));
        }
        v
    }

    fn cursor_ref(&self) -> Option<Ref> {
        if self.pane != Pane::Branches {
            return None;
        }
        match &self.side_cursor {
            Some(SideKey::Ref(full)) => self.refs.iter().find(|r| ref_full(r) == *full).cloned(),
            _ => None,
        }
    }

    fn side_step(&mut self, d: i64) {
        let items = self.side_items();
        if items.is_empty() {
            return;
        }
        let at = self.side_cursor.as_ref().and_then(|c| items.iter().position(|(k, _)| k == c));
        let i = match at {
            Some(i) => (i as i64 + d).clamp(0, items.len() as i64 - 1) as usize,
            None => 0,
        };
        self.side_cursor = Some(items[i].0.clone());
        self.side_preview();
    }

    /// Shows what's under the sidebar cursor, as a click would.
    fn side_preview(&mut self) {
        self.reveal_side();
        match self.side_cursor.clone() {
            Some(SideKey::Wip) => {
                if self.sel != Sel::Wip {
                    self.select(Sel::Wip);
                }
            }
            Some(SideKey::Ref(_)) => {
                let target = self.cursor_ref().map(|r| r.target);
                if let Some(&i) = target.as_ref().and_then(|t| self.hash_index.get(t)) {
                    if self.sel_row() != Some(i) {
                        self.select(Sel::Commit(i));
                    }
                    let y = (i + 1) as f32 * ROW_H;
                    self.graph_scroll.reveal(y - ROW_H, y + 2.0 * ROW_H, self.graph_view_h);
                }
            }
            _ => {}
        }
    }

    fn reveal_side(&mut self) {
        let y = self.side_cursor.as_ref().and_then(|c| self.side_rows.iter().find(|(k, _)| k == c)).map(|(_, y)| *y);
        if let Some(y) = y {
            self.side_scroll.reveal(y - 30.0, y + LIST_ROW_H + 8.0, self.side_view_h);
        }
    }

    fn delete_menu(&mut self) {
        match self.side_cursor.clone() {
            Some(SideKey::Worktree(p)) => {
                let it = danger("Remove worktree", Act::RemoveWorktree(p.display().to_string()));
                self.kb_menu(tilde(&p), None, vec![it]);
            }
            Some(SideKey::Ref(_)) => {
                let Some(r) = self.cursor_ref() else { return };
                let it = match r.kind {
                    RefKind::Local if r.is_head => return self.toast_err("Can't delete the checked-out branch"),
                    RefKind::Local if self.branch_worktree(&r.name).is_some() => return self.toast_err(format!("{} is checked out in another worktree", r.name)),
                    RefKind::Local => {
                        let mut it = danger(format!("Delete branch {}", r.name), Act::DeleteBranch(r.name.clone()));
                        it.detail = "local · undoable".into();
                        it
                    }
                    RefKind::Tag => {
                        let mut it = danger(format!("Delete tag {}", r.name), Act::DeleteTag(r.name.clone()));
                        it.detail = "here, not on remotes · undoable".into();
                        it
                    }
                    RefKind::Remote => return self.toast_err("Deleting remote branches isn't supported"),
                };
                self.kb_menu(r.name.clone(), None, vec![it]);
            }
            _ => {}
        }
    }

    /// M: merge the branch under the cursor into the current one (a merge commit, or a
    /// fast-forward when that's possible).
    fn merge_menu(&mut self) {
        let Some(r) = self.cursor_ref().filter(|r| !r.is_head) else {
            return self.toast_err(format!("Pick the branch to merge into {}", self.head_name));
        };
        let head = self.head_name.clone();
        let head_hash = self.head_row.map_or_else(|| "HEAD".to_string(), |i| self.commits[i].hash.clone());
        let items = self.merge_items((&r.name, &r.name, &r.target), (None, &head, &head_hash));
        let note = self.preview_note("HEAD", &r.target, &r.name);
        self.kb_menu(format!("Merge {} into {head}", r.name), note, items);
    }

    /// f: bring a branch up to its upstream, if that's a fast-forward.
    fn fast_forward_key(&mut self) {
        let Some(r) = self.cursor_ref().filter(|r| r.kind == RefKind::Local) else { return };
        if r.upstream.is_empty() || r.gone {
            return self.toast_err(format!("{} has no upstream to fast-forward from (u sets one)", r.name));
        }
        match (r.ahead, r.behind) {
            (_, 0) => self.toast_ok(format!("{} is up to date with {}", r.name, r.upstream)),
            (0, _) => self.act(Act::FastForward { src: r.upstream.clone(), dst: (!r.is_head).then(|| r.name.clone()) }),
            _ => self.toast_err(format!("{} and {} have diverged — merge or rebase instead", r.name, r.upstream)),
        }
    }

    // ---- files ------------------------------------------------------------------------------

    /// The rows the keyboard walks, top to bottom: the working copy's conflicts, then its
    /// unstaged and staged trees; or the commit's (stash's, comparison's) changes.
    fn tree_rows(&self) -> Vec<(TreeId, String)> {
        if !self.showing_wip() {
            return self.changes_tree.rows().iter().map(|r| (TreeId::Changes, r.path.clone())).collect();
        }
        let mut v: Vec<(TreeId, String)> = self.changes.iter().filter(|c| c.conflicted()).map(|c| (TreeId::Conflicts, c.path.clone())).collect();
        v.extend(self.unstaged_tree.rows().iter().map(|r| (TreeId::Unstaged, r.path.clone())));
        v.extend(self.staged_tree.rows().iter().map(|r| (TreeId::Staged, r.path.clone())));
        v
    }

    pub(super) fn tree(&self, id: TreeId) -> Option<&FileTree> {
        match id {
            TreeId::Unstaged => Some(&self.unstaged_tree),
            TreeId::Staged => Some(&self.staged_tree),
            TreeId::Changes => Some(&self.changes_tree),
            TreeId::Conflicts => None,
        }
    }

    fn tree_mut(&mut self, id: TreeId) -> Option<&mut FileTree> {
        match id {
            TreeId::Unstaged => Some(&mut self.unstaged_tree),
            TreeId::Staged => Some(&mut self.staged_tree),
            TreeId::Changes => Some(&mut self.changes_tree),
            TreeId::Conflicts => None,
        }
    }

    /// `path`'s row in that list, as the list stands (a file in a closed folder: the folder's).
    fn row_in(&self, id: TreeId, path: &str) -> Option<(TreeId, String)> {
        let wip = self.showing_wip();
        match (id, self.tree(id)) {
            (TreeId::Conflicts, _) => (wip && self.changes.iter().any(|c| c.conflicted() && c.path == path)).then(|| (id, path.to_string())),
            (TreeId::Changes, _) if wip => None,
            (TreeId::Unstaged | TreeId::Staged, _) if !wip => None,
            (_, Some(t)) => t.row_near(path).map(|r| (id, t.rows()[r].path.clone())),
            (_, None) => None,
        }
    }

    /// The changes tree's row for the file the diff is at.
    fn diff_row(&self) -> Option<(TreeId, String)> {
        let f = self.diff.current_file(self.diff.split()).or((!self.diff.files.is_empty()).then_some(0))?;
        self.row_in(TreeId::Changes, &self.diff.files.get(f)?.path)
    }

    /// The keyboard's row in the file lists: where it was put, while that row is there; or
    /// the row of the file on screen.
    pub(super) fn tree_at(&self) -> Option<(TreeId, String)> {
        if let Some(at) = self.tree_cursor.as_ref().and_then(|(id, p)| self.row_in(*id, p)) {
            return Some(at);
        }
        if !self.showing_wip() {
            return self.diff_row();
        }
        let (p, staged) = self.wip_file.as_ref()?;
        let conflict = self.changes.iter().any(|c| &c.path == p && c.conflicted());
        self.row_in(if conflict { TreeId::Conflicts } else if *staged { TreeId::Staged } else { TreeId::Unstaged }, p)
    }

    /// Where h goes: (list, row, whether it closes that folder) — the open folder under the
    /// cursor, or the folder the row is in.
    fn folder_left(&self) -> Option<(TreeId, String, bool)> {
        let (id, p) = self.tree_at()?;
        let t = self.tree(id)?;
        if p.ends_with('/') && t.is_open(&p) {
            return Some((id, p, true));
        }
        let up = t.parent(t.row_of(&p)?)?;
        Some((id, t.rows()[up].path.clone(), false))
    }

    /// The file the tree's cursor is on, or a folder's first, as an index into the diff.
    fn tree_file_index(&self) -> Option<usize> {
        let (_, p) = self.tree_at()?;
        self.diff.files.iter().position(|f| if p.ends_with('/') { f.path.starts_with(&p) } else { f.path == p })
    }

    /// Puts the keyboard on a row and shows it: a working-copy file's diff, or the commit's
    /// diff scrolled to the file (a folder: its first).
    fn tree_go(&mut self, (id, path): (TreeId, String)) {
        self.tree_cursor = Some((id, path.clone()));
        if let Some(t) = self.tree_mut(id) {
            if let Some(r) = t.row_of(&path) {
                t.scroll_to(r);
            }
        }
        match id {
            TreeId::Changes => {
                if let Some(fi) = self.tree_file_index() {
                    let split = self.diff.split();
                    self.diff.jump_to_file(fi, split);
                }
            }
            _ if path.ends_with('/') => {}
            _ => self.show_wip_file(path, id == TreeId::Staged),
        }
    }

    /// The file the focused pane points at (in the lists, a folder's path).
    fn cursor_file(&self) -> Option<String> {
        if self.pane == Pane::Main {
            return match self.showing_wip() {
                true => self.wip_file.as_ref().map(|(p, _)| p.clone()),
                false => self.diff.cursor.and_then(|c| self.diff.files.get(c.0)).map(|f| f.path.clone()),
            };
        }
        self.tree_at().map(|(_, p)| p.trim_end_matches('/').to_string())
    }

    /// j / k in the lists, folders included.
    fn file_step(&mut self, d: i64) {
        self.sync_changes_tree();
        let rows = self.tree_rows();
        if rows.is_empty() {
            return;
        }
        let n = rows.len() as i64;
        let at = self.tree_at().and_then(|c| rows.iter().position(|r| *r == c));
        let i = match at {
            Some(i) => (i as i64 + d).clamp(0, n - 1),
            None => (d - 1).clamp(0, n - 1),
        };
        self.tree_go(rows[i as usize].clone());
    }

    /// space: stage the file (a folder: everything in it) if it's unstaged, unstage it if it's
    /// staged. The cursor stays at the same place in its list, so the next row slides under it.
    fn toggle_staged(&mut self) {
        if !self.showing_wip() {
            return;
        }
        let Some((id, path)) = self.tree_at() else { return };
        let Some(t) = self.tree(id) else {
            self.toast_err("Resolve the conflict first — enter opens the resolver");
            return;
        };
        let files = t.files_under(&path);
        let before = self.tree_rows();
        let at = before.iter().position(|r| r.0 == id && r.1 == path).unwrap_or(0);
        let in_list = before[..at].iter().filter(|r| r.0 == id).count();
        self.stage_paths(&files, id != TreeId::Staged);
        let after = self.tree_rows();
        let same: Vec<&(TreeId, String)> = after.iter().filter(|r| r.0 == id).collect();
        let next = match same.is_empty() {
            false => Some(same[in_list.min(same.len() - 1)]),
            true => after.get(at.min(after.len().saturating_sub(1))),
        };
        match next.cloned() {
            Some(row) => self.tree_go(row),
            None => self.tree_cursor = None,
        }
    }

    fn discard_menu(&mut self) {
        let Some((id, path)) = self.tree_at().filter(|_| self.showing_wip()) else { return };
        if path.ends_with('/') {
            if id != TreeId::Unstaged {
                return;
            }
            let files = self.unstaged_tree.files_under(&path);
            let untracked = files.iter().filter(|f| self.changes.iter().any(|c| &c.path == *f && c.untracked())).count();
            let n = files.len();
            let mut it = danger(format!("Discard changes to {n} file{}", if n == 1 { "" } else { "s" }), Act::DiscardFiles(files));
            it.detail = match untracked {
                0 => "undoable".into(),
                u => format!("deletes {u} untracked · undoable"),
            };
            it.keys = &["u"];
            self.kb_menu(format!("Discard · {path}"), None, vec![it]);
            return;
        }
        let c = self.changes.iter().find(|c| c.path == path).cloned();
        let Some(c) = c else { return };
        let mut items = Vec::new();
        if c.conflicted() {
            items.push(item("Use HEAD's version", Act::ResolveUse(path.clone(), true)));
            items.push(item("Use the incoming version", Act::ResolveUse(path.clone(), false)));
        } else if c.untracked() {
            let mut it = danger(format!("Delete {path}"), Act::DiscardFile(path.clone()));
            it.detail = "untracked · undoable".into();
            items.push(it);
        } else {
            if c.unstaged() {
                let mut it = danger("Discard unstaged changes", Act::DiscardFile(path.clone()));
                it.detail = "undoable".into();
                it.keys = &["u"];
                items.push(it);
            }
            if c.staged() {
                let mut it = danger("Discard staged changes too", Act::DiscardStaged(path.clone()));
                it.detail = "index and file · undoable".into();
                items.push(it);
            }
        }
        self.kb_menu(format!("Discard · {path}"), None, items);
    }

    fn open_file(&mut self) {
        let (Some(p), Some(repo)) = (self.cursor_file(), &self.repo) else { return };
        let full = repo.root.join(&p);
        if full.exists() {
            open_external(&full);
        } else {
            self.toast_err(format!("{p} isn't in the working tree"));
        }
    }

    // ---- the diff ---------------------------------------------------------------------------

    /// s / d in the diff: stage (or unstage) or discard the selected lines, or with none, the
    /// line under the cursor; then keep going from the same place.
    fn stage_selection(&mut self, discard: bool) {
        if !self.diff.selectable() {
            self.toast_err("Only working-copy changes can be staged or discarded");
            return;
        }
        self.diff.take_cursor_line();
        // The file with the selection (the working copy shows one).
        let Some(f) = self.diff.selected.iter().map(|s| s.0).min() else { return };
        self.apply_diff_action(DiffAction::Lines { f, discard });
        if self.pane == Pane::Main && self.diff.cursor.is_none() {
            self.diff.cursor_start();
            if self.diff.cursor.is_none() {
                self.set_pane(Pane::Files);
            }
        }
    }

    /// tab in the diff: the file's staged changes instead of its unstaged ones, or back.
    fn switch_side(&mut self) {
        let Some((path, staged)) = self.wip_file.clone().filter(|_| self.showing_wip()) else { return };
        let other = self.changes.iter().any(|c| c.path == path && if staged { c.unstaged() } else { c.staged() });
        if !other {
            self.toast_err(if staged { "No unstaged changes in this file" } else { "Nothing staged in this file" });
            return;
        }
        self.diff.cursor_clear();
        let id = if staged { TreeId::Unstaged } else { TreeId::Staged };
        self.tree_go((id, path));
        self.diff.cursor_start();
    }
}
