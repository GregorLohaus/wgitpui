//! lazygit-style keyboard control: which pane has focus, what each command of the keymap
//! (`keys.rs`) does there, keyboard-driven menus, and the ring that shows the focused pane.

use super::*;
use crate::ui::KeyPress;

/// Panes left to right, as laid out (wide mode hides the first two).
const ORDER: [Pane; 4] = [Pane::Branches, Pane::Commits, Pane::Files, Pane::Main];

impl GitApp {
    /// Keys for the focused pane, or for an open menu. A key that runs a command is used up.
    pub(super) fn keyboard(&mut self, ui: &mut Ui) {
        if self.menu.is_some() {
            self.menu_keys(ui);
            return;
        }
        let mut keys = std::mem::take(&mut ui.input.keys).into_iter();
        let mut rest = Vec::new();
        for k in keys.by_ref() {
            let Some(cmd) = keys::lookup(self.pane, &k) else {
                rest.push(k);
                continue;
            };
            self.kb = true;
            // The key's character mustn't also land in a text field it opens (`c`, `/`).
            ui.input.text.clear();
            self.run_cmd(cmd);
            // A prompt, menu or text field took over: the remaining keys are theirs.
            if self.focus != Focus::None || self.menu.is_some() {
                break;
            }
        }
        rest.extend(keys);
        ui.input.keys = rest;
    }

    /// lazygit's commit message box: enter on the summary line commits, tab goes to the
    /// description and back. Shift / ⌥-enter still make a new line anywhere.
    pub(super) fn message_keys(&mut self, ui: &mut Ui) {
        let on_summary = !self.msg.text[..self.msg.cursor.min(self.msg.text.len())].contains('\n');
        let plain = |k: &KeyPress| !k.mods.shift && !k.mods.alt && !k.mods.ctrl && !k.mods.logo;
        let keys = &mut ui.input.keys;
        if on_summary {
            if let Some(i) = keys.iter().position(|k| k.key == Key::Enter && plain(k)) {
                keys.remove(i);
                self.commit();
                return;
            }
        }
        if let Some(i) = keys.iter().position(|k| k.key == Key::Tab && plain(k)) {
            keys.remove(i);
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

    /// j / k (↓ / ↑), tab, enter, esc and a menu's own keys (`s` / `m` / `h` in the reset menu,
    /// any listed key in the `?` menu). Other keys do nothing while a menu is open.
    fn menu_keys(&mut self, ui: &mut Ui) {
        let keys = std::mem::take(&mut ui.input.keys);
        ui.input.text.clear();
        for k in keys {
            let Some(m) = &mut self.menu else { return };
            let is = |s: &str| keys::Chord::parse(s).matches(&k);
            let live: Vec<usize> = (0..m.items.len()).filter(|&i| m.items[i].act.is_some()).collect();
            if live.is_empty() || is("<esc>") {
                self.menu = None;
                return;
            }
            let at = m.sel.and_then(|s| live.iter().position(|&i| i == s));
            let pick = if is("j") || is("<down>") || is("<tab>") {
                m.sel = Some(live[at.map_or(0, |p| (p + 1) % live.len())]);
                m.reveal_sel();
                None
            } else if is("k") || is("<up>") || is("<s-tab>") {
                m.sel = Some(live[at.map_or(live.len() - 1, |p| (p + live.len() - 1) % live.len())]);
                m.reveal_sel();
                None
            } else if is("<enter>") {
                m.sel
            } else {
                m.items.iter().position(|it| it.act.is_some() && it.keys.iter().any(|s| is(s)))
            };
            if let Some(i) = pick {
                let a = m.items[i].act.clone();
                self.menu = None;
                if let Some(a) = a {
                    self.act(a);
                }
                return;
            }
        }
    }

    /// Opens a menu from the keyboard: next to the focused pane, first item highlighted.
    fn kb_menu(&mut self, title: String, note: Option<(String, Color)>, items: Vec<MenuItem>) {
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

    // ---- commands ---------------------------------------------------------------------------

    pub(super) fn run_cmd(&mut self, cmd: Cmd) {
        if self.repo.is_none() {
            return;
        }
        // The files pane also lists a commit's files; these keys only make sense for yours.
        let wip_only = matches!(cmd, Cmd::ToggleStaged | Cmd::ToggleAll | Cmd::DiscardMenu | Cmd::ResetMenu | Cmd::Amend);
        if wip_only && self.pane == Pane::Files && self.sel != Sel::Wip {
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
            Cmd::Help => self.help_menu(),
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
            Cmd::Stash => self.toast_err("No stash in wgitpui (yet)"),
            // The diff.
            Cmd::NextHunk => self.diff.cursor_hunk(1),
            Cmd::PrevHunk => self.diff.cursor_hunk(-1),
            Cmd::HunkMode => self.diff.toggle_hunk_mode(),
            Cmd::Range => self.diff.toggle_range(),
            Cmd::RangeDown => self.diff.range_step(1),
            Cmd::RangeUp => self.diff.range_step(-1),
            Cmd::Stage => self.stage_selection(false),
            Cmd::Discard => self.stage_selection(true),
            Cmd::SwitchSide => self.switch_side(),
            // Commits and branches.
            Cmd::Checkout => self.checkout_key(),
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
            Cmd::Drop => self.act(Act::DropCommit),
            Cmd::Reword => {
                if let Some(h) = self.selected_commit() {
                    self.act(Act::PromptReword(h));
                }
            }
            Cmd::Worktree => self.worktree_key(),
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
                if self.sel != Sel::Wip && self.diff.cursor.is_none() {
                    self.diff.cursor_in_file(self.file_cursor);
                } else {
                    self.diff.cursor_start();
                }
            }
            Pane::Files if self.sel != Sel::Wip => {
                let n = self.diff.files.len();
                self.file_cursor = self.diff.current_file(self.diff.split()).unwrap_or(0).min(n.saturating_sub(1));
            }
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

    /// 1 workspace, 2 files, 3 branches, 4 commits (lazygit's numbering). Panes wide mode hides
    /// bring the normal layout back.
    fn jump(&mut self, n: u8) {
        if matches!(n, 1 | 3 | 4) {
            self.wide = false;
        }
        match n {
            1 => {
                self.set_pane(Pane::Branches);
                self.side_cursor = Some(SideKey::Wip);
                self.reveal_side();
            }
            2 => {
                if self.sel != Sel::Wip {
                    self.select(Sel::Wip);
                }
                self.set_pane(Pane::Files);
            }
            3 => {
                self.set_pane(Pane::Branches);
                if self.side_cursor == Some(SideKey::Wip) {
                    self.side_cursor = None;
                    self.set_pane(Pane::Branches);
                }
            }
            4 => self.set_pane(Pane::Commits),
            _ => self.toast_err("No stash pane in wgitpui"),
        }
    }

    fn enter(&mut self) {
        match self.pane {
            Pane::Branches => match self.side_cursor.clone() {
                Some(SideKey::Wip) => self.jump(2),
                Some(SideKey::Worktree(p)) => self.act(Act::OpenWorktree(p.display().to_string())),
                Some(SideKey::Ref(_)) => {
                    self.side_preview();
                    self.set_pane(Pane::Commits);
                }
                None => {}
            },
            Pane::Commits => self.set_pane(Pane::Files),
            Pane::Files => {
                let conflicted = self.wip_file.as_ref().filter(|_| self.sel == Sel::Wip).filter(|(p, _)| self.changes.iter().any(|c| &c.path == p && c.conflicted()));
                match conflicted {
                    Some((p, _)) => {
                        let p = p.clone();
                        self.act(Act::OpenResolver(p));
                    }
                    None => self.set_pane(Pane::Main),
                }
            }
            Pane::Main => {}
        }
    }

    fn back(&mut self) {
        match self.pane {
            Pane::Main => {
                self.set_pane(Pane::Files);
                return;
            }
            Pane::Files if self.sel != Sel::Wip && self.compare.is_none() => {
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
            _ if self.sel_row().is_some() && self.compare.is_none() => {
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
            Pane::Files => self.file_step(d),
            Pane::Main => self.diff.cursor_step(d),
        }
    }

    fn page(&mut self, d: i64) {
        match self.pane {
            Pane::Branches => self.side_step(d * 10),
            Pane::Commits => self.graph_step(d * ((self.graph_view_h / ROW_H) as i64 - 2).max(1)),
            Pane::Files => self.file_step(d * 10),
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

    fn worktree_key(&mut self) {
        let from = match self.pane {
            Pane::Branches => self.cursor_ref().map(|r| match r.kind {
                RefKind::Local if self.branch_free(&r.name) => WtFrom { rev: r.name.clone(), label: r.name.clone(), branch: Some(r.name.clone()), prefill: String::new() },
                RefKind::Remote => WtFrom { rev: r.name.clone(), label: r.name.clone(), branch: None, prefill: branch_short(&r).to_string() },
                _ => WtFrom { rev: r.name.clone(), label: r.name.clone(), branch: None, prefill: String::new() },
            }),
            _ => self.wt_from_selection(),
        };
        if let Some(f) = from {
            self.act(Act::NewWorktree(f));
        }
    }

    /// g: reset the current branch to the selected commit or branch; D (files): reset options
    /// for the working copy.
    fn reset_menu(&mut self) {
        let head = self.head_name.clone();
        if self.pane == Pane::Files && self.sel == Sel::Wip {
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

    /// `?`: what the keys do in this pane; picking one (or pressing its key) runs it.
    fn help_menu(&mut self) {
        let items = keys::help(self.pane)
            .into_iter()
            .map(|(k, help, cmd)| {
                let mut it = item(help, Act::Key(cmd));
                it.detail = k.join("  ");
                it.keys = k;
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

    /// The working copy's files in list order: conflicts, unstaged, staged.
    fn wip_items(&self) -> Vec<(String, bool)> {
        let of = |f: &dyn Fn(&Change) -> bool, staged: bool| self.changes.iter().filter(|c| f(c)).map(|c| (c.path.clone(), staged)).collect::<Vec<_>>();
        let mut v = of(&|c| c.conflicted(), false);
        v.extend(of(&|c| c.unstaged() && !c.conflicted(), false));
        v.extend(of(&|c| c.staged() && !c.conflicted(), true));
        v
    }

    /// The file the focused pane points at.
    fn cursor_file(&self) -> Option<String> {
        if self.sel == Sel::Wip {
            return self.wip_file.as_ref().map(|(p, _)| p.clone());
        }
        let f = if self.pane == Pane::Main { self.diff.cursor.map(|c| c.0) } else { Some(self.file_cursor) };
        self.diff.files.get(f?).map(|f| f.path.clone())
    }

    fn file_step(&mut self, d: i64) {
        if self.sel != Sel::Wip {
            let n = self.diff.files.len();
            if n == 0 {
                return;
            }
            self.file_cursor = (self.file_cursor as i64 + d).clamp(0, n as i64 - 1) as usize;
            let split = self.diff.split();
            self.diff.jump_to_file(self.file_cursor, split);
            let y = self.file_cursor as f32 * 22.0;
            let h = self.list_view_h[0];
            self.files_scroll.reveal(y, y + 22.0, h);
            return;
        }
        let items = self.wip_items();
        if items.is_empty() {
            return;
        }
        let at = self.wip_file.as_ref().and_then(|w| items.iter().position(|i| i == w));
        let i = at.map_or(0, |i| (i as i64 + d).clamp(0, items.len() as i64 - 1) as usize);
        self.show_file(items[i].clone());
    }

    /// Shows a working-copy file's diff and keeps its row in view.
    fn show_file(&mut self, f: (String, bool)) {
        let (path, staged) = f.clone();
        if self.wip_file.as_ref() != Some(&f) {
            self.wip_file = Some(f);
            self.diff.selected.clear();
            self.load_wip_diff();
        }
        let list: Vec<&Change> = self.changes.iter().filter(|c| !c.conflicted() && if staged { c.staged() } else { c.unstaged() }).collect();
        if let Some(i) = list.iter().position(|c| c.path == path) {
            let (y, h) = (i as f32 * LIST_ROW_H, self.list_view_h[staged as usize]);
            let scroll = if staged { &mut self.staged_scroll } else { &mut self.unstaged_scroll };
            scroll.reveal(y, y + LIST_ROW_H, h);
        }
    }

    /// space: stage the file if it's unstaged, unstage it if it's staged. The cursor stays at
    /// the same place in the list, so the next file slides under it.
    fn toggle_staged(&mut self) {
        if self.sel != Sel::Wip {
            return;
        }
        let Some((path, staged)) = self.wip_file.clone() else { return };
        if self.changes.iter().any(|c| c.path == path && c.conflicted()) {
            self.toast_err("Resolve the conflict first — enter opens the resolver");
            return;
        }
        let at = self.wip_items().iter().position(|i| *i == (path.clone(), staged)).unwrap_or(0);
        let Some(repo) = &self.repo else { return };
        let r = if staged { repo.unstage(&[&path]) } else { repo.stage(&[&path]) };
        self.result(r, "");
        self.reload_status();
        let items = self.wip_items();
        if let Some(f) = items.get(at.min(items.len().saturating_sub(1))) {
            self.show_file(f.clone());
        }
    }

    fn discard_menu(&mut self) {
        let Some(path) = self.cursor_file().filter(|_| self.sel == Sel::Wip) else { return };
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

    /// space / d in the diff: stage (or unstage) or discard what the cursor selects, then keep
    /// going from the same place.
    fn stage_selection(&mut self, discard: bool) {
        let Some(c) = self.diff.cursor else { return };
        if !self.diff.selectable() || self.diff.selected.is_empty() {
            self.toast_err("Only working-copy changes can be staged");
            return;
        }
        self.apply_diff_action(DiffAction::Lines { f: c.0, discard });
        if self.pane == Pane::Main && self.diff.cursor.is_none() {
            self.diff.cursor_start();
            if self.diff.cursor.is_none() {
                self.set_pane(Pane::Files);
            }
        }
    }

    /// tab in the diff: the file's staged changes instead of its unstaged ones, or back.
    fn switch_side(&mut self) {
        let Some((path, staged)) = self.wip_file.clone().filter(|_| self.sel == Sel::Wip) else { return };
        let other = self.changes.iter().any(|c| c.path == path && if staged { c.unstaged() } else { c.staged() });
        if !other {
            self.toast_err(if staged { "No unstaged changes in this file" } else { "Nothing staged in this file" });
            return;
        }
        self.diff.cursor_clear();
        self.show_file((path, !staged));
        self.diff.cursor_start();
    }
}
