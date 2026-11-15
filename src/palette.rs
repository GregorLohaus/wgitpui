//! Command palette state and fuzzy matching.

use crate::ui::{Scroll, TextEdit};

#[derive(Clone, Debug)]
pub enum Act {
    Refresh,
    Fetch,
    Pull,
    Push,
    StageAll,
    UnstageAll,
    DiscardAll,
    Commit,
    ToggleAmend,
    ShowWip,
    CopyHash,
    Undo,
    BrowseFiles,
    MoveCommit(bool),
    SquashIntoParent,
    DropCommit,
    OpContinue,
    OpAbort,
    Checkout(String),
    CreateBranch(String),
    SelectCommit(usize),
    Open(String),
    CompareWorktree,
    /// Compare HEAD with a ref since their merge base: (hash, label).
    CompareRef(String, String),
    CompareSwap,
    CompareClose,
    ToggleSplit,
    ToggleWide,
    ToggleWhitespace,
    CycleContext,
    PullRebase,
    PullMerge,
    /// Merge `src` into `dst` (None = the current HEAD; otherwise switch to `dst` first).
    MergeInto { src: String, dst: Option<String> },
    /// Rebase `branch` (None = current HEAD) onto `onto`.
    RebaseOnto { branch: Option<String>, onto: String },
    /// Fast-forward `dst` (None = current) to `src`.
    FastForward { src: String, dst: Option<String> },
    CherryPick(String),
    Revert(String),
    /// Move the current branch to a commit: "soft" | "mixed" | "hard".
    Reset(String, &'static str),
    CheckoutDetached(String),
    /// Ask for a name, then create a branch at this commit.
    PromptBranchAt(String),
    /// Create a branch at a commit: (name, hash, switch to it).
    CreateBranchAt(String, String, bool),
    DeleteBranch(String),
    /// Ask which remote branch a local branch should track.
    PromptUpstream(String),
    /// Make `branch` track `upstream` ("origin/main"), or nothing.
    SetUpstream { branch: String, upstream: Option<String> },
    /// Ask for a tag name (and optional message), then tag this commit.
    PromptTagAt(String),
    /// `git tag` (annotated with a message), then optionally push it to a remote, and with it
    /// the checked-out branch if so (see `Palette::tag_push`).
    CreateTag { name: String, at: String, message: Option<String>, push: Option<(String, bool)> },
    /// Push one tag, or all of them (None), to a remote.
    PushTag { tag: Option<String>, remote: String },
    DeleteTag(String),
    /// Press a key next frame (a key bar entry or a `?` item clicked): whatever offers it
    /// then runs.
    Press(&'static str),
    /// Put text on the clipboard.
    CopyText(String),
    /// Ask for a commit's new message, then reword it.
    PromptReword(String),
    Reword { hash: String, message: String },
    /// Ask for a branch's new name, then rename it.
    PromptRename(String),
    RenameBranch { from: String, to: String },
    /// Fold the staged changes into HEAD, keeping its message.
    Amend,
    /// Cherry-pick these commits (oldest first) onto HEAD.
    CherryPickMany(Vec<String>),
    /// Squash the selected commit into its parent, keeping both messages.
    SquashWithMessage,
    /// Take a file's staged changes out of the index and the working tree.
    DiscardStaged(String),
    /// Discard a file's unstaged changes (or delete it, if untracked).
    DiscardFile(String),
    /// The same for several (a folder's).
    DiscardFiles(Vec<String>),
    /// `git switch -`.
    CheckoutPrev,
    /// Show / hide the key bar along the bottom.
    ToggleKeyBar,
    /// The ways to stash (a menu).
    StashMenu,
    /// Ask for a message, then stash.
    PromptStash(crate::git::StashKind),
    Stash { kind: crate::git::StashKind, message: String },
    /// The stash list (a menu).
    StashList,
    /// Show a stash (by hash) in the right-hand panel.
    OpenStash(String),
    ApplyStash(String),
    PopStash(String),
    DropStash(String),
    /// Ask for a branch name, then `git stash branch`.
    PromptStashBranch(String),
    StashBranch { hash: String, branch: String },
    /// Ask for a new message, then rename the stash.
    PromptStashRename(String),
    RenameStash { hash: String, message: String },
    ResolveUse(String, bool),
    MarkResolved(String),
    OpenResolver(String),
    /// Push the current branch to a remote.
    PushTo { remote: String, force: bool },
    /// Show the remote picker under the Push button.
    PushMenu,
    /// Symlink the CLI into /usr/local/bin.
    InstallCli,
    /// Hide / show a branch (full ref name) in the graph and sidebar.
    HideRef(String),
    ShowRef(String),
    ShowAllHidden,
    DeleteConflicted(String),
    /// Show the remote picker under the Fetch / Pull button.
    FetchMenu,
    PullMenu,
    /// `git fetch --prune <remote>`.
    FetchFrom(String),
    /// Pull `branch` from `remote` into the current branch: "ff" | "rebase" | "merge".
    PullFrom { remote: String, branch: String, mode: &'static str },
    /// Ask for a branch name or path, then create a worktree from this starting point.
    NewWorktree(WtFrom),
    /// `git worktree add [-b new_branch | --detach] path rev`, then open it in a tab.
    AddWorktree { path: String, rev: String, new_branch: Option<String>, detach: bool },
    /// Open (or switch to) the tab of the worktree at this path.
    OpenWorktree(String),
    RemoveWorktree(String),
    PruneWorktrees,
}

/// Where a new worktree starts.
#[derive(Clone, Debug)]
pub struct WtFrom {
    /// Commit-ish to check out (branch name, remote branch or hash).
    pub rev: String,
    /// Shown in the prompt ("main", "origin/topic", "a1b2c3d4").
    pub label: String,
    /// A local branch that isn't checked out anywhere, so the worktree can check it out directly.
    pub branch: Option<String>,
    /// Text the prompt starts with (e.g. the local name for a remote branch).
    pub prefill: String,
}

pub struct Item {
    pub label: String,
    pub detail: String,
    pub act: Act,
    pub score: i32,
    pub kind: &'static str,
}

#[derive(Default)]
pub struct Palette {
    pub input: TextEdit,
    pub sel: usize,
    pub query: Option<String>,
    pub items: Vec<Item>,
    pub scroll: Scroll,
    /// When set, the palette is asking for a new branch name to create at this commit.
    pub branch_at: Option<String>,
    /// When set, the palette is asking for a new worktree's branch name or path.
    pub worktree_from: Option<WtFrom>,
    /// When set, the palette is asking for a tag name (and message) for this commit.
    pub tag_at: Option<String>,
    /// Where that tag can be pushed: a remote, and whether the checked-out branch goes along
    /// (the commit is on it, but on none of the remote's branches yet).
    pub tag_push: Option<(String, bool)>,
    /// When set, the palette is asking which remote branch this local branch should track.
    pub upstream_for: Option<String>,
    /// When set, the palette is asking for this commit's new message.
    pub reword_of: Option<String>,
    /// When set, the palette is asking for this branch's new name.
    pub rename_of: Option<String>,
    /// When set, the palette is asking for a stash message (then stashes this way).
    pub stash_kind: Option<crate::git::StashKind>,
    /// When set, the palette is asking for a branch to make from this stash (hash).
    pub stash_branch: Option<String>,
    /// When set, the palette is asking for this stash's new message.
    pub stash_rename: Option<String>,
    /// True on the frame the palette opened: the click that opened it must not close it.
    pub fresh: bool,
}

impl Palette {
    pub fn reset(&mut self, query: &str) {
        self.input.set(query);
        self.query = None;
        self.sel = 0;
        self.branch_at = None;
        self.worktree_from = None;
        self.tag_at = None;
        self.tag_push = None;
        self.upstream_for = None;
        self.reword_of = None;
        self.rename_of = None;
        self.stash_kind = None;
        self.stash_branch = None;
        self.stash_rename = None;
        self.fresh = true;
    }
}

/// Subsequence fuzzy match with bonuses for word starts, runs and substrings.
pub fn fuzzy(q: &str, s: &str) -> Option<i32> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    let sl = s.to_lowercase();
    let chars: Vec<char> = sl.chars().collect();
    let (mut score, mut si, mut prev) = (0i32, 0usize, usize::MAX);
    for qc in q.chars().filter(|c| *c != ' ') {
        let mut found = false;
        while si < chars.len() {
            if chars[si] == qc {
                score += 10;
                if prev != usize::MAX && prev + 1 == si {
                    score += 15;
                }
                if si == 0 || !chars[si - 1].is_alphanumeric() {
                    score += 20;
                }
                prev = si;
                si += 1;
                found = true;
                break;
            }
            si += 1;
            score -= 1;
        }
        if !found {
            return None;
        }
    }
    if sl.contains(&q) {
        score += 60;
    }
    Some(score - chars.len() as i32 / 10)
}

#[cfg(test)]
mod tests {
    use super::fuzzy;

    #[test]
    fn ranks_word_starts_and_substrings() {
        assert!(fuzzy("fb", "feature/bar").unwrap() > fuzzy("fb", "fooaaaab").unwrap());
        assert!(fuzzy("push", "Push current branch").unwrap() > fuzzy("push", "p u s h").unwrap_or(-999));
        assert!(fuzzy("xyz", "abc").is_none());
    }
}
