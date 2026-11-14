//! Git access through the `git` CLI. Porcelain output formats are stable and scriptable,
//! and this keeps libgit2/gix (and their dependency trees) out of the build.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn run(root: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<String, String> {
    run_env(root, args, stdin, &[])
}

fn base_cmd(root: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0") // never hang waiting for credentials
        .env("GIT_OPTIONAL_LOCKS", "0"); // background refreshes must not fight the user's git
    cmd
}

pub fn run_env(root: &Path, args: &[&str], stdin: Option<&[u8]>, env: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = base_cmd(root, args);
    cmd.envs(env.iter().copied())
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("failed to run git: {e}"))?;
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(data).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let err = if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err };
        Err(if err.is_empty() { format!("git {} failed", args.join(" ")) } else { err })
    }
}

/// Like `run`, but returns the exit code with stdout/stderr instead of treating non-zero as an error.
pub fn run_full(root: &Path, args: &[&str]) -> Result<(i32, String, String), String> {
    let out = base_cmd(root, args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// A multi-step operation git is in the middle of (usually stopped on a conflict).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum OpKind {
    Rebase,
    Merge,
    CherryPick,
    Revert,
}

impl OpKind {
    pub fn name(self) -> &'static str {
        match self {
            OpKind::Rebase => "Rebase",
            OpKind::Merge => "Merge",
            OpKind::CherryPick => "Cherry-pick",
            OpKind::Revert => "Revert",
        }
    }
}

/// Result of a dry-run merge.
pub enum MergePreview {
    Clean,
    Conflicts(Vec<String>),
    Unknown,
}

/// Messages from the background history loader.
pub enum LogMsg {
    Batch(Vec<Commit>),
    Done,
    Err(String),
}

const LOG_FORMAT: &str = "--pretty=format:%H%x1f%P%x1f%an%x1f%at%x1f%D%x1f%s%x1e";

fn parse_commit(rec: &str) -> Option<Commit> {
    let rec = rec.trim_start_matches('\n');
    let mut f = rec.split('\x1f');
    let hash = f.next()?.to_string();
    if hash.len() < 7 {
        return None;
    }
    let parents = f.next()?.split_whitespace().map(String::from).collect();
    let author = f.next()?.to_string();
    let time = f.next()?.parse().unwrap_or(0);
    let refs = f.next()?.split(", ").filter(|s| !s.is_empty() && !s.ends_with("/HEAD")).map(String::from).collect();
    let subject = f.next().unwrap_or("").to_string();
    Some(Commit { hash, parents, author, time, refs, subject })
}

/// `git log` arguments for the graph. Hidden refs (full names like `refs/heads/x` or
/// `refs/remotes/origin/x`) are excluded, so commits only they reach drop out of the history.
/// `--exclude` applies to the next `--branches` / `--remotes` and is relative to that namespace.
fn log_args(hidden: &[String]) -> Vec<String> {
    let mut a = vec!["log".to_string()];
    for (ns, flag) in [("refs/heads/", "--branches"), ("refs/remotes/", "--remotes")] {
        a.extend(hidden.iter().filter_map(|h| h.strip_prefix(ns)).map(|n| format!("--exclude={n}")));
        a.push(flag.into());
    }
    a.extend(["--tags", "HEAD", "--date-order", LOG_FORMAT].map(String::from));
    a
}

/// Streams the full history on a background thread in batches. The first batch is small so the
/// graph paints immediately; the stream stops on its own when the receiver is dropped.
pub fn stream_log(root: PathBuf, hidden: Vec<String>) -> mpsc::Receiver<LogMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let args = log_args(&hidden);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let spawned = base_cmd(&root, &args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(LogMsg::Err(format!("failed to run git: {e}")));
                return;
            }
        };
        let mut reader = BufReader::with_capacity(1 << 16, child.stdout.take().unwrap());
        let mut batch = Vec::new();
        let mut limit = 400;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(0x1e, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let rec = String::from_utf8_lossy(&buf);
                    if let Some(c) = parse_commit(rec.trim_end_matches('\x1e')) {
                        batch.push(c);
                    }
                    if batch.len() >= limit {
                        if tx.send(LogMsg::Batch(std::mem::take(&mut batch))).is_err() {
                            let _ = child.kill();
                            let _ = child.wait();
                            return;
                        }
                        limit = 5000;
                    }
                }
            }
        }
        let status = child.wait();
        if !batch.is_empty() {
            let _ = tx.send(LogMsg::Batch(batch));
        }
        let ok = status.is_ok_and(|s| s.success());
        if !ok {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            // A freshly initialized repository has no HEAD yet; that's an empty history, not an error.
            if !(err.contains("unknown revision") || err.contains("bad revision") || err.trim().is_empty()) {
                let _ = tx.send(LogMsg::Err(err.trim().to_string()));
            }
        }
        let _ = tx.send(LogMsg::Done);
    });
    rx
}

#[derive(Clone, Debug)]
pub struct BlameCommit {
    pub hash: String,
    pub author: String,
    pub time: i64,
    pub summary: String,
}

#[derive(Clone, Debug, Default)]
pub struct Blame {
    pub commits: Vec<BlameCommit>,
    /// Per final line: index into `commits`.
    pub line_commit: Vec<u32>,
    pub lines: Vec<String>,
}

pub fn parse_blame(out: &str) -> Blame {
    let mut b = Blame::default();
    let mut idx: HashMap<String, u32> = HashMap::new();
    let mut cur: Option<u32> = None;
    for line in out.split('\n') {
        if let Some(text) = line.strip_prefix('\t') {
            if let Some(c) = cur {
                b.line_commit.push(c);
                b.lines.push(text.to_string());
            }
            continue;
        }
        let mut parts = line.splitn(2, ' ');
        let key = parts.next().unwrap_or("");
        let val = parts.next().unwrap_or("");
        if key.len() == 40 && key.bytes().all(|c| c.is_ascii_hexdigit()) {
            let n = b.commits.len() as u32;
            let i = *idx.entry(key.to_string()).or_insert_with(|| {
                b.commits.push(BlameCommit { hash: key.to_string(), author: String::new(), time: 0, summary: String::new() });
                n
            });
            cur = Some(i);
        } else if let Some(c) = cur {
            let bc = &mut b.commits[c as usize];
            match key {
                "author" => bc.author = val.to_string(),
                "author-time" => bc.time = val.parse().unwrap_or(0),
                "summary" => bc.summary = val.to_string(),
                _ => {}
            }
        }
    }
    b
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub hash: String,
    pub parents: Vec<String>,
    pub author: String,
    pub time: i64,
    pub refs: Vec<String>,
    pub subject: String,
}

impl Commit {
    pub fn short(&self) -> &str {
        &self.hash[..self.hash.len().min(8)]
    }
    pub fn is_head(&self) -> bool {
        self.refs.iter().any(|r| r == "HEAD" || r.starts_with("HEAD -> "))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RefKind {
    Local,
    Remote,
    Tag,
}

#[derive(Clone, Debug)]
pub struct Ref {
    pub name: String,
    pub kind: RefKind,
    pub target: String,
    pub is_head: bool,
    pub ahead: u32,
    pub behind: u32,
    /// Local branches: the remote branch it tracks ("origin/main"), or empty.
    pub upstream: String,
    /// The upstream is configured but its remote branch no longer exists.
    pub gone: bool,
}

/// One `git stash` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Stash {
    /// Its reflog name, "stash@{0}" (changes as stashes come and go; `hash` doesn't).
    pub name: String,
    pub hash: String,
    /// "On main: message", or git's own "WIP on main: 1a2b3c4 last commit's subject".
    pub subject: String,
    pub time: i64,
}

impl Stash {
    /// The branch it was made on, and its message.
    pub fn parts(&self) -> (&str, &str) {
        let s = self.subject.strip_prefix("WIP on ").or_else(|| self.subject.strip_prefix("On ")).unwrap_or(&self.subject);
        s.split_once(": ").unwrap_or(("", s))
    }
}

/// What `git stash push` puts away.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StashKind {
    /// Staged and unstaged changes to tracked files (git's default).
    Changes,
    /// Those, plus untracked files.
    WithUntracked,
    /// Only what's staged.
    Staged,
    /// Everything, but the staged changes stay in the index and the files too.
    KeepIndex,
}

impl StashKind {
    pub fn what(self) -> &'static str {
        match self {
            StashKind::Changes => "all changes",
            StashKind::WithUntracked => "all changes and untracked files",
            StashKind::Staged => "staged changes",
            StashKind::KeepIndex => "all changes, keeping what's staged",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Change {
    pub path: String,
    pub orig: Option<String>,
    pub x: char,
    pub y: char,
}

impl Change {
    pub fn staged(&self) -> bool {
        self.x != ' ' && self.x != '?' && self.x != '!'
    }
    pub fn unstaged(&self) -> bool {
        self.y != ' '
    }
    pub fn untracked(&self) -> bool {
        self.x == '?'
    }
    pub fn conflicted(&self) -> bool {
        matches!((self.x, self.y), ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D'))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Hunk {
    pub header: String,
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct FileDiff {
    pub header: Vec<String>,
    pub path: String,
    /// Path on the old side (differs for renames; empty when the file is new).
    pub old_path: String,
    pub hunks: Vec<Hunk>,
    pub binary: bool,
    pub adds: u32,
    pub dels: u32,
}

/// Diff presentation options that change what git computes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiffOpts {
    /// Lines of context; `u32::MAX` means the whole file.
    pub context: u32,
    pub ignore_ws: bool,
}

impl Default for DiffOpts {
    fn default() -> Self {
        DiffOpts { context: 3, ignore_ws: false }
    }
}

impl DiffOpts {
    fn context_arg(&self) -> String {
        format!("-U{}", self.context.min(1_000_000))
    }
}

pub struct Repo {
    pub root: PathBuf,
}

impl Repo {
    pub fn open(path: &Path) -> Result<Repo, String> {
        let top = run(path, &["rev-parse", "--show-toplevel"], None)?;
        Ok(Repo { root: PathBuf::from(top.trim()) })
    }

    pub fn name(&self) -> String {
        self.root.file_name().map_or_else(|| self.root.display().to_string(), |n| n.to_string_lossy().into())
    }

    pub fn git(&self, args: &[&str]) -> Result<String, String> {
        run(&self.root, args, None)
    }

    pub fn git_in(&self, args: &[&str], stdin: &[u8]) -> Result<String, String> {
        run(&self.root, args, Some(stdin))
    }

    /// Blocking history load (used by tests; the app streams via `stream_log`).
    #[allow(dead_code)]
    pub fn log(&self) -> Result<Vec<Commit>, String> {
        let out = match self.git(&["log", "--branches", "--remotes", "--tags", "HEAD", "--date-order", LOG_FORMAT]) {
            Ok(o) => o,
            Err(e) if e.contains("unknown revision") || e.contains("bad revision") => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        Ok(out.split('\x1e').filter_map(parse_commit).collect())
    }

    pub fn refs(&self) -> Result<Vec<Ref>, String> {
        let out = self.git(&[
            "for-each-ref",
            "--format=%(refname)%1f%(objectname)%1f%(*objectname)%1f%(HEAD)%1f%(upstream:track)%1f%(upstream:short)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ])?;
        let mut refs = Vec::new();
        for line in out.lines() {
            let f: Vec<&str> = line.split('\x1f').collect();
            if f.len() < 6 {
                continue;
            }
            let full = f[0];
            let (kind, name) = if let Some(n) = full.strip_prefix("refs/heads/") {
                (RefKind::Local, n)
            } else if let Some(n) = full.strip_prefix("refs/remotes/") {
                if n.ends_with("/HEAD") {
                    continue;
                }
                (RefKind::Remote, n)
            } else if let Some(n) = full.strip_prefix("refs/tags/") {
                (RefKind::Tag, n)
            } else {
                continue;
            };
            let target = if f[2].is_empty() { f[1] } else { f[2] };
            let num = |key: &str| -> u32 {
                f[4].split(key).nth(1).and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()).unwrap_or(0)
            };
            refs.push(Ref {
                name: name.to_string(),
                kind,
                target: target.to_string(),
                is_head: f[3] == "*",
                ahead: num("ahead "),
                behind: num("behind "),
                upstream: f[5].to_string(),
                gone: f[4] == "[gone]",
            });
        }
        Ok(refs)
    }

    /// The branch `origin/HEAD` points at (the remote's default branch), without the remote.
    pub fn default_branch(&self) -> Option<String> {
        let s = self.git(&["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"]).ok()?;
        let s = s.trim();
        Some(s.split_once('/').map_or(s, |(_, b)| b).to_string()).filter(|b| !b.is_empty())
    }

    pub fn head_name(&self) -> String {
        self.git(&["symbolic-ref", "--short", "-q", "HEAD"])
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "detached HEAD".into())
    }

    pub fn status(&self) -> Result<Vec<Change>, String> {
        let out = self.git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
        let mut it = out.split('\0');
        let mut changes = Vec::new();
        while let Some(e) = it.next() {
            if e.len() < 4 {
                continue;
            }
            let mut cs = e.chars();
            let x = cs.next().unwrap();
            let y = cs.next().unwrap();
            let path = e[3..].to_string();
            let orig = if x == 'R' || x == 'C' { it.next().map(String::from) } else { None };
            changes.push(Change { path, orig, x, y });
        }
        Ok(changes)
    }

    pub fn diff_file(&self, path: &str, staged: bool, o: &DiffOpts) -> Result<Vec<FileDiff>, String> {
        let ctx = o.context_arg();
        let mut args = vec!["diff", "--no-color", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", "-M", &ctx];
        if o.ignore_ws {
            args.push("-w");
        }
        if staged {
            args.push("--cached");
        }
        args.extend(["--", path]);
        Ok(parse_diff(&self.git(&args)?))
    }

    /// An untracked file as a real "new file" patch (`git diff --no-index`), so selected lines
    /// can be staged or discarded like in any other diff.
    pub fn untracked_diff(&self, path: &str) -> FileDiff {
        let whole = FileDiff { path: path.to_string(), binary: true, ..Default::default() };
        // Huge and non-UTF-8 files are only handled as a whole.
        match std::fs::read(self.root.join(path)) {
            Ok(b) if b.len() <= 2_000_000 && std::str::from_utf8(&b).is_ok() => {}
            _ => return whole,
        }
        let args = ["diff", "--no-index", "--no-color", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", "--", "/dev/null", path];
        // Exit code 1 means "differences found".
        let Ok((0 | 1, out, _)) = run_full(&self.root, &args) else { return whole };
        let Some(mut fd) = parse_diff(&out).pop() else { return whole };
        if fd.hunks.is_empty() && !fd.binary {
            // Empty file: an empty hunk still carries the Stage / Delete buttons.
            fd.hunks.push(Hunk { header: "@@ -0,0 +1,0 @@ empty file".into(), old_start: 0, new_start: 1, lines: vec![] });
        }
        fd.path = path.to_string();
        fd
    }

    pub fn show(&self, hash: &str, o: &DiffOpts) -> Result<Vec<FileDiff>, String> {
        let ctx = o.context_arg();
        let mut args = vec!["show", "--no-color", "--no-ext-diff", "--format=", "-M", "--diff-merges=first-parent", &ctx];
        if o.ignore_ws {
            args.push("-w");
        }
        args.push(hash);
        Ok(parse_diff(&self.git(&args)?))
    }

    /// Diff between two endpoints. `None` means the working tree. With `merge_base`, shows what
    /// `b` changed since it diverged from `a` (`a...b`, the "pull request" view).
    pub fn compare(&self, a: Option<&str>, b: Option<&str>, merge_base: bool, o: &DiffOpts) -> Result<Vec<FileDiff>, String> {
        let ctx = o.context_arg();
        let mut args = vec!["diff", "--no-color", "--no-ext-diff", "-M", &ctx];
        if o.ignore_ws {
            args.push("-w");
        }
        let range;
        match (a, b) {
            (Some(a), Some(b)) if merge_base => {
                range = format!("{a}...{b}");
                args.push(&range);
            }
            (Some(a), Some(b)) => args.extend([a, b]),
            (Some(a), None) => args.push(a),
            (None, Some(b)) => args.extend(["-R", b]),
            (None, None) => {}
        }
        Ok(parse_diff(&self.git(&args)?))
    }

    /// Hashes reachable from `b` but not from `a` (newest first, capped).
    pub fn range_hashes(&self, a: &str, b: &str, limit: usize) -> Vec<String> {
        let n = limit.to_string();
        self.git(&["rev-list", "-n", &n, &format!("{a}..{b}")]).unwrap_or_default().lines().map(String::from).collect()
    }

    pub fn count_range(&self, a: &str, b: &str) -> usize {
        self.git(&["rev-list", "--count", &format!("{a}..{b}")]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    }

    /// (author line, date, full message)
    pub fn commit_info(&self, hash: &str) -> Result<(String, String, String), String> {
        let out = self.git(&[
            "show",
            "-s",
            "--date=format-local:%Y-%m-%d %H:%M",
            "--format=%an <%ae>%x1f%ad%x1f%B",
            hash,
        ])?;
        let mut f = out.splitn(3, '\x1f');
        Ok((
            f.next().unwrap_or("").to_string(),
            f.next().unwrap_or("").to_string(),
            f.next().unwrap_or("").trim_end().to_string(),
        ))
    }

    pub fn apply_cached(&self, patch: &str, reverse: bool) -> Result<String, String> {
        let mut args = vec!["apply", "--cached", "--recount", "--whitespace=nowarn"];
        if reverse {
            args.push("-R");
        }
        args.push("-");
        self.git_in(&args, patch.as_bytes())
    }

    pub fn stage(&self, paths: &[&str]) -> Result<String, String> {
        let mut args = vec!["add", "-A", "--"];
        args.extend_from_slice(paths);
        self.git(&args)
    }

    pub fn unstage(&self, paths: &[&str]) -> Result<String, String> {
        let mut args = vec!["restore", "--staged", "--"];
        args.extend_from_slice(paths);
        self.git(&args).or_else(|_| {
            // No HEAD yet (initial commit): drop the paths from the index instead.
            let mut args = vec!["rm", "--cached", "-r", "-q", "--"];
            args.extend_from_slice(paths);
            self.git(&args)
        })
    }

    pub fn commit(&self, message: &str, amend: bool) -> Result<String, String> {
        let mut args = vec!["commit", "-F", "-"];
        if amend {
            args.push("--amend");
        }
        self.git_in(&args, message.as_bytes())
    }

    pub fn git_env(&self, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
        run_env(&self.root, args, None, env)
    }

    /// Resolves a path inside the git directory (works for worktrees too).
    pub fn git_path(&self, name: &str) -> PathBuf {
        let p = self.git(&["rev-parse", "--git-path", name]).map(|s| s.trim().to_string()).unwrap_or_default();
        let p = PathBuf::from(p);
        if p.is_absolute() { p } else { self.root.join(p) }
    }

    pub fn rev_parse(&self, rev: &str) -> Option<String> {
        self.git(&["rev-parse", "--verify", "-q", rev]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    /// Cheap fingerprint of every ref + HEAD (and what each branch tracks); if unchanged, the
    /// history and branch list don't need reloading.
    pub fn refs_signature(&self) -> String {
        let refs = self.git(&["for-each-ref", "--format=%(objectname) %(refname) %(upstream)"]).unwrap_or_default();
        let head = self.git(&["rev-parse", "-q", "HEAD"]).unwrap_or_default();
        format!("{head}\n{refs}")
    }

    pub fn blame(&self, rev: Option<&str>, path: &str) -> Result<Blame, String> {
        let mut args = vec!["blame", "--porcelain"];
        if let Some(r) = rev {
            args.push(r);
        }
        args.extend(["--", path]);
        Ok(parse_blame(&self.git(&args)?))
    }

    pub fn ls_tree(&self, rev: &str) -> Result<Vec<String>, String> {
        let out = self.git(&["ls-tree", "-r", "--name-only", "-z", rev])?;
        Ok(out.split('\0').filter(|s| !s.is_empty()).map(String::from).collect())
    }

    /// File contents at a revision, or from the working tree when `rev` is None.
    pub fn read_file(&self, rev: Option<&str>, path: &str) -> Result<String, String> {
        match rev {
            Some(r) => self.git(&["show", &format!("{r}:{path}")]),
            None => std::fs::read(self.root.join(path))
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .map_err(|e| e.to_string()),
        }
    }

    /// Applies a patch to the working tree (used by discard and its undo).
    pub fn apply_worktree(&self, patch: &str, reverse: bool) -> Result<String, String> {
        self.apply_worktree_args(patch, reverse, false)
    }

    fn apply_worktree_args(&self, patch: &str, reverse: bool, check: bool) -> Result<String, String> {
        let mut args = vec!["apply", "--recount", "--whitespace=nowarn", "--binary"];
        if reverse {
            args.push("-R");
        }
        if check {
            args.push("--check");
        }
        args.push("-");
        self.git_in(&args, patch.as_bytes())
    }

    /// Discards staged changes, or some of their lines, from both the index and the working tree
    /// (`file` is from the staged diff; see `lines_patch`). Changes nothing unless both take it,
    /// i.e. unless the working tree still has those lines as staged. Returns the reverse-applied
    /// patches (index, working tree) for undo.
    pub fn discard_staged(&self, file: &FileDiff, hunks: &[usize], selected: Option<&dyn Fn(usize, usize) -> bool>) -> Result<(String, String), String> {
        let index = lines_patch(file, hunks, selected, true, &|n| n);
        // The same lines in the working tree sit wherever its unstaged changes moved them.
        let out = self.git(&["diff", "--no-color", "--no-ext-diff", "-U0", "--src-prefix=a/", "--dst-prefix=b/", "--", &file.path])?;
        let unstaged: Vec<Hunk> = parse_diff(&out).into_iter().flat_map(|f| f.hunks).collect();
        let worktree = lines_patch(file, hunks, selected, true, &line_map(&unstaged));
        let changed_again = |e: String| format!("Not discarded: {} has newer changes to these lines in the working tree\n{e}", file.path);
        self.apply_worktree_args(&worktree, true, true).map_err(changed_again)?;
        self.apply_cached(&index, true)?;
        if let Err(e) = self.apply_worktree(&worktree, true) {
            let _ = self.apply_cached(&index, false);
            return Err(changed_again(e));
        }
        Ok((index, worktree))
    }

    /// The stash list, newest first.
    pub fn stashes(&self) -> Vec<Stash> {
        let out = self.git(&["stash", "list", "--format=%gd%x1f%H%x1f%gs%x1f%ct"]).unwrap_or_default();
        out.lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\x1f').collect();
                let [name, hash, subject, time] = f[..] else { return None };
                Some(Stash { name: name.into(), hash: hash.into(), subject: subject.into(), time: time.parse().unwrap_or(0) })
            })
            .collect()
    }

    /// Stashes the working copy's changes; returns the new stash's hash.
    pub fn stash_push(&self, kind: StashKind, message: &str) -> Result<String, String> {
        let before = self.rev_parse("refs/stash");
        let mut args = vec!["stash", "push"];
        match kind {
            StashKind::Changes => {}
            StashKind::WithUntracked => args.push("--include-untracked"),
            StashKind::Staged => args.push("--staged"),
            StashKind::KeepIndex => args.push("--keep-index"),
        }
        if !message.trim().is_empty() {
            args.extend(["-m", message.trim()]);
        }
        self.git(&args)?;
        // "No local changes to save" still exits 0.
        match self.rev_parse("refs/stash") {
            Some(h) if Some(&h) != before.as_ref() => Ok(h),
            _ => Err(format!("Nothing to stash ({})", kind.what())),
        }
    }

    /// What a stash would bring back: its changes against the commit it was made on, untracked
    /// files included.
    pub fn stash_diff(&self, name: &str, o: &DiffOpts) -> Result<Vec<FileDiff>, String> {
        let ctx = o.context_arg();
        let mut args = vec!["stash", "show", "-p", "--include-untracked", "--no-color", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", "-M", &ctx];
        if o.ignore_ws {
            args.push("-w");
        }
        args.push(name);
        Ok(parse_diff(&self.git(&args)?))
    }

    /// The full unstaged patch of one file, binary-safe.
    pub fn file_patch(&self, path: &str) -> Result<String, String> {
        self.git(&["diff", "--binary", "--no-color", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", "--", path])
    }

    pub fn op_in_progress(&self) -> Option<OpKind> {
        if self.rebase_in_progress() {
            Some(OpKind::Rebase)
        } else if self.git_path("MERGE_HEAD").exists() {
            Some(OpKind::Merge)
        } else if self.git_path("CHERRY_PICK_HEAD").exists() {
            Some(OpKind::CherryPick)
        } else if self.git_path("REVERT_HEAD").exists() {
            Some(OpKind::Revert)
        } else {
            None
        }
    }

    pub fn op_continue(&self, op: OpKind) -> Result<String, String> {
        let env = [("GIT_EDITOR", "true")];
        match op {
            OpKind::Rebase => self.git_env(&["rebase", "--continue"], &env),
            OpKind::Merge => self.git_env(&["commit", "--no-edit"], &env),
            OpKind::CherryPick => self.git_env(&["cherry-pick", "--continue"], &env),
            OpKind::Revert => self.git_env(&["revert", "--continue"], &env),
        }
    }

    pub fn op_abort(&self, op: OpKind) -> Result<String, String> {
        match op {
            OpKind::Rebase => self.git(&["rebase", "--abort"]),
            OpKind::Merge => self.git(&["merge", "--abort"]),
            OpKind::CherryPick => self.git(&["cherry-pick", "--abort"]),
            OpKind::Revert => self.git(&["revert", "--abort"]),
        }
    }

    /// Dry-runs merging `theirs` into `ours` without touching the index or working tree.
    pub fn merge_preview(&self, ours: &str, theirs: &str) -> MergePreview {
        match run_full(&self.root, &["merge-tree", "--write-tree", "--name-only", "--no-messages", ours, theirs]) {
            Ok((0, _, _)) => MergePreview::Clean,
            Ok((1, out, _)) => {
                let files: Vec<String> = out.lines().skip(1).filter(|l| !l.is_empty()).map(String::from).collect();
                MergePreview::Conflicts(files)
            }
            _ => MergePreview::Unknown,
        }
    }

    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        run_full(&self.root, &["merge-base", "--is-ancestor", a, b]).is_ok_and(|r| r.0 == 0)
    }

    /// A conflicted file shown as a diff: HEAD side as removals, incoming side as additions,
    /// conflict markers as notes. In split view the two sides line up next to each other.
    pub fn conflict_diff(&self, path: &str) -> FileDiff {
        let mut fd = FileDiff { path: path.to_string(), ..Default::default() };
        let Ok(bytes) = std::fs::read(self.root.join(path)) else {
            fd.binary = true;
            return fd;
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut lines = Vec::new();
        let mut side = 0; // 0 = outside, 1 = HEAD, 2 = base (diff3), 3 = incoming
        let mut base_lines = 0;
        for l in text.lines() {
            if l.starts_with("<<<<<<<") {
                side = 1;
                lines.push(format!("\\ ⟨ HEAD side{}", l.trim_start_matches('<')));
            } else if l.starts_with("|||||||") && side == 1 {
                side = 2;
                base_lines = 0;
            } else if l.starts_with("=======") && side != 0 {
                if side == 2 {
                    // diff3/zdiff3: summarize the ancestor instead of showing it as context,
                    // so the two sides line up next to each other.
                    lines.push(format!("\\ common ancestor · {base_lines} line{} hidden", if base_lines == 1 { "" } else { "s" }));
                }
                side = 3;
                lines.push("\\ incoming side".to_string());
            } else if side == 2 {
                base_lines += 1;
            } else if l.starts_with(">>>>>>>") && side != 0 {
                side = 0;
                lines.push(format!("\\ ⟩ end of conflict{}", l.trim_start_matches('>')));
            } else {
                let tag = match side {
                    1 => '-',
                    3 => '+',
                    _ => ' ',
                };
                lines.push(format!("{tag}{l}"));
            }
        }
        fd.dels = lines.iter().filter(|l| l.starts_with('-')).count() as u32;
        fd.adds = lines.iter().filter(|l| l.starts_with('+')).count() as u32;
        fd.hunks.push(Hunk { header: "@@ conflict markers @@".into(), old_start: 1, new_start: 1, lines });
        fd
    }

    /// Configured remotes with their push URLs, in `git remote` order.
    pub fn remotes(&self) -> Vec<(String, String)> {
        let out = self.git(&["remote", "-v"]).unwrap_or_default();
        let mut v: Vec<(String, String)> = Vec::new();
        for line in out.lines().filter(|l| l.ends_with("(push)")) {
            let mut parts = line.split_whitespace();
            if let (Some(name), Some(url)) = (parts.next(), parts.next()) {
                if !v.iter().any(|(n, _)| n == name) {
                    v.push((name.to_string(), url.to_string()));
                }
            }
        }
        v
    }

    /// A branch's raw upstream settings (`branch.<name>.remote` and `.merge`), to put back later.
    pub fn upstream_config(&self, branch: &str) -> Option<(String, String)> {
        let get = |key: &str| self.git(&["config", "--get", &format!("branch.{branch}.{key}")]).ok().map(|s| s.trim().to_string());
        Some((get("remote")?, get("merge")?))
    }

    /// The current branch's upstream, e.g. "origin/main".
    pub fn upstream(&self) -> Option<String> {
        self.git(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    /// Branches hidden from the graph, stored per repository inside the git dir.
    pub fn hidden_refs(&self) -> Vec<String> {
        std::fs::read_to_string(self.git_path("wgitpui-hidden-refs"))
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("refs/"))
            .map(String::from)
            .collect()
    }

    pub fn set_hidden_refs(&self, refs: &[String]) -> Result<(), String> {
        let path = self.git_path("wgitpui-hidden-refs");
        if refs.is_empty() {
            let _ = std::fs::remove_file(path);
            return Ok(());
        }
        std::fs::write(path, refs.join("\n") + "\n").map_err(|e| e.to_string())
    }

    pub fn rebase_in_progress(&self) -> bool {
        self.git_path("rebase-merge").exists() || self.git_path("rebase-apply").exists()
    }

    /// Runs `git rebase -i` non-interactively with a prepared todo list.
    /// `base` is the commit to rebase onto; None means `--root`.
    pub fn rebase_with_todo(&self, base: Option<&str>, todo: &str) -> Result<String, String> {
        self.rebase_todo(base, todo, None)
    }

    /// `rebase_with_todo` where the todo rewords a commit to `message`.
    pub fn reword_with_todo(&self, base: Option<&str>, todo: &str, message: &str) -> Result<String, String> {
        self.rebase_todo(base, todo, Some(message))
    }

    fn rebase_todo(&self, base: Option<&str>, todo: &str, message: Option<&str>) -> Result<String, String> {
        let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        let todo_path = self.git_path("wgitpui-rebase-todo");
        std::fs::write(&todo_path, todo).map_err(|e| e.to_string())?;
        let editor = format!("cp {}", quote(&todo_path));
        // Git hands the editor the message file: copying ours over it is the edit.
        let msg_path = self.git_path("wgitpui-reword-msg");
        let msg_editor = match message {
            Some(m) => {
                std::fs::write(&msg_path, m).map_err(|e| e.to_string())?;
                format!("cp {}", quote(&msg_path))
            }
            None => "true".to_string(),
        };
        let mut args = vec!["-c", "rebase.autoSquash=false", "-c", "rebase.updateRefs=false", "rebase", "-i", "--autostash"];
        match base {
            Some(b) => args.push(b),
            None => args.push("--root"),
        }
        let r = self.git_env(&args, &[("GIT_SEQUENCE_EDITOR", &editor), ("GIT_EDITOR", &msg_editor)]);
        let _ = std::fs::remove_file(&todo_path);
        let _ = std::fs::remove_file(&msg_path);
        r
    }


    pub fn last_message(&self) -> String {
        self.git(&["log", "-1", "--format=%B"]).map(|s| s.trim_end().to_string()).unwrap_or_default()
    }

    /// Every working tree of this repository (the main one first), from `git worktree list`.
    pub fn worktrees(&self) -> Vec<Worktree> {
        parse_worktrees(&self.git(&["worktree", "list", "--porcelain"]).unwrap_or_default())
    }

    /// `git worktree add`: check out `rev` at `path`, optionally on a new branch, or detached.
    pub fn worktree_add(&self, path: &str, rev: &str, new_branch: Option<&str>, detach: bool) -> Result<String, String> {
        let mut args = vec!["worktree", "add"];
        if let Some(b) = new_branch {
            args.extend(["-b", b]);
        } else if detach {
            args.push("--detach");
        }
        args.extend(["--", path, rev]);
        self.git(&args)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    /// Checked-out branch (short name); None when detached or bare.
    pub branch: Option<String>,
    pub bare: bool,
    pub locked: bool,
    /// The directory is gone; `git worktree prune` would drop the entry.
    pub prunable: bool,
    /// Changed files (filled in by the caller; listing doesn't run status).
    pub changes: usize,
}

impl Worktree {
    pub fn name(&self) -> String {
        self.path.file_name().map_or_else(|| self.path.display().to_string(), |n| n.to_string_lossy().into())
    }
}

pub fn parse_worktrees(out: &str) -> Vec<Worktree> {
    let mut v: Vec<Worktree> = Vec::new();
    for line in out.lines() {
        let (key, val) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            v.push(Worktree { path: PathBuf::from(val), ..Default::default() });
            continue;
        }
        let Some(w) = v.last_mut() else { continue };
        match key {
            "HEAD" => w.head = val.to_string(),
            "branch" => w.branch = Some(val.strip_prefix("refs/heads/").unwrap_or(val).to_string()),
            "bare" => w.bare = true,
            "locked" => w.locked = true,
            "prunable" => w.prunable = true,
            _ => {}
        }
    }
    v
}

/// Number of changed files in the working tree at `root` (cheap: untracked dirs aren't expanded).
pub fn change_count(root: &Path) -> usize {
    let Ok(out) = run(root, &["status", "--porcelain=v1", "-z", "--untracked-files=normal"], None) else { return 0 };
    let (mut n, mut it) = (0, out.split('\0'));
    while let Some(e) = it.next() {
        if e.len() >= 4 {
            n += 1;
            if matches!(e.as_bytes()[0], b'R' | b'C') {
                it.next(); // a rename's original path is its own field
            }
        }
    }
    n
}

/// Path equality that sees through symlinks (macOS /tmp → /private/tmp) and trailing slashes.
pub fn same_path(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    for line in text.strip_suffix('\n').unwrap_or(text).split('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest.rsplit_once(" b/").map_or(rest, |(_, b)| b).to_string();
            files.push(FileDiff { header: vec![line.to_string()], path, ..Default::default() });
            in_hunk = false;
            continue;
        }
        let Some(f) = files.last_mut() else { continue };
        if line.starts_with("@@") {
            let mut parts = line.split(' ');
            parts.next();
            let num = |s: Option<&str>| -> u32 {
                s.and_then(|s| s[1..].split(',').next()?.parse().ok()).unwrap_or(0)
            };
            let old_start = num(parts.next());
            let new_start = num(parts.next());
            f.hunks.push(Hunk { header: line.to_string(), old_start, new_start, lines: vec![] });
            in_hunk = true;
        } else if in_hunk {
            match line.as_bytes().first() {
                Some(b'+') => f.adds += 1,
                Some(b'-') => f.dels += 1,
                _ => {}
            }
            // An empty line is a blank context line, as `diff.suppressBlankEmpty` prints it.
            let line = if line.is_empty() { " " } else { line };
            f.hunks.last_mut().unwrap().lines.push(line.to_string());
        } else {
            if let Some(p) = line.strip_prefix("+++ b/") {
                f.path = p.to_string();
            }
            if let Some(p) = line.strip_prefix("--- a/") {
                f.old_path = p.to_string();
            }
            if line.starts_with("Binary files") {
                f.binary = true;
            }
            f.header.push(line.to_string());
        }
    }
    files
}

/// Builds a patch from some of a file's hunks (ascending), optionally restricted to the selected
/// `+`/`-` lines: `selected(hunk, line)`.
///
/// Staging (`reverse == false`) applies the patch forward to the diff's old side (the index), so
/// unselected `-` lines become context and unselected `+` lines are dropped. Unstaging and
/// discarding (`reverse == true`) apply it with `-R` to the diff's new side (the index, or the
/// working tree), which is the mirror image. Hunk counts are recomputed by `git apply --recount`.
///
/// Each hunk header says where the hunk sits in the file being patched once the patch's earlier
/// hunks are in. Git starts looking for a hunk's lines there and takes the nearest match, so a
/// position that is off (the other side's line number, say) puts lines in the wrong place in a
/// file with repeated blocks. `at` maps a line of the diff's side to the file actually patched,
/// when that one differs elsewhere (the working tree, for a patch made from the staged diff).
pub fn lines_patch(file: &FileDiff, hunks: &[usize], selected: Option<&dyn Fn(usize, usize) -> bool>, reverse: bool, at: &dyn Fn(u32) -> u32) -> String {
    let mut body = String::new();
    let (mut old_total, mut new_total) = (0, 0);
    // How far the hunks so far move the lines after them.
    let mut shift = 0i64;
    for &hi in hunks {
        let h = &file.hunks[hi];
        let (lines, old_n, new_n) = hunk_lines(h, &|l| selected.is_none_or(|f| f(hi, l)), reverse);
        // The hunk's first line in the file being patched (an empty side is numbered by the line
        // before it).
        let (start, side) = if reverse { (h.new_start, b'+') } else { (h.old_start, b'-') };
        let empty = !h.lines.iter().any(|l| l.as_bytes().first().is_some_and(|&t| t == b' ' || t == side));
        let first = at(start + empty as u32) as i64;
        let (patched_n, result_n) = if reverse { (new_n, old_n) } else { (old_n, new_n) };
        let pos = |p: i64, n: u32| if n > 0 { p } else { (p - 1).max(0) };
        let (p, r) = (pos(first, patched_n), pos(first + shift, result_n));
        let (o, n) = if reverse { (r, p) } else { (p, r) };
        body.push_str(&format!("@@ -{o},{old_n} +{n},{new_n} @@\n"));
        body.push_str(&lines);
        shift += result_n as i64 - patched_n as i64;
        old_total += old_n;
        new_total += new_n;
    }
    // A partial selection can leave lines on the side the header says is empty: lines unstaged
    // from (or discarded in) a new file, lines staged from a deleted one. Git rejects that
    // ("new file depends on old contents"), so such a patch becomes a plain modification.
    let created = file.header.iter().any(|l| l.starts_with("new file mode"));
    let deleted = file.header.iter().any(|l| l.starts_with("deleted file mode"));
    let header = if (created && old_total > 0) || (deleted && new_total > 0) { modification_header(&file.header) } else { file.header.clone() };
    let mut out = String::new();
    for l in &header {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&body);
    out
}

/// One hunk's body for [`lines_patch`], with its old and new line counts.
fn hunk_lines(h: &Hunk, selected: &dyn Fn(usize) -> bool, reverse: bool) -> (String, u32, u32) {
    // (text, ends with a newline, on the old side, on the new side). Unselected lines stay as
    // they are in the file being patched, and don't make it into the result.
    let mut lines: Vec<(&str, bool, bool, bool)> = Vec::with_capacity(h.lines.len());
    for (i, l) in h.lines.iter().enumerate() {
        let tag = l.as_bytes().first().copied().unwrap_or(b' ');
        if tag == b'\\' {
            // "\ No newline at end of file" is about the line before it.
            if let Some(prev) = lines.last_mut() {
                prev.1 = false;
            }
            continue;
        }
        let (old, new) = match (tag, selected(i)) {
            (b'-', true) => (true, false),
            (b'+', true) => (false, true),
            (b'-', false) => (!reverse, !reverse),
            (b'+', false) => (reverse, reverse),
            _ => (true, true),
        };
        lines.push((l.get(1..).unwrap_or(""), true, old, new));
    }
    lines.retain(|l| l.2 || l.3);
    // The file being patched keeps its line endings exactly. In the result only the very last
    // line may lack a newline, so a line that used to end the file but is now followed by
    // another one gains one (on that side only, which takes a `-`/`+` pair for a context line).
    let last_old = lines.iter().rposition(|l| l.2);
    let last_new = lines.iter().rposition(|l| l.3);
    let mut out = String::new();
    let mut emit = |tag: char, text: &str, newline: bool| {
        out.push(tag);
        out.push_str(text);
        out.push('\n');
        if !newline {
            out.push_str("\\ No newline at end of file\n");
        }
    };
    let (mut old_n, mut new_n) = (0, 0);
    for (k, &(text, nl, old, new)) in lines.iter().enumerate() {
        let old_nl = nl || (reverse && Some(k) != last_old);
        let new_nl = nl || (!reverse && Some(k) != last_new);
        match (old, new) {
            (true, true) if old_nl == new_nl => emit(' ', text, old_nl),
            (true, true) => {
                emit('-', text, old_nl);
                emit('+', text, new_nl);
            }
            (true, false) => emit('-', text, old_nl),
            _ => emit('+', text, new_nl),
        }
        old_n += old as u32;
        new_n += new as u32;
    }
    (out, old_n, new_n)
}

/// For a zero-context diff's hunks, maps a line number on the diff's old side to the same line on
/// its new side (lines inside a changed range map to where the range starts).
pub fn line_map(hunks: &[Hunk]) -> impl Fn(u32) -> u32 + '_ {
    move |line| {
        let mut shift = 0i64;
        for h in hunks {
            let count = |t: u8| h.lines.iter().filter(|l| l.as_bytes().first() == Some(&t)).count() as i64;
            let (dels, adds) = (count(b'-'), count(b'+'));
            // The old side's first line after the hunk (a pure insertion comes after its start).
            if h.old_start as i64 + dels.max(1) > line as i64 {
                break;
            }
            shift += adds - dels;
        }
        (line as i64 + shift).max(1) as u32
    }
}

/// A new-file or deleted-file patch header rewritten as a modification of the same path.
fn modification_header(header: &[String]) -> Vec<String> {
    // "b/path" → "a/path" and back; names git had to quote keep their quotes.
    let swap = |name: &str, prefix: &str| match name.strip_prefix('"') {
        Some(q) => format!("\"{prefix}{}", q.get(1..).unwrap_or("")),
        None => format!("{prefix}{}", name.get(1..).unwrap_or("")),
    };
    let new_name = header.iter().find_map(|l| l.strip_prefix("+++ ")).filter(|n| *n != "/dev/null");
    let old_name = header.iter().find_map(|l| l.strip_prefix("--- ")).filter(|n| *n != "/dev/null");
    header
        .iter()
        .filter(|l| !l.starts_with("new file mode") && !l.starts_with("deleted file mode") && !l.starts_with("index "))
        .map(|l| match (l.as_str(), new_name, old_name) {
            ("--- /dev/null", Some(n), _) => format!("--- {}", swap(n, "a")),
            ("+++ /dev/null", _, Some(o)) => format!("+++ {}", swap(o, "b")),
            _ => l.clone(),
        })
        .collect()
}

/// Where a side of a diff comes from, for line attribution.
#[derive(Clone, Debug, PartialEq)]
pub enum Side {
    Rev(String),
    Index,
    Worktree,
}

/// `git blame` of one file on one side of a diff. Lines that aren't committed yet get the
/// all-zero hash. Runs without a `Repo` so it can be called from worker threads.
pub fn blame_at(root: &Path, side: &Side, path: &str) -> Option<Blame> {
    if path.is_empty() {
        return None;
    }
    let out = match side {
        Side::Rev(r) => run(root, &["blame", "--porcelain", r, "--", path], None),
        Side::Worktree => run(root, &["blame", "--porcelain", "--", path], None),
        Side::Index => {
            let content = run(root, &["show", &format!(":{path}")], None).ok()?;
            run(root, &["blame", "--porcelain", "--contents", "-", "--", path], Some(content.as_bytes()))
        }
    };
    out.ok().map(|o| parse_blame(&o))
}

pub const UNCOMMITTED: &str = "0000000000000000000000000000000000000000";

/// Local UTC offset in seconds (asked once from `date`, so no time-zone crate is needed).
fn utc_offset() -> i64 {
    use std::sync::OnceLock;
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        let out = std::process::Command::new("date").arg("+%z").output().ok();
        let s = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        if s.len() == 5 {
            let sign = if s.starts_with('-') { -1 } else { 1 };
            let h: i64 = s[1..3].parse().unwrap_or(0);
            let m: i64 = s[3..5].parse().unwrap_or(0);
            sign * (h * 3600 + m * 60)
        } else {
            0
        }
    })
}

/// "2026-11-12 17:01" in local time.
pub fn abs_time(t: i64) -> String {
    let t = t + utc_offset();
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

/// Compact age: "now", "5m", "3h", "2d", "4mo", "2y".
pub fn short_age(t: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let d = (now - t).max(0);
    match d {
        0..60 => "now".into(),
        60..3600 => format!("{}m", d / 60),
        3600..86400 => format!("{}h", d / 3600),
        86400..2_592_000 => format!("{}d", d / 86400),
        2_592_000..31_536_000 => format!("{}mo", d / 2_592_000),
        _ => format!("{}y", d / 31_536_000),
    }
}

pub fn rel_time(t: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let d = (now - t).max(0);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        86400..2_592_000 => format!("{}d ago", d / 86400),
        2_592_000..31_536_000 => format!("{}mo ago", d / 2_592_000),
        _ => format!("{}y ago", d / 31_536_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(name: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("wgitpui-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "-q"], None).unwrap();
        run(&dir, &["config", "user.email", "t@example.com"], None).unwrap();
        run(&dir, &["config", "user.name", "T"], None).unwrap();
        std::fs::write(dir.join("f.txt"), "a\nb\nc\nd\n").unwrap();
        run(&dir, &["add", "."], None).unwrap();
        run(&dir, &["commit", "-qm", "init"], None).unwrap();
        Repo::open(&dir).unwrap()
    }

    /// A patch of one hunk, optionally restricted to the selected `+`/`-` lines.
    fn hunk_patch(file: &FileDiff, hunk: usize, selected: Option<&dyn Fn(usize) -> bool>, reverse: bool) -> String {
        match selected {
            Some(f) => lines_patch(file, &[hunk], Some(&|_, l| f(l)), reverse, &|n| n),
            None => lines_patch(file, &[hunk], None, reverse, &|n| n),
        }
    }

    /// Indices of the hunk lines equal to one of `want` (e.g. "+y").
    fn pick(fd: &FileDiff, want: &[&str]) -> Vec<usize> {
        fd.hunks[0].lines.iter().enumerate().filter(|(_, l)| want.contains(&l.as_str())).map(|(i, _)| i).collect()
    }

    #[test]
    fn line_staging_on_new_and_deleted_files_in_a_worktree() {
        let main = temp_repo("newfile-lines");
        let wt_dir = main.root.with_file_name(format!("{}-wt", main.root.file_name().unwrap().to_string_lossy()));
        let _ = std::fs::remove_dir_all(&wt_dir);
        main.git(&["worktree", "add", "-q", "-b", "wt", wt_dir.to_str().unwrap()]).unwrap();
        let repo = Repo::open(&wt_dir).unwrap();
        let index = |p: &str| repo.git(&["show", &format!(":{p}")]).unwrap();

        // Untracked: stage one line, then the last one (which has no trailing newline).
        std::fs::write(repo.root.join("u"), "x\ny\nz").unwrap();
        let fd = repo.untracked_diff("u");
        assert!(fd.header.iter().any(|l| l.starts_with("new file mode")), "{:?}", fd.header);
        let sel = pick(&fd, &["+y"]);
        repo.apply_cached(&hunk_patch(&fd, 0, Some(&|i| sel.contains(&i)), false), false).unwrap();
        assert_eq!(index("u"), "y\n");
        let fd = &repo.diff_file("u", false, &DiffOpts::default()).unwrap()[0];
        let sel = pick(fd, &["+z"]);
        repo.apply_cached(&hunk_patch(fd, 0, Some(&|i| sel.contains(&i)), false), false).unwrap();
        assert_eq!(index("u"), "y\nz");

        // Staged new file: unstage one line (this used to fail with
        // "new file n depends on old contents").
        std::fs::write(repo.root.join("n"), "1\n2\n3\n").unwrap();
        repo.stage(&["n"]).unwrap();
        let fd = &repo.diff_file("n", true, &DiffOpts::default()).unwrap()[0];
        let sel = pick(fd, &["+2"]);
        repo.apply_cached(&hunk_patch(fd, 0, Some(&|i| sel.contains(&i)), true), true).unwrap();
        assert_eq!(index("n"), "1\n3\n");
        assert_eq!(std::fs::read_to_string(repo.root.join("n")).unwrap(), "1\n2\n3\n", "working tree untouched");
        // Unstaging every line takes the file out of the index again.
        let fd = &repo.diff_file("n", true, &DiffOpts::default()).unwrap()[0];
        let sel = pick(fd, &["+1", "+3"]);
        repo.apply_cached(&hunk_patch(fd, 0, Some(&|i| sel.contains(&i)), true), true).unwrap();
        assert!(repo.git(&["ls-files", "--", "n"]).unwrap().trim().is_empty());

        // Untracked: discard one line, then undo.
        std::fs::write(repo.root.join("d"), "p\nq\nr\n").unwrap();
        let fd = repo.untracked_diff("d");
        let sel = pick(&fd, &["+q"]);
        let patch = hunk_patch(&fd, 0, Some(&|i| sel.contains(&i)), true);
        repo.apply_worktree(&patch, true).unwrap();
        assert_eq!(std::fs::read_to_string(repo.root.join("d")).unwrap(), "p\nr\n");
        repo.apply_worktree(&patch, false).unwrap();
        assert_eq!(std::fs::read_to_string(repo.root.join("d")).unwrap(), "p\nq\nr\n");

        // Deleted in the working tree: stage the removal of one line.
        std::fs::remove_file(repo.root.join("f.txt")).unwrap();
        let fd = &repo.diff_file("f.txt", false, &DiffOpts::default()).unwrap()[0];
        let sel = pick(fd, &["-b"]);
        repo.apply_cached(&hunk_patch(fd, 0, Some(&|i| sel.contains(&i)), false), false).unwrap();
        assert_eq!(index("f.txt"), "a\nc\nd\n");

        // Staged deletion: unstage the removal of one line.
        std::fs::write(repo.root.join("g"), "k\nl\nm\n").unwrap();
        repo.stage(&["g"]).unwrap();
        repo.commit("g", false).unwrap();
        repo.git(&["rm", "-q", "g"]).unwrap();
        let fd = &repo.diff_file("g", true, &DiffOpts::default()).unwrap()[0];
        let sel = pick(fd, &["-l"]);
        repo.apply_cached(&hunk_patch(fd, 0, Some(&|i| sel.contains(&i)), true), true).unwrap();
        assert_eq!(index("g"), "l\n");

        let _ = main.git(&["worktree", "remove", "--force", wt_dir.to_str().unwrap()]);
        let _ = std::fs::remove_dir_all(&main.root);
    }

    #[test]
    fn parses_worktree_list() {
        let out = "worktree /code/app\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\n\
                   worktree /code/app-x\nHEAD 2222222222222222222222222222222222222222\ndetached\nlocked reason\n\n\
                   worktree /code/gone\nHEAD 3333333333333333333333333333333333333333\nbranch refs/heads/feature/y\nprunable gitdir file points to non-existent location\n";
        let w = parse_worktrees(out);
        assert_eq!(w.len(), 3);
        assert_eq!((w[0].path.to_str(), w[0].branch.as_deref()), (Some("/code/app"), Some("main")));
        assert!(w[1].branch.is_none() && w[1].locked && !w[1].prunable);
        assert_eq!(w[2].branch.as_deref(), Some("feature/y"));
        assert!(w[2].prunable);
        assert_eq!(w[1].name(), "app-x");
    }

    #[test]
    fn counts_changes_with_renames() {
        let repo = temp_repo("count");
        assert_eq!(change_count(&repo.root), 0);
        repo.git(&["mv", "f.txt", "g.txt"]).unwrap();
        std::fs::write(repo.root.join("new"), "n").unwrap();
        assert_eq!(change_count(&repo.root), 2);
    }

    #[test]
    fn stage_and_unstage_selected_lines() {
        let repo = temp_repo("lines");
        // Change b -> B and d -> D, plus add a new line.
        std::fs::write(repo.root.join("f.txt"), "a\nB\nc\nD\ne\n").unwrap();
        let files = repo.diff_file("f.txt", false, &DiffOpts::default()).unwrap();
        let h = &files[0].hunks[0];
        // Select only the "-b" / "+B" pair.
        let pick: Vec<usize> = h.lines.iter().enumerate().filter(|(_, l)| *l == "-b" || *l == "+B").map(|(i, _)| i).collect();
        let sel = |i: usize| pick.contains(&i);
        let patch = hunk_patch(&files[0], 0, Some(&sel), false);
        repo.apply_cached(&patch, false).unwrap();
        let staged = repo.git(&["show", ":f.txt"]).unwrap();
        assert_eq!(staged, "a\nB\nc\nd\n");

        // Now unstage it again by selecting the same lines in the staged diff.
        let files = repo.diff_file("f.txt", true, &DiffOpts::default()).unwrap();
        let h = &files[0].hunks[0];
        let pick: Vec<usize> = h.lines.iter().enumerate().filter(|(_, l)| l.starts_with(['+', '-'])).map(|(i, _)| i).collect();
        let sel = |i: usize| pick.contains(&i);
        let patch = hunk_patch(&files[0], 0, Some(&sel), true);
        repo.apply_cached(&patch, true).unwrap();
        assert_eq!(repo.git(&["show", ":f.txt"]).unwrap(), "a\nb\nc\nd\n");
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    #[test]
    fn partial_unstage_keeps_unselected() {
        let repo = temp_repo("unstage");
        std::fs::write(repo.root.join("f.txt"), "a\nB\nc\nD\n").unwrap();
        repo.stage(&["f.txt"]).unwrap();
        let files = repo.diff_file("f.txt", true, &DiffOpts::default()).unwrap();
        let h = &files[0].hunks[0];
        let pick: Vec<usize> = h.lines.iter().enumerate().filter(|(_, l)| *l == "-d" || *l == "+D").map(|(i, _)| i).collect();
        let sel = |i: usize| pick.contains(&i);
        repo.apply_cached(&hunk_patch(&files[0], 0, Some(&sel), true), true).unwrap();
        assert_eq!(repo.git(&["show", ":f.txt"]).unwrap(), "a\nB\nc\nd\n");
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    /// `lines` joined, ending in a newline or not.
    fn text_of(lines: &[&str], eol: bool) -> String {
        lines.join("\n") + if eol { "\n" } else { "" }
    }

    #[test]
    fn staged_lines_land_where_they_are_in_repeated_blocks() {
        // Two lines added at the top, and "X" between blocks that repeat every two lines further
        // down. Staging just "X" used to put it two lines too low: the patch said where X sits in
        // the working tree (below the two new lines), and git takes the nearest spot where the
        // lines around it match.
        let repo = temp_repo("repeated");
        let pairs = |n: usize| (0..n).flat_map(|_| ["a", "b"]).collect::<Vec<_>>();
        std::fs::write(repo.root.join("r"), text_of(&[vec!["top"], pairs(10)].concat(), true)).unwrap();
        repo.stage(&["r"]).unwrap();
        std::fs::write(repo.root.join("r"), text_of(&[vec!["top", "n1", "n2"], pairs(5), vec!["X"], pairs(5)].concat(), true)).unwrap();
        let fd = &repo.diff_file("r", false, &DiffOpts::default()).unwrap()[0];
        assert_eq!(fd.hunks.len(), 2);
        let x = fd.hunks[1].lines.iter().position(|l| l == "+X").unwrap();
        repo.apply_cached(&lines_patch(fd, &[1], Some(&|h, l| (h, l) == (1, x)), false, &|n| n), false).unwrap();
        assert_eq!(repo.git(&["show", ":r"]).unwrap(), text_of(&[vec!["top"], pairs(5), vec!["X"], pairs(5)].concat(), true));
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    #[test]
    fn line_selection_at_a_missing_final_newline_keeps_lines_apart() {
        // "x\ny" becomes "x\nz", neither with a final newline, so both sides of the hunk end in
        // "\ No newline at end of file". Picking one of the two lines used to leave that marker on
        // a line that is no longer last, which glued it to the next one ("x\nyz").
        let repo = temp_repo("eof");
        let read = |p: &str| std::fs::read_to_string(repo.root.join(p)).unwrap();
        std::fs::write(repo.root.join("e"), "x\ny").unwrap();
        repo.stage(&["e"]).unwrap();
        repo.commit("e", false).unwrap();
        std::fs::write(repo.root.join("e"), "x\nz").unwrap();
        let fd = &repo.diff_file("e", false, &DiffOpts::default()).unwrap()[0];
        // Stage the new line only: y stays, z follows it.
        let sel = pick(fd, &["+z"]);
        repo.apply_cached(&hunk_patch(fd, 0, Some(&|i| sel.contains(&i)), false), false).unwrap();
        assert_eq!(repo.git(&["show", ":e"]).unwrap(), "x\ny\nz");
        // Discard the removal only: y comes back, in front of z.
        let sel = pick(fd, &["-y"]);
        repo.apply_worktree(&hunk_patch(fd, 0, Some(&|i| sel.contains(&i)), true), true).unwrap();
        assert_eq!(read("e"), "x\ny\nz");
        // Unstage the removal only, from a staged "x\nz".
        std::fs::write(repo.root.join("e"), "x\nz").unwrap();
        repo.stage(&["e"]).unwrap();
        let fd = &repo.diff_file("e", true, &DiffOpts::default()).unwrap()[0];
        let sel = pick(fd, &["-y"]);
        repo.apply_cached(&hunk_patch(fd, 0, Some(&|i| sel.contains(&i)), true), true).unwrap();
        assert_eq!(repo.git(&["show", ":e"]).unwrap(), "x\ny\nz");
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    #[test]
    fn discard_staged_lines_from_index_and_working_tree() {
        let repo = temp_repo("discard-staged");
        let nums: Vec<String> = (1..=20).map(|i| i.to_string()).collect();
        let text = |v: &[String]| v.join("\n") + "\n";
        std::fs::write(repo.root.join("n"), text(&nums)).unwrap();
        repo.stage(&["n"]).unwrap();
        repo.commit("n", false).unwrap();
        // Staged: 3 → three (first hunk), 17 → seventeen (second hunk).
        let mut staged = nums.clone();
        staged[2] = "three".into();
        staged[16] = "seventeen".into();
        std::fs::write(repo.root.join("n"), text(&staged)).unwrap();
        repo.stage(&["n"]).unwrap();
        // Unstaged on top: a new first line, so the working tree's line numbers differ.
        let wt: Vec<String> = [vec!["0".to_string()], staged.clone()].concat();
        std::fs::write(repo.root.join("n"), text(&wt)).unwrap();

        let fd = &repo.diff_file("n", true, &DiffOpts::default()).unwrap()[0];
        assert_eq!(fd.hunks.len(), 2);
        let sel = pick(fd, &["-3", "+three"]);
        let (index_patch, wt_patch) = repo.discard_staged(fd, &[0], Some(&|h, l| h == 0 && sel.contains(&l))).unwrap();
        let mut want = nums.clone();
        want[16] = "seventeen".into();
        assert_eq!(repo.git(&["show", ":n"]).unwrap(), text(&want));
        assert_eq!(std::fs::read_to_string(repo.root.join("n")).unwrap(), text(&[vec!["0".to_string()], want].concat()));
        // Both patches put back the way they came (undo).
        repo.apply_worktree(&wt_patch, false).unwrap();
        repo.apply_cached(&index_patch, false).unwrap();
        assert_eq!(repo.git(&["show", ":n"]).unwrap(), text(&staged));
        assert_eq!(std::fs::read_to_string(repo.root.join("n")).unwrap(), text(&wt));

        // Lines edited again in the working tree are left alone, and so is the index.
        let mut edited = wt.clone();
        edited[3] = "THREE".into();
        std::fs::write(repo.root.join("n"), text(&edited)).unwrap();
        let e = repo.discard_staged(fd, &[0], Some(&|h, l| h == 0 && sel.contains(&l))).unwrap_err();
        assert!(e.starts_with("Not discarded"), "{e}");
        assert_eq!(repo.git(&["show", ":n"]).unwrap(), text(&staged));
        assert_eq!(std::fs::read_to_string(repo.root.join("n")).unwrap(), text(&edited));
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    #[test]
    fn blank_context_lines_survive_suppress_blank_empty() {
        // With diff.suppressBlankEmpty git prints a blank context line as an empty line.
        let out = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n";
        let fd = &parse_diff(out)[0];
        assert_eq!(fd.hunks[0].lines, [" a", " ", "-b", "+c"]);
    }

    #[test]
    fn stashes_push_list_and_show() {
        let repo = temp_repo("stash");
        let read = |p: &str| std::fs::read_to_string(repo.root.join(p)).unwrap_or_default();
        assert!(repo.stash_push(StashKind::Changes, "nothing").is_err(), "nothing to stash");
        std::fs::write(repo.root.join("f.txt"), "a\nB\nc\nd\n").unwrap();
        std::fs::write(repo.root.join("new"), "untracked\n").unwrap();
        // Tracked changes only (git's default): the untracked file stays.
        let h = repo.stash_push(StashKind::Changes, "first").unwrap();
        assert_eq!(read("f.txt"), "a\nb\nc\nd\n");
        assert_eq!(read("new"), "untracked\n");
        let list = repo.stashes();
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].name.as_str(), list[0].hash.as_str(), list[0].parts().1), ("stash@{0}", h.as_str(), "first"));
        let files = repo.stash_diff("stash@{0}", &DiffOpts::default()).unwrap();
        assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["f.txt"]);
        // With untracked files: they go too, and the diff shows them.
        let h2 = repo.stash_push(StashKind::WithUntracked, "").unwrap();
        assert!(!repo.root.join("new").exists());
        assert_eq!(repo.stashes()[0].hash, h2);
        assert!(repo.stashes()[0].subject.starts_with("WIP on "), "git's own message");
        let files = repo.stash_diff("stash@{0}", &DiffOpts::default()).unwrap();
        assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["new"]);
        repo.git(&["stash", "pop", "-q"]).unwrap();
        // Only what's staged; and everything but keeping the index.
        std::fs::write(repo.root.join("new"), "untracked, changed\n").unwrap();
        repo.stage(&["new"]).unwrap();
        std::fs::write(repo.root.join("f.txt"), "a\nb\nc\nD\n").unwrap();
        repo.stash_push(StashKind::Staged, "staged").unwrap();
        assert!(!repo.root.join("new").exists());
        assert_eq!(read("f.txt"), "a\nb\nc\nD\n", "unstaged changes stay");
        repo.git(&["stash", "pop", "-q", "--index"]).unwrap();
        repo.stash_push(StashKind::KeepIndex, "keep").unwrap();
        assert_eq!(read("new"), "untracked, changed\n", "the staged file stays");
        assert_eq!(read("f.txt"), "a\nb\nc\nd\n");
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    #[test]
    fn stash_subjects_split_into_branch_and_message() {
        let st = |subject: &str| Stash { name: "stash@{0}".into(), hash: "h".into(), subject: subject.into(), time: 0 };
        assert_eq!(st("On feature/x: half done").parts(), ("feature/x", "half done"));
        assert_eq!(st("WIP on main: 1a2b3c4 Last commit: subject").parts(), ("main", "1a2b3c4 Last commit: subject"));
        assert_eq!(st("odd").parts(), ("", "odd"));
    }

    #[test]
    fn discard_selected_lines_then_undo() {
        let repo = temp_repo("discard");
        let edited = "a\nB\nc\nD\n";
        std::fs::write(repo.root.join("f.txt"), edited).unwrap();
        let files = repo.diff_file("f.txt", false, &DiffOpts::default()).unwrap();
        let h = &files[0].hunks[0];
        let pick: Vec<usize> = h.lines.iter().enumerate().filter(|(_, l)| *l == "-d" || *l == "+D").map(|(i, _)| i).collect();
        let sel = |i: usize| pick.contains(&i);
        let patch = hunk_patch(&files[0], 0, Some(&sel), true);
        repo.apply_worktree(&patch, true).unwrap();
        assert_eq!(std::fs::read_to_string(repo.root.join("f.txt")).unwrap(), "a\nB\nc\nd\n");
        repo.apply_worktree(&patch, false).unwrap(); // undo
        assert_eq!(std::fs::read_to_string(repo.root.join("f.txt")).unwrap(), edited);

        // Whole-file discard + undo.
        let full = repo.file_patch("f.txt").unwrap();
        repo.apply_worktree(&full, true).unwrap();
        assert_eq!(std::fs::read_to_string(repo.root.join("f.txt")).unwrap(), "a\nb\nc\nd\n");
        repo.apply_worktree(&full, false).unwrap();
        assert_eq!(std::fs::read_to_string(repo.root.join("f.txt")).unwrap(), edited);
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    #[test]
    fn blame_and_stream() {
        let repo = temp_repo("blame");
        std::fs::write(repo.root.join("f.txt"), "a\nb\nc\nd\nnew\n").unwrap();
        let b = repo.blame(None, "f.txt").unwrap();
        assert_eq!(b.lines.len(), 5);
        assert_eq!(b.commits[b.line_commit[0] as usize].summary, "init");
        assert!(b.commits[b.line_commit[4] as usize].hash.starts_with("0000000"));
        let b = repo.blame(Some("HEAD"), "f.txt").unwrap();
        assert_eq!(b.lines, ["a", "b", "c", "d"]);
        assert_eq!(repo.ls_tree("HEAD").unwrap(), ["f.txt"]);

        let rx = stream_log(repo.root.clone(), vec![]);
        let mut n = 0;
        loop {
            match rx.recv().unwrap() {
                LogMsg::Batch(cs) => n += cs.len(),
                LogMsg::Done => break,
                LogMsg::Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(n, 1);
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    #[test]
    fn merge_preview_conflicts_and_resolution() {
        let repo = temp_repo("merge");
        let g = |args: &[&str]| repo.git(args).unwrap();
        g(&["switch", "-q", "-c", "side"]);
        std::fs::write(repo.root.join("f.txt"), "a\nSIDE\nc\nd\n").unwrap();
        g(&["commit", "-qam", "side change"]);
        std::fs::write(repo.root.join("other.txt"), "x\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "side adds file"]);
        g(&["switch", "-q", "-"]);

        // Non-conflicting while main hasn't moved; fast-forward possible.
        assert!(matches!(repo.merge_preview("HEAD", "side"), MergePreview::Clean));
        assert!(repo.is_ancestor("HEAD", "side"));

        std::fs::write(repo.root.join("f.txt"), "a\nMAIN\nc\nd\n").unwrap();
        g(&["commit", "-qam", "main change"]);
        match repo.merge_preview("HEAD", "side") {
            MergePreview::Conflicts(files) => assert_eq!(files, ["f.txt"]),
            _ => panic!("expected a conflict"),
        }
        assert!(repo.op_in_progress().is_none());

        // A real merge stops on the conflict; the conflict view splits the two sides.
        assert!(repo.git(&["merge", "--no-edit", "side"]).is_err());
        assert_eq!(repo.op_in_progress(), Some(OpKind::Merge));
        let st = repo.status().unwrap();
        assert!(st.iter().any(|c| c.path == "f.txt" && c.conflicted()));
        let fd = repo.conflict_diff("f.txt");
        let lines = &fd.hunks[0].lines;
        assert!(lines.contains(&"-MAIN".to_string()));
        assert!(lines.contains(&"+SIDE".to_string()));
        // 3 markers, or 4 with merge.conflictStyle=diff3/zdiff3 (common-ancestor section).
        assert!((3..=4).contains(&lines.iter().filter(|l| l.starts_with('\\')).count()));

        // Abort restores the pre-merge state.
        repo.op_abort(OpKind::Merge).unwrap();
        assert!(repo.op_in_progress().is_none());

        // Resolve by taking a side, then continue.
        assert!(repo.git(&["merge", "--no-edit", "side"]).is_err());
        g(&["checkout", "--theirs", "--", "f.txt"]);
        g(&["add", "--", "f.txt"]);
        repo.op_continue(OpKind::Merge).unwrap();
        assert!(repo.op_in_progress().is_none());
        assert_eq!(std::fs::read_to_string(repo.root.join("f.txt")).unwrap(), "a\nSIDE\nc\nd\n");
        assert_eq!(repo.log().unwrap()[0].parents.len(), 2);
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    #[test]
    fn hidden_branches_leave_the_log() {
        let repo = temp_repo("hidden");
        let g = |args: &[&str]| repo.git(args).unwrap();
        g(&["switch", "-q", "-c", "experiment"]);
        std::fs::write(repo.root.join("x.txt"), "x").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "experiment only"]);
        g(&["switch", "-q", "-"]);
        let subjects = |hidden: Vec<String>| {
            let rx = stream_log(repo.root.clone(), hidden);
            let mut out = Vec::new();
            loop {
                match rx.recv().unwrap() {
                    LogMsg::Batch(cs) => out.extend(cs.into_iter().map(|c| c.subject)),
                    LogMsg::Done => break,
                    LogMsg::Err(e) => panic!("{e}"),
                }
            }
            out
        };
        assert!(subjects(vec![]).contains(&"experiment only".to_string()));
        assert_eq!(subjects(vec!["refs/heads/experiment".into()]), ["init"]);
        repo.set_hidden_refs(&["refs/heads/experiment".into()]).unwrap();
        assert_eq!(repo.hidden_refs(), ["refs/heads/experiment"]);
        repo.set_hidden_refs(&[]).unwrap();
        assert!(repo.hidden_refs().is_empty());
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    #[test]
    fn dates_and_side_blame() {
        // 2021-01-01 00:00 UTC, shifted by whatever the local offset is.
        let s = abs_time(1_609_459_200 - utc_offset());
        assert_eq!(s, "2021-01-01 00:00");
        let repo = temp_repo("sideblame");
        std::fs::write(repo.root.join("f.txt"), "a\nB\nc\nd\n").unwrap();
        let head = blame_at(&repo.root, &Side::Rev("HEAD".into()), "f.txt").unwrap();
        let wt = blame_at(&repo.root, &Side::Worktree, "f.txt").unwrap();
        let idx = blame_at(&repo.root, &Side::Index, "f.txt").unwrap();
        assert_eq!(head.commits[head.line_commit[1] as usize].summary, "init");
        assert_eq!(wt.commits[wt.line_commit[1] as usize].hash, UNCOMMITTED);
        assert_eq!(idx.commits[idx.line_commit[1] as usize].summary, "init");
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    #[test]
    fn parses_status_and_log() {
        let repo = temp_repo("status");
        std::fs::write(repo.root.join("new.txt"), "x").unwrap();
        std::fs::write(repo.root.join("f.txt"), "changed\n").unwrap();
        let st = repo.status().unwrap();
        assert!(st.iter().any(|c| c.path == "new.txt" && c.untracked()));
        assert!(st.iter().any(|c| c.path == "f.txt" && c.unstaged() && !c.staged()));
        let log = repo.log().unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].is_head());
        let _ = std::fs::remove_dir_all(&repo.root);
    }

    /// Random edits (repetitive content, missing final newlines) and random line selections,
    /// staged, discarded and unstaged, each checked against what that selection should give.
    /// A few hundred git calls, so it only runs on request:
    /// `cargo test fuzz_line_selection -- --ignored` (vary with ITERS, SEED, CTX).
    #[test]
    #[ignore]
    fn fuzz_line_selection() {
        let repo = temp_repo("fuzz");
        let iters: usize = std::env::var("ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(200);
        let mut seed: u64 = std::env::var("SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x9e37_79b9_7f4a_7c15);
        let ctx: u32 = std::env::var("CTX").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
        let mut rnd = move |n: usize| -> usize {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        const ALPHA: [&str; 8] = ["a", "b", "c", "}", "", "x", "y", "{"];
        type Text = Vec<(String, bool)>;
        fn text(lines: &[String], eol: bool) -> Text {
            let n = lines.len();
            lines.iter().enumerate().map(|(i, l)| (l.clone(), i + 1 < n || eol)).collect()
        }
        fn ser(t: &[(String, bool)]) -> String {
            let n = t.len();
            let mut s = String::new();
            for (i, (l, nl)) in t.iter().enumerate() {
                s.push_str(l);
                if *nl || i + 1 < n {
                    s.push('\n');
                }
            }
            s
        }
        // (index into hunk.lines, tag, text, ends with newline)
        fn parsed(h: &Hunk) -> Vec<(usize, u8, String, bool)> {
            let mut out: Vec<(usize, u8, String, bool)> = vec![];
            for (i, l) in h.lines.iter().enumerate() {
                match l.as_bytes().first() {
                    Some(b'\\') => {
                        if let Some(last) = out.last_mut() {
                            last.3 = false;
                        }
                    }
                    Some(&t) => out.push((i, t, l[1..].to_string(), true)),
                    None => out.push((i, b' ', String::new(), true)),
                }
            }
            out
        }
        fn expect(target: &Text, h: &Hunk, sel: &dyn Fn(usize) -> bool, reverse: bool) -> String {
            let p = parsed(h);
            let (side_tag, start) = if reverse { (b'+', h.new_start) } else { (b'-', h.old_start) };
            let n = p.iter().filter(|x| x.1 == b' ' || x.1 == side_tag).count();
            let begin = if n > 0 { start as usize - 1 } else { start as usize };
            let mut out: Text = target[..begin].to_vec();
            for (i, t, s, nl) in &p {
                let keep = match (*t, reverse) {
                    (b' ', _) => true,
                    (b'-', false) => !sel(*i),
                    (b'+', false) => sel(*i),
                    (b'+', true) => !sel(*i),
                    (b'-', true) => sel(*i),
                    _ => true,
                };
                if keep {
                    out.push((s.clone(), *nl));
                }
            }
            out.extend_from_slice(&target[begin + n..]);
            let len = out.len();
            for (i, l) in out.iter_mut().enumerate() {
                if i + 1 < len {
                    l.1 = true;
                }
            }
            ser(&out)
        }
        let p = "z.txt";
        let file = repo.root.join(p);
        let opts = DiffOpts { context: ctx, ignore_ws: false };
        let (mut runs, mut errs, mut wrong) = (0, 0, 0);
        for it in 0..iters {
            // Repetitive content, like real code: a short block repeated, lightly mutated.
            let block: Vec<String> = (0..2 + rnd(4)).map(|_| ALPHA[rnd(ALPHA.len())].to_string()).collect();
            let n = 1 + rnd(50);
            let mut old: Vec<String> = (0..n).map(|i| block[i % block.len()].clone()).collect();
            for _ in 0..rnd(4) {
                let at = rnd(old.len());
                old[at] = ALPHA[rnd(ALPHA.len())].to_string();
            }
            let old_eol = rnd(3) != 0;
            let mut new = old.clone();
            for _ in 0..1 + rnd(6) {
                match rnd(3) {
                    0 => {
                        let at = rnd(new.len() + 1);
                        for k in 0..1 + rnd(3) {
                            new.insert(at + k, ALPHA[rnd(ALPHA.len())].to_string());
                        }
                    }
                    1 if !new.is_empty() => {
                        let at = rnd(new.len());
                        new.remove(at);
                    }
                    _ if !new.is_empty() => {
                        let at = rnd(new.len());
                        new[at] = ALPHA[rnd(ALPHA.len())].to_string();
                    }
                    _ => {}
                }
            }
            let new_eol = if rnd(4) == 0 { !old_eol } else { old_eol };
            if new.is_empty() {
                continue;
            }
            // Canonical form: "x\n" is one line with a newline, not two.
            let canon = |t: &Text| -> Text {
                let s = ser(t);
                let eol = s.ends_with('\n');
                let body = s.strip_suffix('\n').unwrap_or(&s);
                if body.is_empty() && !eol {
                    return vec![];
                }
                let v: Vec<String> = body.split('\n').map(String::from).collect();
                text(&v, eol)
            };
            let (to, tn) = (canon(&text(&old, old_eol)), canon(&text(&new, new_eol)));
            let (so, sn) = (ser(&to), ser(&tn));
            if so == sn {
                continue;
            }
            for kind in 0..3 {
                // 0 = stage (index → worktree diff), 1 = discard (same diff), 2 = unstage (HEAD → index diff).
                std::fs::write(&file, &so).unwrap();
                repo.git(&["add", "--", p]).unwrap();
                if kind == 2 {
                    repo.git(&["commit", "-qm", "x", "--allow-empty"]).unwrap();
                    std::fs::write(&file, &sn).unwrap();
                    repo.git(&["add", "--", p]).unwrap();
                } else {
                    std::fs::write(&file, &sn).unwrap();
                }
                let fd = &repo.diff_file(p, kind == 2, &opts).unwrap()[0];
                let hi = rnd(fd.hunks.len());
                let h = &fd.hunks[hi];
                let changes: Vec<usize> = h.lines.iter().enumerate().filter(|(_, l)| l.starts_with(['+', '-'])).map(|(i, _)| i).collect();
                let mut sel: Vec<usize> = changes.iter().copied().filter(|_| rnd(2) == 0).collect();
                if sel.is_empty() {
                    sel.push(changes[rnd(changes.len())]);
                }
                let pick = |i: usize| sel.contains(&i);
                let reverse = kind != 0;
                let patch = hunk_patch(fd, hi, Some(&pick), reverse);
                let target = if reverse { &tn } else { &to };
                let want = expect(target, h, &pick, reverse);
                let r = match kind {
                    0 => repo.apply_cached(&patch, false),
                    1 => repo.apply_worktree(&patch, true),
                    _ => repo.apply_cached(&patch, true),
                };
                runs += 1;
                let got = match (&r, kind) {
                    (Err(_), _) => None,
                    (_, 1) => Some(std::fs::read_to_string(&file).unwrap()),
                    _ => Some(repo.git(&["show", &format!(":{p}")]).unwrap()),
                };
                let name = ["stage", "discard", "unstage"][kind];
                match got {
                    None => {
                        errs += 1;
                        if errs <= 5 {
                            eprintln!("=== #{it} {name}: ERROR {:?}\nold={so:?}\nnew={sn:?}\nsel={sel:?}\nhunk:\n{}\n{}\npatch:\n{patch}", r, h.header, h.lines.join("\n"));
                        }
                    }
                    Some(g) if g != want => {
                        wrong += 1;
                        if wrong <= 8 {
                            eprintln!("=== #{it} {name}: WRONG\nold={so:?}\nnew={sn:?}\nsel={sel:?}\nhunk {hi}/{}:\n{}\n{}\npatch:\n{patch}want={want:?}\ngot ={g:?}", fd.hunks.len(), h.header, h.lines.join("\n"));
                        }
                    }
                    _ => {}
                }
            }
        }
        eprintln!("runs={runs} errors={errs} wrong={wrong}");
        let _ = std::fs::remove_dir_all(&repo.root);
        assert_eq!((errs, wrong), (0, 0));
    }
}
