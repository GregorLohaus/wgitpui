//! lazygit-style keyboard navigation: the panes that take focus, key chords written the way
//! lazygit writes them ("j", "J", "<c-j>", "<space>"), and the keymap, which also feeds the `?`
//! help menu.

use crate::ui::{Key, KeyPress};

/// What keyboard focus is on. lazygit's numbered side panels map onto this layout: 1 = the
/// workspace rows and 3 = the branch list (both the sidebar), 2 = the changed files, 4 = the
/// commit graph; `Main` is the diff (lazygit's staging view), entered with enter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pane {
    Branches,
    #[default]
    Commits,
    Files,
    Main,
}

impl Pane {
    pub fn name(self) -> &'static str {
        match self {
            Pane::Branches => "Branches",
            Pane::Commits => "Commits",
            Pane::Files => "Files",
            Pane::Main => "Diff",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    // Moving around.
    Down,
    Up,
    PageDown,
    PageUp,
    Top,
    Bottom,
    PaneLeft,
    PaneRight,
    NextPane,
    PrevPane,
    Jump(u8),
    Enter,
    Back,
    NextTab,
    PrevTab,
    ScrollMainDown,
    ScrollMainUp,
    ScrollLeft,
    ScrollRight,
    // Anywhere.
    Push,
    Pull,
    Refresh,
    Undo,
    Redo,
    Help,
    Search,
    OpMenu,
    Wide(bool),
    Context(bool),
    Whitespace,
    Split,
    Commit,
    RecentRepos,
    // Files.
    ToggleStaged,
    ToggleAll,
    DiscardMenu,
    ResetMenu,
    Amend,
    OpenFile,
    CopyMenu,
    CopyPath,
    Fetch,
    Stash,
    // The diff.
    NextHunk,
    PrevHunk,
    HunkMode,
    Range,
    RangeDown,
    RangeUp,
    Stage,
    Discard,
    SwitchSide,
    // Commits.
    Checkout,
    NewBranch,
    NewTag,
    Revert,
    CopyCommit,
    PasteCommits,
    ClearCopied,
    MoveDown,
    MoveUp,
    Squash,
    Fixup,
    Drop,
    Reword,
    Worktree,
    // Branches.
    Delete,
    Merge,
    Rebase,
    FastForward,
    Upstream,
    Rename,
    CheckoutPrev,
    CheckoutByName,
    PushTag,
}

/// A key plus modifiers. Letters care about shift ("J" isn't "j"); symbols don't, since
/// whether they need shift depends on the keyboard layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chord {
    key: Key,
    shift: bool,
    ctrl: bool,
    alt: bool,
}

impl Chord {
    /// "j", "J", "?", "<c-j>", "<c-R>", "<a-up>", "<s-tab>", "<space>", "<pgdown>".
    pub fn parse(s: &str) -> Chord {
        let named = |n: &str| match n {
            "space" => Key::Char(' '),
            "enter" => Key::Enter,
            "esc" => Key::Escape,
            "tab" => Key::Tab,
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            "pgup" => Key::PageUp,
            "pgdown" => Key::PageDown,
            "home" => Key::Home,
            "end" => Key::End,
            "backspace" => Key::Backspace,
            "delete" => Key::Delete,
            _ => {
                let c = n.chars().next().expect("key");
                Key::Char(c.to_ascii_lowercase())
            }
        };
        let letter_shift = |n: &str| n.chars().count() == 1 && n.chars().next().is_some_and(|c| c.is_ascii_uppercase());
        match s.strip_prefix('<').and_then(|s| s.strip_suffix('>')).filter(|_| s.len() > 2) {
            Some(inner) => {
                let (mods, name) = match inner.split_once('-') {
                    Some((m, n)) if m.len() == 1 && !n.is_empty() => (m, n),
                    _ => ("", inner),
                };
                Chord { key: named(name), shift: mods == "s" || letter_shift(name), ctrl: mods == "c", alt: mods == "a" }
            }
            None => Chord { key: named(s), shift: letter_shift(s), ctrl: false, alt: false },
        }
    }

    pub fn matches(&self, k: &KeyPress) -> bool {
        let symbol = matches!(self.key, Key::Char(c) if !c.is_ascii_alphanumeric() && c != ' ');
        k.key == self.key
            && k.mods.ctrl == self.ctrl
            && k.mods.alt == self.alt
            && !k.mods.logo
            && (symbol || k.mods.shift == self.shift)
    }
}

pub struct Binding {
    /// None: works in every pane (unless that pane binds the key itself).
    pub pane: Option<Pane>,
    pub keys: &'static [&'static str],
    pub cmd: Cmd,
    /// Shown in the `?` menu; empty for plain movement.
    pub help: &'static str,
}

const fn b(pane: Option<Pane>, keys: &'static [&'static str], cmd: Cmd, help: &'static str) -> Binding {
    Binding { pane, keys, cmd, help }
}

const F: Option<Pane> = Some(Pane::Files);
const M: Option<Pane> = Some(Pane::Main);
const C: Option<Pane> = Some(Pane::Commits);
const B: Option<Pane> = Some(Pane::Branches);

/// lazygit's default keys, as far as this app has the feature. Pane bindings win over the
/// global ones.
pub const BINDINGS: &[Binding] = &[
    // Files.
    b(F, &["<space>"], Cmd::ToggleStaged, "Stage / unstage file"),
    b(F, &["a"], Cmd::ToggleAll, "Stage / unstage all"),
    b(F, &["<enter>"], Cmd::Enter, "Stage lines and hunks"),
    b(F, &["d"], Cmd::DiscardMenu, "Discard…"),
    b(F, &["D"], Cmd::ResetMenu, "Reset options…"),
    b(F, &["A"], Cmd::Amend, "Amend last commit with staged changes"),
    b(F, &["e", "o"], Cmd::OpenFile, "Open file"),
    b(F, &["y"], Cmd::CopyMenu, "Copy to clipboard…"),
    b(F, &["<c-o>"], Cmd::CopyPath, "Copy path"),
    b(F, &["f"], Cmd::Fetch, "Fetch"),
    b(F, &["r"], Cmd::Refresh, "Refresh"),
    b(F, &["s", "S"], Cmd::Stash, ""),
    // The diff, as lazygit's staging view.
    b(M, &["h", "<left>"], Cmd::PrevHunk, "Previous hunk"),
    b(M, &["l", "<right>"], Cmd::NextHunk, "Next hunk"),
    b(M, &["a"], Cmd::HunkMode, "Select the whole hunk / one line"),
    b(M, &["v"], Cmd::Range, "Range select"),
    b(M, &["<s-down>"], Cmd::RangeDown, ""),
    b(M, &["<s-up>"], Cmd::RangeUp, ""),
    b(M, &["<space>"], Cmd::Stage, "Stage / unstage selection"),
    b(M, &["d"], Cmd::Discard, "Discard selection"),
    b(M, &["<tab>"], Cmd::SwitchSide, "Switch between unstaged and staged"),
    b(M, &["e", "o"], Cmd::OpenFile, "Open file"),
    b(M, &["H"], Cmd::ScrollLeft, "Scroll left"),
    b(M, &["L"], Cmd::ScrollRight, "Scroll right"),
    // Commits.
    b(C, &["<space>"], Cmd::Checkout, "Check out commit (detached)"),
    b(C, &["<enter>"], Cmd::Enter, "View changed files"),
    b(C, &["n"], Cmd::NewBranch, "New branch here"),
    b(C, &["T"], Cmd::NewTag, "Tag commit"),
    b(C, &["g"], Cmd::ResetMenu, "Reset current branch to commit…"),
    b(C, &["t"], Cmd::Revert, "Revert commit"),
    b(C, &["C"], Cmd::CopyCommit, "Copy commit (cherry-pick)"),
    b(C, &["V"], Cmd::PasteCommits, "Paste copied commits"),
    b(C, &["<c-R>"], Cmd::ClearCopied, "Clear copied commits"),
    b(C, &["<c-j>", "<a-down>"], Cmd::MoveDown, "Move commit down"),
    b(C, &["<c-k>", "<a-up>"], Cmd::MoveUp, "Move commit up"),
    b(C, &["s"], Cmd::Squash, "Squash into the commit below"),
    b(C, &["f"], Cmd::Fixup, "Fixup into the commit below"),
    b(C, &["d"], Cmd::Drop, "Drop commit"),
    b(C, &["r"], Cmd::Reword, "Reword commit"),
    b(C, &["A"], Cmd::Amend, "Amend staged changes into commit"),
    b(C, &["y"], Cmd::CopyMenu, "Copy to clipboard…"),
    b(C, &["w"], Cmd::Worktree, "New worktree here"),
    // Branches (and the workspace rows above them).
    b(B, &["<space>"], Cmd::Checkout, "Check out"),
    b(B, &["<enter>"], Cmd::Enter, "View commits"),
    b(B, &["n"], Cmd::NewBranch, "New branch from here"),
    b(B, &["d"], Cmd::Delete, "Delete"),
    b(B, &["M"], Cmd::Merge, "Merge into current branch…"),
    b(B, &["r"], Cmd::Rebase, "Rebase current branch onto this"),
    b(B, &["f"], Cmd::FastForward, "Fast-forward from upstream"),
    b(B, &["u"], Cmd::Upstream, "Set upstream…"),
    b(B, &["R"], Cmd::Rename, "Rename branch"),
    b(B, &["T"], Cmd::NewTag, "New tag here"),
    b(B, &["P"], Cmd::PushTag, "Push (on a tag: push the tag)"),
    b(B, &["g"], Cmd::ResetMenu, "Reset current branch to this…"),
    b(B, &["-"], Cmd::CheckoutPrev, "Check out previous branch"),
    b(B, &["c"], Cmd::CheckoutByName, "Check out by name"),
    b(B, &["w"], Cmd::Worktree, "New worktree from here"),
    b(B, &["y", "<c-o>"], Cmd::CopyMenu, "Copy name"),
    // Everywhere.
    b(None, &["j", "<down>"], Cmd::Down, ""),
    b(None, &["k", "<up>"], Cmd::Up, ""),
    b(None, &["."], Cmd::PageDown, ""),
    b(None, &[","], Cmd::PageUp, ""),
    b(None, &["<", "<home>"], Cmd::Top, ""),
    b(None, &[">", "<end>"], Cmd::Bottom, ""),
    b(None, &["h", "<left>"], Cmd::PaneLeft, ""),
    b(None, &["l", "<right>"], Cmd::PaneRight, ""),
    b(None, &["<tab>"], Cmd::NextPane, ""),
    b(None, &["<s-tab>"], Cmd::PrevPane, ""),
    b(None, &["1"], Cmd::Jump(1), ""),
    b(None, &["2"], Cmd::Jump(2), ""),
    b(None, &["3"], Cmd::Jump(3), ""),
    b(None, &["4"], Cmd::Jump(4), ""),
    b(None, &["5"], Cmd::Jump(5), ""),
    b(None, &["<enter>"], Cmd::Enter, ""),
    b(None, &["<esc>"], Cmd::Back, ""),
    b(None, &["]"], Cmd::NextTab, "Next tab"),
    b(None, &["["], Cmd::PrevTab, "Previous tab"),
    b(None, &["J", "<pgdown>", "<c-d>"], Cmd::ScrollMainDown, "Scroll diff down"),
    b(None, &["K", "<pgup>", "<c-u>"], Cmd::ScrollMainUp, "Scroll diff up"),
    b(None, &["c"], Cmd::Commit, "Commit"),
    b(None, &["P"], Cmd::Push, "Push"),
    b(None, &["p"], Cmd::Pull, "Pull"),
    b(None, &["R"], Cmd::Refresh, "Refresh"),
    b(None, &["z"], Cmd::Undo, "Undo"),
    b(None, &["<c-z>"], Cmd::Redo, ""),
    b(None, &["m"], Cmd::OpMenu, "Merge / rebase options…"),
    b(None, &["/"], Cmd::Search, "Search (command palette)"),
    b(None, &["+"], Cmd::Wide(true), "Bigger diff (wide)"),
    b(None, &["_"], Cmd::Wide(false), "Normal layout"),
    b(None, &["}"], Cmd::Context(true), "More diff context"),
    b(None, &["{"], Cmd::Context(false), "Less diff context"),
    b(None, &["<c-w>"], Cmd::Whitespace, "Toggle whitespace in diffs"),
    b(None, &["|"], Cmd::Split, "Split / unified diff"),
    b(None, &["<c-r>"], Cmd::RecentRepos, "Recent repositories"),
    b(None, &["?"], Cmd::Help, ""),
];

/// The command for a key press in a pane: the pane's own bindings first, then global ones.
pub fn lookup(pane: Pane, k: &KeyPress) -> Option<Cmd> {
    let hit = |p: Option<Pane>| BINDINGS.iter().find(|b| b.pane == p && b.keys.iter().any(|s| Chord::parse(s).matches(k)));
    hit(Some(pane)).or_else(|| hit(None)).map(|b| b.cmd)
}

/// The `?` menu for a pane: (keys, what they do, command), the pane's own first. Keys a pane
/// takes over don't show up again from the global list.
pub fn help(pane: Pane) -> Vec<(&'static [&'static str], &'static str, Cmd)> {
    let own: Vec<&Binding> = BINDINGS.iter().filter(|b| b.pane == Some(pane)).collect();
    let taken = |key: &str| own.iter().any(|b| b.keys.contains(&key));
    own.iter()
        .copied()
        .chain(BINDINGS.iter().filter(|b| b.pane.is_none() && !b.keys.iter().all(|k| taken(k))))
        .filter(|b| !b.help.is_empty())
        .map(|b| (b.keys, b.help, b.cmd))
        .collect()
}

/// A key press as lazygit writes it (tests and `--keys` screenshots drive the app with these).
pub fn press(s: &str) -> KeyPress {
    let c = Chord::parse(s);
    KeyPress { key: c.key, mods: crate::ui::Mods { shift: c.shift, ctrl: c.ctrl, alt: c.alt, logo: false } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Mods;

    fn press(key: Key, shift: bool, ctrl: bool) -> KeyPress {
        KeyPress { key, mods: Mods { shift, ctrl, alt: false, logo: false } }
    }

    #[test]
    fn chords_tell_case_and_modifiers_apart() {
        let j = press(Key::Char('j'), false, false);
        let shift_j = press(Key::Char('j'), true, false);
        let ctrl_j = press(Key::Char('j'), false, true);
        assert!(Chord::parse("j").matches(&j) && !Chord::parse("j").matches(&shift_j) && !Chord::parse("j").matches(&ctrl_j));
        assert!(Chord::parse("J").matches(&shift_j) && !Chord::parse("J").matches(&j));
        assert!(Chord::parse("<c-j>").matches(&ctrl_j) && !Chord::parse("<c-j>").matches(&j));
        assert!(Chord::parse("<c-R>").matches(&press(Key::Char('r'), true, true)));
        assert!(Chord::parse("<s-tab>").matches(&press(Key::Tab, true, false)) && !Chord::parse("<tab>").matches(&press(Key::Tab, true, false)));
        // Symbols match however they were typed.
        assert!(Chord::parse("?").matches(&press(Key::Char('?'), true, false)));
        assert!(Chord::parse("<").matches(&press(Key::Char('<'), true, false)));
        assert!(Chord::parse("<space>").matches(&press(Key::Char(' '), false, false)));
        // ⌘ combinations belong to the app's own shortcuts.
        let cmd_j = KeyPress { key: Key::Char('j'), mods: Mods { logo: true, ..Default::default() } };
        assert!(!Chord::parse("j").matches(&cmd_j));
    }

    #[test]
    fn panes_override_global_keys() {
        let h = press(Key::Char('h'), false, false);
        assert_eq!(lookup(Pane::Main, &h), Some(Cmd::PrevHunk));
        assert_eq!(lookup(Pane::Commits, &h), Some(Cmd::PaneLeft));
        let f = press(Key::Char('f'), false, false);
        assert_eq!((lookup(Pane::Files, &f), lookup(Pane::Commits, &f), lookup(Pane::Branches, &f)), (Some(Cmd::Fetch), Some(Cmd::Fixup), Some(Cmd::FastForward)));
        let r = press(Key::Char('r'), true, false);
        assert_eq!((lookup(Pane::Branches, &r), lookup(Pane::Commits, &r)), (Some(Cmd::Rename), Some(Cmd::Refresh)));
        // The help lists a pane's own keys first, without global ones it shadows.
        let h = help(Pane::Main);
        assert_eq!(h[0].1, "Previous hunk");
        assert!(!help(Pane::Branches).iter().any(|(_, _, c)| *c == Cmd::Refresh), "R renames there");
    }
}
