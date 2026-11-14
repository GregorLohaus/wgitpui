//! Reading a repository: what opening one and every refresh need from git, read as plain data
//! that any thread can produce, then taken in on the main thread. Tabs open this way (their
//! repository is read on a worker, so the window never waits for one), and so does the refresh
//! when the window or a tab comes back into view.

use super::*;

/// What a refresh reads from git.
pub struct RepoState {
    head_name: String,
    /// The refs, HEAD's name and the hidden refs at a glance: when it changes, the history is
    /// streamed again.
    sig: String,
    /// When `sig` changed (or was never seen): the refs, and the history streaming again.
    refs: Option<Refs>,
    op: Option<OpKind>,
    stashes: Vec<Stash>,
    /// None when the working copy was reloaded since this was read (that's newer).
    status: Option<StatusRead>,
    worktrees: Vec<Worktree>,
}

struct Refs {
    refs: Vec<Ref>,
    default_branch: Option<String>,
    log: mpsc::Receiver<LogMsg>,
    /// What the history stream sent first, waited for so the graph doesn't flash empty.
    first: Option<LogMsg>,
}

impl RepoState {
    /// `seen`: the signature the history was last streamed for (None reads the refs anyway).
    /// `busy`: worktrees a job is adding or removing, whose changes aren't counted.
    pub(super) fn read(repo: &Repo, hidden: &[String], seen: Option<&str>, busy: &[PathBuf]) -> RepoState {
        let head_name = repo.head_name();
        let sig = format!("{}\n{head_name}\n{}", repo.refs_signature(), hidden.join(","));
        // The history streams while the rest is read.
        let started = Instant::now();
        let refs = (seen != Some(sig.as_str())).then(|| {
            let log = git::stream_log(repo.root.clone(), hidden.to_vec());
            let refs = repo.refs().unwrap_or_default();
            // Only a repository whose main line has an unusual name needs to ask which one it is.
            let conventional = refs.iter().any(|r| r.kind != RefKind::Tag && TRUNKS.contains(&branch_short(r)));
            let default_branch = if conventional { None } else { repo.default_branch() };
            (refs, default_branch, log)
        });
        let op = repo.op_in_progress();
        let stashes = repo.stashes();
        let status = Some(StatusRead::read(repo));
        let worktrees = read_worktrees(repo, busy);
        let refs = refs.map(|(refs, default_branch, log)| {
            let first = log.recv_timeout(Duration::from_millis(400).saturating_sub(started.elapsed())).ok();
            Refs { refs, default_branch, log, first }
        });
        RepoState { head_name, sig, refs, op, stashes, status, worktrees }
    }
}

/// The working copy: its status, and the lines each changed file adds and removes.
pub struct StatusRead {
    changes: Result<Vec<Change>, String>,
    /// Lines added and removed per file: not staged, staged.
    stats: [HashMap<String, (u32, u32)>; 2],
    /// Lines in each untracked file.
    untracked: HashMap<String, u32>,
}

impl StatusRead {
    pub(super) fn read(repo: &Repo) -> StatusRead {
        let changes = repo.status();
        let stats = [repo.numstat(false), repo.numstat(true)];
        let mut budget = 64_000_000;
        let untracked = match &changes {
            Ok(cs) => cs.iter().filter(|c| c.untracked()).map(|c| (c.path.clone(), git::line_count(&repo.root.join(&c.path), &mut budget))).collect(),
            Err(_) => HashMap::new(),
        };
        StatusRead { changes, stats, untracked }
    }
}

/// The repository's working trees, with how many files changed in each of the others (not in
/// one a job is busy with, a bare or a missing one; this one's count is its status').
fn read_worktrees(repo: &Repo, busy: &[PathBuf]) -> Vec<Worktree> {
    let mut wts = repo.worktrees();
    for w in &mut wts {
        let skip = w.bare || w.prunable || busy.contains(&w.path) || git::same_path(&w.path, &repo.root);
        w.changes = if skip { 0 } else { git::change_count(&w.path) };
    }
    wts
}

/// A repository read for a tab, ready to show (`GitApp::opened`).
pub struct Opened {
    repo: Repo,
    hidden: Vec<String>,
    state: RepoState,
}

/// Reads a repository the way opening it does. Any thread can do this.
pub fn read_repo(path: &Path) -> Result<Opened, String> {
    let repo = Repo::open(path)?;
    let mut hidden = repo.hidden_refs();
    hidden.sort();
    let state = RepoState::read(&repo, &hidden, None, &[]);
    Ok(Opened { repo, hidden, state })
}

/// A refresh being read on a worker thread, and how far the state had come when it started.
pub(super) struct Refresh {
    rx: mpsc::Receiver<RepoState>,
    full: u64,
    status: u64,
}

impl GitApp {
    /// Opens a repository here and now.
    pub(super) fn open(&mut self, path: PathBuf) {
        match read_repo(&path) {
            Ok(o) => {
                self.opened(o);
                self.pick_head();
            }
            Err(e) => {
                if self.repo.is_some() {
                    self.toast_err(format!("{}: {e}", path.display()));
                }
            }
        }
    }

    /// Shows a repository `read_repo` read (on a worker thread, for a tab).
    pub fn opened(&mut self, o: Opened) {
        self.pending = None;
        self.refresh = None;
        self.hidden = o.hidden.into_iter().collect();
        self.repo = Some(o.repo);
        self.sel = Sel::Wip;
        self.selected = None;
        self.wip_file = None;
        self.file_view = None;
        self.undo.clear();
        self.tree = TreeView::default();
        self.graph_scroll = Scroll::default();
        self.copied.clear();
        self.refs_sig.clear();
        self.apply_state(o.state);
        // With nothing to commit it opens on the latest commit, once the history has it. Like
        // lazygit, the keys start in the files when there are changes, else in the commits.
        let clean = self.changes.is_empty();
        self.select_head = clean;
        self.pane = if clean { Pane::Commits } else { Pane::Files };
    }

    /// Selects HEAD's commit for a repository that opened with nothing to commit, as soon as
    /// the history has it (unless something else was picked meanwhile).
    pub(super) fn pick_head(&mut self) {
        if !self.select_head {
            return;
        }
        match self.head_row {
            Some(h) => {
                self.select_head = false;
                if self.sel == Sel::Wip && self.changes.is_empty() {
                    self.select(Sel::Commit(h));
                }
            }
            // No commits at all.
            None if !self.loading => self.select_head = false,
            None => {}
        }
    }

    /// Reloads refs + status, here and now (after the app changed something). History is only
    /// re-streamed when a ref or HEAD actually moved, so refreshes on huge repositories stay cheap.
    pub(super) fn reload_all(&mut self, force: bool) {
        let Some(repo) = &self.repo else { return };
        let st = RepoState::read(repo, &self.hidden_sorted(), (!force).then_some(self.refs_sig.as_str()), &self.busy_worktrees());
        self.apply_state(st);
    }

    /// Catches up with what changed outside the app (a terminal, another tool) without holding
    /// up the window: a worker thread reads it, `take_refresh` takes it in.
    pub fn on_focus(&mut self) {
        let Some(repo) = &self.repo else { return };
        if self.job.is_some() || self.refresh.is_some() {
            return;
        }
        let (repo, hidden, seen, busy) = (repo.clone(), self.hidden_sorted(), self.refs_sig.clone(), self.busy_worktrees());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(RepoState::read(&repo, &hidden, Some(&seen), &busy));
        });
        self.refresh = Some(Refresh { rx, full: self.full_gen, status: self.status_gen });
    }

    /// Takes in a refresh read in the background, unless the app reloaded since it started: a
    /// full reload makes it old news, a reload of the working copy just its status.
    pub(super) fn take_refresh(&mut self, ui: &mut Ui) {
        let Some(r) = &self.refresh else { return };
        let mut st = match r.rx.try_recv() {
            Ok(st) => st,
            Err(mpsc::TryRecvError::Empty) => {
                ui.wake_in(Duration::from_millis(30));
                return;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.refresh = None;
                return;
            }
        };
        let (full, status) = (r.full, r.status);
        self.refresh = None;
        if full != self.full_gen {
            return;
        }
        if status != self.status_gen {
            st.status = None;
        }
        self.apply_state(st);
    }

    /// Tests: waits until the background refresh has been read (not taken in yet).
    #[cfg(test)]
    pub(super) fn refresh_read(&mut self) {
        if let Some(r) = &mut self.refresh {
            let st = r.rx.recv().expect("the refresh never came back");
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(st);
            r.rx = rx;
        }
    }

    fn hidden_sorted(&self) -> Vec<String> {
        let mut h: Vec<String> = self.hidden.iter().cloned().collect();
        h.sort();
        h
    }

    fn busy_worktrees(&self) -> Vec<PathBuf> {
        self.wt_jobs.iter().map(|j| j.wt.path.clone()).collect()
    }

    /// Takes in what a refresh read.
    fn apply_state(&mut self, st: RepoState) {
        self.full_gen += 1;
        if let Some(r) = st.refs {
            self.refs_sig = st.sig;
            self.head_name = st.head_name;
            self.local_names = r.refs.iter().filter(|r| r.kind == RefKind::Local).map(|r| r.name.clone()).collect();
            self.refs = r.refs;
            self.default_branch = r.default_branch;
            self.restore_sel = self.selected.as_ref().map(|c| c.hash.clone());
            self.commits.clear();
            self.graph = Graph::default();
            self.hash_index.clear();
            self.head_row = None;
            self.marks.clear();
            self.chain = None;
            self.owners.clear();
            self.lineage = None;
            self.log_rx = Some(r.log);
            self.loading = true;
            self.take_log(r.first.into_iter().collect());
        }
        self.op = st.op;
        self.stashes = st.stashes;
        if self.stash_view.as_ref().is_some_and(|h| !self.stashes.iter().any(|s| &s.hash == h)) {
            self.close_stash();
        }
        if let Some(s) = st.status {
            self.apply_status(s);
        }
        self.worktrees = st.worktrees;
        if let Some(repo) = &self.repo {
            let n = self.changes.len();
            for w in self.worktrees.iter_mut().filter(|w| git::same_path(&w.path, &repo.root)) {
                w.changes = n;
            }
        }
    }

    pub(super) fn reload_status(&mut self) {
        let Some(repo) = &self.repo else { return };
        let s = StatusRead::read(repo);
        self.apply_status(s);
    }

    /// Takes in the working copy's status: the two file trees, and the file the diff shows.
    fn apply_status(&mut self, s: StatusRead) {
        self.status_gen += 1;
        match s.changes {
            Ok(c) => self.changes = c,
            Err(e) => self.toast_err(e),
        }
        let entries = |staged: bool| -> Vec<Entry> {
            self.changes
                .iter()
                .filter(|c| !c.conflicted() && if staged { c.staged() } else { c.unstaged() })
                .map(|c| {
                    let (adds, dels) = match s.stats[staged as usize].get(&c.path) {
                        Some(&n) => n,
                        None if c.untracked() => (s.untracked.get(&c.path).copied().unwrap_or(0), 0),
                        None => (0, 0),
                    };
                    let from = c.orig.clone().filter(|_| staged);
                    Entry { path: c.path.clone(), from, badge: status_badge(if staged { c.x } else { c.y }), adds, dels }
                })
                .collect()
        };
        let (unstaged, staged) = (entries(false), entries(true));
        self.unstaged_tree.set(unstaged);
        self.staged_tree.set(staged);
        let valid = self.wip_file.as_ref().is_some_and(|(p, staged)| {
            self.changes.iter().any(|c| &c.path == p && if *staged { c.staged() } else { c.unstaged() })
        });
        if !valid {
            // The top of the lists: a conflict, else the first unstaged file, else staged.
            let conflict = self.changes.iter().find(|c| c.conflicted()).map(|c| (c.path.clone(), false));
            let unstaged = self.unstaged_tree.first_file().map(|p| (p.to_string(), false));
            let staged = self.staged_tree.first_file().map(|p| (p.to_string(), true));
            self.wip_file = conflict.or(unstaged).or(staged);
        }
        if self.showing_wip() {
            self.load_wip_diff();
        }
    }

    /// While a tab's repository is being read: its name, and that it's coming.
    pub(super) fn draw_pending(&self, ui: &mut Ui, r: Rect) {
        let Some(p) = &self.pending else { return };
        ui.p.rect(r, theme::BG);
        let msg = format!("Opening {}…", dir_name(p));
        let w = ui.p.measure(UI, 14.0, &msg);
        ui.p.text(r.x + (r.w - w) * 0.5, r.cy() - 30.0, UI, 14.0, &msg, theme::DIM);
    }
}

/// A folder's name ("wgitpui" for ~/Dev/wgitpui), the way a tab shows a repository.
pub fn dir_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into())
}
