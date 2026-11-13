//! The git client itself: layout, views and actions. Everything is redrawn from state each frame.

use crate::chrome;
use crate::diffview::{BlameSrc, DiffAction, DiffMode, DiffView};
use crate::fileview::{FileAction, FileView, TreeAction, TreeView};
use crate::font::{MONO, UI};
use crate::git::{self, Change, Commit, DiffOpts, LogMsg, MergePreview, OpKind, Ref, RefKind, Repo, Side, Worktree};
use crate::graph::Graph;
use crate::palette::{fuzzy, Act, Item, Palette, WtFrom};
use crate::rebase::{self, ChainItem, RebaseOp};
use crate::resolve::{ConflictDoc, ResolveAction};
use crate::ui::{theme, Color, Cursor, Key, Rect, Scroll, TextEdit, Ui};
use crate::workspace::tilde;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const TOP_H: f32 = 46.0;
const SIDEBAR_W: f32 = 236.0;
const ROW_H: f32 = 28.0;
const LANE_W: f32 = 15.0;
const MAX_LANES_SHOWN: usize = 14;
const LIST_ROW_H: f32 = 24.0;
const MINIMAP_W: f32 = 12.0;
const MAX_CHAIN: usize = 500;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sel {
    Wip,
    Commit(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    None,
    Message,
    Palette,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    Split,
    /// The divider between the left column and the diff (wide layouts).
    ColSplit,
    Minimap,
    Scrubber,
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Changes,
    Files,
}

#[derive(Clone, Copy, PartialEq)]
enum Layout {
    Auto,
    Unified,
    Split,
}

/// One side of a comparison. `rev` is a full hash; None means the working tree.
#[derive(Clone, PartialEq)]
struct End {
    rev: Option<String>,
    label: String,
}

struct Compare {
    a: End,
    b: End,
    /// `a...b`: only what `b` changed since it diverged from `a`.
    merge_base: bool,
    ahead: usize,
    behind: usize,
    range: HashSet<usize>,
    files_scroll: Scroll,
}

struct MenuItem {
    label: String,
    /// Secondary text, right-aligned and dimmed.
    detail: String,
    /// None = separator.
    act: Option<Act>,
    danger: bool,
}

/// Right-click / drop context menu.
struct Menu {
    at: (f32, f32),
    title: String,
    note: Option<(String, Color)>,
    items: Vec<MenuItem>,
    /// The click that opened the menu must not also close it.
    fresh: bool,
}

fn item(label: impl Into<String>, act: Act) -> MenuItem {
    MenuItem { label: label.into(), detail: String::new(), act: Some(act), danger: false }
}

fn danger(label: impl Into<String>, act: Act) -> MenuItem {
    MenuItem { label: label.into(), detail: String::new(), act: Some(act), danger: true }
}

fn sep() -> MenuItem {
    MenuItem { label: String::new(), detail: String::new(), act: None, danger: false }
}

/// A branch label being dragged onto another branch.
struct BDrag {
    name: String,
    start: (f32, f32),
    active: bool,
}

/// Every destructive action records how to reverse itself.
#[derive(Clone)]
enum Undo {
    /// A patch that was reverse-applied to the working tree.
    Patch { patch: String, label: String },
    /// An untracked file that was deleted.
    Restore { path: String, data: Vec<u8>, label: String },
    /// HEAD moved (commit, amend, rebase); go back to `to`.
    Reset { to: String, soft: bool, label: String },
    Many(Vec<Undo>, String),
    /// Run a git command (e.g. recreate a deleted branch).
    Cmd { args: Vec<String>, label: String },
    /// Move `branch` back to `to` (switching to it first), then optionally switch to `back_to`.
    BranchMove { branch: Option<String>, to: String, back_to: Option<String>, label: String },
}

impl Undo {
    fn label(&self) -> &str {
        match self {
            Undo::Patch { label, .. }
            | Undo::Restore { label, .. }
            | Undo::Reset { label, .. }
            | Undo::Many(_, label)
            | Undo::Cmd { label, .. }
            | Undo::BranchMove { label, .. } => label,
        }
    }
}

struct Toast {
    msg: String,
    err: bool,
    until: Instant,
    undo: bool,
}

struct Job {
    label: String,
    rx: mpsc::Receiver<Result<String, String>>,
}

/// A worktree being added (checked out) or removed (its folder deleted, build output and all)
/// on a background thread; both can take a while. Its sidebar row shows a spinner meanwhile.
struct WtJob {
    wt: Worktree,
    adding: bool,
    rx: mpsc::Receiver<Result<String, String>>,
    /// A fresh add / remove: its undo step, recorded once git is done (and a new worktree opens
    /// in a tab). An undo that adds or removes a worktree: that step itself, which goes back on
    /// the undo stack if it fails.
    undo: Undo,
    is_undo: bool,
}

impl WtJob {
    fn status(&self) -> &'static str {
        if self.adding { "adding…" } else { "removing…" }
    }
}

/// An in-progress drag of a commit row in the graph (interactive rebase).
struct GDrag {
    src: usize,
    start: (f32, f32),
    active: bool,
    target: Option<(usize, RebaseOp)>,
    pushed: HashSet<usize>,
}

pub struct GitApp {
    repo: Option<Repo>,
    // History (streamed in the background).
    commits: Vec<Commit>,
    graph: Graph,
    hash_index: HashMap<String, usize>,
    refs: Vec<Ref>,
    local_names: HashSet<String>,
    marks: Vec<(usize, Color)>,
    head_row: Option<usize>,
    head_name: String,
    refs_sig: String,
    log_rx: Option<mpsc::Receiver<LogMsg>>,
    loading: bool,
    restore_sel: Option<String>,
    chain: Option<Vec<usize>>,
    // Working copy.
    changes: Vec<Change>,
    /// Every working tree of the repository, this one included (change counts filled in).
    worktrees: Vec<Worktree>,
    op: Option<OpKind>,
    /// Conflict resolver: parsed documents (keeps per-file progress) and the open one.
    docs: HashMap<String, ConflictDoc>,
    resolving: Option<String>,
    wide_before: Option<bool>,
    menu: Option<Menu>,
    bdrag: Option<BDrag>,
    ref_hits: Vec<(Rect, String)>,
    /// Full ref names hidden from the graph and branch lists.
    hidden: HashSet<String>,
    // Selection + panels.
    sel: Sel,
    selected: Option<Commit>,
    wip_file: Option<(String, bool)>,
    info: Option<(String, String, String)>,
    diff: DiffView,
    msg: TextEdit,
    amend: bool,
    focus: Focus,
    palette: Palette,
    tab: Tab,
    tree: TreeView,
    file_view: Option<FileView>,
    playing: Option<Instant>,
    compare: Option<Compare>,
    layout: Layout,
    opts: DiffOpts,
    wide: bool,
    /// Show who / which commit / when next to diff lines.
    authors: bool,
    // Scrolling + dragging.
    graph_scroll: Scroll,
    graph_view_h: f32,
    side_scroll: Scroll,
    unstaged_scroll: Scroll,
    staged_scroll: Scroll,
    files_scroll: Scroll,
    right_w: Option<f32>,
    /// Width of the left column in wide layouts (working copy and commit; None = automatic).
    col_w: Option<f32>,
    drag: Drag,
    gdrag: Option<GDrag>,
    // Feedback.
    toast: Option<Toast>,
    job: Option<Job>,
    wt_jobs: Vec<WtJob>,
    undo: Vec<Undo>,
    /// Hosted in a tab: the tab strip shows the repo name, and "open repository" requests go
    /// to the workspace instead of replacing this repo.
    pub in_tab: bool,
    pub open_requests: Vec<PathBuf>,
    /// Tabs to close because their worktree was removed.
    pub close_requests: Vec<PathBuf>,
    /// Where the Push / Pull / Fetch buttons drop their menus (updated as the top bar draws).
    remote_btns: [(f32, f32); 3],
    /// Recently opened repositories (for the palette), provided by the workspace.
    pub recent: Vec<PathBuf>,
}

impl GitApp {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut app = GitApp {
            repo: None,
            commits: vec![],
            graph: Graph::default(),
            hash_index: HashMap::new(),
            refs: vec![],
            local_names: HashSet::new(),
            marks: vec![],
            head_row: None,
            head_name: String::new(),
            refs_sig: String::new(),
            log_rx: None,
            loading: false,
            restore_sel: None,
            chain: None,
            changes: vec![],
            worktrees: vec![],
            op: None,
            docs: HashMap::new(),
            resolving: None,
            wide_before: None,
            menu: None,
            bdrag: None,
            ref_hits: vec![],
            hidden: HashSet::new(),
            sel: Sel::Wip,
            selected: None,
            wip_file: None,
            info: None,
            diff: DiffView::empty(),
            msg: TextEdit::default(),
            amend: false,
            focus: Focus::None,
            palette: Palette::default(),
            tab: Tab::Changes,
            tree: TreeView::default(),
            file_view: None,
            playing: None,
            compare: None,
            layout: Layout::Auto,
            opts: DiffOpts::default(),
            wide: false,
            authors: true,
            graph_scroll: Scroll::default(),
            graph_view_h: 600.0,
            side_scroll: Scroll::default(),
            unstaged_scroll: Scroll::default(),
            staged_scroll: Scroll::default(),
            files_scroll: Scroll::default(),
            right_w: None,
            col_w: None,
            drag: Drag::None,
            gdrag: None,
            toast: None,
            job: None,
            wt_jobs: vec![],
            undo: vec![],
            in_tab: false,
            open_requests: vec![],
            close_requests: vec![],
            remote_btns: [(f32::MAX, TOP_H + 6.0); 3],
            recent: vec![],
        };
        let path = path.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        app.open(path);
        app
    }

    pub fn title(&self) -> String {
        match &self.repo {
            Some(r) => format!("{} — {} · wgitpui", r.name(), self.head_name),
            None => "wgitpui".into(),
        }
    }

    fn open(&mut self, path: PathBuf) {
        match Repo::open(&path) {
            Ok(repo) => {
                self.hidden = repo.hidden_refs().into_iter().collect();
                self.repo = Some(repo);
                self.sel = Sel::Wip;
                self.selected = None;
                self.wip_file = None;
                self.file_view = None;
                self.undo.clear();
                self.tree = TreeView::default();
                self.graph_scroll = Scroll::default();
                self.reload_all(true);
                if self.changes.is_empty() {
                    if let Some(h) = self.head_row {
                        self.select(Sel::Commit(h));
                    }
                }
            }
            Err(e) => {
                if self.repo.is_some() {
                    self.toast_err(format!("{}: {e}", path.display()));
                }
            }
        }
    }

    // ---- hooks used by main.rs (screenshot mode) -------------------------------------------

    /// Blocks until the whole history has streamed in.
    pub fn finish_loading(&mut self) {
        while self.loading {
            self.drain_log(Some(Duration::from_millis(200)));
        }
    }

    pub fn select_row(&mut self, i: usize) {
        if i < self.commits.len() {
            self.select(Sel::Commit(i));
            self.graph_scroll.jump((i + 1) as f32 * ROW_H - self.graph_view_h * 0.4);
            self.graph_scroll.offset = self.graph_scroll.target;
        }
    }

    pub fn open_palette(&mut self, query: &str) {
        self.focus = Focus::Palette;
        self.palette.reset(query);
    }

    pub fn show_files(&mut self, open: Option<String>) {
        if let Sel::Commit(_) = self.sel {
            self.tab = Tab::Files;
            self.load_tree();
            if let (Some(p), Some(repo), Some(c)) = (open, &self.repo, &self.selected) {
                self.file_view = Some(FileView::load(repo, Some(c.hash.clone()), p, true));
            }
        }
    }

    /// Screenshot hook: compare two history rows and pick the diff layout.
    pub fn debug_compare(&mut self, a: usize, b: usize, split: Option<bool>, wide: bool) {
        if a < self.commits.len() && b < self.commits.len() {
            let (ea, eb) = (self.end_for_row(a), self.end_for_row(b));
            self.start_compare(ea, eb, false);
        }
        if let Some(sp) = split {
            self.layout = if sp { Layout::Split } else { Layout::Unified };
        }
        self.wide = wide;
    }

    /// Screenshot hook: open a branch's context menu.
    pub fn debug_branch_menu(&mut self, name: &str, at: (f32, f32)) {
        self.open_branch_menu(name, at);
    }

    /// Screenshot hook: open the resolver on `path`, applying per-block choices
    /// ("o" yours, "t" incoming, "b" both, "-" leave unresolved).
    pub fn debug_resolve(&mut self, path: &str, choices: &str) {
        self.open_resolver(path);
        if let Some(d) = self.docs.get_mut(path) {
            d.debug_choose(choices);
        }
    }

    /// Screenshot hook: open the Push / Pull / Fetch remote picker.
    pub fn debug_remote_menu(&mut self, kind: &str, at: (f32, f32)) {
        match kind {
            "pull" => self.open_pull_menu(at),
            "fetch" => self.open_fetch_menu(at),
            _ => self.open_push_menu(at),
        }
    }

    pub fn on_focus(&mut self) {
        if self.job.is_none() {
            self.reload_all(false);
        }
    }

    // ---- feedback ---------------------------------------------------------------------------

    fn toast_ok(&mut self, msg: impl Into<String>) {
        self.toast = Some(Toast { msg: msg.into(), err: false, until: Instant::now() + Duration::from_secs(3), undo: false });
    }

    fn toast_err(&mut self, msg: impl Into<String>) {
        self.toast = Some(Toast { msg: msg.into(), err: true, until: Instant::now() + Duration::from_secs(7), undo: false });
    }

    fn result(&mut self, r: Result<String, String>, ok: &str) -> bool {
        match r {
            Ok(_) => {
                if !ok.is_empty() {
                    self.toast_ok(ok);
                }
                true
            }
            Err(e) => {
                self.toast_err(e);
                false
            }
        }
    }

    // ---- undo -------------------------------------------------------------------------------

    /// Records an undo step, and keeps a copy of discarded content in .git/wgitpui-trash so
    /// nothing is ever truly lost, even across restarts.
    fn push_undo(&mut self, u: Undo) {
        if let Some(repo) = &self.repo {
            let dir = repo.git_path("wgitpui-trash");
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
            let mut n = 0;
            let mut save = |u: &Undo| {
                n += 1;
                match u {
                    Undo::Patch { patch, .. } => {
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::fs::write(dir.join(format!("{stamp}-{n}.patch")), patch);
                    }
                    Undo::Restore { path, data, .. } => {
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::fs::write(dir.join(format!("{stamp}-{n}-{}", path.replace('/', "__"))), data);
                    }
                    _ => {}
                }
            };
            match &u {
                Undo::Many(items, _) => items.iter().for_each(&mut save),
                other => save(other),
            }
        }
        let label = u.label().to_string();
        self.undo.push(u);
        let key = if cfg!(target_os = "macos") { "⌘Z" } else { "Ctrl+Z" };
        self.toast = Some(Toast {
            msg: format!("Did: {label}  ·  {key} to undo"),
            err: false,
            until: Instant::now() + Duration::from_secs(6),
            undo: true,
        });
    }

    fn apply_undo(repo: &Repo, u: &Undo) -> Result<String, String> {
        match u {
            Undo::Patch { patch, .. } => repo.apply_worktree(patch, false),
            Undo::Restore { path, data, .. } => {
                let p = repo.root.join(path);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(p, data).map(|_| String::new()).map_err(|e| e.to_string())
            }
            Undo::Reset { to, soft, .. } => repo.git(&["reset", if *soft { "--soft" } else { "--keep" }, to]),
            Undo::Many(items, _) => {
                for it in items.iter().rev() {
                    Self::apply_undo(repo, it)?;
                }
                Ok(String::new())
            }
            Undo::Cmd { args, .. } => {
                let a: Vec<&str> = args.iter().map(String::as_str).collect();
                repo.git(&a)
            }
            Undo::BranchMove { branch, to, back_to, .. } => {
                if let Some(b) = branch {
                    if repo.head_name() != *b {
                        repo.git(&["switch", b])?;
                    }
                }
                repo.git(&["reset", "--keep", to])?;
                if let Some(b) = back_to {
                    repo.git(&["switch", b])?;
                }
                Ok(String::new())
            }
        }
    }

    fn undo(&mut self) {
        // A worktree job in flight hasn't recorded its step yet; don't undo an older one instead.
        if let Some(j) = self.wt_jobs.first() {
            let verb = if j.adding { "adding" } else { "removing" };
            self.toast_err(format!("Still {verb} {} — undo when it's done", j.wt.name()));
            return;
        }
        let (Some(repo), Some(u)) = (&self.repo, self.undo.pop()) else {
            self.toast_ok("Nothing to undo");
            return;
        };
        // Undoing "add worktree" deletes a whole tree, undoing "remove worktree" checks one out
        // again: off the UI thread, like any worktree add / remove.
        let (removes, adds) = (removed_worktrees(&u), added_worktrees(&u));
        if let Some(p) = removes.first().or(adds.first()) {
            let wt = self.worktrees.iter().find(|w| w.path == *p).cloned().unwrap_or_else(|| Worktree { path: p.clone(), ..Default::default() });
            let adding = removes.is_empty();
            let (root, step) = (repo.root.clone(), u.clone());
            self.close_requests.extend(removes);
            self.start_wt_job(wt, adding, u, true, move || Self::apply_undo(&Repo { root }, &step));
            return;
        }
        let r = Self::apply_undo(repo, &u);
        let label = u.label().to_string();
        if self.result(r, &format!("Undid: {label}")) {
            self.reload_all(false);
        } else {
            self.undo.push(u);
        }
    }

    /// A worktree at `path` is being added or removed in the background.
    fn wt_busy(&self, path: &Path) -> bool {
        self.wt_jobs.iter().any(|j| j.wt.path == path)
    }

    /// Runs `work` (a worktree add or remove) on its own thread; the sidebar tracks it.
    fn start_wt_job(&mut self, wt: Worktree, adding: bool, undo: Undo, is_undo: bool, work: impl FnOnce() -> Result<String, String> + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        self.wt_jobs.push(WtJob { wt, adding, rx, undo, is_undo });
    }

    /// Picks up finished worktree jobs: records undo (opening new worktrees in a tab) or
    /// reports the error, then refreshes.
    fn poll_wt_jobs(&mut self, ui: &mut Ui) {
        if self.wt_jobs.is_empty() {
            return;
        }
        let mut done = Vec::new();
        let mut i = 0;
        while i < self.wt_jobs.len() {
            match self.wt_jobs[i].rx.try_recv() {
                Ok(res) => done.push((self.wt_jobs.remove(i), res)),
                Err(mpsc::TryRecvError::Disconnected) => done.push((self.wt_jobs.remove(i), Err("worktree job stopped unexpectedly".into()))),
                Err(mpsc::TryRecvError::Empty) => i += 1,
            }
        }
        if !self.wt_jobs.is_empty() {
            ui.wake_in(Duration::from_millis(100));
        }
        if done.is_empty() {
            return;
        }
        let mut open = Vec::new();
        for (j, res) in done {
            match (res, j.is_undo) {
                (Ok(_), false) => {
                    if j.adding {
                        open.push(j.wt.path.display().to_string());
                    }
                    self.push_undo(j.undo);
                }
                (Ok(_), true) => self.toast_ok(format!("Undid: {}", j.undo.label())),
                (Err(e), false) => self.toast_err(e),
                (Err(e), true) => {
                    self.undo.push(j.undo);
                    self.toast_err(e);
                }
            }
        }
        self.reload_all(false);
        for p in open {
            self.act(Act::OpenWorktree(p));
        }
    }

    // ---- loading ----------------------------------------------------------------------------

    /// Reloads refs + status. History is only re-streamed when a ref or HEAD actually moved,
    /// so focus-triggered refreshes on huge repositories stay cheap.
    fn reload_all(&mut self, force: bool) {
        let Some(repo) = &self.repo else { return };
        let head_name = repo.head_name();
        let mut hidden: Vec<String> = self.hidden.iter().cloned().collect();
        hidden.sort();
        let sig = format!("{}\n{head_name}\n{}", repo.refs_signature(), hidden.join(","));
        if force || sig != self.refs_sig {
            self.refs_sig = sig;
            self.head_name = head_name;
            self.refs = repo.refs().unwrap_or_default();
            self.local_names = self.refs.iter().filter(|r| r.kind == RefKind::Local).map(|r| r.name.clone()).collect();
            self.restore_sel = self.selected.as_ref().map(|c| c.hash.clone());
            self.commits.clear();
            self.graph = Graph::default();
            self.hash_index.clear();
            self.head_row = None;
            self.marks.clear();
            self.chain = None;
            self.log_rx = Some(git::stream_log(repo.root.clone(), hidden));
            self.loading = true;
            // Wait briefly for the first batch so the graph doesn't flash empty.
            self.drain_log(Some(Duration::from_millis(400)));
        }
        if let Some(repo) = &self.repo {
            self.op = repo.op_in_progress();
        }
        self.reload_status();
        self.reload_worktrees();
    }

    /// Lists the repository's working trees and counts the changes in each of the others.
    fn reload_worktrees(&mut self) {
        let Some(repo) = &self.repo else { return };
        let mut wts = repo.worktrees();
        for w in &mut wts {
            w.changes = if git::same_path(&w.path, &repo.root) {
                self.changes.len()
            } else if w.bare || w.prunable || self.wt_jobs.iter().any(|j| j.wt.path == w.path) {
                0
            } else {
                git::change_count(&w.path)
            };
        }
        self.worktrees = wts;
    }

    fn is_current_wt(&self, w: &Worktree) -> bool {
        self.repo.as_ref().is_some_and(|r| git::same_path(&w.path, &r.root))
    }

    /// The other working trees (not this one, not a bare main entry), in `git worktree list` order.
    fn other_worktrees(&self) -> Vec<Worktree> {
        self.worktrees.iter().filter(|w| !w.bare && !self.is_current_wt(w)).cloned().collect()
    }

    /// Another worktree that has `branch` checked out (git won't check it out a second time).
    fn branch_worktree(&self, branch: &str) -> Option<&Worktree> {
        self.worktrees.iter().find(|w| w.branch.as_deref() == Some(branch) && !self.is_current_wt(w) && !self.wt_busy(&w.path))
    }

    /// The other worktrees for the sidebar, each with "adding…" / "removing…" while a background
    /// job works on it. Those stay listed until git is done, whatever a refresh in between sees.
    fn sidebar_worktrees(&self) -> Vec<(Worktree, Option<&'static str>)> {
        let busy = |p: &Path| self.wt_jobs.iter().find(|j| j.wt.path == p).map(WtJob::status);
        let mut v: Vec<(Worktree, Option<&'static str>)> = self.other_worktrees().into_iter().map(|w| {
            let b = busy(&w.path);
            (w, b)
        }).collect();
        for j in &self.wt_jobs {
            if !v.iter().any(|(w, _)| w.path == j.wt.path) && !self.is_current_wt(&j.wt) {
                v.push((j.wt.clone(), Some(j.status())));
            }
        }
        v
    }

    /// A local branch no worktree has checked out.
    fn branch_free(&self, branch: &str) -> bool {
        self.local_names.contains(branch)
            && branch != self.head_name
            && !self.worktrees.iter().any(|w| w.branch.as_deref() == Some(branch))
    }

    /// Starting point for a new worktree: the selected commit (checking out its branch when one
    /// is free), or the current HEAD.
    fn wt_from_selection(&self) -> Option<WtFrom> {
        if let Some(i) = self.sel_row() {
            let c = &self.commits[i];
            let free = c.refs.iter().map(|r| r.strip_prefix("HEAD -> ").unwrap_or(r)).find(|r| self.branch_free(r)).map(String::from);
            return Some(match free {
                Some(b) => WtFrom { rev: b.clone(), label: b.clone(), branch: Some(b), prefill: String::new() },
                None => WtFrom { rev: c.hash.clone(), label: c.short().to_string(), branch: None, prefill: String::new() },
            });
        }
        let head = self.repo.as_ref()?.rev_parse("HEAD")?;
        Some(WtFrom { rev: head, label: self.head_name.clone(), branch: None, prefill: String::new() })
    }

    /// Default location for a new worktree: next to the main one, as `<repo>-<name>`.
    fn default_wt_path(&self, name: &str) -> String {
        let main = self.worktrees.first().map(|w| w.path.clone()).or_else(|| self.root()).unwrap_or_default();
        let parent = main.parent().map(Path::to_path_buf).unwrap_or_default();
        let base = main.file_name().map_or_else(|| "worktree".into(), |n| n.to_string_lossy().into_owned());
        let slug: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
        let mut p = parent.join(format!("{base}-{slug}"));
        let mut n = 2;
        while p.exists() {
            p = parent.join(format!("{base}-{slug}-{n}"));
            n += 1;
        }
        p.display().to_string()
    }

    fn drain_log(&mut self, wait: Option<Duration>) {
        let Some(rx) = &self.log_rx else { return };
        let mut msgs = Vec::new();
        if let Some(d) = wait {
            if let Ok(m) = rx.recv_timeout(d) {
                msgs.push(m);
            }
        }
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
        }
        if msgs.is_empty() {
            return;
        }
        let mut done = false;
        for m in msgs {
            match m {
                LogMsg::Batch(cs) => {
                    for c in cs {
                        let i = self.commits.len();
                        self.graph.push(&c);
                        self.hash_index.insert(c.hash.clone(), i);
                        if self.head_row.is_none() && c.is_head() {
                            self.head_row = Some(i);
                        }
                        if self.restore_sel.as_deref() == Some(c.hash.as_str()) {
                            self.restore_sel = None;
                            self.sel = Sel::Commit(i);
                        }
                        self.commits.push(c);
                    }
                }
                LogMsg::Done => done = true,
                LogMsg::Err(e) => self.toast_err(e),
            }
        }
        self.chain = None;
        self.marks = self
            .refs
            .iter()
            .filter(|r| !self.hidden.contains(&ref_full(r)))
            .filter_map(|r| {
                let row = *self.hash_index.get(&r.target)?;
                let c = match r.kind {
                    RefKind::Local => theme::ACCENT,
                    RefKind::Remote => theme::CYAN.alpha(0.55),
                    RefKind::Tag => theme::YELLOW.alpha(0.8),
                };
                Some((row, c))
            })
            .collect();
        if done {
            self.graph.finish();
            self.loading = false;
            self.log_rx = None;
            if self.restore_sel.take().is_some() {
                // The selected commit no longer exists (e.g. rewritten by a rebase).
                match self.head_row {
                    Some(h) if self.changes.is_empty() => self.select(Sel::Commit(h)),
                    _ => self.select(Sel::Wip),
                }
            }
        }
    }

    fn reload_status(&mut self) {
        let Some(repo) = &self.repo else { return };
        match repo.status() {
            Ok(c) => self.changes = c,
            Err(e) => self.toast_err(e),
        }
        let valid = self.wip_file.as_ref().is_some_and(|(p, staged)| {
            self.changes.iter().any(|c| &c.path == p && if *staged { c.staged() } else { c.unstaged() })
        });
        if !valid {
            self.wip_file = self
                .changes
                .iter()
                .find(|c| c.unstaged())
                .map(|c| (c.path.clone(), false))
                .or_else(|| self.changes.iter().find(|c| c.staged()).map(|c| (c.path.clone(), true)));
        }
        if self.sel == Sel::Wip {
            self.load_wip_diff();
        }
    }

    fn load_wip_diff(&mut self) {
        let Some(repo) = &self.repo else { return };
        let view = match &self.wip_file {
            None => DiffView::new(vec![], DiffMode::Unstaged, String::new()),
            Some((path, staged)) => {
                let untracked = !staged && self.changes.iter().any(|c| &c.path == path && c.untracked());
                let conflicted = self.changes.iter().any(|c| &c.path == path && c.conflicted());
                let key = format!("{}:{path}", if *staged { "S" } else { "U" });
                if conflicted {
                    DiffView::new(vec![repo.conflict_diff(path)], DiffMode::Commit, key)
                        .note("Conflict: red = HEAD side, green = incoming side. Edit the file or pick a side, then mark it resolved.".into())
                } else if untracked {
                    DiffView::new(vec![repo.untracked_diff(path)], DiffMode::Untracked, key)
                        .with_blame(BlameSrc { root: repo.root.clone(), old: None, new: Some(Side::Worktree) })
                } else {
                    let mode = if *staged { DiffMode::Staged } else { DiffMode::Unstaged };
                    match repo.diff_file(path, *staged, &self.opts) {
                        // Patches from a whitespace-insensitive diff don't apply cleanly, so
                        // staging is disabled while whitespace is hidden.
                        Ok(files) if self.opts.ignore_ws => DiffView::new(files, DiffMode::Commit, key)
                            .note("Whitespace changes hidden — staging and discarding are disabled".into()),
                        Ok(files) => {
                            // Unstaged: index → working tree. Staged: HEAD → index.
                            let (old, new) = if *staged { (Side::Rev("HEAD".into()), Side::Index) } else { (Side::Index, Side::Worktree) };
                            DiffView::new(files, mode, key).with_blame(BlameSrc { root: repo.root.clone(), old: Some(old), new: Some(new) })
                        }
                        Err(e) => DiffView::new(vec![], mode, key).note(e),
                    }
                }
            }
        };
        self.set_diff(view);
    }

    /// Swaps in a new diff, keeping scroll and collapsed files when it's the same content
    /// being re-shown (after staging a hunk, changing context lines, toggling whitespace…).
    fn set_diff(&mut self, view: DiffView) {
        Self::replace_diff(&mut self.diff, view);
    }

    fn replace_diff(old: &mut DiffView, mut view: DiffView) {
        if view.key == old.key {
            view.scroll = std::mem::take(&mut old.scroll);
            view.hscroll = old.hscroll;
            if !old.collapsed.is_empty() {
                view.collapsed = std::mem::take(&mut old.collapsed);
                let c = view.collapsed.clone();
                view.set_all_collapsed(false);
                for f in c {
                    view.toggle_collapse(f);
                }
            }
        }
        *old = view;
    }

    /// The selected commit row, if the selection is valid for the currently loaded history.
    fn sel_row(&self) -> Option<usize> {
        match (self.sel, &self.selected) {
            (Sel::Commit(i), Some(c)) if self.commits.get(i).is_some_and(|x| x.hash == c.hash) => Some(i),
            _ => None,
        }
    }

    fn select(&mut self, sel: Sel) {
        self.sel = sel;
        match sel {
            Sel::Wip => {
                self.selected = None;
                self.info = None;
                self.playing = None;
                if self.file_view.as_ref().is_some_and(|f| f.rev.is_some()) {
                    self.file_view = None;
                }
                self.reload_status();
            }
            Sel::Commit(i) => {
                let Some(repo) = &self.repo else { return };
                let c = self.commits[i].clone();
                let files = repo.show(&c.hash, &self.opts).unwrap_or_default();
                self.info = repo.commit_info(&c.hash).ok();
                let src = Self::commit_blame(repo, &c);
                Self::replace_diff(&mut self.diff, DiffView::new(files, DiffMode::Commit, c.hash.clone()).with_blame(src));
                self.files_scroll = Scroll::default();
                if let Some(fv) = &mut self.file_view {
                    if fv.rev.is_some() {
                        // Time travel: the open file follows the selection through history.
                        fv.rev = Some(c.hash.clone());
                        fv.reload(repo);
                    } else {
                        self.file_view = None;
                    }
                }
                self.selected = Some(c);
                if self.focus == Focus::Message {
                    self.focus = Focus::None;
                }
                if self.tab == Tab::Files {
                    self.load_tree();
                }
            }
        }
    }

    fn load_tree(&mut self) {
        let (Some(repo), Some(c)) = (&self.repo, &self.selected) else { return };
        let changed = self.diff.files.iter().map(|f| f.path.clone()).collect();
        self.tree.load(repo, &c.hash, changed);
    }

    // ---- comparison ------------------------------------------------------------------------

    fn end_for_row(&self, row: usize) -> End {
        let c = &self.commits[row];
        let name = c.refs.iter().map(|r| r.strip_prefix("HEAD -> ").unwrap_or(r)).find(|r| *r != "HEAD" && !r.starts_with("tag: "));
        End { rev: Some(c.hash.clone()), label: name.map_or_else(|| c.short().to_string(), String::from) }
    }

    fn start_compare(&mut self, a: End, b: End, merge_base: bool) {
        if a == b {
            return;
        }
        self.compare = Some(Compare { a, b, merge_base, ahead: 0, behind: 0, range: HashSet::new(), files_scroll: Scroll::default() });
        self.file_view = None;
        self.load_compare();
    }

    /// ⌘/shift-click in the graph: compare the current selection with `row` (older side = A).
    fn compare_with_row(&mut self, row: usize) {
        let clicked = self.end_for_row(row);
        let (a, b) = match self.sel_row() {
            None => (clicked, End { rev: None, label: "Working tree".into() }),
            Some(s) if s == row => return,
            Some(s) if row > s => (clicked, self.end_for_row(s)),
            Some(s) => (self.end_for_row(s), clicked),
        };
        self.start_compare(a, b, false);
    }

    fn load_compare(&mut self) {
        let (Some(repo), Some(c)) = (&self.repo, &mut self.compare) else { return };
        let (a, b) = (c.a.rev.as_deref(), c.b.rev.as_deref());
        let mb = c.merge_base && a.is_some() && b.is_some();
        let key = format!("cmp:{:?}:{:?}:{mb}", a, b);
        let side = |r: Option<&str>| Some(r.map_or(Side::Worktree, |r| Side::Rev(r.to_string())));
        // In merge-base mode the old side is the common ancestor, not A itself.
        let old = match (mb, a, b) {
            (true, Some(a), Some(b)) => repo.git(&["merge-base", a, b]).ok().map(|s| Side::Rev(s.trim().to_string())),
            _ => side(a),
        };
        let src = BlameSrc { root: repo.root.clone(), old, new: side(b) };
        let view = match repo.compare(a, b, mb, &self.opts) {
            Ok(files) => DiffView::new(files, DiffMode::Commit, key).with_blame(src),
            Err(e) => DiffView::new(vec![], DiffMode::Commit, key).note(e),
        };
        let (ra, rb) = (a.unwrap_or("HEAD"), b.unwrap_or("HEAD"));
        c.ahead = repo.count_range(ra, rb);
        c.behind = repo.count_range(rb, ra);
        c.range = repo.range_hashes(ra, rb, 20_000).iter().filter_map(|h| self.hash_index.get(h).copied()).collect();
        self.set_diff(view);
    }

    /// Re-fetches whatever diff is on screen (after changing context or whitespace options).
    fn reload_diff(&mut self) {
        if self.compare.is_some() {
            self.load_compare();
        } else if self.sel == Sel::Wip {
            self.load_wip_diff();
        } else if let (Some(i), Some(repo)) = (self.sel_row(), &self.repo) {
            let c = self.commits[i].clone();
            let files = repo.show(&c.hash, &self.opts).unwrap_or_default();
            let src = Self::commit_blame(repo, &c);
            self.set_diff(DiffView::new(files, DiffMode::Commit, c.hash.clone()).with_blame(src));
        }
    }

    /// A commit's diff: old side = its first parent, new side = the commit.
    fn commit_blame(repo: &Repo, c: &Commit) -> BlameSrc {
        BlameSrc { root: repo.root.clone(), old: c.parents.first().map(|p| Side::Rev(p.clone())), new: Some(Side::Rev(c.hash.clone())) }
    }

    fn split_for(&self, w: f32) -> bool {
        match self.layout {
            Layout::Split => true,
            Layout::Unified => false,
            Layout::Auto => w >= 980.0,
        }
    }

    fn reveal_row(&mut self, row: usize) {
        let y = row as f32 * ROW_H;
        self.graph_scroll.reveal(y - ROW_H, y + 2.0 * ROW_H, self.graph_view_h);
    }

    fn select_hash(&mut self, hash: &str) -> bool {
        match self.hash_index.get(hash).copied() {
            Some(i) => {
                self.select(Sel::Commit(i));
                self.reveal_row(i + 1);
                true
            }
            None => {
                self.toast_err("That commit isn't in the loaded history");
                false
            }
        }
    }

    // ---- actions ----------------------------------------------------------------------------

    fn spawn(&mut self, label: &str, args: Vec<String>) {
        let Some(repo) = &self.repo else { return };
        if self.job.is_some() {
            return;
        }
        let root = repo.root.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let _ = tx.send(git::run(&root, &args, None));
        });
        self.job = Some(Job { label: label.to_string(), rx });
    }

    fn discard_file(&self, path: &str) -> Result<Undo, String> {
        let repo = self.repo.as_ref().ok_or("no repository")?;
        let untracked = self.changes.iter().any(|c| c.path == path && c.untracked());
        if untracked {
            let full = repo.root.join(path);
            let data = std::fs::read(&full).map_err(|e| e.to_string())?;
            std::fs::remove_file(&full).map_err(|e| e.to_string())?;
            Ok(Undo::Restore { path: path.to_string(), data, label: format!("delete {path}") })
        } else {
            let patch = repo.file_patch(path)?;
            if patch.trim().is_empty() {
                return Err(format!("{path} has no unstaged changes"));
            }
            repo.apply_worktree(&patch, true)?;
            Ok(Undo::Patch { patch, label: format!("discard changes to {path}") })
        }
    }

    fn act(&mut self, a: Act) {
        let Some(repo) = &self.repo else {
            if let Act::Open(p) = a {
                if self.in_tab {
                    self.open_requests.push(expand_home(&p));
                } else {
                    self.open(expand_home(&p));
                }
            }
            return;
        };
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match a {
            Act::Refresh => {
                self.reload_all(true);
                self.toast_ok("Refreshed");
            }
            Act::Fetch => self.spawn("Fetching all remotes", own(&["fetch", "--all", "--prune"])),
            Act::FetchFrom(remote) => self.spawn(&format!("Fetching {remote}"), own(&["fetch", "--prune", &remote])),
            Act::FetchMenu => self.open_fetch_menu(self.remote_btns[2]),
            Act::PullMenu => self.open_pull_menu(self.remote_btns[1]),
            Act::PullFrom { remote, branch, mode } => {
                if !self.local_names.contains(&self.head_name) {
                    self.toast_err("Detached HEAD — check out a branch to pull into");
                    return;
                }
                let how: &[&str] = match mode {
                    "rebase" => &["--rebase", "--autostash"],
                    "merge" => &["--no-rebase", "--no-edit"],
                    _ => &["--ff-only"],
                };
                let mut args = own(&["pull"]);
                args.extend(own(how));
                args.extend([remote.clone(), branch]);
                self.spawn(&format!("Pulling from {remote}"), args);
            }
            Act::NewWorktree(from) => {
                self.focus = Focus::Palette;
                self.palette.reset(&from.prefill);
                self.palette.worktree_from = Some(from);
            }
            Act::AddWorktree { path, rev, new_branch, detach } => {
                let full = expand_home(&path);
                let full = if full.is_absolute() { full } else { repo.root.join(full) };
                // Canonical (macOS /tmp → /private/tmp), so undo and tab matching see the path git
                // reports. The worktree doesn't exist yet; its parent folder usually does.
                let full = match (full.parent().and_then(|d| d.canonicalize().ok()), full.file_name()) {
                    (Some(dir), Some(name)) => dir.join(name),
                    _ => full,
                };
                if self.wt_busy(&full) {
                    return;
                }
                let p = full.display().to_string();
                // Undo runs these in reverse: remove the worktree, then the branch made for it.
                let mut steps = Vec::new();
                if let Some(b) = &new_branch {
                    steps.push(Undo::Cmd { args: own(&["branch", "-D", b]), label: String::new() });
                }
                steps.push(Undo::Cmd { args: own(&["worktree", "remove", &p]), label: String::new() });
                let name = full.file_name().map_or_else(|| p.clone(), |n| n.to_string_lossy().into_owned());
                let undo = Undo::Many(steps, format!("add worktree {name}"));
                let branch = new_branch.clone().or_else(|| (!detach).then(|| rev.clone()));
                let wt = Worktree { path: full, branch, ..Default::default() };
                // Checking out a big tree can take a while: git runs on its own thread, the new row
                // shows a spinner, and the worktree opens in a tab once it's ready.
                let root = repo.root.clone();
                self.start_wt_job(wt, true, undo, false, move || Repo { root }.worktree_add(&p, &rev, new_branch.as_deref(), detach));
            }
            Act::OpenWorktree(p) => {
                let path = PathBuf::from(&p);
                if !path.exists() {
                    self.toast_err(format!("{} no longer exists — prune it from the worktree list", tilde(&path)));
                } else if self.in_tab {
                    self.open_requests.push(path);
                } else {
                    self.open(path);
                }
            }
            Act::RemoveWorktree(p) => {
                let Some(w) = self.worktrees.iter().find(|w| w.path == Path::new(&p)).cloned() else { return };
                if w.changes > 0 {
                    let n = w.changes;
                    self.toast_err(format!("{} has {n} changed file{} — commit or discard them in its tab first", w.name(), if n == 1 { "" } else { "s" }));
                    return;
                }
                if w.locked {
                    self.toast_err(format!("{} is locked — unlock it first (git worktree unlock)", w.name()));
                    return;
                }
                if self.wt_busy(&w.path) {
                    return;
                }
                let args = match &w.branch {
                    Some(b) => own(&["worktree", "add", &p, b]),
                    None => own(&["worktree", "add", "--detach", &p, &w.head]),
                };
                let undo = Undo::Cmd { args, label: format!("remove worktree {}", w.name()) };
                // Deleting the folder can take a while: git runs on its own thread, the row shows
                // a spinner, and its tab closes now.
                let root = repo.root.clone();
                self.close_requests.push(w.path.clone());
                self.start_wt_job(w, false, undo, false, move || git::run(&root, &["worktree", "remove", &p], None));
            }
            Act::PruneWorktrees => {
                let r = repo.git(&["worktree", "prune"]);
                self.result(r, "Pruned stale worktrees");
                self.reload_all(false);
            }
            Act::Pull => self.spawn("Pulling", own(&["pull", "--ff-only"])),
            Act::Push => {
                let has_upstream = repo.git(&["rev-parse", "--abbrev-ref", "@{u}"]).is_ok();
                let args = if has_upstream { own(&["push"]) } else { own(&["push", "-u", "origin", "HEAD"]) };
                self.spawn("Pushing", args);
            }
            Act::StageAll => {
                let r = repo.git(&["add", "-A"]);
                self.result(r, "");
                self.reload_status();
            }
            Act::UnstageAll => {
                let paths: Vec<String> = self.changes.iter().filter(|c| c.staged()).map(|c| c.path.clone()).collect();
                if !paths.is_empty() {
                    let p: Vec<&str> = paths.iter().map(String::as_str).collect();
                    let r = repo.unstage(&p);
                    self.result(r, "");
                }
                self.reload_status();
            }
            Act::DiscardAll => {
                let paths: Vec<String> = self.changes.iter().filter(|c| c.unstaged()).map(|c| c.path.clone()).collect();
                let mut done = Vec::new();
                for p in &paths {
                    match self.discard_file(p) {
                        Ok(u) => done.push(u),
                        Err(e) => self.toast_err(e),
                    }
                }
                if !done.is_empty() {
                    let n = done.len();
                    self.push_undo(Undo::Many(done, format!("discard {n} file{}", if n == 1 { "" } else { "s" })));
                }
                self.reload_status();
            }
            Act::Commit => self.commit(),
            Act::ToggleAmend => {
                self.amend = !self.amend;
                if self.amend && self.msg.text.trim().is_empty() {
                    let m = repo.last_message();
                    self.msg.set(&m);
                }
                self.select(Sel::Wip);
            }
            Act::ShowWip => self.select(Sel::Wip),
            Act::CopyHash => {
                if let Some(c) = &self.selected {
                    let h = c.hash.clone();
                    crate::clipboard::copy(&h);
                    self.toast_ok(format!("Copied {}", &h[..8]));
                }
            }
            Act::Undo => self.undo(),
            Act::BrowseFiles => {
                if self.sel_row().is_some() {
                    self.tab = Tab::Files;
                    self.load_tree();
                }
            }
            Act::MoveCommit(up) => self.keyboard_rebase(if up { RebaseOp::Above } else { RebaseOp::Below }),
            Act::SquashIntoParent => self.keyboard_rebase(RebaseOp::Squash),
            Act::DropCommit => self.keyboard_rebase(RebaseOp::Drop),
            Act::OpContinue => {
                if let Some(op) = self.op {
                    let r = repo.op_continue(op);
                    let ok = format!("{} continued", op.name());
                    self.after_op(r, &ok, op.name(), None);
                }
            }
            Act::OpAbort => {
                if let Some(op) = self.op {
                    let r = repo.op_abort(op);
                    self.result(r, &format!("{} aborted", op.name()));
                    self.reload_all(false);
                }
            }
            Act::PullRebase => self.spawn("Pulling", own(&["pull", "--rebase", "--autostash"])),
            Act::PullMerge => self.spawn("Pulling", own(&["pull", "--no-rebase", "--no-edit"])),
            Act::MergeInto { src, dst } => {
                let prev = self.head_name.clone();
                if let Some(d) = &dst {
                    if let Err(e) = repo.git(&["switch", d]) {
                        self.toast_err(e);
                        return;
                    }
                }
                let undo = self.branch_undo(dst.clone(), dst.as_ref().map(|_| prev), format!("merge {src}"));
                let repo = self.repo.as_ref().unwrap();
                let r = repo.git_env(&["merge", "--no-edit", &src], &[("GIT_EDITOR", "true")]);
                let into = dst.unwrap_or_else(|| self.head_name.clone());
                self.after_op(r, &format!("Merged {src} into {into}"), "Merge", undo);
            }
            Act::RebaseOnto { branch, onto } => {
                let prev = self.head_name.clone();
                let target = branch.clone().unwrap_or_else(|| prev.clone());
                let mut args = vec!["rebase", "--autostash", onto.as_str()];
                if let Some(b) = &branch {
                    args.push(b);
                }
                let old = repo.rev_parse(branch.as_deref().unwrap_or("HEAD"));
                let back = branch.as_ref().filter(|b| **b != prev).map(|_| prev.clone());
                let undo = old.map(|to| Undo::BranchMove { branch: branch.clone(), to, back_to: back, label: format!("rebase {target} onto {onto}") });
                let r = repo.git(&args);
                self.after_op(r, &format!("Rebased {target} onto {onto}"), "Rebase", undo);
            }
            Act::FastForward { src, dst } => {
                let (r, undo) = match &dst {
                    Some(d) if *d != self.head_name => {
                        let old = repo.rev_parse(d);
                        let r = repo.git(&["fetch", ".", &format!("{src}:{d}")]);
                        let undo = old.map(|o| Undo::Cmd { args: vec!["branch".into(), "-f".into(), d.clone(), o], label: format!("fast-forward {d}") });
                        (r, undo)
                    }
                    _ => {
                        let undo = self.branch_undo(None, None, format!("fast-forward to {src}"));
                        (self.repo.as_ref().unwrap().git(&["merge", "--ff-only", &src]), undo)
                    }
                };
                self.after_op(r, &format!("Fast-forwarded to {src}"), "Fast-forward", undo);
            }
            Act::CherryPick(h) => {
                let undo = self.branch_undo(None, None, format!("cherry-pick {}", &h[..8.min(h.len())]));
                let r = self.repo.as_ref().unwrap().git(&["cherry-pick", &h]);
                self.after_op(r, "Cherry-picked", "Cherry-pick", undo);
            }
            Act::Revert(h) => {
                let undo = self.branch_undo(None, None, format!("revert {}", &h[..8.min(h.len())]));
                let r = self.repo.as_ref().unwrap().git_env(&["revert", "--no-edit", &h], &[("GIT_EDITOR", "true")]);
                self.after_op(r, "Reverted", "Revert", undo);
            }
            Act::Reset(h, mode) => {
                let Some(old) = repo.rev_parse("HEAD") else { return };
                // A hard reset throws away local changes: keep them so undo can bring them back.
                let saved = if mode == "hard" { repo.git(&["diff", "--binary", "HEAD"]).unwrap_or_default() } else { String::new() };
                let r = repo.git(&["reset", &format!("--{mode}"), &h]);
                if self.result(r, "") {
                    let label = format!("reset {} to {} ({mode})", self.head_name, &h[..8.min(h.len())]);
                    let reset = Undo::Reset { to: old, soft: mode != "hard", label: label.clone() };
                    let u = if saved.trim().is_empty() {
                        reset
                    } else {
                        Undo::Many(vec![Undo::Patch { patch: saved, label: String::new() }, reset], label)
                    };
                    self.push_undo(u);
                }
                self.reload_all(false);
            }
            Act::CheckoutDetached(h) => {
                let r = repo.git(&["switch", "--detach", &h]);
                self.result(r, "Checked out commit (detached HEAD)");
                self.reload_all(false);
            }
            Act::PromptBranchAt(h) => {
                self.focus = Focus::Palette;
                self.palette.reset("");
                self.palette.branch_at = Some(h);
            }
            Act::CreateBranchAt(name, h, switch) => {
                let prev = self.head_name.clone();
                let r = if switch { repo.git(&["switch", "-c", &name, &h]) } else { repo.git(&["branch", &name, &h]) };
                if self.result(r, "") {
                    // Undo: switch back if we moved, then delete the branch.
                    let mut steps = vec![Undo::Cmd { args: vec!["branch".into(), "-D".into(), name.clone()], label: String::new() }];
                    if switch && self.local_names.contains(&prev) {
                        steps.push(Undo::Cmd { args: vec!["switch".into(), prev], label: String::new() });
                    }
                    self.push_undo(Undo::Many(steps, format!("create branch {name}")));
                }
                self.reload_all(false);
            }
            Act::DeleteBranch(name) => {
                let Some(hash) = repo.rev_parse(&format!("refs/heads/{name}")) else { return };
                let r = repo.git(&["branch", "-D", &name]);
                if self.result(r, "") {
                    self.push_undo(Undo::Cmd { args: vec!["branch".into(), name.clone(), hash], label: format!("delete branch {name}") });
                }
                self.reload_all(false);
            }
            Act::ResolveUse(path, ours) => {
                let side = if ours { "--ours" } else { "--theirs" };
                let r = repo.git(&["checkout", side, "--", &path]).and_then(|_| repo.git(&["add", "--", &path]));
                self.result(r, &format!("Resolved {path}"));
                self.reload_status();
            }
            Act::OpenResolver(path) => self.open_resolver(&path),
            // From the palette: anchor at the top-right, under the Push button (x is clamped on draw).
            Act::HideRef(full) => {
                self.hidden.insert(full.clone());
                self.save_hidden();
                let name = full.trim_start_matches("refs/heads/").trim_start_matches("refs/remotes/").to_string();
                self.toast_ok(format!("Hid {name} — it's listed under HIDDEN in the sidebar"));
            }
            Act::ShowRef(full) => {
                self.hidden.remove(&full);
                self.save_hidden();
            }
            Act::ShowAllHidden => {
                self.hidden.clear();
                self.save_hidden();
            }
            Act::InstallCli => {
                if self.job.is_none() {
                    let (tx, rx) = mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = tx.send(crate::cli::install_command());
                    });
                    self.job = Some(Job { label: "Installing command".into(), rx });
                }
            }
            Act::PushMenu => self.open_push_menu(self.remote_btns[0]),
            Act::PushTo { remote, force } => {
                if !self.local_names.contains(&self.head_name) {
                    self.toast_err("Detached HEAD — check out a branch to push");
                    return;
                }
                let branch = self.head_name.clone();
                let mut args = vec!["push".to_string()];
                if force {
                    args.push("--force-with-lease".into());
                }
                // Only set an upstream when there isn't one, so pushing a copy to a second remote
                // doesn't silently change what the branch tracks.
                if repo.upstream().is_none() {
                    args.push("-u".into());
                }
                args.push(remote.clone());
                args.push(branch);
                let label = if force { format!("Force-pushing to {remote}") } else { format!("Pushing to {remote}") };
                self.spawn(&label, args);
            }
            Act::DeleteConflicted(path) => {
                let r = repo.git(&["rm", "-q", "--", &path]);
                self.result(r, &format!("Deleted {path}"));
                self.reload_status();
            }
            Act::MarkResolved(path) => {
                let r = repo.git(&["add", "--", &path]);
                self.result(r, &format!("Marked {path} resolved"));
                self.reload_status();
            }
            Act::Checkout(name) => {
                // git refuses to check a branch out twice; go to the worktree that has it instead.
                if let Some(w) = self.branch_worktree(&name) {
                    let p = w.path.display().to_string();
                    self.act(Act::OpenWorktree(p));
                    return;
                }
                let r = repo.git(&["switch", &name]);
                self.result(r, &format!("Switched to {name}"));
                self.reload_all(false);
            }
            Act::CreateBranch(name) => {
                let r = repo.git(&["switch", "-c", &name]);
                self.result(r, &format!("Created branch {name}"));
                self.reload_all(false);
            }
            Act::SelectCommit(i) => {
                self.select(Sel::Commit(i));
                self.reveal_row(i + 1);
            }
            Act::Open(p) => {
                if self.in_tab {
                    self.open_requests.push(expand_home(&p));
                } else {
                    self.open(expand_home(&p));
                }
            }
            Act::CompareWorktree => {
                if let Some(i) = self.sel_row() {
                    let a = self.end_for_row(i);
                    self.start_compare(a, End { rev: None, label: "Working tree".into() }, false);
                }
            }
            Act::CompareRef(hash, label) => {
                let head = repo.rev_parse("HEAD");
                let a = End { rev: head, label: self.head_name.clone() };
                self.start_compare(a, End { rev: Some(hash), label }, true);
            }
            Act::CompareSwap => {
                if let Some(c) = &mut self.compare {
                    std::mem::swap(&mut c.a, &mut c.b);
                    self.load_compare();
                }
            }
            Act::CompareClose => {
                self.compare = None;
                let sel = self.sel;
                self.select(sel);
            }
            Act::ToggleSplit => {
                let w = if self.wide { 1600.0 } else { 0.0 };
                self.layout = if self.split_for(w) { Layout::Unified } else { Layout::Split };
            }
            Act::ToggleWide => self.wide = !self.wide,
            Act::ToggleWhitespace => {
                self.opts.ignore_ws = !self.opts.ignore_ws;
                self.reload_diff();
            }
            Act::CycleContext => {
                self.opts.context = match self.opts.context {
                    3 => 10,
                    10 => 25,
                    25 => u32::MAX,
                    _ => 3,
                };
                self.reload_diff();
            }
        }
    }

    fn commit(&mut self) {
        let Some(repo) = &self.repo else { return };
        let msg = self.msg.text.trim().to_string();
        if msg.is_empty() {
            self.toast_err("Write a commit message first");
            return;
        }
        if !self.amend && !self.changes.iter().any(Change::staged) {
            self.toast_err("Nothing staged — stage files, hunks or lines first");
            return;
        }
        let old = repo.rev_parse("HEAD");
        let amend = self.amend;
        match repo.commit(&msg, amend) {
            Ok(_) => {
                self.msg = TextEdit::default();
                self.amend = false;
                self.focus = Focus::None;
                match old {
                    Some(to) => self.push_undo(Undo::Reset { to, soft: true, label: if amend { "amend".into() } else { "commit".into() } }),
                    None => self.toast_ok("Committed"),
                }
                self.reload_all(false);
            }
            Err(e) => self.toast_err(e),
        }
    }

    fn apply_diff_action(&mut self, a: DiffAction) {
        let Some(repo) = &self.repo else { return };
        match a {
            DiffAction::StageFile(p) => {
                let r = repo.stage(&[&p]);
                self.result(r, "");
            }
            DiffAction::DeleteUntracked(p) => match self.discard_file(&p) {
                Ok(u) => self.push_undo(u),
                Err(e) => self.toast_err(e),
            },
            DiffAction::OpenCommit(h) => {
                self.select_hash(&h);
                return;
            }
            DiffAction::Blame(path) => {
                let rev = self.selected.as_ref().map(|c| c.hash.clone()).filter(|_| self.sel != Sel::Wip);
                self.file_view = Some(FileView::load(repo, rev.clone(), path, true));
                if rev.is_some() {
                    self.tab = Tab::Files;
                    self.load_tree();
                }
                return;
            }
            DiffAction::Hunk { f, h, lines_only } | DiffAction::Discard { f, h, lines_only } => {
                let discard = matches!(a, DiffAction::Discard { .. });
                let file = &self.diff.files[f];
                let sel = &self.diff.selected;
                let pick = |l: usize| sel.contains(&(f, h, l));
                let filter: Option<&dyn Fn(usize) -> bool> = if lines_only { Some(&pick) } else { None };
                // Unstaging and discarding both reverse-apply; see git::hunk_patch.
                let reverse = discard || self.diff.mode == DiffMode::Staged;
                let patch = git::hunk_patch(file, h, filter, reverse);
                if discard {
                    let what = if lines_only { format!("{} lines", self.diff.selected.len()) } else { "hunk".into() };
                    let label = format!("discard {what} in {}", file.path);
                    match repo.apply_worktree(&patch, true) {
                        Ok(_) => self.push_undo(Undo::Patch { patch, label }),
                        Err(e) => self.toast_err(e),
                    }
                } else {
                    let r = repo.apply_cached(&patch, reverse);
                    self.result(r, "");
                }
            }
        }
        self.diff.selected.clear();
        self.reload_status();
    }

    // ---- interactive rebase -----------------------------------------------------------------

    /// HEAD's linear first-parent history (newest first), stopping at merges.
    fn rebase_chain(&mut self) -> Vec<usize> {
        if let Some(c) = &self.chain {
            return c.clone();
        }
        let mut out = Vec::new();
        if let Some(mut i) = self.head_row {
            while out.len() < MAX_CHAIN {
                let c = &self.commits[i];
                if c.parents.len() > 1 {
                    break;
                }
                out.push(i);
                match c.parents.first().and_then(|p| self.hash_index.get(p)) {
                    Some(&j) => i = j,
                    None => break,
                }
            }
        }
        self.chain = Some(out.clone());
        out
    }

    fn pushed_rows(&self, chain: &[usize]) -> HashSet<usize> {
        let Some(repo) = &self.repo else { return HashSet::new() };
        let limit = MAX_CHAIN.to_string();
        let unpushed: HashSet<String> = repo
            .git(&["rev-list", "-n", &limit, "HEAD", "--not", "--remotes"])
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect();
        if unpushed.len() >= MAX_CHAIN {
            return HashSet::new();
        }
        chain.iter().copied().filter(|&r| !unpushed.contains(&self.commits[r].hash)).collect()
    }

    fn run_rebase(&mut self, src: usize, target: usize, op: RebaseOp) {
        if let Some(op) = self.op {
            self.toast_err(format!("A {} is in progress — continue or abort it first", op.name().to_lowercase()));
            return;
        }
        let chain = self.rebase_chain();
        let items: Vec<ChainItem> = chain
            .iter()
            .map(|&r| {
                let c = &self.commits[r];
                ChainItem { hash: &c.hash, subject: &c.subject, parent: c.parents.first().map(String::as_str) }
            })
            .collect();
        let (Some(si), Some(ti)) = (chain.iter().position(|&r| r == src), chain.iter().position(|&r| r == target)) else {
            return;
        };
        let Some(plan) = rebase::plan(&items, si, ti, op) else { return };
        let Some(repo) = &self.repo else { return };
        let old = repo.rev_parse("HEAD");
        match repo.rebase_with_todo(plan.base.as_deref(), &plan.todo) {
            Ok(_) => {
                if let Some(to) = old {
                    self.push_undo(Undo::Reset { to, soft: false, label: plan.label });
                }
            }
            Err(e) => {
                if repo.rebase_in_progress() {
                    self.toast_err("Rebase stopped on a conflict — resolve it, stage, then Continue (or Abort)");
                } else {
                    self.toast_err(e);
                }
            }
        }
        self.reload_all(false);
    }

    /// Undo record for an operation that moves `branch` (None = current HEAD).
    fn branch_undo(&self, branch: Option<String>, back_to: Option<String>, label: String) -> Option<Undo> {
        let repo = self.repo.as_ref()?;
        let to = repo.rev_parse(branch.as_deref().unwrap_or("HEAD"))?;
        let branch = branch.or_else(|| self.local_names.contains(&self.head_name).then(|| self.head_name.clone()));
        Some(Undo::BranchMove { branch, to, back_to, label })
    }

    /// Common tail for merge / rebase / cherry-pick / revert: record undo on success, explain
    /// what to do when git stopped on a conflict.
    fn after_op(&mut self, r: Result<String, String>, ok: &str, what: &str, undo: Option<Undo>) {
        match r {
            Ok(_) => match undo {
                Some(u) => self.push_undo(u),
                None => self.toast_ok(ok),
            },
            Err(e) => {
                let stopped = self.repo.as_ref().and_then(|r| r.op_in_progress());
                if stopped.is_some() {
                    self.toast_err(format!("{what} stopped on conflicts — resolve them in the working copy, then Continue"));
                } else {
                    self.toast_err(e);
                }
            }
        }
        self.reload_all(false);
        if self.op.is_some() {
            self.compare = None;
            self.select(Sel::Wip);
            if let Some(c) = self.changes.iter().find(|c| c.conflicted()) {
                let p = c.path.clone();
                self.open_resolver(&p);
            }
        }
    }

    /// ⌥↑/⌥↓, squash and drop on the selected commit.
    fn keyboard_rebase(&mut self, op: RebaseOp) {
        let Some(row) = self.sel_row() else {
            self.toast_err("Select a commit on the current branch first");
            return;
        };
        let chain = self.rebase_chain();
        let Some(pos) = chain.iter().position(|&r| r == row) else {
            self.toast_err("Only commits on the current branch (above any merge) can be rewritten");
            return;
        };
        let target = match op {
            RebaseOp::Above => pos.checked_sub(1),
            RebaseOp::Below | RebaseOp::Squash => (pos + 1 < chain.len()).then_some(pos + 1),
            RebaseOp::Drop => Some(pos),
        };
        match target {
            Some(t) => self.run_rebase(row, chain[t], op),
            None => self.toast_err("Nothing to move past"),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Frame
    // -----------------------------------------------------------------------------------------

    /// Whole-window frame (standalone use; tabs call `frame_in`).
    #[cfg(test)]
    pub fn frame(&mut self, ui: &mut Ui) {
        let screen = ui.screen;
        self.frame_in(ui, screen);
    }

    /// Keeps background work moving while this repository's tab isn't visible.
    pub fn background(&mut self, ui: &mut Ui) {
        self.drain_log(None);
        if self.loading {
            ui.wake_in(Duration::from_millis(60));
        }
        self.poll_job(ui);
        self.poll_wt_jobs(ui);
    }

    /// (repo name, branch, number of changed files, operation in progress / conflicts)
    pub fn tab_info(&self) -> (String, String, usize, bool) {
        let name = self.repo.as_ref().map_or_else(|| "—".into(), |r| r.name());
        let busy = self.op.is_some() || self.changes.iter().any(|c| c.conflicted());
        (name, self.head_name.clone(), self.changes.len(), busy)
    }

    pub fn root(&self) -> Option<PathBuf> {
        self.repo.as_ref().map(|r| r.root.clone())
    }

    pub fn frame_in(&mut self, ui: &mut Ui, bounds: Rect) {
        self.drain_log(None);
        if self.loading {
            ui.wake_in(Duration::from_millis(30));
        }
        self.poll_job(ui);
        self.poll_wt_jobs(ui);
        self.tick_playback(ui);
        self.global_keys(ui);

        let mut area = bounds;
        ui.p.rect(area, theme::BG);
        self.ref_hits.clear();
        if self.focus == Focus::Palette || self.menu.is_some() {
            ui.blocked = true;
        }
        let top = area.take_top(TOP_H);
        self.top_bar(ui, top);
        if self.repo.is_none() {
            self.no_repo(ui, area);
        } else {
            if self.op.is_some() {
                let banner = area.take_top(40.0);
                self.op_banner(ui, banner);
            }
            if self.wide {
                self.detail(ui, area);
            } else {
                let side = area.take_left(SIDEBAR_W);
                self.sidebar(ui, side);
                let rw = self.right_w.unwrap_or((area.w * 0.48).max(420.0)).clamp(320.0, (area.w - 360.0).max(320.0));
                let right = area.take_right(rw);
                self.splitter(ui, right, area.w + rw);
                self.graph_view(ui, area);
                self.detail(ui, right);
            }
        }
        ui.blocked = false;
        self.branch_drag(ui);
        if self.menu.is_some() {
            self.draw_menu(ui);
        }
        if self.focus == Focus::Palette {
            self.palette(ui);
        }
        self.draw_toast(ui);
    }

    fn poll_job(&mut self, ui: &mut Ui) {
        let Some(job) = &self.job else { return };
        match job.rx.try_recv() {
            Ok(r) => {
                let l = job.label.as_str();
                let done = [("Pushing", "Pushed"), ("Fetching", "Fetched"), ("Pulling", "Pulled")]
                    .iter()
                    .find_map(|(ing, ed)| l.strip_prefix(ing).map(|rest| format!("{ed}{rest}")))
                    .unwrap_or_else(|| "Done".to_string());
                let reports = l.starts_with("Installing");
                self.job = None;
                match r {
                    // Jobs that aren't git commands report their own outcome.
                    Ok(msg) if reports => self.toast_ok(msg),
                    r => {
                        self.result(r, &done);
                    }
                }
                self.reload_all(false);
            }
            Err(mpsc::TryRecvError::Empty) => ui.wake_in(Duration::from_millis(100)),
            Err(mpsc::TryRecvError::Disconnected) => self.job = None,
        }
    }

    fn tick_playback(&mut self, ui: &mut Ui) {
        let Some(next) = self.playing else { return };
        let Some(row) = self.sel_row() else {
            self.playing = None;
            return;
        };
        if ui.now >= next {
            if row == 0 {
                self.playing = None;
                return;
            }
            self.select(Sel::Commit(row - 1));
            self.reveal_row(row);
            self.playing = Some(ui.now + Duration::from_millis(350));
        }
        if let Some(t) = self.playing {
            ui.wake_in(t.saturating_duration_since(ui.now));
        }
    }

    fn global_keys(&mut self, ui: &mut Ui) {
        let input = &mut ui.input;
        if input.take_key(Key::Char('k'), true).is_some() || input.take_key(Key::Char('p'), true).is_some() {
            if self.focus == Focus::Palette {
                self.focus = Focus::None;
            } else {
                self.focus = Focus::Palette;
                self.palette.reset("");
            }
        }
        if ui.input.take_key(Key::Char('r'), true).is_some() {
            self.act(Act::Refresh);
        }
        if self.focus == Focus::Message && ui.input.take_key(Key::Enter, true).is_some() {
            self.commit();
        }
        if self.focus == Focus::Message && ui.input.take_key(Key::Escape, false).is_some() {
            self.focus = Focus::None;
        }
        if self.focus != Focus::None {
            return;
        }
        if ui.input.take_key(Key::Char('z'), true).is_some() {
            self.undo();
        }
        if self.resolving.is_some() {
            return;
        }
        if ui.input.take_key(Key::Escape, false).is_some() {
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
        if ui.input.take_key(Key::Char('w'), false).is_some() {
            self.act(Act::ToggleWide);
        }
        if ui.input.take_key(Key::Char('v'), false).is_some() {
            self.act(Act::ToggleSplit);
        }
        // ⌥↑/⌥↓ reorders the selected commit (keyboard interactive rebase).
        if let Some(i) = ui.input.keys.iter().position(|k| matches!(k.key, Key::Up | Key::Down) && k.mods.alt) {
            let k = ui.input.keys.remove(i);
            self.act(Act::MoveCommit(k.key == Key::Up));
        }
        let input = &mut ui.input;
        let down = input.take_key(Key::Down, false).is_some() || input.take_key(Key::Char('j'), false).is_some();
        let up = input.take_key(Key::Up, false).is_some() || input.take_key(Key::Char('k'), false).is_some();
        let row = match self.sel {
            Sel::Wip => Some(0),
            Sel::Commit(_) => self.sel_row().map(|i| i + 1),
        };
        if let Some(row) = row {
            let n = self.commits.len() + 1;
            let target = if down { Some((row + 1).min(n - 1)) } else if up { Some(row.saturating_sub(1)) } else { None };
            if let Some(t) = target.filter(|&t| t != row) {
                self.select(if t == 0 { Sel::Wip } else { Sel::Commit(t - 1) });
                self.reveal_row(t);
            }
        }
        if ui.input.take_key(Key::Char('c'), false).is_some() {
            self.select(Sel::Wip);
            self.focus = Focus::Message;
        }
        if ui.input.take_key(Key::Char('s'), false).is_some() && self.sel == Sel::Wip {
            self.act(Act::StageAll);
        }
        if ui.input.take_key(Key::Char('f'), false).is_some() && self.sel_row().is_some() {
            if self.tab == Tab::Files {
                self.tab = Tab::Changes;
                self.file_view = None;
            } else {
                self.act(Act::BrowseFiles);
            }
        }
    }

    // ---- top bar / chrome -------------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut Ui, r: Rect) {
        ui.p.rect(r, theme::PANEL);
        ui.p.rect(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), theme::BORDER);
        let mut hot: Vec<Rect> = Vec::new();
        let cy = r.cy();
        let mut x = if self.in_tab {
            r.x + 16.0
        } else {
            let lx = r.x + chrome::LEFT_INSET + 20.0;
            ui.p.line((lx, cy - 8.0), (lx, cy + 8.0), 2.0, theme::ACCENT);
            ui.p.curve((lx, cy + 2.0), (lx + 9.0, cy - 8.0), 2.0, theme::PURPLE);
            ui.p.circle((lx, cy + 8.0), 3.0, theme::ACCENT);
            ui.p.circle((lx + 9.0, cy - 8.0), 3.0, theme::PURPLE);
            lx + 22.0
        };
        if let Some(repo) = &self.repo {
            if !self.in_tab {
                x += ui.p.text(x, cy, UI, 14.0, &repo.name(), theme::TEXT) + 12.0;
            }
            let head = self.refs.iter().find(|r| r.is_head && r.kind == RefKind::Local);
            let mut label = self.head_name.clone();
            if let Some(h) = head {
                if h.ahead > 0 {
                    label.push_str(&format!("  ↑{}", h.ahead));
                }
                if h.behind > 0 {
                    label.push_str(&format!("  ↓{}", h.behind));
                }
            }
            x += ui.pill(x, cy, &label, theme::ACCENT, theme::ACCENT.alpha(0.14), false) + 10.0;
            if self.loading {
                ui.p.text(x, cy, UI, 11.5, &format!("loading history… {}", self.commits.len()), theme::FAINT);
            }
        } else {
            ui.p.text(x, cy, UI, 14.0, "wgitpui", theme::TEXT);
        }

        // Command palette launcher.
        let sw = (r.w * 0.32).clamp(220.0, 420.0);
        let search = Rect::new(r.x + (r.w - sw) * 0.5, cy - 14.0, sw, 28.0);
        hot.push(search);
        let hov = ui.hovered(search);
        ui.p.rounded(search, 7.0, if hov { theme::HOVER } else { theme::PANEL2 });
        ui.p.rounded_stroke(search, 7.0, 1.0, theme::BORDER);
        ui.p.text_fit(search.x + 12.0, cy, search.w - 70.0, UI, 12.5, "Search commits, branches, actions…", theme::FAINT);
        let hint = if cfg!(target_os = "macos") { "⌘K" } else { "Ctrl K" };
        let hw = ui.p.measure(UI, 11.5, hint);
        ui.p.text(search.right() - hw - 12.0, cy, UI, 11.5, hint, theme::DIM);
        if hov {
            ui.cursor = Cursor::Pointer;
            if ui.input.pressed {
                self.focus = Focus::Palette;
                self.palette.reset("");
            }
        }

        // Remote operations.
        if self.repo.is_some() {
            if let Some(job) = &self.job {
                let label = format!("{}…", job.label);
                let w = ui.p.measure(UI, 12.5, &label);
                let x = r.right() - 16.0 - w;
                ui.p.text(x, cy, UI, 12.5, &label, theme::DIM);
                spinner(ui, (x - 16.0, cy), theme::ACCENT);
            } else {
                let bw = 64.0;
                let mut bx = r.right() - 12.0 - bw;
                // Each opens a remote picker anchored under itself.
                for (i, label) in ["Push", "Pull", "Fetch"].into_iter().enumerate() {
                    let b = Rect::new(bx, cy - 14.0, bw, 28.0);
                    hot.push(b);
                    let at = (b.x, b.bottom() + 6.0);
                    self.remote_btns[i] = at;
                    if ui.button(b, label, false, true) {
                        match i {
                            0 => self.open_push_menu(at),
                            1 => self.open_pull_menu(at),
                            _ => self.open_fetch_menu(at),
                        }
                    }
                    bx -= bw + 8.0;
                }
            }
        }

        // Empty title-bar space moves the window; double-click zooms, like a native title bar.
        if ui.input.pressed && ui.hovered(r) && !hot.iter().any(|h| h.contains(ui.input.mouse)) {
            if ui.input.double {
                ui.toggle_maximize = true;
            } else {
                ui.drag_window = true;
            }
        }
    }

    fn no_repo(&mut self, ui: &mut Ui, r: Rect) {
        let cy = r.cy() - 40.0;
        let lines: [(&str, f32, Color); 3] = [
            ("Not a git repository", 20.0, theme::TEXT),
            ("Pass a path on the command line:  wgitpui ~/code/project", 13.0, theme::DIM),
            ("…or press ⌘K / Ctrl+K and type a path like ~/code/project", 13.0, theme::DIM),
        ];
        for (i, (s, size, c)) in lines.iter().enumerate() {
            let w = ui.p.measure(UI, *size, s);
            ui.p.text(r.x + (r.w - w) * 0.5, cy + i as f32 * 30.0, UI, *size, s, *c);
        }
    }

    fn op_banner(&mut self, ui: &mut Ui, r: Rect) {
        let name = self.op.map_or("Operation", |o| o.name());
        ui.p.rect(r, theme::ORANGE.alpha(0.12));
        ui.p.rect(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), theme::ORANGE.alpha(0.4));
        ui.p.circle((r.x + 18.0, r.cy()), 4.0, theme::ORANGE);
        let conflicts = self.changes.iter().filter(|c| c.conflicted()).count();
        let msg = if conflicts > 0 {
            format!("{name} paused · {conflicts} conflicted file{} — resolve in the working copy, then continue", if conflicts == 1 { "" } else { "s" })
        } else {
            format!("{name} paused — conflicts resolved, ready to continue")
        };
        ui.p.text_fit(r.x + 32.0, r.cy(), r.w - 240.0, UI, 12.5, &msg, theme::TEXT);
        let cont = match self.op {
            Some(OpKind::Merge) => "Commit merge",
            Some(OpKind::Rebase) => "Continue rebase",
            _ => "Continue",
        };
        let cw = ui.p.measure(UI, 12.5, cont) + 28.0;
        if ui.button(Rect::new(r.right() - 12.0 - cw, r.y + 6.0, cw, 28.0), cont, true, conflicts == 0) {
            self.act(Act::OpContinue);
        }
        if ui.danger_button(Rect::new(r.right() - 12.0 - cw - 8.0 - 74.0, r.y + 6.0, 74.0, 28.0), "Abort") {
            self.act(Act::OpAbort);
        }
    }

    fn section_header(ui: &mut Ui, x: f32, y: f32, label: &str) {
        ui.p.text(x, y + 13.0, UI, 10.5, label, theme::FAINT);
    }

    fn sidebar(&mut self, ui: &mut Ui, r: Rect) {
        ui.p.rect(r, theme::PANEL);
        ui.p.rect(Rect::new(r.right() - 1.0, r.y, 1.0, r.h), theme::BORDER);
        let hidden = |i: usize| self.hidden.contains(&ref_full(&self.refs[i]));
        let of = |k: RefKind| (0..self.refs.len()).filter(|&i| self.refs[i].kind == k && !hidden(i)).collect::<Vec<usize>>();
        let (locals, remotes, tags) = (of(RefKind::Local), of(RefKind::Remote), of(RefKind::Tag));
        let hidden_list: Vec<usize> = (0..self.refs.len()).filter(|&i| hidden(i)).collect();
        let sections = [("BRANCHES", &locals), ("REMOTES", &remotes), ("HIDDEN", &hidden_list), ("TAGS", &tags)];
        let others = self.sidebar_worktrees();
        // Branch → the other worktree that has it checked out.
        let elsewhere: HashMap<&str, String> =
            others.iter().filter(|(_, busy)| busy.is_none()).filter_map(|(w, _)| Some((w.branch.as_deref()?, w.name()))).collect();
        let content = 30.0
            + LIST_ROW_H * (1 + others.len()) as f32
            + 12.0
            + sections.iter().filter(|s| !s.1.is_empty()).map(|s| 30.0 + s.1.len() as f32 * LIST_ROW_H + 8.0).sum::<f32>();
        let off = self.side_scroll.update(ui, r, content);
        ui.p.push_clip(r);
        let x = r.x + 14.0;
        let mut y = r.y + 8.0 - off;
        Self::section_header(ui, x, y, "WORKSPACE");
        // "+" adds a worktree (from the selected commit, or HEAD).
        let new_wt = {
            let b = Rect::new(r.right() - 34.0, y + 3.0, 22.0, 20.0);
            let hov = ui.hovered(b);
            if hov {
                ui.p.rounded(b, 5.0, theme::HOVER);
                ui.cursor = Cursor::Pointer;
            }
            let (cx, cy) = (b.x + 11.0, b.cy());
            let c = if hov { theme::TEXT } else { theme::DIM };
            ui.p.line((cx - 5.0, cy), (cx + 5.0, cy), 1.6, c);
            ui.p.line((cx, cy - 5.0), (cx, cy + 5.0), 1.6, c);
            hov && ui.input.pressed
        };
        y += 26.0;

        let row = Rect::new(r.x + 6.0, y, r.w - 12.0, LIST_ROW_H);
        let sel = self.sel == Sel::Wip;
        let hov = ui.hovered(row);
        if sel || hov {
            ui.p.rounded(row, 5.0, if sel { theme::SELECT } else { theme::HOVER });
        }
        ui.p.ring((x + 6.0, row.cy()), 4.0, 1.5, theme::YELLOW);
        ui.p.text(x + 18.0, row.cy(), UI, 12.5, "Working copy", theme::TEXT);
        if !self.changes.is_empty() {
            ui.count_badge(row.right() - 6.0, row.cy(), self.changes.len(), theme::YELLOW);
        }
        if hov {
            ui.cursor = Cursor::Pointer;
            if ui.input.pressed {
                self.select(Sel::Wip);
                self.reveal_row(0);
            }
        }
        y += LIST_ROW_H;

        // The repository's other worktrees: click to open one in its own tab.
        let mut open_wt: Option<PathBuf> = None;
        let mut wt_menu: Option<PathBuf> = None;
        for (w, busy) in &others {
            let row = Rect::new(r.x + 6.0, y, r.w - 12.0, LIST_ROW_H);
            y += LIST_ROW_H;
            // Rows being added or removed are inert until git is done.
            let hov = ui.hovered(row) && busy.is_none();
            if hov {
                ui.p.rounded(row, 5.0, theme::HOVER);
            }
            let faint = w.prunable || busy.is_some();
            let fg = if faint { theme::FAINT } else { theme::TEXT.mix(theme::DIM, 0.3) };
            // Folder icon.
            let (ix, iy) = (x + 1.0, row.cy());
            ui.p.rounded_stroke(Rect::new(ix, iy - 3.5, 11.0, 8.0), 1.5, 1.2, if faint { theme::FAINT } else { theme::DIM });
            ui.p.line((ix + 1.0, iy - 4.5), (ix + 4.5, iy - 4.5), 1.2, if faint { theme::FAINT } else { theme::DIM });
            let bw = if busy.is_some() {
                spinner(ui, (row.right() - 14.0, row.cy()), theme::DIM);
                22.0
            } else if w.changes > 0 {
                ui.count_badge(row.right() - 6.0, row.cy(), w.changes, theme::YELLOW) + 6.0
            } else {
                0.0
            };
            let room = row.w - 30.0 - bw;
            let nw = ui.p.text_fit(x + 18.0, row.cy(), room, UI, 12.5, &w.name(), fg);
            let detail = if let Some(b) = busy {
                b.to_string()
            } else if w.prunable {
                "missing".to_string()
            } else {
                let b = w.branch.clone().unwrap_or_else(|| format!("@{}", &w.head[..7.min(w.head.len())]));
                if w.locked { format!("{b} · locked") } else { b }
            };
            if room - nw > 30.0 {
                ui.p.text_fit(x + 18.0 + nw + 7.0, row.cy(), room - nw - 7.0, UI, 11.0, &detail, theme::FAINT);
            }
            if hov {
                ui.cursor = Cursor::Pointer;
                if ui.input.pressed && !w.prunable {
                    open_wt = Some(w.path.clone());
                }
                if ui.input.right_pressed || (ui.input.pressed && w.prunable) {
                    wt_menu = Some(w.path.clone());
                }
            }
        }
        y += 12.0;

        let mut clicked: Option<(usize, bool, bool)> = None;
        let mut rmenu: Option<String> = None;
        let mut new_branch = false;
        let mut show_one: Option<String> = None;
        let mut show_all = false;
        let sel_row = self.sel_row();
        for (title, list) in sections {
            if list.is_empty() {
                continue;
            }
            Self::section_header(ui, x, y, title);
            if title == "BRANCHES" {
                let b = Rect::new(r.right() - 34.0, y + 3.0, 22.0, 20.0);
                let hov = ui.hovered(b);
                if hov {
                    ui.p.rounded(b, 5.0, theme::HOVER);
                    ui.cursor = Cursor::Pointer;
                }
                let (cx, cy) = (b.x + 11.0, b.cy());
                let c = if hov { theme::TEXT } else { theme::DIM };
                ui.p.line((cx - 5.0, cy), (cx + 5.0, cy), 1.6, c);
                ui.p.line((cx, cy - 5.0), (cx, cy + 5.0), 1.6, c);
                if hov && ui.input.pressed {
                    new_branch = true;
                }
            }
            if title == "HIDDEN" {
                let label = "Show all";
                let w = ui.p.measure(UI, 11.0, label) + 14.0;
                let b = Rect::new(r.right() - 12.0 - w, y + 3.0, w, 20.0);
                let hov = ui.hovered(b);
                if hov {
                    ui.p.rounded(b, 5.0, theme::HOVER);
                    ui.cursor = Cursor::Pointer;
                }
                ui.p.text(b.x + 7.0, b.cy(), UI, 11.0, label, if hov { theme::TEXT } else { theme::FAINT });
                if hov && ui.input.pressed {
                    show_all = true;
                }
            }
            let hidden_section = title == "HIDDEN";
            y += 30.0;
            for &ri in list.iter() {
                let rf = &self.refs[ri];
                let row = Rect::new(r.x + 6.0, y, r.w - 12.0, LIST_ROW_H);
                y += LIST_ROW_H;
                if row.bottom() < r.y || row.y > r.bottom() {
                    continue;
                }
                let target_row = self.hash_index.get(&rf.target).copied();
                let selected = sel_row.is_some() && sel_row == target_row;
                let hov = ui.hovered(row);
                if selected || hov {
                    ui.p.rounded(row, 5.0, if selected { theme::SELECT.alpha(0.6) } else { theme::HOVER });
                }
                let (dot, fg) = match rf.kind {
                    RefKind::Local if rf.is_head => (theme::ACCENT, theme::TEXT),
                    RefKind::Local => (theme::FAINT, theme::TEXT.mix(theme::DIM, 0.3)),
                    RefKind::Remote => (theme::CYAN.alpha(0.5), theme::DIM),
                    RefKind::Tag => (theme::YELLOW.alpha(0.7), theme::DIM),
                };
                if hidden_section {
                    // Eye-off icon.
                    let (ex, ey) = (x + 6.0, row.cy());
                    ui.p.ring((ex, ey), 3.5, 1.2, theme::FAINT);
                    ui.p.line((ex - 5.0, ey + 5.0), (ex + 5.0, ey - 5.0), 1.2, theme::FAINT);
                    let show = Rect::new(row.right() - 50.0, row.y + 2.0, 46.0, LIST_ROW_H - 4.0);
                    ui.p.text_fit(x + 18.0, row.cy(), row.w - 30.0 - if hov { 52.0 } else { 0.0 }, UI, 12.5, &rf.name, theme::FAINT);
                    if hov {
                        ui.cursor = Cursor::Pointer;
                        if ui.button(show, "Show", false, true) {
                            show_one = Some(ref_full(rf));
                        } else if ui.input.right_pressed {
                            rmenu = Some(rf.name.clone());
                        } else if ui.input.pressed {
                            clicked = Some((ri, false, false));
                        }
                    }
                    continue;
                }
                if rf.is_head {
                    ui.p.circle((x + 6.0, row.cy()), 4.0, dot);
                } else {
                    ui.p.ring((x + 6.0, row.cy()), 3.5, 1.3, dot);
                }
                let mut track = String::new();
                if rf.kind == RefKind::Local {
                    if let Some(wt) = elsewhere.get(rf.name.as_str()) {
                        // Checked out in another worktree (double-click opens it there).
                        track.push_str(wt);
                        track.push(' ');
                    }
                }
                if rf.ahead > 0 {
                    track.push_str(&format!("↑{}", rf.ahead));
                }
                if rf.behind > 0 {
                    track.push_str(&format!(" ↓{}", rf.behind));
                }
                let track = track.trim().to_string();
                let tw = if track.is_empty() { 0.0 } else { (ui.p.measure(UI, 10.5, &track) + 8.0).min(row.w * 0.45) };
                ui.p.text_fit(x + 18.0, row.cy(), row.w - 30.0 - tw, UI, 12.5, &rf.name, fg);
                if !track.is_empty() {
                    ui.p.text_fit(row.right() - tw, row.cy(), tw - 4.0, UI, 10.5, &track, theme::DIM);
                }
                if rf.kind != RefKind::Tag {
                    self.ref_hits.push((row, rf.name.clone()));
                }
                if hov {
                    ui.cursor = Cursor::Pointer;
                    if ui.input.pressed {
                        clicked = Some((ri, ui.input.double, ui.input.mods.primary() || ui.input.mods.shift));
                        if rf.kind != RefKind::Tag {
                            self.bdrag = Some(BDrag { name: rf.name.clone(), start: ui.input.mouse, active: false });
                        }
                    }
                    if ui.input.right_pressed {
                        rmenu = Some(rf.name.clone());
                    }
                }
            }
            y += 8.0;
        }
        ui.p.pop_clip();

        if new_wt {
            if let Some(from) = self.wt_from_selection() {
                self.act(Act::NewWorktree(from));
            }
        }
        if let Some(p) = open_wt {
            self.act(Act::OpenWorktree(p.display().to_string()));
        }
        if let Some(p) = wt_menu {
            self.open_worktree_menu(&p, ui.input.mouse);
        }
        if let Some(name) = rmenu {
            self.open_branch_menu(&name, ui.input.mouse);
        }
        if let Some(f) = show_one {
            self.act(Act::ShowRef(f));
        }
        if show_all {
            self.act(Act::ShowAllHidden);
        }
        if new_branch {
            // Branch from the selected commit, or from HEAD.
            let at = self.sel_row().map(|i| self.commits[i].hash.clone()).or_else(|| self.repo.as_ref()?.rev_parse("HEAD"));
            if let Some(h) = at {
                self.act(Act::PromptBranchAt(h));
            }
        }
        if let Some((ri, double, compare)) = clicked {
            let rf = self.refs[ri].clone();
            if compare {
                self.act(Act::CompareRef(rf.target.clone(), rf.name.clone()));
            } else if double && rf.kind == RefKind::Local && !rf.is_head {
                self.act(Act::Checkout(rf.name));
            } else if let Some(&i) = self.hash_index.get(&rf.target) {
                self.select(Sel::Commit(i));
                let y = (i + 1) as f32 * ROW_H;
                self.graph_scroll.jump(y - self.graph_view_h * 0.35);
            }
        }
    }

    fn splitter(&mut self, ui: &mut Ui, right: Rect, total_w: f32) {
        let handle = Rect::new(right.x - 3.0, right.y, 6.0, right.h);
        if (ui.hovered(handle) && self.drag == Drag::None) || self.drag == Drag::Split {
            ui.cursor = Cursor::ResizeH;
            if ui.input.pressed && self.drag == Drag::None {
                self.drag = Drag::Split;
            }
        }
        if self.drag == Drag::Split {
            if ui.input.down {
                let w = right.right() - ui.input.mouse.0;
                self.right_w = Some(w.clamp(320.0, (total_w - 360.0).max(320.0)));
            } else {
                self.drag = Drag::None;
            }
        }
    }

    // ---- graph ------------------------------------------------------------------------------

    fn lane_x(r: &Rect, lane: u16) -> f32 {
        r.x + 16.0 + lane as f32 * LANE_W
    }

    fn graph_view(&mut self, ui: &mut Ui, r: Rect) {
        ui.p.rect(r, theme::BG);
        let mut list = r;
        let minimap = list.take_right(MINIMAP_W);
        self.graph_view_h = list.h;
        let n = self.commits.len() + 1;
        let content = n as f32 * ROW_H + 40.0;
        let off = self.graph_scroll.update(ui, list, content);
        let first = (off / ROW_H) as usize;
        let last = (first + (list.h / ROW_H) as usize + 2).min(n);
        let lanes_shown = self.graph.max_lanes.clamp(1, MAX_LANES_SHOWN);
        let lanes_w = 16.0 + lanes_shown as f32 * LANE_W + 6.0;
        let row_y = |i: usize| list.y + i as f32 * ROW_H - off;
        let lane_clip = Rect::new(list.x, list.y, lanes_w, list.h);
        let chain = self.rebase_chain();
        let sel_row = self.sel_row();
        let dragging = self.gdrag.as_ref().is_some_and(|g| g.active);

        ui.p.push_clip(list);
        let mut clicked = None;
        for i in first..last {
            let row = Rect::new(list.x, row_y(i), list.w, ROW_H);
            let selected = match self.sel {
                Sel::Wip => i == 0,
                Sel::Commit(_) => sel_row.is_some_and(|c| i == c + 1),
            };
            let hov = ui.hovered(row) && self.drag == Drag::None && !dragging;
            if selected {
                ui.p.rect(row, theme::SELECT.alpha(0.75));
                ui.p.rect(Rect::new(row.x, row.y, 2.0, row.h), theme::ACCENT);
            } else if hov {
                ui.p.rect(row, theme::HOVER.alpha(0.7));
            }
            if hov && i > 0 && ui.input.right_pressed {
                self.open_commit_menu(i - 1, ui.input.mouse);
            }
            if hov {
                let draggable = i > 0 && chain.contains(&(i - 1));
                ui.cursor = if draggable && ui.input.mouse.0 > list.x + lanes_w { Cursor::Grab } else { Cursor::Pointer };
                if ui.input.pressed {
                    clicked = Some(i);
                    if draggable && !ui.input.mods.primary() && !ui.input.mods.shift {
                        self.gdrag = Some(GDrag {
                            src: i - 1,
                            start: ui.input.mouse,
                            active: false,
                            target: None,
                            pushed: HashSet::new(),
                        });
                    }
                }
            }
        }

        // Edges.
        ui.p.push_clip(lane_clip);
        let head_lane = self.head_row.map_or(0, |h| self.graph.rows[h].lane);
        if let Some(h) = self.head_row {
            // Dashed tether from the working copy to HEAD.
            let x = Self::lane_x(&list, head_lane);
            let (y0, y1) = (row_y(0) + ROW_H * 0.5, row_y(h + 1) + ROW_H * 0.5);
            let (a, b) = (y0.max(list.y - 10.0), y1.min(list.bottom() + 10.0));
            let mut y = a - ((a - y0) % 7.0);
            while y < b {
                ui.p.line((x, y.max(y0)), (x, (y + 3.5).min(y1)), 1.6, theme::YELLOW.alpha(0.6));
                y += 7.0;
            }
        }
        for i in first.saturating_sub(1).max(1)..last {
            let y = row_y(i) + ROW_H * 0.5;
            for s in &self.graph.rows[i - 1].segs {
                ui.p.curve((Self::lane_x(&list, s.from), y), (Self::lane_x(&list, s.to), y + ROW_H), 1.8, theme::lane(s.color as usize));
            }
        }
        // Nodes.
        for i in first..last {
            let cy = row_y(i) + ROW_H * 0.5;
            if i == 0 {
                let x = Self::lane_x(&list, head_lane);
                ui.p.circle((x, cy), 5.5, theme::BG);
                ui.p.ring((x, cy), 5.5, 1.8, theme::YELLOW);
                continue;
            }
            let c = &self.commits[i - 1];
            let g = &self.graph.rows[i - 1];
            let x = Self::lane_x(&list, g.lane);
            let color = theme::lane(g.color as usize);
            if c.is_head() {
                ui.p.circle((x, cy), 7.0, theme::BG);
                ui.p.ring((x, cy), 7.0, 2.0, color);
                ui.p.circle((x, cy), 3.0, color);
            } else if c.parents.len() > 1 {
                ui.p.circle((x, cy), 4.5, theme::BG);
                ui.p.ring((x, cy), 4.5, 1.8, color);
            } else {
                ui.p.circle((x, cy), 4.8, color);
            }
        }
        ui.p.pop_clip();

        // Text columns.
        if let Some(c) = &self.compare {
            // Commits in the compared range get a violet rail; endpoints get A / B badges.
            for i in first.max(1)..last {
                if c.range.contains(&(i - 1)) {
                    ui.p.rect(Rect::new(list.x, row_y(i), list.w, ROW_H), theme::PURPLE.alpha(0.06));
                    ui.p.rect(Rect::new(list.x, row_y(i), 3.0, ROW_H), theme::PURPLE.alpha(0.7));
                }
            }
            for (end, letter, color) in [(&c.a, "A", theme::CYAN), (&c.b, "B", theme::PURPLE)] {
                let row = match &end.rev {
                    Some(h) => self.hash_index.get(h).map(|&r| r + 1),
                    None => Some(0),
                };
                if let Some(row) = row.filter(|r| (first..last).contains(r)) {
                    let y = row_y(row);
                    ui.p.rect(Rect::new(list.x, y, list.w, ROW_H), color.alpha(0.10));
                    let b = Rect::new(list.x + lanes_w - 20.0, y + 6.0, 16.0, 16.0);
                    ui.p.rounded(b, 4.0, color);
                    let w = ui.p.measure(UI, 10.5, letter);
                    ui.p.text(b.x + (16.0 - w) * 0.5, b.cy(), UI, 10.5, letter, theme::BG);
                }
            }
        }
        let tx = list.x + lanes_w + 4.0;
        let mut pill_menu: Option<String> = None;
        let show_meta = list.w > 560.0;
        let meta_w = if show_meta { 250.0 } else { 0.0 };
        for i in first..last {
            let cy = row_y(i) + ROW_H * 0.5;
            if i == 0 {
                let label = if self.changes.is_empty() {
                    "Working copy clean".to_string()
                } else {
                    let staged = self.changes.iter().filter(|c| c.staged()).count();
                    format!("Uncommitted changes · {} files, {} staged", self.changes.len(), staged)
                };
                let c = if self.changes.is_empty() { theme::FAINT } else { theme::YELLOW };
                ui.p.text(tx, cy, UI, 13.0, &label, c);
                continue;
            }
            let c = &self.commits[i - 1];
            let mut x = tx;
            let max_x = list.right() - meta_w - 12.0;
            let decos: Vec<&String> = c.refs.iter().filter(|d| !self.deco_hidden(d)).collect();
            for rf in decos {
                if x > max_x - 60.0 {
                    break;
                }
                let (label, fg, bg, outline) = if let Some(b) = rf.strip_prefix("HEAD -> ") {
                    (b, theme::BG, theme::ACCENT, false)
                } else if rf == "HEAD" {
                    ("HEAD", theme::BG, theme::ORANGE, false)
                } else if let Some(t) = rf.strip_prefix("tag: ") {
                    (t, theme::YELLOW, theme::YELLOW, true)
                } else if self.local_names.contains(rf.as_str()) {
                    let lc = theme::lane(self.graph.rows[i - 1].color as usize);
                    (rf.as_str(), lc, lc.alpha(0.16), false)
                } else {
                    (rf.as_str(), theme::CYAN.alpha(0.85), theme::CYAN, true)
                };
                let pw = ui.pill(x, cy, label, fg, bg, outline);
                if !rf.starts_with("tag: ") && rf != "HEAD" {
                    let pr = Rect::new(x, cy - 9.0, pw, 18.0);
                    let name = label.to_string();
                    if ui.hovered(pr) {
                        ui.cursor = Cursor::Grab;
                        if ui.input.pressed {
                            self.gdrag = None;
                            self.bdrag = Some(BDrag { name: name.clone(), start: ui.input.mouse, active: false });
                        }
                        if ui.input.right_pressed {
                            pill_menu = Some(name.clone());
                        }
                    }
                    self.ref_hits.push((pr, name));
                }
                x += pw + 6.0;
            }
            let subject_color = if c.parents.len() > 1 { theme::DIM } else { theme::TEXT };
            ui.p.text_fit(x, cy, max_x - x, UI, 13.0, &c.subject, subject_color);
            if show_meta {
                let mx = list.right() - meta_w;
                ui.p.text_fit(mx, cy, 110.0, UI, 12.0, &c.author, theme::DIM);
                ui.p.text(mx + 120.0, cy, UI, 12.0, &git::rel_time(c.time), theme::FAINT);
                ui.p.text(mx + 190.0, cy, MONO, 11.5, &c.hash[..7], theme::FAINT);
            }
        }

        if let Some(name) = pill_menu {
            self.open_branch_menu(&name, ui.input.mouse);
        }
        self.graph_drag(ui, list, off, &chain, lanes_w);
        ui.p.pop_clip();

        if let Some(i) = clicked {
            let m = ui.input.mods;
            if (m.primary() || m.shift) && i > 0 {
                self.compare_with_row(i - 1);
            } else {
                self.compare = None;
                self.select(if i == 0 { Sel::Wip } else { Sel::Commit(i - 1) });
                if self.focus == Focus::Message && i != 0 {
                    self.focus = Focus::None;
                }
            }
        }
        self.minimap(ui, minimap, n, content);
    }

    /// Drag a commit row to reorder it, or onto another commit to squash (interactive rebase).
    fn graph_drag(&mut self, ui: &mut Ui, list: Rect, off: f32, chain: &[usize], lanes_w: f32) {
        let Some(g) = &self.gdrag else { return };
        if !ui.input.down {
            let g = self.gdrag.take().unwrap();
            if g.active {
                if let Some((t, op)) = g.target {
                    self.run_rebase(g.src, t, op);
                }
            }
            return;
        }
        let m = ui.input.mouse;
        if !g.active {
            if (m.0 - g.start.0).abs() + (m.1 - g.start.1).abs() < 6.0 {
                return;
            }
            let pushed = self.pushed_rows(chain);
            let g = self.gdrag.as_mut().unwrap();
            g.active = true;
            g.pushed = pushed;
        }
        ui.cursor = Cursor::Grabbing;
        ui.animating = true;
        // Auto-scroll near the edges.
        if m.1 < list.y + 36.0 {
            self.graph_scroll.target -= 10.0;
        } else if m.1 > list.bottom() - 36.0 {
            self.graph_scroll.target += 10.0;
        }
        let g = self.gdrag.as_mut().unwrap();
        let row_f = (m.1 - list.y + off) / ROW_H;
        let row = row_f.floor() as isize - 1;
        let frac = row_f.fract();
        g.target = None;
        if row >= 0 && (row as usize) != g.src && chain.contains(&(row as usize)) {
            let op = if frac < 0.3 {
                RebaseOp::Above
            } else if frac > 0.7 {
                RebaseOp::Below
            } else {
                RebaseOp::Squash
            };
            g.target = Some((row as usize, op));
        }
        let (src, target) = (g.src, g.target);
        let pushed_involved = {
            let lo = chain.iter().position(|&r| r == src).unwrap_or(0);
            let hi = target.and_then(|(t, _)| chain.iter().position(|&r| r == t)).unwrap_or(lo);
            let deepest = lo.max(hi).min(chain.len().saturating_sub(1));
            chain.get(..=deepest).is_some_and(|c| c.iter().any(|r| g.pushed.contains(r)))
        };
        let row_y = |i: usize| list.y + (i + 1) as f32 * ROW_H - off;

        // Dim the source row.
        ui.p.rect(Rect::new(list.x, row_y(src), list.w, ROW_H), theme::BG.alpha(0.65));
        let mut label = "Drop on a commit of the current branch".to_string();
        if let Some((t, op)) = target {
            let y = row_y(t);
            let tr = Rect::new(list.x + 4.0, y, list.w - 8.0, ROW_H);
            match op {
                RebaseOp::Above | RebaseOp::Below => {
                    let ly = if op == RebaseOp::Above { y } else { y + ROW_H };
                    ui.p.line((list.x + lanes_w, ly), (list.right() - 12.0, ly), 2.5, theme::ACCENT);
                    ui.p.ring((list.x + lanes_w, ly), 4.0, 2.0, theme::ACCENT);
                    label = "Move here".into();
                }
                RebaseOp::Squash => {
                    ui.p.rounded(tr, 6.0, theme::PURPLE.alpha(0.12));
                    ui.p.rounded_stroke(tr, 6.0, 1.5, theme::PURPLE);
                    label = format!("Squash into “{}”", self.commits[t].subject.chars().take(40).collect::<String>());
                }
                RebaseOp::Drop => {}
            }
        }
        if pushed_involved {
            label.push_str("  ·  rewrites pushed history");
        }
        // Ghost of the dragged commit.
        let subject = &self.commits[src].subject;
        let gw = (ui.p.measure(UI, 13.0, subject) + 40.0).clamp(220.0, list.w * 0.7);
        let ghost = Rect::new(m.0 - 24.0, m.1 - 15.0, gw, 30.0);
        ui.p.rounded(Rect::new(ghost.x, ghost.y + 4.0, ghost.w, ghost.h), 8.0, Color::hex(0x000000).alpha(0.4));
        ui.p.rounded(ghost, 8.0, theme::RAISED);
        ui.p.rounded_stroke(ghost, 8.0, 1.0, theme::ACCENT.alpha(0.6));
        ui.p.circle((ghost.x + 14.0, ghost.cy()), 4.5, theme::ACCENT);
        ui.p.text_fit(ghost.x + 26.0, ghost.cy(), ghost.w - 34.0, UI, 13.0, subject, theme::TEXT);
        let lc = if target.is_none() { theme::FAINT } else if pushed_involved { theme::ORANGE } else { theme::DIM };
        ui.p.text(ghost.x + 8.0, ghost.bottom() + 12.0, UI, 11.5, &label, lc);
    }

    /// Scrollbar that doubles as a map of where refs, HEAD and the selection sit in history.
    fn minimap(&mut self, ui: &mut Ui, r: Rect, n: usize, content: f32) {
        ui.p.rect(r, theme::PANEL);
        ui.p.rect(Rect::new(r.x, r.y, 1.0, r.h), theme::BORDER);
        let frac = |row: usize| r.y + (row as f32 + 0.5) / n.max(1) as f32 * r.h;
        for &(row, c) in &self.marks {
            ui.p.rect(Rect::new(r.x + 3.0, frac(row + 1) - 0.75, r.w - 6.0, 1.5), c);
        }
        if let Some(h) = self.head_row {
            ui.p.rounded(Rect::new(r.x + 2.0, frac(h + 1) - 1.5, r.w - 4.0, 3.0), 1.5, theme::TEXT);
        }
        if let Some(i) = self.sel_row() {
            ui.p.rect(Rect::new(r.x + 1.0, frac(i + 1) - 1.0, r.w - 2.0, 2.0), theme::ORANGE);
        }
        let view = (self.graph_view_h / content).min(1.0);
        let thumb_h = (view * r.h).max(24.0);
        let max = self.graph_scroll.max.max(1.0);
        let thumb_y = r.y + (self.graph_scroll.offset / max) * (r.h - thumb_h);
        let thumb = Rect::new(r.x + 1.0, thumb_y, r.w - 2.0, thumb_h);
        let hov = ui.hovered(r);
        ui.p.rounded(thumb, 3.0, theme::TEXT.alpha(if hov || self.drag == Drag::Minimap { 0.14 } else { 0.07 }));
        if hov {
            ui.cursor = Cursor::Pointer;
            if ui.input.pressed && self.drag == Drag::None {
                self.drag = Drag::Minimap;
            }
        }
        if self.drag == Drag::Minimap {
            if ui.input.down {
                let t = ((ui.input.mouse.1 - r.y - thumb_h * 0.5) / (r.h - thumb_h).max(1.0)).clamp(0.0, 1.0);
                self.graph_scroll.target = t * max;
                self.graph_scroll.offset = self.graph_scroll.target;
            } else {
                self.drag = Drag::None;
            }
        }
    }

    // ---- detail panel -----------------------------------------------------------------------

    fn detail(&mut self, ui: &mut Ui, r: Rect) {
        ui.p.rect(r, theme::PANEL);
        ui.p.rect(Rect::new(r.x, r.y, 1.0, r.h), theme::BORDER);
        let inner = Rect::new(r.x + 1.0, r.y, r.w - 1.0, r.h);
        if self.resolving.is_some() {
            self.resolver_panel(ui, inner);
            return;
        }
        if self.compare.is_some() {
            self.compare_panel(ui, inner);
            return;
        }
        if self.sel == Sel::Wip && self.file_view.as_ref().is_some_and(|f| f.rev.is_none()) {
            let a = self.file_view.as_mut().unwrap().draw(ui, inner, true);
            self.file_action(a);
            return;
        }
        match (self.sel, self.sel_row()) {
            (Sel::Wip, _) => self.wip_panel(ui, inner),
            (Sel::Commit(_), Some(i)) => self.commit_panel(ui, inner, i),
            (Sel::Commit(_), None) => {
                ui.p.text(inner.x + 16.0, inner.y + 24.0, UI, 13.0, "Loading…", theme::FAINT);
            }
        }
    }

    fn file_action(&mut self, a: Option<FileAction>) {
        match a {
            Some(FileAction::Back) => self.file_view = None,
            Some(FileAction::ToggleBlame) => {
                if let (Some(fv), Some(repo)) = (&mut self.file_view, &self.repo) {
                    fv.toggle_blame(repo);
                }
            }
            Some(FileAction::OpenCommit(hash)) => {
                if self.file_view.as_ref().is_some_and(|f| f.rev.is_none()) {
                    self.file_view = None;
                    self.tab = Tab::Changes;
                }
                self.select_hash(&hash);
            }
            None => {}
        }
    }

    /// One list of changed files (unstaged or staged). Returns (clicked, toggled, discarded) paths.
    fn file_list(&mut self, ui: &mut Ui, r: Rect, staged: bool) -> (Option<String>, Option<String>, Option<String>) {
        let files: Vec<&Change> = self.changes.iter().filter(|c| !c.conflicted() && if staged { c.staged() } else { c.unstaged() }).collect();
        let scroll = if staged { &mut self.staged_scroll } else { &mut self.unstaged_scroll };
        let off = scroll.update(ui, r, files.len() as f32 * LIST_ROW_H);
        let (mut click, mut toggle, mut discard) = (None, None, None);
        ui.p.push_clip(r);
        if files.is_empty() {
            let msg = if staged { "Nothing staged" } else { "No unstaged changes" };
            ui.p.text(r.x + 16.0, r.y + LIST_ROW_H * 0.5, UI, 12.0, msg, theme::FAINT);
        }
        for (i, c) in files.iter().enumerate() {
            let row = Rect::new(r.x + 6.0, r.y + i as f32 * LIST_ROW_H - off, r.w - 12.0, LIST_ROW_H);
            if row.bottom() < r.y || row.y > r.bottom() {
                continue;
            }
            let selected = self.wip_file.as_ref().is_some_and(|(p, s)| p == &c.path && *s == staged);
            let hov = ui.hovered(row);
            if selected || hov {
                ui.p.rounded(row, 5.0, if selected { theme::SELECT } else { theme::HOVER });
            }
            let code = if staged { c.x } else { c.y };
            let (letter, color) = match code {
                _ if c.conflicted() => ('!', theme::RED),
                'M' => ('M', theme::YELLOW),
                'A' => ('A', theme::GREEN),
                'D' => ('D', theme::RED),
                'R' => ('R', theme::PURPLE),
                'C' => ('C', theme::PURPLE),
                '?' => ('N', theme::GREEN),
                'T' => ('T', theme::CYAN),
                other => (other, theme::DIM),
            };
            let badge = Rect::new(row.x + 6.0, row.cy() - 8.0, 16.0, 16.0);
            ui.p.rounded(badge, 4.0, color.alpha(0.16));
            let lw = ui.p.measure(MONO, 10.5, &letter.to_string());
            ui.p.text(badge.x + (16.0 - lw) * 0.5, badge.cy(), MONO, 10.5, &letter.to_string(), color);
            let (dir, name) = match c.path.rfind('/') {
                Some(p) => (&c.path[..p + 1], &c.path[p + 1..]),
                None => ("", c.path.as_str()),
            };
            let btn_w = if hov { if staged { 66.0 } else { 136.0 } } else { 0.0 };
            let avail = row.w - 34.0 - btn_w;
            let x = row.x + 30.0;
            let nw = ui.p.text_fit(x, row.cy(), avail, UI, 12.5, name, theme::TEXT);
            let detail = match &c.orig {
                Some(o) => format!("← {o}"),
                None => dir.trim_end_matches('/').to_string(),
            };
            if !detail.is_empty() {
                ui.p.text_fit(x + nw + 6.0, row.cy(), avail - nw - 6.0, UI, 11.5, &detail, theme::FAINT);
            }
            if hov {
                let b = Rect::new(row.right() - 64.0, row.y + 2.0, 60.0, LIST_ROW_H - 4.0);
                let d = Rect::new(row.right() - 134.0, row.y + 2.0, 66.0, LIST_ROW_H - 4.0);
                if ui.button(b, if staged { "Unstage" } else { "Stage" }, false, true) {
                    toggle = Some(c.path.clone());
                } else if !staged && ui.danger_button(d, "Discard") {
                    discard = Some(c.path.clone());
                } else if ui.input.pressed {
                    if ui.input.double {
                        toggle = Some(c.path.clone());
                    } else {
                        click = Some(c.path.clone());
                    }
                }
                ui.cursor = Cursor::Pointer;
            }
        }
        ui.p.pop_clip();
        (click, toggle, discard)
    }

    /// Wide mode splits the working copy and commit panels into two columns (when there's room).
    fn columns(&self, r: Rect) -> bool {
        self.wide && r.w >= 860.0
    }

    /// Width of the left column in wide layouts.
    fn side_col_w(&self, panel_w: f32) -> f32 {
        let auto = (panel_w * 0.3).clamp(340.0, 460.0);
        self.col_w.unwrap_or(auto).clamp(300.0, (panel_w - 480.0).max(300.0))
    }

    /// Wide layout: a resizable column on the left (file lists, commit details) and the rest of
    /// the panel, where the diff goes. Returns (column, rest).
    fn split_columns(&mut self, ui: &mut Ui, r: Rect) -> (Rect, Rect) {
        let mut rest = r;
        let col = rest.take_left(self.side_col_w(r.w));
        ui.p.rect(Rect::new(rest.x, rest.y, 1.0, rest.h), theme::BORDER);
        // Drag handle just inside the column's margin, so it never overlaps rows or the diff.
        let handle = Rect::new(rest.x - 5.0, r.y, 6.0, r.h);
        if (ui.hovered(handle) && self.drag == Drag::None) || self.drag == Drag::ColSplit {
            ui.cursor = Cursor::ResizeH;
            if ui.input.pressed && self.drag == Drag::None {
                self.drag = Drag::ColSplit;
            }
        }
        if self.drag == Drag::ColSplit {
            if ui.input.down {
                self.col_w = Some((ui.input.mouse.0 - r.x).clamp(300.0, (r.w - 480.0).max(300.0)));
            } else {
                self.drag = Drag::None;
            }
        }
        (col, Rect::new(rest.x + 1.0, rest.y, rest.w - 1.0, rest.h))
    }

    fn wip_panel(&mut self, ui: &mut Ui, r: Rect) {
        // Wide: file lists and the commit message form a column on the left; the diff gets the rest.
        let columns = self.columns(r);
        let (mut area, diff_area) = if columns { self.split_columns(ui, r) } else { (r, Rect::default()) };
        let head = area.take_top(44.0);
        ui.p.text(head.x + 16.0, head.cy(), UI, 15.0, "Working copy", theme::TEXT);
        let bw = ui.p.measure(UI, 12.0, &self.head_name);
        ui.p.text(head.right() - 16.0 - bw, head.cy(), UI, 12.0, &self.head_name, theme::DIM);

        let n_unstaged = self.changes.iter().filter(|c| c.unstaged() && !c.conflicted()).count();
        let n_staged = self.changes.iter().filter(|c| c.staged() && !c.conflicted()).count();
        let list_h = |n: usize| (n.max(1) as f32 * LIST_ROW_H).min(LIST_ROW_H * 7.0) + 4.0;

        let conflicts: Vec<String> = self.changes.iter().filter(|c| c.conflicted()).map(|c| c.path.clone()).collect();
        if !conflicts.is_empty() {
            let hdr = area.take_top(30.0);
            ui.p.text(hdr.x + 16.0, hdr.cy(), UI, 11.0, &format!("CONFLICTS  ·  {}", conflicts.len()), theme::RED);
            let lr = area.take_top(list_h(conflicts.len()));
            ui.p.push_clip(lr);
            let mut act = None;
            for (i, path) in conflicts.iter().enumerate() {
                let row = Rect::new(lr.x + 6.0, lr.y + i as f32 * LIST_ROW_H, lr.w - 12.0, LIST_ROW_H);
                let selected = self.wip_file.as_ref().is_some_and(|(p, _)| p == path);
                let hov = ui.hovered(row);
                if selected || hov {
                    ui.p.rounded(row, 5.0, if selected { theme::SELECT } else { theme::HOVER });
                }
                let badge = Rect::new(row.x + 6.0, row.cy() - 8.0, 16.0, 16.0);
                ui.p.rounded(badge, 4.0, theme::RED.alpha(0.18));
                ui.p.text(badge.x + 6.0, badge.cy(), MONO, 10.5, "!", theme::RED);
                let bw = if hov { 270.0 } else { 0.0 };
                let nw = ui.p.text_fit(row.x + 30.0, row.cy(), row.w - 40.0 - bw, UI, 12.5, path, theme::TEXT);
                if let Some(d) = self.docs.get(path) {
                    if !hov {
                        let prog = format!("{}/{}", d.resolved(), d.total());
                        ui.p.text(row.x + 40.0 + nw, row.cy(), UI, 11.0, &prog, theme::FAINT);
                    }
                }
                if hov {
                    let mut bx = row.right() - 4.0;
                    for (label, a, primary) in [
                        ("Resolve…", Act::OpenResolver(path.clone()), true),
                        ("Use incoming", Act::ResolveUse(path.clone(), false), false),
                        ("Use HEAD", Act::ResolveUse(path.clone(), true), false),
                    ] {
                        let w = ui.p.measure(UI, 12.5, label) + 16.0;
                        bx -= w + 4.0;
                        if ui.button(Rect::new(bx, row.y + 2.0, w, LIST_ROW_H - 4.0), label, primary, true) {
                            act = Some(a);
                        }
                    }
                    if act.is_none() && ui.input.pressed {
                        act = Some(Act::OpenResolver(path.clone()));
                    }
                    ui.cursor = Cursor::Pointer;
                }
            }
            ui.p.pop_clip();
            if let Some(a) = act {
                self.act(a);
                return;
            }
        }

        // Column: the commit message is pinned to the bottom and the two lists share what's left.
        // A list that needs less than half gets just what it needs; the other one gets the rest.
        let composer_r = if columns { area.take_bottom((area.h * 0.28).clamp(132.0, 220.0)) } else { Rect::default() };
        let (unstaged_h, staged_h) = if columns {
            let avail = (area.h - 60.0).max(2.0 * LIST_ROW_H);
            let need = |n: usize| n.max(1) as f32 * LIST_ROW_H + 4.0;
            let (nu, ns) = (need(n_unstaged), need(n_staged));
            let half = avail * 0.5;
            if nu + ns <= avail || nu <= half {
                (nu, avail - nu)
            } else if ns <= half {
                (avail - ns, ns)
            } else {
                (half, avail - half)
            }
        } else {
            (list_h(n_unstaged), list_h(n_staged))
        };

        // Unstaged
        let hdr = area.take_top(30.0);
        ui.p.text(hdr.x + 16.0, hdr.cy(), UI, 11.0, &format!("UNSTAGED  ·  {n_unstaged}"), theme::FAINT);
        if n_unstaged > 0 {
            if ui.button(Rect::new(hdr.right() - 92.0, hdr.y + 3.0, 80.0, 24.0), "Stage all", false, true) {
                self.act(Act::StageAll);
                return;
            }
            if ui.danger_button(Rect::new(hdr.right() - 184.0, hdr.y + 3.0, 86.0, 24.0), "Discard all") {
                self.act(Act::DiscardAll);
                return;
            }
        }
        let lr = area.take_top(unstaged_h);
        let (c1, t1, d1) = self.file_list(ui, lr, false);

        // Staged
        let hdr = area.take_top(30.0);
        ui.p.rect(Rect::new(hdr.x + 12.0, hdr.y, hdr.w - 24.0, 1.0), theme::BORDER);
        ui.p.text(hdr.x + 16.0, hdr.cy(), UI, 11.0, &format!("STAGED  ·  {n_staged}"), theme::FAINT);
        if n_staged > 0 && ui.button(Rect::new(hdr.right() - 92.0, hdr.y + 3.0, 80.0, 24.0), "Unstage all", false, true) {
            self.act(Act::UnstageAll);
            return;
        }
        let lr = area.take_top(staged_h);
        let (c2, t2, _) = self.file_list(ui, lr, true);

        // Commit composer
        let mut composer = if columns {
            ui.p.rect(Rect::new(composer_r.x + 12.0, composer_r.y, composer_r.w - 24.0, 1.0), theme::BORDER);
            composer_r.pad(12.0, 8.0)
        } else {
            area.take_top(132.0).pad(12.0, 6.0)
        };
        let editor = composer.take_top(composer.h - 36.0);
        if self.focus == Focus::Message {
            self.msg.handle(&mut ui.input, true);
        }
        let placeholder = "Commit message  (c to focus, ⌘↵ to commit)";
        if self.msg.draw(ui, editor, placeholder, self.focus == Focus::Message, true) {
            self.focus = Focus::Message;
        } else if ui.input.pressed && self.focus == Focus::Message && !ui.hovered(editor) {
            self.focus = Focus::None;
        }
        let row = composer.take_top(36.0);
        let tog = Rect::new(row.x, row.cy() - 9.0, 18.0, 18.0);
        let tog_hit = Rect::new(row.x, row.y, 90.0, row.h);
        ui.p.rounded(tog, 4.0, if self.amend { theme::ACCENT } else { theme::BG });
        ui.p.rounded_stroke(tog, 4.0, 1.0, if self.amend { theme::ACCENT } else { theme::FAINT });
        if self.amend {
            ui.p.line((tog.x + 4.5, tog.cy()), (tog.x + 7.5, tog.cy() + 3.5), 2.0, theme::BG);
            ui.p.line((tog.x + 7.5, tog.cy() + 3.5), (tog.x + 13.5, tog.cy() - 4.0), 2.0, theme::BG);
        }
        ui.p.text(tog.right() + 8.0, row.cy(), UI, 12.5, "Amend", theme::DIM);
        if ui.hovered(tog_hit) {
            ui.cursor = Cursor::Pointer;
            if ui.input.pressed {
                self.act(Act::ToggleAmend);
            }
        }
        let can_commit = !self.msg.text.trim().is_empty() && (n_staged > 0 || self.amend);
        let label = if self.amend { "Amend commit" } else if n_staged > 0 { "Commit" } else { "Commit (nothing staged)" };
        let bw = ui.p.measure(UI, 12.5, label) + 32.0;
        if ui.button(Rect::new(row.right() - bw, row.cy() - 15.0, bw, 30.0), label, true, can_commit) {
            self.commit();
        }

        // The toolbar lines up with the column's header when they sit side by side.
        let mut diff_r = if columns { diff_area } else { area };
        let bar = diff_r.take_top(if columns { 44.0 } else { 34.0 });
        self.diff_toolbar(ui, bar);
        let split = self.split_for(diff_r.w);
        if let Some(a) = self.diff.draw(ui, diff_r, split, self.authors) {
            self.apply_diff_action(a);
        }

        for (path, staged) in [(c1, false), (c2, true)] {
            if let Some(p) = path {
                self.wip_file = Some((p, staged));
                self.diff.selected.clear();
                self.load_wip_diff();
            }
        }
        if let Some(p) = t1 {
            let r = self.repo.as_ref().unwrap().stage(&[&p]);
            self.result(r, "");
            self.reload_status();
        }
        if let Some(p) = t2 {
            let r = self.repo.as_ref().unwrap().unstage(&[&p]);
            self.result(r, "");
            self.reload_status();
        }
        if let Some(p) = d1 {
            match self.discard_file(&p) {
                Ok(u) => self.push_undo(u),
                Err(e) => self.toast_err(e),
            }
            self.reload_status();
        }
    }

    /// Subject, hash, author, date, parents and (up to `max_body` lines of) the message.
    fn commit_header(&mut self, ui: &mut Ui, area: &mut Rect, c: &Commit, max_body: usize) -> Option<String> {
        let pad = 16.0;
        let width = area.w - 2.0 * pad;
        let (author, date, message) = self.info.clone().unwrap_or_default();
        let mut parts = message.splitn(2, '\n');
        let subject = parts.next().filter(|s| !s.is_empty()).unwrap_or(&c.subject).to_string();
        let body = parts.next().unwrap_or("").trim().to_string();
        let subj_lines = ui.p.wrap(UI, 15.5, &subject, width);
        let compact = self.tab == Tab::Files;
        let body_lines: Vec<String> = if body.is_empty() || compact { vec![] } else { ui.p.wrap(UI, 12.5, &body, width) };
        let body_shown = body_lines.len().min(max_body);
        let subj_shown = if compact { 1 } else { subj_lines.len().min(3) };
        let header_h = 14.0 + subj_shown as f32 * 22.0 + 32.0 + body_shown as f32 * 18.0 + if body_shown > 0 { 8.0 } else { 0.0 };
        let head = area.take_top(header_h);
        let mut y = head.y + 14.0;
        for l in subj_lines.iter().take(subj_shown) {
            ui.p.text_fit(head.x + pad, y + 10.0, width, UI, 15.5, l, theme::TEXT);
            y += 22.0;
        }
        let cy = y + 13.0;
        let mut x = head.x + pad;
        let hash_label = c.short().to_string();
        let hw = ui.p.measure(MONO, 11.5, &hash_label) + 14.0;
        let hash_r = Rect::new(x, cy - 10.0, hw, 20.0);
        let hov = ui.hovered(hash_r);
        ui.p.rounded(hash_r, 5.0, if hov { theme::ACCENT.alpha(0.25) } else { theme::RAISED });
        ui.p.text(x + 7.0, cy, MONO, 11.5, &hash_label, theme::ACCENT);
        if hov {
            ui.cursor = Cursor::Pointer;
            if ui.input.pressed {
                self.act(Act::CopyHash);
            }
        }
        x += hw + 10.0;
        let who = if author.is_empty() { c.author.clone() } else { author.split(" <").next().unwrap_or("").to_string() };
        x += ui.p.text_fit(x, cy, (head.right() - x - 200.0).max(60.0), UI, 12.0, &who, theme::DIM) + 10.0;
        x += ui.p.text(x, cy, UI, 12.0, &date, theme::FAINT) + 14.0;
        let mut goto = None;
        for p in &c.parents {
            if x > head.right() - 70.0 {
                break;
            }
            let label = format!("↳ {}", &p[..7.min(p.len())]);
            let w = ui.p.measure(MONO, 11.0, &label) + 8.0;
            let pr = Rect::new(x, cy - 9.0, w, 18.0);
            if ui.hovered(pr) {
                ui.p.rounded(pr, 4.0, theme::HOVER);
                ui.cursor = Cursor::Pointer;
                if ui.input.pressed {
                    goto = Some(p.clone());
                }
            }
            ui.p.text(x + 4.0, cy, MONO, 11.0, &label, theme::FAINT);
            x += w + 4.0;
        }
        y += 32.0;
        for l in body_lines.iter().take(body_shown) {
            ui.p.text_fit(head.x + pad, y + 9.0, width, UI, 12.5, l, theme::DIM.mix(theme::TEXT, 0.35));
            y += 18.0;
        }
        goto
    }

    /// Layout, context, whitespace, collapse and wide controls shared by every diff.
    fn diff_toolbar(&mut self, ui: &mut Ui, r: Rect) {
        ui.p.rect(r, theme::PANEL);
        ui.p.rect(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), theme::BORDER);
        ui.p.push_clip(r);
        let cy = r.cy();
        // Right side first, so the left controls know how much room they have.
        let wide_label = if self.wide { "Wide ✓" } else { "Wide" };
        let ww = ui.p.measure(UI, 12.5, wide_label) + 20.0;
        let mut rx = r.right() - 10.0 - ww;
        if ui.button(Rect::new(rx, cy - 12.0, ww, 24.0), wide_label, self.wide, true) {
            self.act(Act::ToggleWide);
        }
        if self.diff.files.len() > 1 {
            let all = self.diff.collapsed.len() == self.diff.files.len();
            let label = if all { "Expand all" } else { "Collapse all" };
            let lw = ui.p.measure(UI, 12.5, label) + 20.0;
            if rx - lw - 6.0 > r.x + 300.0 {
                rx -= lw + 6.0;
                if ui.button(Rect::new(rx, cy - 12.0, lw, 24.0), label, false, true) {
                    self.diff.set_all_collapsed(!all);
                }
            }
        }
        let limit = rx - 8.0;

        let mut x = r.x + 12.0;
        let split = self.split_for(r.w);
        let (w, clicked) = ui.segmented(x, cy, &["Unified", "Split"], split as usize);
        if let Some(i) = clicked {
            self.layout = if i == 1 { Layout::Split } else { Layout::Unified };
        }
        x += w + 8.0;
        let ctx = match self.opts.context {
            u32::MAX => "Context: file".to_string(),
            n => format!("Context: {n}"),
        };
        let cw = ui.p.measure(UI, 12.5, &ctx) + 20.0;
        if x + cw <= limit {
            if ui.button(Rect::new(x, cy - 12.0, cw, 24.0), &ctx, false, true) {
                self.act(Act::CycleContext);
            }
            x += cw + 8.0;
        }
        // Shorter label when space is tight; hidden entirely if it still doesn't fit
        // (it's also in the palette: "Toggle ignore whitespace").
        for label in ["Ignore whitespace", "Ignore ws"] {
            let lw = ui.p.measure(UI, 11.5, label) + 30.0;
            if x + lw <= limit {
                let (_, ws) = ui.toggle_chip(x, cy, label, self.opts.ignore_ws);
                if ws {
                    self.act(Act::ToggleWhitespace);
                }
                x += lw + 8.0;
                break;
            }
        }
        let aw = ui.p.measure(UI, 11.5, "Authors") + 30.0;
        if x + aw <= limit {
            let (_, t) = ui.toggle_chip(x, cy, "Authors", self.authors);
            if t {
                self.authors = !self.authors;
            }
        }
        ui.p.pop_clip();
    }

    /// File index for a diff: status badge, name, path, stats; tracks the scrolled-to file.
    fn diff_file_index(&mut self, ui: &mut Ui, r: Rect, scroll: &mut Scroll, split: bool) -> Option<usize> {
        ui.p.rect(r, theme::PANEL);
        let rh = 26.0;
        let n = self.diff.files.len();
        let off = scroll.update(ui, r, n as f32 * rh + 8.0);
        let current = self.diff.current_file(split);
        let mut jump = None;
        ui.p.push_clip(r);
        for fi in 0..n {
            let row = Rect::new(r.x + 6.0, r.y + 4.0 + fi as f32 * rh - off, r.w - 12.0, rh);
            if row.bottom() < r.y || row.y > r.bottom() {
                continue;
            }
            let hov = ui.hovered(row);
            if current == Some(fi) {
                ui.p.rounded(row, 5.0, theme::SELECT.alpha(0.7));
            } else if hov {
                ui.p.rounded(row, 5.0, theme::HOVER);
            }
            if hov {
                ui.cursor = Cursor::Pointer;
                if ui.input.pressed {
                    jump = Some(fi);
                }
            }
            let (letter, color) = self.diff.status[fi].badge();
            let badge = Rect::new(row.x + 6.0, row.cy() - 8.0, 16.0, 16.0);
            ui.p.rounded(badge, 4.0, color.alpha(0.16));
            ui.p.text(badge.x + 4.0, badge.cy(), MONO, 10.5, &letter.to_string(), color);
            let f = &self.diff.files[fi];
            let stat = format!("+{} -{}", f.adds, f.dels);
            let sw = ui.p.measure(MONO, 10.5, &stat);
            let (dir, name) = match f.path.rfind('/') {
                Some(p) => (&f.path[..p], &f.path[p + 1..]),
                None => ("", f.path.as_str()),
            };
            let avail = row.w - 40.0 - sw;
            let nw = ui.p.text_fit(row.x + 30.0, row.cy(), avail, UI, 12.5, name, theme::TEXT);
            if !dir.is_empty() {
                ui.p.text_fit(row.x + 36.0 + nw, row.cy(), avail - nw - 6.0, UI, 11.0, dir, theme::FAINT);
            }
            ui.p.text(row.right() - sw - 6.0, row.cy(), MONO, 10.5, &stat, theme::FAINT);
        }
        ui.p.pop_clip();
        jump
    }

    fn compare_chip(ui: &mut Ui, x: f32, cy: f32, letter: &str, color: Color, label: &str, detail: &str, max_w: f32) -> f32 {
        let lw = ui.p.measure(UI, 12.5, label);
        let dw = ui.p.measure(UI, 11.5, detail).min((max_w - lw - 50.0).max(0.0));
        let w = 34.0 + lw + if dw > 0.0 { dw + 10.0 } else { 0.0 } + 8.0;
        let r = Rect::new(x, cy - 14.0, w, 28.0);
        ui.p.rounded(r, 8.0, theme::RAISED);
        ui.p.rounded_stroke(r, 8.0, 1.0, color.alpha(0.45));
        let b = Rect::new(x + 6.0, cy - 8.0, 16.0, 16.0);
        ui.p.rounded(b, 4.0, color);
        let tw = ui.p.measure(UI, 10.5, letter);
        ui.p.text(b.x + (16.0 - tw) * 0.5, cy, UI, 10.5, letter, theme::BG);
        ui.p.text(x + 30.0, cy, UI, 12.5, label, theme::TEXT);
        if dw > 0.0 {
            ui.p.text_fit(x + 40.0 + lw, cy, dw, UI, 11.5, detail, theme::DIM);
        }
        w
    }

    fn compare_panel(&mut self, ui: &mut Ui, r: Rect) {
        let mut area = r;
        let head = area.take_top(84.0);
        let mut act = None;
        {
            let c = self.compare.as_ref().unwrap();
            let subject = |e: &End| match &e.rev {
                Some(h) => self.hash_index.get(h).map(|&i| self.commits[i].subject.clone()).unwrap_or_default(),
                None => "uncommitted changes".to_string(),
            };
            let (sa, sb) = (subject(&c.a), subject(&c.b));
            let cy = head.y + 26.0;
            ui.p.text(head.x + 16.0, cy, UI, 10.5, "COMPARE", theme::FAINT);
            let close = Rect::new(head.right() - 40.0, cy - 13.0, 28.0, 26.0);
            if ui.button(close, "", false, true) {
                act = Some(Act::CompareClose);
            }
            let (ccx, ccy) = (close.x + 14.0, close.cy());
            ui.p.line((ccx - 4.5, ccy - 4.5), (ccx + 4.5, ccy + 4.5), 1.6, theme::DIM);
            ui.p.line((ccx - 4.5, ccy + 4.5), (ccx + 4.5, ccy - 4.5), 1.6, theme::DIM);
            let room = (close.x - head.x - 150.0) * 0.5;
            let mut x = head.x + 84.0;
            x += Self::compare_chip(ui, x, cy, "A", theme::CYAN, &c.a.label, &sa, room) + 6.0;
            let swap = Rect::new(x, cy - 12.0, 30.0, 24.0);
            if ui.button(swap, "⇄", false, true) {
                act = Some(Act::CompareSwap);
            }
            x += 36.0;
            Self::compare_chip(ui, x, cy, "B", theme::PURPLE, &c.b.label, &sb, room);

            let cy2 = head.y + 62.0;
            let mut x = head.x + 16.0;
            let both = c.a.rev.is_some() && c.b.rev.is_some();
            if both {
                let (_, clicked) = ui.segmented(x, cy2, &["Exact diff  A → B", "Since merge-base  A…B"], c.merge_base as usize);
                if let Some(i) = clicked {
                    let mb = i == 1;
                    if let Some(c) = &mut self.compare {
                        c.merge_base = mb;
                    }
                    self.load_compare();
                }
                x += 300.0;
            }
            if let Some(c) = &self.compare {
                let (adds, dels) = self.diff.files.iter().fold((0, 0), |(a, d), f| (a + f.adds, d + f.dels));
                let summary = format!(
                    "B is {} ahead, {} behind  ·  {} file{}",
                    c.ahead,
                    c.behind,
                    self.diff.files.len(),
                    if self.diff.files.len() == 1 { "" } else { "s" }
                );
                x += ui.p.text_fit(x, cy2, (head.right() - x - 120.0).max(0.0), UI, 12.0, &summary, theme::DIM) + 10.0;
                x += ui.p.text(x, cy2, MONO, 11.5, &format!("+{adds} "), theme::GREEN);
                ui.p.text(x, cy2, MONO, 11.5, &format!("-{dels}"), theme::RED);
            }
        }
        ui.p.rect(Rect::new(head.x, head.bottom() - 1.0, head.w, 1.0), theme::BORDER);
        let bar = area.take_top(34.0);
        self.diff_toolbar(ui, bar);

        let narrow = area.w < 760.0;
        let list_r = if narrow {
            area.take_top((self.diff.files.len().max(1) as f32 * 26.0 + 8.0).min(150.0))
        } else {
            area.take_left(260.0)
        };
        if narrow {
            ui.p.rect(Rect::new(area.x, area.y, area.w, 1.0), theme::BORDER);
        } else {
            ui.p.rect(Rect::new(area.x, area.y, 1.0, area.h), theme::BORDER);
        }
        let split = self.split_for(area.w);
        let mut scroll = self.compare.as_mut().map(|c| std::mem::take(&mut c.files_scroll)).unwrap_or_default();
        let jump = self.diff_file_index(ui, list_r, &mut scroll, split);
        if let Some(c) = &mut self.compare {
            c.files_scroll = scroll;
        }
        let diff_r = if narrow { area } else { Rect::new(area.x + 1.0, area.y, area.w - 1.0, area.h) };
        let action = self.diff.draw(ui, diff_r, split, self.authors);
        if let Some(DiffAction::OpenCommit(h)) = &action {
            let h = h.clone();
            self.compare = None;
            self.select_hash(&h);
            return;
        }
        if let Some(DiffAction::Blame(path)) = action {
            // Blame the B side of the comparison.
            if let (Some(repo), Some(c)) = (&self.repo, &self.compare) {
                self.file_view = Some(FileView::load(repo, c.b.rev.clone(), path, true));
                self.compare = None;
                if self.file_view.as_ref().is_some_and(|f| f.rev.is_some()) {
                    if let Some(h) = self.file_view.as_ref().and_then(|f| f.rev.clone()) {
                        if let Some(&i) = self.hash_index.get(&h) {
                            let keep = self.file_view.take();
                            self.select(Sel::Commit(i));
                            self.tab = Tab::Files;
                            self.load_tree();
                            self.file_view = keep;
                        }
                    }
                } else {
                    self.select(Sel::Wip);
                }
            }
        }
        if let Some(fi) = jump {
            self.diff.jump_to_file(fi, split);
        }
        if let Some(a) = act {
            self.act(a);
        }
    }

    fn tabs(&mut self, ui: &mut Ui, r: Rect) {
        ui.p.rect(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), theme::BORDER);
        let nf = self.diff.files.len();
        let tabs = [(Tab::Changes, format!("Changes  {nf}")), (Tab::Files, "Files · time machine".to_string())];
        let mut x = r.x + 12.0;
        for (t, label) in tabs {
            let w = ui.p.measure(UI, 12.5, &label) + 20.0;
            let tr = Rect::new(x, r.y, w, r.h);
            let active = self.tab == t;
            let hov = ui.hovered(tr);
            let color = if active {
                theme::TEXT
            } else if hov {
                theme::DIM.mix(theme::TEXT, 0.5)
            } else {
                theme::DIM
            };
            ui.p.text(x + 10.0, r.cy(), UI, 12.5, &label, color);
            if active {
                ui.p.rounded(Rect::new(x + 6.0, r.bottom() - 2.5, w - 12.0, 2.5), 1.25, theme::ACCENT);
            }
            if hov {
                ui.cursor = Cursor::Pointer;
                if ui.input.pressed && !active {
                    self.tab = t;
                    if t == Tab::Files {
                        self.load_tree();
                    } else {
                        self.file_view = None;
                    }
                }
            }
            x += w + 4.0;
        }
        if self.tab == Tab::Changes {
            let hint = if cfg!(target_os = "macos") { "⌘-click another commit to compare" } else { "Ctrl-click another commit to compare" };
            let w = ui.p.measure(UI, 11.0, hint);
            if r.w > w + 300.0 {
                ui.p.text(r.right() - w - 16.0, r.cy(), UI, 11.0, hint, theme::FAINT);
            }
        }
        if self.tab == Tab::Files {
            let hint = format!("{} files", self.tree.file_count());
            let w = ui.p.measure(UI, 11.5, &hint);
            ui.p.text(r.right() - w - 16.0, r.cy(), UI, 11.5, &hint, theme::FAINT);
        }
    }

    fn commit_panel(&mut self, ui: &mut Ui, r: Rect, i: usize) {
        let c = self.commits[i].clone();
        // Wide: the commit's details and files (or its tree) form a column on the left, the diff
        // (or the opened file) gets the rest. Same column as the working copy's.
        let columns = self.columns(r);
        let (mut area, mut main) = if columns { self.split_columns(ui, r) } else { (r, Rect::default()) };
        let goto = self.commit_header(ui, &mut area, &c, if columns { 14 } else { 8 });
        let tabs_r = area.take_top(34.0);
        self.tabs(ui, tabs_r);

        match self.tab {
            Tab::Changes => self.changes_tab(ui, area, columns.then_some(main)),
            Tab::Files if columns => {
                // The tree stays in the column; the timeline and the opened file go on the right.
                let scrub = main.take_top(64.0);
                self.scrubber(ui, scrub, i);
                self.tree.select(self.file_view.as_ref().map(|f| f.path.clone()));
                if let Some(TreeAction::Open(p)) = self.tree.draw(ui, area)
                    && let Some(repo) = &self.repo
                {
                    self.file_view = Some(FileView::load(repo, Some(c.hash.clone()), p, false));
                }
                if self.file_view.is_some() {
                    let a = self.file_view.as_mut().unwrap().draw(ui, main, true);
                    self.file_action(a);
                } else {
                    ui.p.rect(main, theme::BG);
                    let hint = "Pick a file to see it at this commit";
                    let w = ui.p.measure(UI, 13.0, hint);
                    ui.p.text(main.x + (main.w - w) * 0.5, main.y + main.h * 0.4, UI, 13.0, hint, theme::FAINT);
                }
            }
            Tab::Files => {
                let scrub = area.take_top(64.0);
                self.scrubber(ui, scrub, i);
                if self.file_view.is_some() {
                    let a = self.file_view.as_mut().unwrap().draw(ui, area, true);
                    self.file_action(a);
                } else if let Some(TreeAction::Open(p)) = self.tree.draw(ui, area) {
                    if let Some(repo) = &self.repo {
                        self.file_view = Some(FileView::load(repo, Some(c.hash.clone()), p, false));
                    }
                }
            }
        }
        if let Some(p) = goto {
            self.select_hash(&p);
        }
    }

    /// The commit's changed files, then its diff. With `diff_area` (wide layout) the list fills
    /// the column (`area`) and the diff gets the area on the right.
    fn changes_tab(&mut self, ui: &mut Ui, mut area: Rect, diff_area: Option<Rect>) {
        let nf = self.diff.files.len();
        let (adds, dels) = self.diff.files.iter().fold((0, 0), |(a, d), f| (a + f.adds, d + f.dels));
        let hdr = area.take_top(26.0);
        let mut hx = hdr.x + 16.0;
        hx += ui.p.text(hx, hdr.cy(), UI, 11.0, &format!("{nf} FILE{}  ·  ", if nf == 1 { "" } else { "S" }), theme::FAINT);
        hx += ui.p.text(hx, hdr.cy(), MONO, 11.0, &format!("+{adds} "), theme::GREEN);
        ui.p.text(hx, hdr.cy(), MONO, 11.0, &format!("-{dels}"), theme::RED);
        let list_r = match diff_area {
            Some(_) => area,
            None => area.take_top((nf.max(1) as f32 * 22.0).min(22.0 * 6.0) + 6.0),
        };
        let mut diff_r = diff_area.unwrap_or(area);
        let split = self.split_for(diff_r.w);
        let off = self.files_scroll.update(ui, list_r, nf as f32 * 22.0);
        let current = self.diff.current_file(split);
        let mut jump = None;
        ui.p.push_clip(list_r);
        for (fi, f) in self.diff.files.iter().enumerate() {
            let row = Rect::new(list_r.x + 6.0, list_r.y + fi as f32 * 22.0 - off, list_r.w - 12.0, 22.0);
            if row.bottom() < list_r.y || row.y > list_r.bottom() {
                continue;
            }
            let hov = ui.hovered(row);
            if current == Some(fi) {
                ui.p.rounded(row, 5.0, theme::SELECT.alpha(0.6));
            }
            if hov {
                ui.p.rounded(row, 5.0, theme::HOVER);
                ui.cursor = Cursor::Pointer;
                if ui.input.pressed {
                    jump = Some(fi);
                }
            }
            let total = (f.adds + f.dels).max(1) as f32;
            let bar = Rect::new(row.x + 8.0, row.cy() - 3.0, 36.0, 6.0);
            ui.p.rounded(bar, 3.0, theme::RAISED);
            let gw = bar.w * f.adds as f32 / total;
            if f.adds > 0 {
                ui.p.rounded(Rect::new(bar.x, bar.y, gw.max(3.0), bar.h), 3.0, theme::GREEN.alpha(0.85));
            }
            if f.dels > 0 {
                ui.p.rounded(Rect::new(bar.x + gw, bar.y, (bar.w - gw).max(3.0), bar.h), 3.0, theme::RED.alpha(0.85));
            }
            ui.p.text_fit(row.x + 54.0, row.cy(), row.w - 130.0, UI, 12.5, &f.path, theme::TEXT);
            let stat = format!("+{} -{}", f.adds, f.dels);
            let sw = ui.p.measure(MONO, 11.0, &stat);
            ui.p.text(row.right() - sw - 8.0, row.cy(), MONO, 11.0, &stat, theme::FAINT);
        }
        ui.p.pop_clip();

        // Same toolbar height as in the wide working copy, so the diff doesn't shift between them.
        let bar = diff_r.take_top(if diff_area.is_some() { 44.0 } else { 34.0 });
        self.diff_toolbar(ui, bar);
        if let Some(a) = self.diff.draw(ui, diff_r, split, self.authors) {
            self.apply_diff_action(a);
        }
        if let Some(fi) = jump {
            self.diff.jump_to_file(fi, split);
        }
    }

    /// Timeline across the whole loaded history: drag to travel, ▶ to replay forward in time.
    fn scrubber(&mut self, ui: &mut Ui, r: Rect, sel: usize) {
        ui.p.rect(r, theme::PANEL2);
        ui.p.rect(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), theme::BORDER);
        let n = self.commits.len();
        let play = Rect::new(r.x + 12.0, r.y + 20.0, 28.0, 28.0);
        let phov = ui.hovered(play);
        ui.p.rounded(play, 14.0, if phov { theme::HOVER } else { theme::RAISED });
        let (cx, cy) = (play.x + 14.0, play.cy());
        if self.playing.is_some() {
            ui.p.rect(Rect::new(cx - 5.0, cy - 5.0, 3.5, 10.0), theme::TEXT);
            ui.p.rect(Rect::new(cx + 1.5, cy - 5.0, 3.5, 10.0), theme::TEXT);
        } else {
            ui.p.line((cx - 3.0, cy - 5.5), (cx - 3.0, cy + 5.5), 2.0, theme::TEXT);
            ui.p.line((cx - 3.0, cy - 5.5), (cx + 5.0, cy), 2.0, theme::TEXT);
            ui.p.line((cx - 3.0, cy + 5.5), (cx + 5.0, cy), 2.0, theme::TEXT);
        }
        if phov {
            ui.cursor = Cursor::Pointer;
            if ui.input.pressed {
                self.playing = if self.playing.is_some() { None } else { Some(ui.now) };
            }
        }
        if n < 2 {
            return;
        }
        let track = Rect::new(play.right() + 14.0, r.y + 30.0, r.right() - play.right() - 30.0, 8.0);
        // Newest on the right, so the timeline reads left → right like time.
        let x_of = |i: usize| track.right() - (i as f32 / (n - 1) as f32) * track.w;
        ui.p.rounded(track, 4.0, theme::BG);
        for &(row, c) in &self.marks {
            let x = x_of(row);
            ui.p.rect(Rect::new(x - 0.75, track.y - 4.0, 1.5, track.h + 8.0), c.alpha(0.7));
        }
        let kx = x_of(sel);
        ui.p.rounded(Rect::new(kx, track.y, track.right() - kx, track.h), 4.0, theme::ACCENT.alpha(0.18));
        if let Some(h) = self.head_row {
            ui.p.circle((x_of(h), track.cy()), 3.0, theme::TEXT);
        }
        ui.p.circle((kx, track.cy()), 8.0, theme::BG);
        ui.p.ring((kx, track.cy()), 8.0, 2.5, theme::ORANGE);
        let c = &self.commits[sel];
        let label = format!("{} · {}", git::rel_time(c.time), c.subject);
        let lw = ui.p.measure(UI, 11.5, &label).min(track.w);
        let lx = (kx - lw * 0.5).clamp(track.x, track.right() - lw);
        ui.p.text_fit(lx, r.y + 12.0, lw, UI, 11.5, &label, theme::DIM.mix(theme::TEXT, 0.4));
        ui.p.text(track.x, track.bottom() + 12.0, UI, 10.5, "older", theme::FAINT);
        let nw = ui.p.measure(UI, 10.5, "newer");
        ui.p.text(track.right() - nw, track.bottom() + 12.0, UI, 10.5, "newer", theme::FAINT);

        let hit = Rect::new(track.x - 8.0, track.y - 14.0, track.w + 16.0, track.h + 28.0);
        if ui.hovered(hit) {
            ui.cursor = Cursor::Pointer;
            if ui.input.pressed && self.drag == Drag::None {
                self.drag = Drag::Scrubber;
                self.playing = None;
            }
        }
        if self.drag == Drag::Scrubber {
            if ui.input.down {
                let t = ((track.right() - ui.input.mouse.0) / track.w).clamp(0.0, 1.0);
                let i = (t * (n - 1) as f32).round() as usize;
                if i != sel {
                    self.select(Sel::Commit(i));
                    self.reveal_row(i + 1);
                }
            } else {
                self.drag = Drag::None;
            }
        }
    }

    // ---- palette ----------------------------------------------------------------------------

    fn build_palette_items(&mut self) {
        let q = self.palette.input.text.trim().to_string();
        let mut items: Vec<Item> = Vec::new();
        if let Some(h) = self.palette.branch_at.clone() {
            let valid = !q.is_empty() && !q.contains(char::is_whitespace) && !q.starts_with('-');
            if valid && self.local_names.contains(&q) {
                items.push(Item {
                    label: format!("A branch named '{q}' already exists"),
                    detail: "pick another name".into(),
                    act: Act::Checkout(q),
                    score: 0,
                    kind: "branch",
                });
            } else if valid {
                let short = &h[..8.min(h.len())];
                items.push(Item {
                    label: format!("Create branch '{q}' at {short} and switch to it"),
                    detail: "git switch -c".into(),
                    act: Act::CreateBranchAt(q.clone(), h.clone(), true),
                    score: 1,
                    kind: "new",
                });
                items.push(Item {
                    label: format!("Create branch '{q}' at {short} without switching"),
                    detail: "git branch".into(),
                    act: Act::CreateBranchAt(q, h, false),
                    score: 0,
                    kind: "new",
                });
            }
            self.palette.items = items;
            return;
        }
        if let Some(from) = self.palette.worktree_from.clone() {
            self.palette.items = self.worktree_prompt_items(&from, &q);
            return;
        }
        let mut create: Option<Item> = None;
        let mut push = |label: String, detail: String, act: Act, kind: &'static str, extra: &str, boost: i32| {
            let hay = if extra.is_empty() { label.clone() } else { format!("{label} {extra}") };
            if let Some(s) = fuzzy(&q, &hay) {
                items.push(Item { label, detail, act, score: s + boost, kind });
            }
        };
        if self.repo.is_some() {
            let undo_detail = self.undo.last().map_or("nothing to undo".to_string(), |u| u.label().to_string());
            let mut actions = vec![
                ("Fetch all remotes", "git fetch --all --prune".to_string(), Act::Fetch),
                ("Pull (fast-forward only)", "git pull --ff-only".into(), Act::Pull),
                ("Push current branch", "git push".into(), Act::Push),
                ("Push to…", "pick a remote".into(), Act::PushMenu),
                ("Pull from…", "pick a remote · rebase / merge when diverged".into(), Act::PullMenu),
                ("Fetch from…", "pick a remote".into(), Act::FetchMenu),
                ("Stage all changes", "git add -A".into(), Act::StageAll),
                ("Unstage all changes", "git restore --staged".into(), Act::UnstageAll),
                ("Discard all unstaged changes", "undoable".into(), Act::DiscardAll),
                ("Commit staged changes", "⌘↵ in the message box".into(), Act::Commit),
                ("Toggle amend", "rewrite the last commit".into(), Act::ToggleAmend),
                ("Undo", undo_detail, Act::Undo),
                ("Show working copy", "c".into(), Act::ShowWip),
                ("Refresh", "⌘R".into(), Act::Refresh),
            ];
            if self.compare.is_some() {
                actions.push(("Swap compare sides", "A ⇄ B".into(), Act::CompareSwap));
                actions.push(("Close comparison", "Esc".into(), Act::CompareClose));
            }
            actions.extend([
                ("Toggle split / unified diff", "v".to_string(), Act::ToggleSplit),
                ("Toggle wide diff view", "w · hides the graph".into(), Act::ToggleWide),
                ("Toggle ignore whitespace", "git diff -w".into(), Act::ToggleWhitespace),
                ("Cycle diff context lines", "3 → 10 → 25 → whole file".into(), Act::CycleContext),
            ]);
            if self.sel_row().is_some() {
                actions.extend([
                    ("Compare with working tree", "selected commit ↔ your files".to_string(), Act::CompareWorktree),
                    ("Copy commit hash", "of the selected commit".to_string(), Act::CopyHash),
                    ("Browse files at this commit", "time machine · f".into(), Act::BrowseFiles),
                    ("Move commit up", "⌥↑ · interactive rebase".into(), Act::MoveCommit(true)),
                    ("Move commit down", "⌥↓ · interactive rebase".into(), Act::MoveCommit(false)),
                    ("Squash commit into parent", "fixup · interactive rebase".into(), Act::SquashIntoParent),
                    ("Drop commit", "interactive rebase · undoable".into(), Act::DropCommit),
                ]);
            }
            if let Some(c) = self.changes.iter().find(|c| c.conflicted()) {
                let n = self.changes.iter().filter(|c| c.conflicted()).count();
                actions.push(("Resolve conflicts", format!("{n} file{} · opens the resolver", if n == 1 { "" } else { "s" }), Act::OpenResolver(c.path.clone())));
            }
            if let Some(op) = self.op {
                actions.push(("Continue operation", format!("{} --continue", op.name().to_lowercase()), Act::OpContinue));
                actions.push(("Abort operation", format!("{} --abort", op.name().to_lowercase()), Act::OpAbort));
            }
            if let Some(h) = self.sel_row().map(|i| self.commits[i].hash.clone()).or_else(|| self.repo.as_ref()?.rev_parse("HEAD")) {
                actions.push(("New branch…", "from the selected commit · or just type a name".into(), Act::PromptBranchAt(h)));
            }
            if crate::cli::app_bundle().is_some() && !crate::cli::command_installed() {
                actions.push(("Install 'wgitpui' command", "open repos from the terminal".into(), Act::InstallCli));
            }
            actions.push(("Pull with rebase", "git pull --rebase --autostash".into(), Act::PullRebase));
            actions.push(("Pull with merge", "git pull --no-rebase".into(), Act::PullMerge));
            if let Some(i) = self.sel_row().filter(|&i| Some(i) != self.head_row) {
                let h = self.commits[i].hash.clone();
                let head = self.head_name.clone();
                actions.extend([
                    ("Cherry-pick selected commit", format!("onto {head}"), Act::CherryPick(h.clone())),
                    ("Revert selected commit", "new commit undoing it".into(), Act::Revert(h.clone())),
                    ("Merge selected commit", format!("into {head}"), Act::MergeInto { src: h.clone(), dst: None }),
                    ("Rebase onto selected commit", format!("moves {head}"), Act::RebaseOnto { branch: None, onto: h.clone() }),
                    ("Reset to selected commit (soft)", "keep changes staged".into(), Act::Reset(h.clone(), "soft")),
                    ("Reset to selected commit (mixed)", "keep changes unstaged".into(), Act::Reset(h.clone(), "mixed")),
                    ("Reset to selected commit (hard)", "discard changes · undoable".into(), Act::Reset(h.clone(), "hard")),
                    ("Create branch at selected commit…", "name it next".into(), Act::PromptBranchAt(h)),
                ]);
            }
            for (l, d, a) in actions {
                push(l.into(), d, a, "action", "", 30);
            }
            if let Some(from) = self.wt_from_selection() {
                let detail = format!("from {} · git worktree add", from.label);
                push("New worktree…".into(), detail, Act::NewWorktree(from), "action", "", 28);
            }
            let others = self.other_worktrees();
            for w in &others {
                let detail = w.branch.clone().unwrap_or_else(|| "detached".into());
                if !w.prunable && !self.wt_busy(&w.path) {
                    push(format!("Open worktree {}", w.name()), detail, Act::OpenWorktree(w.path.display().to_string()), "repo", "", 12);
                }
            }
            if others.iter().any(|w| w.prunable) {
                push("Prune stale worktrees".into(), "git worktree prune".into(), Act::PruneWorktrees, "action", "", 20);
            }
            let (remotes, upstream) = self.remotes_upstream_first();
            let head = self.head_name.clone();
            for (name, url) in remotes {
                let rbranch = upstream.as_ref().filter(|(r, _)| *r == name).map_or_else(|| head.clone(), |(_, b)| b.clone());
                push(format!("Push {head} to {name}"), short_url(&url), Act::PushTo { remote: name.clone(), force: false }, "action", "", 25);
                push(
                    format!("Pull {name}/{rbranch} into {head}"),
                    "fast-forward only".into(),
                    Act::PullFrom { remote: name.clone(), branch: rbranch, mode: "ff" },
                    "action",
                    "",
                    24,
                );
                push(format!("Fetch {name}"), short_url(&url), Act::FetchFrom(name), "action", "", 24);
            }
            for rf in self.refs.iter().filter(|r| !r.is_head && r.kind != RefKind::Tag) {
                let full = ref_full(rf);
                if self.hidden.contains(&full) {
                    push(format!("Show branch {}", rf.name), "currently hidden".into(), Act::ShowRef(full), "branch", "", 5);
                } else {
                    push(format!("Hide branch {}", rf.name), "from the graph and sidebar".into(), Act::HideRef(full), "branch", "", 0);
                }
                push(
                    format!("Merge {} into {}", rf.name, self.head_name),
                    "git merge".into(),
                    Act::MergeInto { src: rf.name.clone(), dst: None },
                    "merge",
                    "",
                    0,
                );
                push(
                    format!("Rebase {} onto {}", self.head_name, rf.name),
                    "git rebase".into(),
                    Act::RebaseOnto { branch: None, onto: rf.name.clone() },
                    "rebase",
                    "",
                    0,
                );
            }
            for rf in self.refs.iter().filter(|r| !r.is_head) {
                push(
                    format!("Compare {} ↔ {}", self.head_name, rf.name),
                    "since they diverged · ⌘-click in sidebar".into(),
                    Act::CompareRef(rf.target.clone(), rf.name.clone()),
                    "compare",
                    "",
                    5,
                );
            }
            for rf in &self.refs {
                match rf.kind {
                    RefKind::Local if !rf.is_head => {
                        push(format!("Checkout {}", rf.name), "local branch".into(), Act::Checkout(rf.name.clone()), "branch", "", 20)
                    }
                    RefKind::Remote => {
                        let short = rf.name.split_once('/').map_or(rf.name.as_str(), |(_, b)| b).to_string();
                        if !self.local_names.contains(&short) {
                            push(format!("Checkout {}", rf.name), format!("creates local '{short}'"), Act::Checkout(short), "remote", "", 10)
                        }
                    }
                    _ => {}
                }
            }
            if !q.is_empty() {
                for (i, c) in self.commits.iter().enumerate().take(50_000) {
                    let detail = format!("{} · {} · {}", c.short(), c.author, git::rel_time(c.time));
                    let boost = if c.hash.starts_with(&q) { 200 } else { 0 };
                    push(c.subject.clone(), detail, Act::SelectCommit(i), "commit", &c.hash[..10], boost - 10);
                }
                let valid_branch = !q.contains(char::is_whitespace) && !q.starts_with(['/', '~', '.', '-']);
                if valid_branch && !self.local_names.contains(&q) {
                    create = Some(Item {
                        label: format!("Create branch '{q}' from {}", self.head_name),
                        detail: "git switch -c".into(),
                        act: Act::CreateBranch(q.clone()),
                        score: 0,
                        kind: "new",
                    });
                }
            }
        }
        let current = self.root();
        for r in self.recent.iter().filter(|r| Some(*r) != current.as_ref()) {
            let name = r.file_name().map_or_else(|| r.display().to_string(), |n| n.to_string_lossy().into());
            let label = format!("Open {name}");
            if let Some(sc) = fuzzy(&q, &format!("{label} {}", r.display())) {
                items.push(Item { label, detail: r.display().to_string(), act: Act::Open(r.display().to_string()), score: sc + 8, kind: "repo" });
            }
        }
        if q.starts_with(['/', '~', '.']) {
            items.push(Item {
                label: format!("Open repository {q}"),
                detail: "switch to another repository".into(),
                act: Act::Open(q.clone()),
                score: 10_000,
                kind: "open",
            });
        }
        items.sort_by(|a, b| b.score.cmp(&a.score));
        items.truncate(200);
        if let Some(c) = create {
            // First when nothing really matches the name (you're naming something new);
            // otherwise just below the best few matches so search still comes first.
            let ql = q.to_lowercase();
            let strong = items.iter().take(3).filter(|i| i.label.to_lowercase().contains(&ql)).count();
            items.insert(strong.min(items.len()), c);
        }
        self.palette.items = items;
    }

    /// The new-worktree prompt: type a new branch name (the folder goes next to the main
    /// worktree), or a path to choose where it goes.
    fn worktree_prompt_items(&self, from: &WtFrom, q: &str) -> Vec<Item> {
        let mut items = Vec::new();
        let is_path = q.starts_with(['/', '~', '.']);
        let detail = |p: &str| tilde(Path::new(p));
        if let Some(b) = from.branch.as_ref().filter(|_| q.is_empty() || is_path) {
            let path = if is_path { q.to_string() } else { self.default_wt_path(b) };
            items.push(Item {
                label: format!("Check out {b} in a new worktree"),
                detail: detail(&path),
                act: Act::AddWorktree { path, rev: b.clone(), new_branch: None, detach: false },
                score: 2,
                kind: "new",
            });
        }
        let valid_name = !q.is_empty() && !is_path && !q.contains(char::is_whitespace) && !q.starts_with('-');
        if valid_name && self.local_names.contains(q) {
            items.push(Item {
                label: format!("A branch named '{q}' already exists"),
                detail: "pick another name".into(),
                act: Act::NewWorktree(from.clone()),
                score: 1,
                kind: "branch",
            });
        } else if valid_name {
            let path = self.default_wt_path(q);
            items.push(Item {
                label: format!("New branch '{q}' from {} in a new worktree", from.label),
                detail: detail(&path),
                act: Act::AddWorktree { path, rev: from.rev.clone(), new_branch: Some(q.to_string()), detach: false },
                score: 1,
                kind: "new",
            });
        }
        if q.is_empty() || is_path {
            let path = if is_path { q.to_string() } else { self.default_wt_path(&from.label) };
            items.push(Item {
                label: format!("Detached worktree at {}", from.label),
                detail: detail(&path),
                act: Act::AddWorktree { path, rev: from.rev.clone(), new_branch: None, detach: true },
                score: 0,
                kind: "new",
            });
        }
        items
    }

    fn palette(&mut self, ui: &mut Ui) {
        let input = &mut ui.input;
        if input.take_key(Key::Escape, false).is_some() {
            self.focus = Focus::None;
            return;
        }
        let n = self.palette.items.len();
        if input.take_key(Key::Down, false).is_some() && n > 0 {
            self.palette.sel = (self.palette.sel + 1).min(n - 1);
        }
        if input.take_key(Key::Up, false).is_some() {
            self.palette.sel = self.palette.sel.saturating_sub(1);
        }
        let enter = input.take_key(Key::Enter, false).is_some();
        self.palette.input.handle(&mut ui.input, false);
        if self.palette.query.as_deref() != Some(self.palette.input.text.as_str()) {
            self.palette.query = Some(self.palette.input.text.clone());
            self.build_palette_items();
            self.palette.sel = 0;
            self.palette.scroll = Scroll::default();
        }
        if enter {
            if let Some(item) = self.palette.items.get(self.palette.sel) {
                let a = item.act.clone();
                self.focus = Focus::None;
                self.act(a);
                return;
            }
        }

        let s = ui.screen;
        ui.p.rect(s, Color::hex(0x000000).alpha(0.5));
        let w = (s.w - 40.0).min(680.0);
        let row_h = 38.0;
        let visible = self.palette.items.len().min(11);
        let list_h = visible as f32 * row_h;
        let bx = Rect::new(s.x + (s.w - w) * 0.5, s.y + 70.0, w, 52.0 + list_h + if visible > 0 { 10.0 } else { 0.0 });
        let fresh = std::mem::replace(&mut self.palette.fresh, false);
        if ui.input.pressed && !fresh && !bx.contains(ui.input.mouse) {
            self.focus = Focus::None;
            return;
        }
        ui.p.rounded(Rect::new(bx.x - 1.0, bx.y + 6.0, bx.w + 2.0, bx.h + 4.0), 14.0, Color::hex(0x000000).alpha(0.35));
        ui.p.rounded(bx, 12.0, theme::PANEL2);
        ui.p.rounded_stroke(bx, 12.0, 1.0, theme::BORDER.mix(theme::TEXT, 0.08));
        let input_r = Rect::new(bx.x + 8.0, bx.y + 8.0, bx.w - 16.0, 36.0);
        let placeholder = match &self.palette.worktree_from {
            Some(f) => format!("New worktree from {}: a new branch name, or a path…", f.label),
            None if self.palette.branch_at.is_some() => "New branch name…".into(),
            None => "Type a command, branch, commit, or a path to open…".into(),
        };
        self.palette.input.draw(ui, input_r, &placeholder, true, false);

        let list = Rect::new(bx.x + 6.0, input_r.bottom() + 8.0, bx.w - 12.0, list_h);
        let sel = self.palette.sel;
        self.palette.scroll.reveal(sel as f32 * row_h, (sel + 1) as f32 * row_h, list.h);
        let off = self.palette.scroll.update(ui, list, self.palette.items.len() as f32 * row_h);
        let mut chosen = None;
        ui.p.push_clip(list);
        for (i, item) in self.palette.items.iter().enumerate() {
            let row = Rect::new(list.x, list.y + i as f32 * row_h - off, list.w, row_h);
            if row.bottom() < list.y || row.y > list.bottom() {
                continue;
            }
            let hov = ui.hovered(row);
            if i == sel {
                ui.p.rounded(row, 8.0, theme::SELECT);
            } else if hov {
                ui.p.rounded(row, 8.0, theme::HOVER);
            }
            if hov {
                ui.cursor = Cursor::Pointer;
                if ui.input.pressed {
                    chosen = Some(i);
                }
            }
            let kc = match item.kind {
                "action" => theme::PURPLE,
                "branch" => theme::ACCENT,
                "remote" => theme::CYAN,
                "commit" => theme::DIM,
                _ => theme::GREEN,
            };
            let kw = ui.p.measure(UI, 10.0, item.kind) + 12.0;
            let kr = Rect::new(row.x + 10.0, row.cy() - 9.0, kw, 18.0);
            ui.p.rounded(kr, 5.0, kc.alpha(0.14));
            ui.p.text(kr.x + 6.0, kr.cy(), UI, 10.0, item.kind, kc);
            let lx = kr.right() + 10.0;
            let dw = ui.p.measure(UI, 11.5, &item.detail).min(row.w * 0.4);
            ui.p.text_fit(lx, row.cy(), row.right() - lx - dw - 24.0, UI, 13.0, &item.label, theme::TEXT);
            ui.p.text_fit(row.right() - dw - 12.0, row.cy(), dw, UI, 11.5, &item.detail, theme::FAINT);
        }
        ui.p.pop_clip();
        if let Some(i) = chosen {
            let a = self.palette.items[i].act.clone();
            self.focus = Focus::None;
            self.act(a);
        }
    }

    // ---- context menus + branch drag-and-drop -----------------------------------------------

    fn save_hidden(&mut self) {
        let mut v: Vec<String> = self.hidden.iter().cloned().collect();
        v.sort();
        if let Some(repo) = &self.repo {
            if let Err(e) = repo.set_hidden_refs(&v) {
                self.toast_err(e);
            }
        }
        self.reload_all(false);
    }

    /// Whether a `%D` decoration ("main", "origin/main", "HEAD -> main") names a hidden branch.
    fn deco_hidden(&self, d: &str) -> bool {
        if self.hidden.is_empty() || d.starts_with("HEAD") || d.starts_with("tag: ") {
            return false;
        }
        if self.local_names.contains(d) {
            self.hidden.contains(&format!("refs/heads/{d}"))
        } else {
            self.hidden.contains(&format!("refs/remotes/{d}"))
        }
    }

    fn find_ref(&self, name: &str) -> Option<&Ref> {
        self.refs.iter().find(|r| r.name == name)
    }

    fn preview_note(&self, ours: &str, theirs: &str, theirs_label: &str) -> Option<(String, Color)> {
        let repo = self.repo.as_ref()?;
        let n = repo.count_range(ours, theirs);
        if n == 0 {
            return Some((format!("Nothing to bring in — {theirs_label} is already contained"), theme::DIM));
        }
        let commits = format!("{n} commit{}", if n == 1 { "" } else { "s" });
        Some(match repo.merge_preview(ours, theirs) {
            MergePreview::Clean => (format!("{commits} · merges cleanly"), theme::GREEN),
            MergePreview::Conflicts(files) => {
                let list = files.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
                let more = if files.len() > 3 { format!(" +{}", files.len() - 3) } else { String::new() };
                (format!("{commits} · conflicts in {list}{more}"), theme::ORANGE)
            }
            MergePreview::Unknown => (commits, theme::DIM),
        })
    }

    /// Remote picker for Push: one entry per remote with ahead/behind against that remote's
    /// copy of the branch; the upstream remote first.
    fn open_push_menu(&mut self, at: (f32, f32)) {
        let Some(repo) = &self.repo else { return };
        let branch = self.head_name.clone();
        let mut remotes = repo.remotes();
        if remotes.is_empty() {
            self.toast_err("No remotes configured — add one with: git remote add origin <url>");
            return;
        }
        let upstream = repo.upstream();
        let up_remote = upstream.as_ref().and_then(|u| u.split_once('/').map(|(r, _)| r.to_string()));
        remotes.sort_by_key(|(n, _)| Some(n) != up_remote.as_ref());
        let detached = !self.local_names.contains(&branch);
        let mut items = Vec::new();
        let mut diverged: Option<String> = None;
        if !detached {
            for (name, url) in &remotes {
                let rb = format!("{name}/{branch}");
                let state = match repo.rev_parse(&format!("refs/remotes/{rb}")) {
                    Some(_) => {
                        let ahead = repo.count_range(&rb, "HEAD");
                        let behind = repo.count_range("HEAD", &rb);
                        if behind > 0 && diverged.is_none() {
                            diverged = Some(name.clone());
                        }
                        match (ahead, behind) {
                            (0, 0) => "up to date".to_string(),
                            (a, 0) => format!("↑{a}"),
                            (a, b) => format!("↑{a} ↓{b}"),
                        }
                    }
                    None => "new branch".to_string(),
                };
                let label = if Some(name) == up_remote.as_ref() { format!("{name}  (upstream)") } else { name.clone() };
                let mut it = item(label, Act::PushTo { remote: name.clone(), force: false });
                it.detail = format!("{state}  ·  {}", short_url(url));
                items.push(it);
            }
            if let Some(r) = &diverged {
                items.push(sep());
                let mut it = danger(format!("Force push to {r} (with lease)"), Act::PushTo { remote: r.clone(), force: true });
                it.detail = "overwrites remote commits you don't have".into();
                items.push(it);
            }
        }
        let note = if detached {
            Some(("Detached HEAD — check out a branch to push".to_string(), theme::ORANGE))
        } else if let Some(r) = &diverged {
            Some((format!("{branch} and {r}/{branch} have diverged — pull first, or force push"), theme::ORANGE))
        } else if upstream.is_none() {
            Some(("No upstream yet — the remote you pick becomes the upstream".to_string(), theme::DIM))
        } else {
            None
        };
        self.menu = Some(Menu { at, title: format!("Push {branch} to…"), note, items, fresh: true });
    }

    /// Remotes with the current branch's upstream remote first, plus that upstream ("origin/main").
    fn remotes_upstream_first(&self) -> (Vec<(String, String)>, Option<(String, String)>) {
        let Some(repo) = &self.repo else { return (vec![], None) };
        let mut remotes = repo.remotes();
        // "origin/feature/x" → ("origin", "feature/x"); remote names can't contain '/' here
        // because we match against the configured list.
        let upstream = repo.upstream().and_then(|u| {
            remotes.iter().find_map(|(n, _)| u.strip_prefix(&format!("{n}/")).map(|b| (n.clone(), b.to_string())))
        });
        let up = upstream.as_ref().map(|(r, _)| r.clone());
        remotes.sort_by_key(|(n, _)| Some(n) != up.as_ref());
        (remotes, upstream)
    }

    /// Remote picker for Fetch: every remote at once, or one of them.
    fn open_fetch_menu(&mut self, at: (f32, f32)) {
        let (remotes, upstream) = self.remotes_upstream_first();
        if remotes.is_empty() {
            self.toast_err("No remotes configured — add one with: git remote add origin <url>");
            return;
        }
        let mut items = Vec::new();
        if remotes.len() > 1 {
            let mut it = item("All remotes", Act::Fetch);
            it.detail = format!("{} remotes · prunes deleted branches", remotes.len());
            items.push(it);
            items.push(sep());
        }
        for (name, url) in &remotes {
            let up = upstream.as_ref().is_some_and(|(r, _)| r == name);
            let mut it = item(if up { format!("{name}  (upstream)") } else { name.clone() }, Act::FetchFrom(name.clone()));
            it.detail = short_url(url);
            items.push(it);
        }
        self.menu = Some(Menu { at, title: "Fetch from…".into(), note: None, items, fresh: true });
    }

    /// Remote picker for Pull: each remote that has this branch, with how far behind / ahead it
    /// is (as of the last fetch). Diverged remotes also offer rebase and merge.
    fn open_pull_menu(&mut self, at: (f32, f32)) {
        let (remotes, upstream) = self.remotes_upstream_first();
        if remotes.is_empty() {
            self.toast_err("No remotes configured — add one with: git remote add origin <url>");
            return;
        }
        let Some(repo) = &self.repo else { return };
        let branch = self.head_name.clone();
        let detached = !self.local_names.contains(&branch);
        let mut items = Vec::new();
        let mut diverged: Option<(String, String, usize)> = None;
        let mut any = false;
        if !detached {
            for (name, url) in &remotes {
                // The upstream may track a differently named branch; other remotes use the same name.
                let up = upstream.as_ref().filter(|(r, _)| r == name);
                let rbranch = up.map_or_else(|| branch.clone(), |(_, b)| b.clone());
                let rb = format!("{name}/{rbranch}");
                if repo.rev_parse(&format!("refs/remotes/{rb}")).is_none() {
                    continue;
                }
                any = true;
                let ahead = repo.count_range(&rb, "HEAD");
                let behind = repo.count_range("HEAD", &rb);
                let state = match (ahead, behind) {
                    (_, 0) => "up to date".to_string(),
                    (0, b) => format!("↓{b}"),
                    (a, b) => {
                        if diverged.is_none() {
                            diverged = Some((name.clone(), rbranch.clone(), a));
                        }
                        format!("↓{b} ↑{a} · diverged")
                    }
                };
                let label = if up.is_some() { format!("{rb}  (upstream)") } else { rb.clone() };
                let mut it = item(label, Act::PullFrom { remote: name.clone(), branch: rbranch, mode: "ff" });
                it.detail = format!("{state}  ·  {}", short_url(url));
                items.push(it);
            }
            if let Some((r, b, a)) = &diverged {
                items.push(sep());
                let mut it = item(format!("Rebase onto {r}/{b}"), Act::PullFrom { remote: r.clone(), branch: b.clone(), mode: "rebase" });
                it.detail = format!("replays your {a} commit{} on top", if *a == 1 { "" } else { "s" });
                items.push(it);
                let mut it = item(format!("Merge {r}/{b}"), Act::PullFrom { remote: r.clone(), branch: b.clone(), mode: "merge" });
                it.detail = "creates a merge commit".into();
                items.push(it);
            }
        }
        let note = if detached {
            Some(("Detached HEAD — check out a branch to pull into".to_string(), theme::ORANGE))
        } else if !any {
            Some((format!("No remote has {branch} yet — fetch, or push it first"), theme::DIM))
        } else if let Some((r, b, _)) = &diverged {
            Some((format!("{branch} and {r}/{b} have diverged — rebase or merge instead of fast-forward"), theme::ORANGE))
        } else {
            Some(("Fast-forward only · counts are as of the last fetch".to_string(), theme::DIM))
        };
        self.menu = Some(Menu { at, title: format!("Pull into {branch} from…"), note, items, fresh: true });
    }

    /// Right-click on a worktree in the sidebar.
    fn open_worktree_menu(&mut self, path: &Path, at: (f32, f32)) {
        let Some(w) = self.worktrees.iter().find(|w| w.path == path).cloned() else { return };
        let is_main = self.worktrees.first().is_some_and(|m| m.path == w.path);
        let p = w.path.display().to_string();
        let mut items = Vec::new();
        if !w.prunable {
            items.push(item("Open in tab", Act::OpenWorktree(p.clone())));
        }
        if w.prunable {
            let mut it = item("Prune stale worktrees", Act::PruneWorktrees);
            it.detail = "git worktree prune".into();
            items.push(it);
        } else if !is_main {
            items.push(sep());
            let mut it = danger("Remove worktree", Act::RemoveWorktree(p));
            it.detail = if w.changes > 0 { "commit or discard its changes first".into() } else { "undoable · keeps the branch".into() };
            items.push(it);
        }
        let what = match &w.branch {
            Some(b) => format!("on {b}"),
            None => format!("detached at {}", &w.head[..8.min(w.head.len())]),
        };
        let mut note = format!("{what} · {}", match w.changes {
            0 => "clean".to_string(),
            1 => "1 changed file".to_string(),
            n => format!("{n} changed files"),
        });
        if w.locked {
            note.push_str(" · locked");
        }
        if w.prunable {
            note = "The folder is gone — prune to drop it from git's list".into();
        }
        let color = if w.prunable || w.changes > 0 { theme::YELLOW } else { theme::DIM };
        let title = format!("{}{}", tilde(&w.path), if is_main { "  (main worktree)" } else { "" });
        self.menu = Some(Menu { at, title, note: Some((note, color)), items, fresh: true });
    }

    fn open_commit_menu(&mut self, row: usize, at: (f32, f32)) {
        let c = &self.commits[row];
        let (hash, short) = (c.hash.clone(), c.short().to_string());
        let head = self.head_name.clone();
        let is_head = Some(row) == self.head_row;
        let free = c.refs.iter().map(|r| r.strip_prefix("HEAD -> ").unwrap_or(r)).find(|r| self.branch_free(r)).map(String::from);
        let from = match free {
            Some(b) => WtFrom { rev: b.clone(), label: b.clone(), branch: Some(b), prefill: String::new() },
            None => WtFrom { rev: hash.clone(), label: short.clone(), branch: None, prefill: String::new() },
        };
        let mut items = vec![
            item("Create branch here…", Act::PromptBranchAt(hash.clone())),
            item("Check out this commit (detached)", Act::CheckoutDetached(hash.clone())),
            item("New worktree here…", Act::NewWorktree(from)),
            sep(),
        ];
        let note = if is_head { None } else { self.preview_note("HEAD", &hash, &short) };
        if !is_head {
            items.extend([
                item(format!("Merge into {head}"), Act::MergeInto { src: hash.clone(), dst: None }),
                item(format!("Rebase {head} onto this"), Act::RebaseOnto { branch: None, onto: hash.clone() }),
                item(format!("Cherry-pick onto {head}"), Act::CherryPick(hash.clone())),
            ]);
        }
        items.extend([
            item("Revert this commit", Act::Revert(hash.clone())),
            sep(),
            item(format!("Reset {head} here · keep staged (soft)"), Act::Reset(hash.clone(), "soft")),
            item(format!("Reset {head} here · keep unstaged (mixed)"), Act::Reset(hash.clone(), "mixed")),
            danger(format!("Reset {head} here · discard changes (hard)"), Act::Reset(hash.clone(), "hard")),
        ]);
        let title = format!("{short} · {}", c.subject);
        self.menu = Some(Menu { at, title, note, items, fresh: true });
    }

    fn open_branch_menu(&mut self, name: &str, at: (f32, f32)) {
        let Some(rf) = self.find_ref(name).cloned() else { return };
        let head = self.head_name.clone();
        let mut items = Vec::new();
        let local_twin = rf.name.split_once('/').map(|(_, b)| b.to_string());
        let wt = |rev: &str, branch: Option<String>, prefill: String| {
            Act::NewWorktree(WtFrom { rev: rev.to_string(), label: rf.name.clone(), branch, prefill })
        };
        match rf.kind {
            RefKind::Local if !rf.is_head => match self.branch_worktree(&rf.name) {
                Some(w) => {
                    let mut it = item(format!("Open worktree {}", w.name()), Act::OpenWorktree(w.path.display().to_string()));
                    it.detail = format!("{} is checked out there", rf.name);
                    items.push(it);
                }
                None => {
                    items.push(item(format!("Check out {}", rf.name), Act::Checkout(rf.name.clone())));
                    items.push(item("Check out in new worktree…", wt(&rf.name, Some(rf.name.clone()), String::new())));
                }
            },
            RefKind::Local => items.push(item("New worktree from here…", wt(&rf.name, None, String::new()))),
            RefKind::Remote => {
                let b = local_twin.clone().unwrap_or_default();
                if self.local_names.contains(&b) {
                    items.push(item(format!("Check out {b}"), Act::Checkout(b.clone())));
                    let free = self.branch_free(&b).then(|| b.clone());
                    items.push(item("New worktree from here…", wt(&rf.name, free, String::new())));
                } else {
                    items.push(item(format!("Check out as local '{b}'"), Act::Checkout(b.clone())));
                    items.push(item(format!("Check out as local '{b}' in new worktree…"), wt(&rf.name, None, b)));
                }
            }
            RefKind::Tag => items.push(item("New worktree from here…", wt(&rf.name, None, String::new()))),
        }
        let mut note = None;
        if !rf.is_head {
            note = self.preview_note("HEAD", &rf.target, &rf.name);
            items.push(item(format!("Merge {} into {head}", rf.name), Act::MergeInto { src: rf.name.clone(), dst: None }));
            items.push(item(format!("Rebase {head} onto {}", rf.name), Act::RebaseOnto { branch: None, onto: rf.name.clone() }));
            if self.repo.as_ref().is_some_and(|r| r.is_ancestor("HEAD", &rf.target)) {
                items.push(item(format!("Fast-forward {head} to {}", rf.name), Act::FastForward { src: rf.name.clone(), dst: None }));
            }
            items.push(item(format!("Compare {head} ↔ {}", rf.name), Act::CompareRef(rf.target.clone(), rf.name.clone())));
        }
        items.push(item("Create branch from here…", Act::PromptBranchAt(rf.target.clone())));
        if rf.kind != RefKind::Tag {
            let full = ref_full(&rf);
            if self.hidden.contains(&full) {
                items.push(item("Show branch in graph", Act::ShowRef(full)));
            } else if !rf.is_head {
                let mut it = item("Hide branch", Act::HideRef(full));
                it.detail = "from the graph and branch list".into();
                items.push(it);
            }
        }
        if rf.kind == RefKind::Local && !rf.is_head && self.branch_worktree(&rf.name).is_none() {
            items.push(sep());
            items.push(danger(format!("Delete branch {}", rf.name), Act::DeleteBranch(rf.name.clone())));
        }
        self.menu = Some(Menu { at, title: rf.name.clone(), note, items, fresh: true });
    }

    /// Menu shown when one branch label is dropped onto another.
    fn open_drop_menu(&mut self, src: &str, dst: &str, at: (f32, f32)) {
        let (Some(s), Some(d)) = (self.find_ref(src).cloned(), self.find_ref(dst).cloned()) else { return };
        let mut items = Vec::new();
        let opt = |r: &Ref| if r.is_head { None } else { Some(r.name.clone()) };
        if d.kind == RefKind::Local {
            items.push(item(format!("Merge {src} into {dst}"), Act::MergeInto { src: src.to_string(), dst: opt(&d) }));
            if self.repo.as_ref().is_some_and(|r| r.is_ancestor(&d.target, &s.target)) {
                items.push(item(format!("Fast-forward {dst} to {src}"), Act::FastForward { src: src.to_string(), dst: opt(&d) }));
            }
        }
        if s.kind == RefKind::Local {
            items.push(item(format!("Rebase {src} onto {dst}"), Act::RebaseOnto { branch: opt(&s), onto: dst.to_string() }));
        }
        items.push(sep());
        items.push(item(format!("Compare {dst} ↔ {src}"), Act::CompareRef(s.target.clone(), s.name.clone())));
        let note = self.preview_note(&d.target, &s.target, src);
        self.menu = Some(Menu { at, title: format!("{src}  →  {dst}"), note, items, fresh: true });
    }

    fn branch_drag(&mut self, ui: &mut Ui) {
        let Some(d) = &mut self.bdrag else { return };
        let m = ui.input.mouse;
        if !ui.input.down {
            let d = self.bdrag.take().unwrap();
            if d.active {
                if let Some((_, t)) = self.ref_hits.iter().find(|(r, n)| r.contains(m) && *n != d.name).cloned() {
                    self.open_drop_menu(&d.name, &t, m);
                }
            }
            return;
        }
        if !d.active {
            if (m.0 - d.start.0).abs() + (m.1 - d.start.1).abs() < 6.0 {
                return;
            }
            d.active = true;
        }
        let name = d.name.clone();
        ui.cursor = Cursor::Grabbing;
        ui.animating = true;
        let target = self.ref_hits.iter().find(|(r, n)| r.contains(m) && *n != name).cloned();
        if let Some((r, _)) = &target {
            ui.p.rounded_stroke(Rect::new(r.x - 2.0, r.y - 2.0, r.w + 4.0, r.h + 4.0), 10.0, 2.0, theme::ACCENT);
        }
        let label = match &target {
            Some((_, t)) => format!("{name}  →  {t}"),
            None => format!("{name}  →  drop on a branch"),
        };
        let w = ui.p.measure(UI, 12.5, &label) + 24.0;
        let g = Rect::new(m.0 + 12.0, m.1 + 8.0, w, 26.0);
        ui.p.rounded(Rect::new(g.x, g.y + 3.0, g.w, g.h), 8.0, Color::hex(0x000000).alpha(0.4));
        ui.p.rounded(g, 8.0, theme::RAISED);
        ui.p.rounded_stroke(g, 8.0, 1.0, theme::ACCENT.alpha(if target.is_some() { 0.8 } else { 0.3 }));
        ui.p.text(g.x + 12.0, g.cy(), UI, 12.5, &label, if target.is_some() { theme::TEXT } else { theme::DIM });
    }

    fn draw_menu(&mut self, ui: &mut Ui) {
        if ui.input.take_key(Key::Escape, false).is_some() {
            self.menu = None;
            return;
        }
        let Some(m) = &self.menu else { return };
        let row_h = 30.0;
        let mut w: f32 = ui.p.measure(UI, 11.5, &m.title).min(380.0) + 32.0;
        for it in &m.items {
            let dw = if it.detail.is_empty() { 0.0 } else { ui.p.measure(UI, 11.5, &it.detail) + 24.0 };
            w = w.max(ui.p.measure(UI, 13.0, &it.label) + 40.0 + dw);
        }
        if let Some((n, _)) = &m.note {
            w = w.max((ui.p.measure(UI, 11.5, n) + 40.0).min(460.0));
        }
        let w = w.clamp(240.0, 560.0);
        let body: f32 = m.items.iter().map(|it| if it.act.is_some() { row_h } else { 9.0 }).sum();
        let h = 34.0 + if m.note.is_some() { 26.0 } else { 0.0 } + body + 8.0;
        let s = ui.screen;
        let x = m.at.0.min(s.right() - w - 8.0).max(8.0);
        let y = m.at.1.min(s.bottom() - h - 8.0).max(8.0);
        let r = Rect::new(x, y, w, h);
        let fresh = m.fresh;
        if !fresh && (ui.input.pressed || ui.input.right_pressed) && !r.contains(ui.input.mouse) {
            self.menu = None;
            return;
        }
        ui.p.rounded(Rect::new(r.x, r.y + 6.0, r.w, r.h), 12.0, Color::hex(0x000000).alpha(0.45));
        ui.p.rounded(r, 10.0, theme::PANEL2);
        ui.p.rounded_stroke(r, 10.0, 1.0, theme::BORDER.mix(theme::TEXT, 0.1));
        ui.p.text_fit(r.x + 14.0, r.y + 18.0, r.w - 28.0, UI, 11.5, &m.title, theme::FAINT);
        let mut cy = r.y + 34.0;
        if let Some((n, c)) = &m.note {
            ui.p.circle((r.x + 18.0, cy + 9.0), 3.5, *c);
            ui.p.text_fit(r.x + 28.0, cy + 9.0, r.w - 40.0, UI, 11.5, n, *c);
            cy += 26.0;
        }
        let mut chosen = None;
        for it in &m.items {
            match &it.act {
                None => {
                    ui.p.rect(Rect::new(r.x + 10.0, cy + 4.0, r.w - 20.0, 1.0), theme::BORDER);
                    cy += 9.0;
                }
                Some(a) => {
                    let row = Rect::new(r.x + 5.0, cy, r.w - 10.0, row_h);
                    let hov = row.contains(ui.input.mouse);
                    if hov {
                        ui.p.rounded(row, 6.0, if it.danger { theme::RED.alpha(0.18) } else { theme::SELECT });
                        ui.cursor = Cursor::Pointer;
                        if ui.input.pressed {
                            chosen = Some(a.clone());
                        }
                    }
                    let fg = if it.danger { theme::RED } else { theme::TEXT };
                    let dw = if it.detail.is_empty() { 0.0 } else { ui.p.measure(UI, 11.5, &it.detail).min(row.w * 0.6) };
                    ui.p.text_fit(row.x + 10.0, row.cy(), row.w - 30.0 - dw, UI, 13.0, &it.label, fg);
                    if dw > 0.0 {
                        ui.p.text_fit(row.right() - 10.0 - dw, row.cy(), dw, UI, 11.5, &it.detail, theme::DIM);
                    }
                    cy += row_h;
                }
            }
        }
        if let Some(m) = &mut self.menu {
            m.fresh = false;
        }
        if let Some(a) = chosen {
            self.menu = None;
            self.act(a);
        }
    }

    // ---- conflict resolver --------------------------------------------------------------------

    fn open_resolver(&mut self, path: &str) {
        let Some(repo) = &self.repo else { return };
        if !self.docs.contains_key(path) {
            if let Ok(bytes) = std::fs::read(repo.root.join(path)) {
                if let Some(mut doc) = ConflictDoc::parse(path, bytes) {
                    // Who wrote each side: HEAD's version and the incoming commit's version.
                    let theirs_rev = match self.op {
                        Some(OpKind::Merge) => Some("MERGE_HEAD"),
                        Some(OpKind::Rebase) => Some("REBASE_HEAD"),
                        Some(OpKind::CherryPick) => Some("CHERRY_PICK_HEAD"),
                        Some(OpKind::Revert) => Some("REVERT_HEAD^"),
                        None => None,
                    };
                    let ours = git::blame_at(&repo.root, &Side::Rev("HEAD".into()), path);
                    let theirs = theirs_rev
                        .filter(|r| repo.rev_parse(r).is_some())
                        .and_then(|r| git::blame_at(&repo.root, &Side::Rev(r.into()), path));
                    doc.attribute(ours, theirs);
                    self.docs.insert(path.to_string(), doc);
                }
            }
        }
        if self.sel != Sel::Wip {
            self.select(Sel::Wip);
        }
        self.compare = None;
        self.file_view = None;
        self.wip_file = Some((path.to_string(), false));
        self.resolving = Some(path.to_string());
        if self.wide_before.is_none() {
            // Resolving is focused work: give it the whole window, restore on close.
            self.wide_before = Some(self.wide);
            self.wide = true;
        }
    }

    fn close_resolver(&mut self) {
        self.resolving = None;
        if let Some(w) = self.wide_before.take() {
            self.wide = w;
        }
        self.load_wip_diff();
    }

    /// Human names for the two sides. During a rebase git's "ours" is the branch being rebased
    /// onto and "theirs" is your own commit, which trips everyone up, so say it plainly.
    fn side_labels(&self, doc: Option<&ConflictDoc>) -> (String, String) {
        let marker = doc.map(|d| d.incoming_label()).unwrap_or_default();
        let theirs = if marker.is_empty() { "incoming".to_string() } else { marker };
        match self.op {
            Some(OpKind::Rebase) => ("Upstream · what you're rebasing onto".into(), format!("Your commit · {theirs}")),
            Some(OpKind::CherryPick) => (format!("Current · {}", self.head_name), format!("Cherry-picked · {theirs}")),
            Some(OpKind::Revert) => (format!("Current · {}", self.head_name), format!("Revert · {theirs}")),
            _ => (format!("Yours · {}", self.head_name), format!("Incoming · {theirs}")),
        }
    }

    fn resolver_panel(&mut self, ui: &mut Ui, r: Rect) {
        let path = self.resolving.clone().unwrap();
        let op = self.op.map_or("Conflict".to_string(), |o| format!("{} conflict", o.name()));
        if !self.docs.contains_key(&path) {
            self.unmarked_conflict(ui, r, &path);
            return;
        }
        let (ours, theirs) = self.side_labels(self.docs.get(&path));
        let doc = self.docs.get_mut(&path).unwrap();
        let action = doc.draw(ui, r, (&ours, &theirs), &op);
        match action {
            Some(ResolveAction::Back) => self.close_resolver(),
            Some(ResolveAction::Reload) => {
                self.docs.remove(&path);
                self.open_resolver(&path);
            }
            Some(ResolveAction::OpenExternal) => {
                if let Some(repo) = &self.repo {
                    open_external(&repo.root.join(&path));
                }
            }
            Some(ResolveAction::Save) => self.save_resolution(&path),
            None => {}
        }
    }

    fn save_resolution(&mut self, path: &str) {
        let (Some(repo), Some(doc)) = (&self.repo, self.docs.get(path)) else { return };
        let text = doc.render();
        let original = doc.original.clone();
        let full = repo.root.join(path);
        let r = std::fs::write(&full, text).map_err(|e| e.to_string()).and_then(|_| repo.git(&["add", "--", path]));
        if !self.result(r, "") {
            return;
        }
        // Undo: put the markers back and let git restore the unmerged index entries.
        self.push_undo(Undo::Many(
            vec![
                Undo::Restore { path: path.to_string(), data: original, label: String::new() },
                Undo::Cmd { args: vec!["update-index".into(), "--unresolve".into(), "--".into(), path.to_string()], label: String::new() },
            ],
            format!("resolve {path}"),
        ));
        self.docs.remove(path);
        self.reload_status();
        match self.changes.iter().find(|c| c.conflicted()).map(|c| c.path.clone()) {
            Some(next) => self.open_resolver(&next),
            None => {
                self.close_resolver();
                let what = self.op.map_or("operation".to_string(), |o| o.name().to_lowercase());
                self.toast_ok(format!("All conflicts resolved — click Continue to finish the {what}"));
            }
        }
    }

    /// Conflicts without markers: modify/delete, binary files, or markers already removed by hand.
    fn unmarked_conflict(&mut self, ui: &mut Ui, r: Rect, path: &str) {
        ui.p.rect(r, theme::BG);
        let change = self.changes.iter().find(|c| c.path == path).cloned();
        let mut act = None;
        let mut y = r.y + 24.0;
        if ui.button(Rect::new(r.x + 16.0, y - 13.0, 64.0, 26.0), "← Back", false, true) {
            self.close_resolver();
            return;
        }
        ui.p.text_fit(r.x + 92.0, y, r.w - 110.0, UI, 14.0, path, theme::TEXT);
        y += 48.0;
        let (title, detail, buttons): (&str, &str, Vec<(&str, Act, bool)>) = match &change {
            None => ("Resolved", "This file is no longer in conflict.", vec![]),
            Some(c) if c.x == 'D' || c.y == 'D' => (
                if c.x == 'D' { "Deleted on your side, changed on the incoming side" } else { "Changed on your side, deleted on the incoming side" },
                "Keep the file with its changes, or delete it.",
                vec![("Keep file", Act::MarkResolved(path.to_string()), true), ("Delete file", Act::DeleteConflicted(path.to_string()), false)],
            ),
            Some(_) => (
                "No conflict markers in this file",
                "It may be binary, or you already edited the markers away. Pick a version, or mark it resolved as it is.",
                vec![
                    ("Mark resolved", Act::MarkResolved(path.to_string()), true),
                    ("Use incoming", Act::ResolveUse(path.to_string(), false), false),
                    ("Use HEAD", Act::ResolveUse(path.to_string(), true), false),
                ],
            ),
        };
        ui.p.text(r.x + 24.0, y, UI, 16.0, title, theme::TEXT);
        y += 28.0;
        ui.p.text_fit(r.x + 24.0, y, r.w - 48.0, UI, 12.5, detail, theme::DIM);
        y += 36.0;
        let mut x = r.x + 24.0;
        for (label, a, primary) in buttons {
            let w = ui.p.measure(UI, 12.5, label) + 28.0;
            if ui.button(Rect::new(x, y - 15.0, w, 30.0), label, primary, true) {
                act = Some(a);
            }
            x += w + 8.0;
        }
        if let Some(a) = act {
            self.act(a);
            if !self.changes.iter().any(|c| c.path == path && c.conflicted()) {
                match self.changes.iter().find(|c| c.conflicted()).map(|c| c.path.clone()) {
                    Some(next) => self.open_resolver(&next),
                    None => self.close_resolver(),
                }
            }
        }
    }

    fn draw_toast(&mut self, ui: &mut Ui) {
        let Some(t) = &self.toast else { return };
        if ui.now >= t.until {
            self.toast = None;
            return;
        }
        ui.wake_in(t.until - ui.now);
        let msg: String = t.msg.lines().take(4).collect::<Vec<_>>().join("  ");
        let (err, has_undo) = (t.err, t.undo);
        let s = ui.screen;
        let max_w = (s.w * 0.5).max(300.0);
        let undo_w = if has_undo { 64.0 } else { 0.0 };
        let w = (ui.p.measure(UI, 12.5, &msg) + 44.0 + undo_w).min(max_w);
        let r = Rect::new(s.right() - w - 20.0, s.bottom() - 58.0, w, 38.0);
        let c = if err { theme::RED } else { theme::GREEN };
        ui.p.rounded(Rect::new(r.x, r.y + 3.0, r.w, r.h), 10.0, Color::hex(0x000000).alpha(0.35));
        ui.p.rounded(r, 10.0, theme::RAISED);
        ui.p.rounded_stroke(r, 10.0, 1.0, c.alpha(0.4));
        ui.p.circle((r.x + 16.0, r.cy()), 4.0, c);
        ui.p.text_fit(r.x + 28.0, r.cy(), r.w - 40.0 - undo_w, UI, 12.5, &msg, theme::TEXT);
        if has_undo {
            let b = Rect::new(r.right() - undo_w - 6.0, r.y + 6.0, undo_w, r.h - 12.0);
            if ui.button(b, "Undo", true, true) {
                self.toast = None;
                self.undo();
                return;
            }
        }
        if ui.hovered(r) && ui.input.pressed {
            self.toast = None;
        }
    }
}

/// "git@github.com:me/repo.git" / "https://github.com/me/repo.git" → "github.com/me/repo".
fn short_url(url: &str) -> String {
    let s = url.trim_end_matches('/').trim_end_matches(".git");
    match s.split_once("://") {
        Some((_, rest)) => rest.rsplit_once('@').map_or(rest, |(_, r)| r).to_string(),
        // Local path: the last two segments are what identify it.
        None if s.starts_with(['/', '.', '~']) || s.contains('\\') => {
            let parts: Vec<&str> = s.split(['/', '\\']).filter(|p| !p.is_empty() && *p != "..").collect();
            match parts.len() {
                0 => s.to_string(),
                1 => parts[0].to_string(),
                n => format!("…/{}/{}", parts[n - 2], parts[n - 1]),
            }
        }
        // scp-like syntax: user@host:path
        None => s.rsplit_once('@').map_or(s, |(_, r)| r).replacen(':', "/", 1),
    }
}

/// Eight dots chasing each other around `c` (keeps the UI animating while shown).
fn spinner(ui: &mut Ui, c: (f32, f32), color: Color) {
    let phase = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()) % 1000) as f32 / 1000.0;
    for k in 0..8 {
        let a = (k as f32 / 8.0 + phase) * std::f32::consts::TAU;
        let alpha = 0.15 + 0.85 * (k as f32 / 8.0);
        ui.p.circle((c.0 + a.cos() * 6.0, c.1 + a.sin() * 6.0), 1.4, color.alpha(alpha));
    }
    ui.animating = true;
}

/// Worktree paths an undo step removes (their tabs should close).
fn removed_worktrees(u: &Undo) -> Vec<PathBuf> {
    match u {
        Undo::Cmd { args, .. } if args.len() >= 3 && args[0] == "worktree" && args[1] == "remove" => {
            vec![PathBuf::from(args.last().unwrap())]
        }
        Undo::Many(items, _) => items.iter().flat_map(removed_worktrees).collect(),
        _ => vec![],
    }
}

/// Worktree paths an undo step adds back (undoing "remove worktree").
fn added_worktrees(u: &Undo) -> Vec<PathBuf> {
    match u {
        Undo::Cmd { args, .. } if args.len() >= 3 && args[0] == "worktree" && args[1] == "add" => {
            args[2..].iter().find(|a| !a.starts_with('-')).map(PathBuf::from).into_iter().collect()
        }
        Undo::Many(items, _) => items.iter().flat_map(added_worktrees).collect(),
        _ => vec![],
    }
}

/// Full ref name ("refs/heads/main", "refs/remotes/origin/main").
fn ref_full(r: &Ref) -> String {
    match r.kind {
        RefKind::Local => format!("refs/heads/{}", r.name),
        RefKind::Remote => format!("refs/remotes/{}", r.name),
        RefKind::Tag => format!("refs/tags/{}", r.name),
    }
}

/// Opens a file with the platform's default application.
fn open_external(path: &std::path::Path) {
    let (cmd, args): (&str, Vec<&std::ffi::OsStr>) = if cfg!(target_os = "macos") {
        ("open", vec![path.as_os_str()])
    } else if cfg!(windows) {
        ("cmd", vec!["/C".as_ref(), "start".as_ref(), "".as_ref(), path.as_os_str()])
    } else {
        ("xdg-open", vec![path.as_os_str()])
    };
    let _ = std::process::Command::new(cmd).args(args).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::KeyPress;

    fn temp_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wgitpui-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@example.com"], &["config", "user.name", "T"]] {
            git::run(&dir, args, None).unwrap();
        }
        for n in ["one", "two"] {
            std::fs::write(dir.join(n), n).unwrap();
            git::run(&dir, &["add", "."], None).unwrap();
            git::run(&dir, &["commit", "-qm", n], None).unwrap();
        }
        dir
    }

    /// Drives the real UI: one frame per call, with the given input applied.
    struct Harness {
        app: GitApp,
        ui: Ui,
    }

    impl Harness {
        fn new(dir: &PathBuf) -> Self {
            let mut h = Harness { app: GitApp::new(Some(dir.clone())), ui: Ui::new(crate::font::Fonts::load().unwrap()) };
            h.app.finish_loading();
            h.frame();
            h
        }
        fn frame(&mut self) {
            self.ui.begin(1400.0, 900.0, 1.0);
            self.app.frame(&mut self.ui);
            self.ui.input.end_frame();
        }
        fn click(&mut self, x: f32, y: f32) {
            self.ui.input.mouse = (x, y);
            self.ui.input.pressed = true;
            self.ui.input.down = true;
            self.frame();
            self.ui.input.down = false;
            self.ui.input.released = true;
            self.frame();
        }
        fn type_text(&mut self, t: &str) {
            self.ui.input.text = t.to_string();
            self.frame();
        }
        fn key(&mut self, key: Key) {
            self.ui.input.keys.push(KeyPress { key, mods: Default::default() });
            self.frame();
        }
        fn branch(&self) -> String {
            self.app.repo.as_ref().unwrap().head_name()
        }
    }

    #[test]
    fn create_branch_from_search_box() {
        let dir = temp_repo("branch-palette");
        let mut h = Harness::new(&dir);
        h.click(700.0, TOP_H * 0.5); // the "Search commits, branches, actions…" box
        assert!(h.app.focus == Focus::Palette, "palette should stay open after clicking the search box");
        h.type_text("feature-x");
        h.key(Key::Enter);
        assert_eq!(h.branch(), "feature-x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_branch_from_commit_menu() {
        let dir = temp_repo("branch-menu");
        let mut h = Harness::new(&dir);
        let older = 1; // "one"
        let at = (500.0, 300.0);
        h.app.open_commit_menu(older, at);
        h.frame();
        // First item of the menu: "Create branch here…" (title row is 34px, then a note row).
        let note = if h.app.menu.as_ref().unwrap().note.is_some() { 26.0 } else { 0.0 };
        h.click(at.0 + 40.0, at.1 + 34.0 + note + 15.0);
        assert!(h.app.focus == Focus::Palette, "name prompt should be open");
        h.type_text("from-one");
        h.key(Key::Enter);
        let repo = h.app.repo.as_ref().unwrap();
        assert_eq!(repo.rev_parse("from-one"), Some(h.app.commits[older].hash.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_branch_with_plus_button_and_undo() {
        let dir = temp_repo("branch-plus");
        let mut h = Harness::new(&dir);
        // Sidebar: WORKSPACE header + working-copy row, then the BRANCHES header with its "+".
        let header_y = TOP_H + 8.0 + 26.0 + LIST_ROW_H + 12.0;
        h.click(SIDEBAR_W - 34.0 + 11.0, header_y + 13.0);
        assert!(h.app.focus == Focus::Palette && h.app.palette.branch_at.is_some(), "+ should open the name prompt");
        h.type_text("topic");
        h.key(Key::Enter);
        assert_eq!(h.branch(), "topic");
        // ⌘Z / Ctrl+Z undoes: back on main, branch gone.
        let mods = crate::ui::Mods { logo: cfg!(target_os = "macos"), ctrl: !cfg!(target_os = "macos"), ..Default::default() };
        h.ui.input.keys.push(KeyPress { key: Key::Char('z'), mods });
        h.frame();
        assert_eq!(h.branch(), "main");
        assert!(h.app.repo.as_ref().unwrap().rev_parse("refs/heads/topic").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    impl Harness {
        /// Clicks the first menu item whose label starts with `prefix` (mirrors draw_menu's layout).
        fn click_menu_item(&mut self, prefix: &str) {
            let m = self.app.menu.as_ref().expect("menu open");
            let (x, mut y) = (m.at.0, m.at.1 + 34.0 + if m.note.is_some() { 26.0 } else { 0.0 });
            let mut target = None;
            for it in &m.items {
                if it.act.is_none() {
                    y += 9.0;
                    continue;
                }
                if it.label.starts_with(prefix) {
                    target = Some((x + 40.0, y + 15.0));
                    break;
                }
                y += 30.0;
            }
            let (cx, cy) = target.unwrap_or_else(|| panic!("no menu item starting with {prefix:?}"));
            self.click(cx, cy);
        }
    }

    #[test]
    fn hide_branch_from_menu_and_show_again() {
        let dir = temp_repo("hide");
        git::run(&dir, &["switch", "-q", "-c", "experiment"], None).unwrap();
        std::fs::write(dir.join("x"), "x").unwrap();
        git::run(&dir, &["add", "."], None).unwrap();
        git::run(&dir, &["commit", "-qm", "experiment only"], None).unwrap();
        git::run(&dir, &["switch", "-q", "main"], None).unwrap();
        let mut h = Harness::new(&dir);
        let has = |h: &Harness| h.app.commits.iter().any(|c| c.subject == "experiment only");
        assert!(has(&h));

        h.app.open_branch_menu("experiment", (400.0, 150.0));
        h.frame();
        h.click_menu_item("Hide branch");
        h.app.finish_loading();
        h.frame();
        assert!(h.app.hidden.contains("refs/heads/experiment"));
        assert!(!has(&h), "hidden branch's commits should leave the graph");

        // Persisted per repository: a fresh app still hides it.
        let mut h2 = Harness::new(&dir);
        assert!(!has(&h2));

        // Sidebar: WORKSPACE, BRANCHES (main), then HIDDEN with the "Show" button on hover.
        let hidden_header = TOP_H + 8.0 + 26.0 + LIST_ROW_H + 12.0 + 30.0 + LIST_ROW_H + 8.0;
        let row_cy = hidden_header + 30.0 + LIST_ROW_H * 0.5;
        h2.ui.input.mouse = (100.0, row_cy);
        h2.frame(); // hover shows the button
        h2.click(SIDEBAR_W - 6.0 - 50.0 + 23.0, row_cy);
        h2.app.finish_loading();
        h2.frame();
        assert!(h2.app.hidden.is_empty());
        assert!(has(&h2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    impl Harness {
        /// Runs frames until the background job (fetch / pull / push) has finished.
        fn wait_job(&mut self) {
            for _ in 0..200 {
                if self.app.job.is_none() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
                self.frame();
            }
            panic!("background job didn't finish");
        }
        /// Runs frames until background worktree adds / removes have finished.
        fn wait_wt_jobs(&mut self) {
            for _ in 0..400 {
                if self.app.wt_jobs.is_empty() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
                self.frame();
            }
            panic!("worktree job didn't finish");
        }
        fn menu_labels(&self) -> Vec<String> {
            self.app.menu.as_ref().expect("menu open").items.iter().filter(|i| i.act.is_some()).map(|i| i.label.clone()).collect()
        }
    }

    #[test]
    fn worktrees_add_commit_redirect_remove_undo() {
        let dir = temp_repo("wt");
        let mut h = Harness::new(&dir);
        h.app.in_tab = true;
        assert!(h.app.other_worktrees().is_empty());

        // New worktree on a new branch, through the prompt the "+" opens.
        let from = h.app.wt_from_selection().unwrap();
        h.app.act(Act::NewWorktree(from));
        h.frame();
        assert!(h.app.palette.worktree_from.is_some());
        h.type_text("wt-topic");
        let item = &h.app.palette.items[0];
        assert!(item.label.starts_with("New branch 'wt-topic'"), "{}", item.label);
        let Act::AddWorktree { path, .. } = item.act.clone() else { panic!() };
        assert!(path.ends_with(&format!("{}-wt-topic", dir.file_name().unwrap().to_string_lossy())), "{path}");
        h.key(Key::Enter);
        // The checkout runs in the background: a busy row until it's done, then a tab.
        assert!(h.app.sidebar_worktrees().iter().any(|(w, busy)| w.path.ends_with(format!("{}-wt-topic", dir.file_name().unwrap().to_string_lossy())) && *busy == Some("adding…")));
        assert!(h.app.open_requests.is_empty());
        h.wait_wt_jobs();
        let others = h.app.other_worktrees();
        assert_eq!(others.len(), 1);
        assert_eq!(others[0].branch.as_deref(), Some("wt-topic"));
        let wt = others[0].path.clone();
        assert_eq!(h.app.open_requests, vec![wt.clone()], "the new worktree opens in a tab");
        h.app.open_requests.clear();

        // Inside the worktree: stage and commit like in any working copy.
        let mut w = Harness::new(&wt);
        std::fs::write(wt.join("three"), "3").unwrap();
        w.app.reload_all(false);
        assert_eq!(w.app.changes.len(), 1);
        w.app.act(Act::StageAll);
        w.app.msg.set("from the worktree");
        w.app.act(Act::Commit);
        assert!(w.app.changes.is_empty());
        assert_eq!(w.app.repo.as_ref().unwrap().last_message(), "from the worktree");
        assert_eq!(w.branch(), "wt-topic");
        // The worktree's tab sees the main working copy as another worktree.
        assert!(w.app.other_worktrees().iter().any(|o| o.branch.as_deref() == Some("main")));

        // Back in the main tab: the branch moved, and checking it out goes to its worktree.
        h.app.reload_all(false);
        let repo = h.app.repo.as_ref().unwrap();
        assert_eq!(repo.git(&["log", "-1", "--format=%s", "wt-topic"]).unwrap().trim(), "from the worktree");
        h.app.act(Act::Checkout("wt-topic".into()));
        assert_eq!(h.branch(), "main");
        assert_eq!(h.app.open_requests, vec![wt.clone()]);

        // Dirty worktrees aren't removed.
        std::fs::write(wt.join("dirty"), "x").unwrap();
        h.app.reload_all(false);
        h.app.act(Act::RemoveWorktree(wt.display().to_string()));
        assert!(wt.exists());
        assert!(h.app.toast.as_ref().is_some_and(|t| t.err));
        std::fs::remove_file(wt.join("dirty")).unwrap();
        h.app.reload_all(false);

        // Clean: removed on a background thread. Its tab closes right away, the row shows it's
        // being removed, and undo waits until git is done.
        let steps = h.app.undo.len();
        h.app.act(Act::RemoveWorktree(wt.display().to_string()));
        assert_eq!(h.app.close_requests, vec![wt.clone()]);
        assert!(h.app.sidebar_worktrees().iter().any(|(w, busy)| w.path == wt && *busy == Some("removing…")));
        h.app.undo();
        assert_eq!(h.app.undo.len(), steps, "no undo while a removal is in flight");
        h.wait_wt_jobs();
        assert!(!wt.exists());
        assert!(h.app.sidebar_worktrees().is_empty());
        assert_eq!(h.app.undo.len(), steps + 1);
        // Undo checks it out again (in the background too), on the same branch.
        h.app.undo();
        assert!(h.app.sidebar_worktrees().iter().any(|(w, busy)| w.path == wt && *busy == Some("adding…")));
        h.wait_wt_jobs();
        assert!(wt.join("three").exists());
        assert_eq!(h.app.other_worktrees()[0].branch.as_deref(), Some("wt-topic"));

        // Undoing the add removes the worktree (in the background too) and its branch.
        h.app.close_requests.clear();
        h.app.undo();
        assert_eq!(h.app.close_requests, vec![wt.clone()]);
        h.wait_wt_jobs();
        assert!(!wt.exists());
        assert!(h.app.repo.as_ref().unwrap().rev_parse("refs/heads/wt-topic").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stage_and_unstage_lines_of_a_new_file_in_a_worktree() {
        let dir = temp_repo("wt-lines");
        let wt = dir.with_file_name(format!("{}-wt", dir.file_name().unwrap().to_string_lossy()));
        let _ = std::fs::remove_dir_all(&wt);
        git::run(&dir, &["worktree", "add", "-q", "-b", "lines", wt.to_str().unwrap()], None).unwrap();
        std::fs::write(wt.join("new"), "x\ny\nz\n").unwrap();
        let mut h = Harness::new(&wt);
        assert_eq!(h.app.wip_file, Some(("new".to_string(), false)));
        assert!(h.app.diff.mode == DiffMode::Untracked);
        let show = |h: &Harness| h.app.repo.as_ref().unwrap().git(&["show", ":new"]);

        // Stage "y" and "z" from the untracked file.
        h.app.diff.selected.extend([(0, 0, 1), (0, 0, 2)]);
        h.app.apply_diff_action(DiffAction::Hunk { f: 0, h: 0, lines_only: true });
        assert!(h.app.toast.as_ref().is_none_or(|t| !t.err), "{:?}", h.app.toast.as_ref().map(|t| &t.msg));
        assert_eq!(show(&h).unwrap(), "y\nz\n");

        // Switch to the staged side of the file and unstage "y" again.
        h.app.wip_file = Some(("new".into(), true));
        h.app.load_wip_diff();
        assert!(h.app.diff.mode == DiffMode::Staged);
        let y = h.app.diff.files[0].hunks[0].lines.iter().position(|l| l == "+y").unwrap();
        h.app.diff.selected.insert((0, 0, y));
        h.app.apply_diff_action(DiffAction::Hunk { f: 0, h: 0, lines_only: true });
        assert!(h.app.toast.as_ref().is_none_or(|t| !t.err), "{:?}", h.app.toast.as_ref().map(|t| &t.msg));
        assert_eq!(show(&h).unwrap(), "z\n");
        let _ = git::run(&dir, &["worktree", "remove", "--force", wt.to_str().unwrap()], None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wide_working_copy_puts_the_staging_column_left_of_the_diff() {
        let dir = temp_repo("wide-wip");
        std::fs::write(dir.join("one"), "1\n").unwrap();
        std::fs::write(dir.join("two"), "2\n").unwrap();
        let mut h = Harness::new(&dir);
        h.app.wide = true;
        h.frame();
        // Column: "Working copy" header (44), UNSTAGED header (30), then one row per file.
        // (Stacked, x = 100 would be in the sidebar.)
        let row = |i: usize| TOP_H + 44.0 + 30.0 + i as f32 * LIST_ROW_H + LIST_ROW_H * 0.5;
        h.click(100.0, row(1));
        assert_eq!(h.app.wip_file, Some(("two".to_string(), false)));
        assert_eq!(h.app.diff.files[0].path, "two");
        // Each row's Stage button sits at the column's right edge.
        let col_right = 1.0 + h.app.side_col_w(1399.0);
        h.click(col_right - 6.0 - 30.0, row(1));
        assert!(h.app.changes.iter().any(|c| c.path == "two" && c.staged() && !c.unstaged()));
        // Dragging the divider resizes the column.
        h.ui.input.mouse = (col_right - 2.0, 500.0);
        h.ui.input.pressed = true;
        h.ui.input.down = true;
        h.frame();
        h.ui.input.mouse = (600.0, 500.0);
        h.frame();
        h.ui.input.down = false;
        h.frame();
        assert_eq!(h.app.side_col_w(1399.0), 599.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wide_commit_view_lists_files_in_a_column_beside_the_diff() {
        let dir = temp_repo("wide-commit");
        let long: String = (0..200).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("a.txt"), &long).unwrap();
        std::fs::write(dir.join("b.txt"), &long).unwrap();
        git::run(&dir, &["add", "."], None).unwrap();
        git::run(&dir, &["commit", "-qm", "two files"], None).unwrap();
        let mut h = Harness::new(&dir);
        assert!(h.app.sel_row() == Some(0) && h.app.diff.files.len() == 2);
        h.app.wide = true;
        h.frame();
        // Column: commit header (one subject line, no body), tabs, "2 FILES" header, then the rows.
        // (Stacked, x = 100 would be in the sidebar.)
        let row = |i: usize| TOP_H + 68.0 + 34.0 + 26.0 + i as f32 * 22.0 + 11.0;
        h.click(100.0, row(1));
        assert!(h.app.diff.scroll.target > 0.0, "clicking b.txt in the column scrolls the diff to it");
        // The Files tab keeps the column: tree on the left, the opened file on the right.
        h.app.act(Act::BrowseFiles);
        h.frame();
        // Same column: compact header (68), tabs (34), then the tree's rows (4px top margin).
        let tree_row = |i: usize| TOP_H + 68.0 + 34.0 + 4.0 + i as f32 * 22.0 + 11.0;
        h.click(100.0, tree_row(0));
        assert_eq!(h.app.file_view.as_ref().map(|f| f.path.as_str()), Some("a.txt"));
        assert_eq!(h.app.tree.selected.as_deref(), Some("a.txt"), "the tree highlights the open file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_and_pull_pick_a_remote() {
        let dir = temp_repo("pull");
        let up = dir.with_file_name(format!("{}-up.git", dir.file_name().unwrap().to_string_lossy()));
        let _ = std::fs::remove_dir_all(&up);
        git::run(&dir, &["clone", "-q", "--bare", ".", up.to_str().unwrap()], None).unwrap();
        git::run(&dir, &["remote", "add", "origin", up.to_str().unwrap()], None).unwrap();
        git::run(&dir, &["fetch", "-q", "origin"], None).unwrap();
        git::run(&dir, &["branch", "-q", "-u", "origin/main"], None).unwrap();
        // Someone else pushes a commit.
        let other = dir.with_file_name(format!("{}-other", dir.file_name().unwrap().to_string_lossy()));
        let _ = std::fs::remove_dir_all(&other);
        git::run(&dir, &["clone", "-q", up.to_str().unwrap(), other.to_str().unwrap()], None).unwrap();
        std::fs::write(other.join("theirs"), "t").unwrap();
        for args in [&["add", "."][..], &["-c", "user.name=O", "-c", "user.email=o@example.com", "commit", "-qm", "theirs"], &["push", "-q"]] {
            git::run(&other, args, None).unwrap();
        }

        let mut h = Harness::new(&dir);
        h.app.open_fetch_menu((900.0, 60.0));
        h.frame();
        assert_eq!(h.menu_labels(), vec!["origin  (upstream)"]);
        h.click_menu_item("origin");
        h.wait_job();
        assert!(h.app.toast.as_ref().is_some_and(|t| t.msg == "Fetched origin"), "{:?}", h.app.toast.as_ref().map(|t| &t.msg));

        h.app.open_pull_menu((900.0, 60.0));
        h.frame();
        let m = h.app.menu.as_ref().unwrap();
        assert_eq!(m.items[0].label, "origin/main  (upstream)");
        assert!(m.items[0].detail.starts_with("↓1"), "{}", m.items[0].detail);
        h.click_menu_item("origin/main");
        h.wait_job();
        assert_eq!(h.app.repo.as_ref().unwrap().last_message(), "theirs");

        // Diverged: rebase and merge are offered.
        std::fs::write(dir.join("mine"), "m").unwrap();
        git::run(&dir, &["add", "."], None).unwrap();
        git::run(&dir, &["commit", "-qm", "mine"], None).unwrap();
        std::fs::write(other.join("theirs2"), "t").unwrap();
        for args in [&["add", "."][..], &["-c", "user.name=O", "-c", "user.email=o@example.com", "commit", "-qm", "theirs 2"], &["push", "-q"]] {
            git::run(&other, args, None).unwrap();
        }
        git::run(&dir, &["fetch", "-q", "origin"], None).unwrap();
        h.app.reload_all(false);
        h.app.open_pull_menu((900.0, 60.0));
        h.frame();
        assert_eq!(h.menu_labels(), vec!["origin/main  (upstream)", "Rebase onto origin/main", "Merge origin/main"]);
        h.click_menu_item("Rebase onto");
        h.wait_job();
        let repo = h.app.repo.as_ref().unwrap();
        assert_eq!(repo.last_message(), "mine");
        assert!(repo.is_ancestor("origin/main", "HEAD"));
        for d in [&dir, &up, &other] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn shortens_remote_urls() {
        assert_eq!(super::short_url("git@github.com:me/repo.git"), "github.com/me/repo");
        assert_eq!(super::short_url("https://github.com/me/repo.git"), "github.com/me/repo");
        assert_eq!(super::short_url("ssh://git@host:2222/team/repo.git"), "host:2222/team/repo");
        assert_eq!(super::short_url("/srv/git/repo.git"), "…/git/repo");
        assert_eq!(super::short_url("../fork.git"), "fork");
    }
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~") {
        Some(rest) => {
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default();
            PathBuf::from(home).join(rest.trim_start_matches(['/', '\\']))
        }
        None => PathBuf::from(p),
    }
}
