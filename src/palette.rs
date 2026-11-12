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
    /// True on the frame the palette opened: the click that opened it must not close it.
    pub fresh: bool,
}

impl Palette {
    pub fn reset(&mut self, query: &str) {
        self.input.set(query);
        self.query = None;
        self.sel = 0;
        self.branch_at = None;
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
