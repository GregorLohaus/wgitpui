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
            "--format=%(refname)%1f%(objectname)%1f%(*objectname)%1f%(HEAD)%1f%(upstream:track)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ])?;
        let mut refs = Vec::new();
        for line in out.lines() {
            let f: Vec<&str> = line.split('\x1f').collect();
            if f.len() < 5 {
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
            });
        }
        Ok(refs)
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
        let mut args = vec!["diff", "--no-color", "--no-ext-diff", "-M", &ctx];
        if o.ignore_ws {
            args.push("-w");
        }
        if staged {
            args.push("--cached");
        }
        args.extend(["--", path]);
        Ok(parse_diff(&self.git(&args)?))
    }

    /// Untracked files have no diff; synthesize an all-additions view of the file.
    pub fn untracked_diff(&self, path: &str) -> FileDiff {
        let full = self.root.join(path);
        let mut fd = FileDiff { path: path.to_string(), ..Default::default() };
        match std::fs::read(&full) {
            Ok(bytes) if bytes.len() > 2_000_000 => fd.binary = true,
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => {
                    let lines: Vec<String> = s.lines().map(|l| format!("+{l}")).collect();
                    fd.adds = lines.len() as u32;
                    fd.hunks.push(Hunk {
                        header: format!("@@ -0,0 +1,{} @@ new file", lines.len()),
                        old_start: 0,
                        new_start: 1,
                        lines,
                    });
                }
                Err(_) => fd.binary = true,
            },
            Err(_) => fd.binary = true,
        }
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

    /// Cheap fingerprint of every ref + HEAD; if unchanged, the history doesn't need reloading.
    pub fn refs_signature(&self) -> String {
        let refs = self.git(&["for-each-ref", "--format=%(objectname) %(refname)"]).unwrap_or_default();
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
        let mut args = vec!["apply", "--recount", "--whitespace=nowarn", "--binary"];
        if reverse {
            args.push("-R");
        }
        args.push("-");
        self.git_in(&args, patch.as_bytes())
    }

    /// The full unstaged patch of one file, binary-safe.
    pub fn file_patch(&self, path: &str) -> Result<String, String> {
        self.git(&["diff", "--binary", "--no-color", "--no-ext-diff", "--", path])
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
        let todo_path = self.git_path("wgitpui-rebase-todo");
        std::fs::write(&todo_path, todo).map_err(|e| e.to_string())?;
        let quoted = format!("'{}'", todo_path.display().to_string().replace('\'', "'\\''"));
        let editor = format!("cp {quoted}");
        let mut args = vec!["-c", "rebase.autoSquash=false", "-c", "rebase.updateRefs=false", "rebase", "-i", "--autostash"];
        match base {
            Some(b) => args.push(b),
            None => args.push("--root"),
        }
        let r = self.git_env(&args, &[("GIT_SEQUENCE_EDITOR", &editor), ("GIT_EDITOR", "true")]);
        let _ = std::fs::remove_file(&todo_path);
        r
    }


    pub fn last_message(&self) -> String {
        self.git(&["log", "-1", "--format=%B"]).map(|s| s.trim_end().to_string()).unwrap_or_default()
    }
}

pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    for line in text.split('\n') {
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
            if line.is_empty() {
                continue; // trailing newline at end of output
            }
            match line.as_bytes()[0] {
                b'+' => f.adds += 1,
                b'-' => f.dels += 1,
                _ => {}
            }
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

/// Builds a patch containing one hunk, optionally restricted to the selected `+`/`-` lines.
///
/// For staging (`reverse == false`, applied forward to the index) unselected `-` lines become
/// context and unselected `+` lines are dropped. For unstaging (`reverse == true`, applied with
/// `-R`) it's the mirror image. Hunk counts are recomputed by `git apply --recount`.
pub fn hunk_patch(file: &FileDiff, hunk: usize, selected: Option<&dyn Fn(usize) -> bool>, reverse: bool) -> String {
    let h = &file.hunks[hunk];
    let mut out = String::new();
    for l in &file.header {
        out.push_str(l);
        out.push('\n');
    }
    let mut body = String::new();
    let (mut old_n, mut new_n) = (0, 0);
    let mut last_kept = false;
    for (i, line) in h.lines.iter().enumerate() {
        let sel = selected.is_none_or(|f| f(i));
        let rest = line.get(1..).unwrap_or("");
        let mut keep = |s: String, o: u32, n: u32| {
            body.push_str(&s);
            body.push('\n');
            old_n += o;
            new_n += n;
            true
        };
        last_kept = match line.as_bytes().first() {
            Some(b'+') if sel => keep(line.clone(), 0, 1),
            Some(b'+') if reverse => keep(format!(" {rest}"), 1, 1),
            Some(b'+') => false,
            Some(b'-') if sel => keep(line.clone(), 1, 0),
            Some(b'-') if !reverse => keep(format!(" {rest}"), 1, 1),
            Some(b'-') => false,
            Some(b'\\') => last_kept && keep(line.clone(), 0, 0),
            _ => keep(line.clone(), 1, 1),
        };
    }
    out.push_str(&format!("@@ -{},{} +{},{} @@\n", h.old_start, old_n, h.new_start, new_n));
    out.push_str(&body);
    out
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
}
