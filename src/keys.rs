//! lazygit-style keyboard navigation: the panes that take focus, key chords written the way
//! lazygit writes them ("j", "J", "<c-j>", "<space>"), and the keymap, which also feeds the `?`
//! help menu.

use crate::ui::{Key, KeyPress};

/// What keyboard focus is on: the panels left to right (1 2 3 4) are the sidebar, the commit
/// graph, the changed files and the diff (lazygit's staging view, also entered with enter).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pane {
    Branches,
    #[default]
    Commits,
    Files,
    /// The file list while a stash is open there (lazygit's stash keys).
    Stash,
    Main,
}

impl Pane {
    pub fn name(self) -> &'static str {
        match self {
            Pane::Branches => "Branches",
            Pane::Commits => "Commits",
            Pane::Files => "Files",
            Pane::Stash => "Stash",
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
    ToggleWide,
    Context(bool),
    Whitespace,
    Split,
    Commit,
    RecentRepos,
    // Files.
    /// Open or close the folder under the cursor.
    ToggleFolder,
    /// h: close the folder, or go up to the one it's in.
    FolderLeft,
    /// l: open the folder, or go into it.
    FolderRight,
    Resolve,
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
    StashMenu,
    StashList,
    Apply,
    Pop,
    // The diff.
    NextHunk,
    PrevHunk,
    SelectLine,
    SelectHunk,
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
    /// The platform's command key: ⌘ on macOS, ctrl elsewhere.
    command: bool,
}

impl Chord {
    /// "j", "J", "?", "<c-j>", "<c-R>", "<a-up>", "<s-tab>", "<c-s-tab>", "<m-k>" (⌘K),
    /// "<space>", "<pgdown>".
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
            _ => Key::Char(n.chars().next().expect("key").to_ascii_lowercase()),
        };
        let upper = |n: &str| n.chars().count() == 1 && n.chars().next().is_some_and(|c| c.is_ascii_uppercase());
        let Some(inner) = s.strip_prefix('<').and_then(|s| s.strip_suffix('>')).filter(|_| s.len() > 2) else {
            return Chord { key: named(s), shift: upper(s), ctrl: false, alt: false, command: false };
        };
        let parts: Vec<&str> = inner.split('-').collect();
        let (mods, name) = match parts.split_last() {
            Some((name, mods)) if !name.is_empty() && mods.iter().all(|m| ["c", "s", "a", "m"].contains(m)) => (mods, *name),
            _ => (&[][..], inner),
        };
        let has = |m: &str| mods.contains(&m);
        Chord { key: named(name), shift: has("s") || upper(name), ctrl: has("c"), alt: has("a"), command: has("m") }
    }

    pub fn matches(&self, k: &KeyPress) -> bool {
        let symbol = matches!(self.key, Key::Char(c) if !c.is_ascii_alphanumeric() && c != ' ');
        let (logo, ctrl) = if cfg!(target_os = "macos") { (self.command, self.ctrl) } else { (false, self.ctrl || self.command) };
        k.key == self.key && k.mods.logo == logo && k.mods.ctrl == ctrl && k.mods.alt == self.alt && (symbol || k.mods.shift == self.shift)
    }
}

pub struct Binding {
    /// None: works in every pane (unless that pane binds the key itself).
    pub pane: Option<Pane>,
    pub keys: &'static [&'static str],
    pub cmd: Cmd,
    /// Shown in the `?` menu; empty for plain movement.
    pub help: &'static str,
    /// A word or two for the key bar at the bottom; empty keeps it off the bar.
    pub bar: &'static str,
}

const fn b(pane: Option<Pane>, keys: &'static [&'static str], cmd: Cmd, help: &'static str, bar: &'static str) -> Binding {
    Binding { pane, keys, cmd, help, bar }
}

const F: Option<Pane> = Some(Pane::Files);
const M: Option<Pane> = Some(Pane::Main);
const C: Option<Pane> = Some(Pane::Commits);
const B: Option<Pane> = Some(Pane::Branches);

/// lazygit's default keys, as far as this app has the feature. Pane bindings win over the
/// global ones.
pub const BINDINGS: &[Binding] = &[
    // Files.
    // Files. The lists are folder trees: on a folder, h / l close and open it (or go up to
    // the folder it's in, or into it); where there's no folder to do that with they move to
    // the next pane, as everywhere.
    b(F, &["<space>"], Cmd::ToggleStaged, "Stage / unstage (a folder: all its files)", "stage"),
    b(F, &["a"], Cmd::ToggleAll, "Stage / unstage all", "all"),
    b(F, &["<enter>"], Cmd::ToggleFolder, "Open / close folder", "open / close"),
    b(F, &["<enter>"], Cmd::Resolve, "Resolve the conflict", "resolve"),
    b(F, &["<enter>"], Cmd::Enter, "Into the diff (stage lines and hunks there)", "diff"),
    b(F, &["h", "<left>"], Cmd::FolderLeft, "Close folder, or go up to the one it's in", "folders"),
    b(F, &["l", "<right>"], Cmd::FolderRight, "Open folder, or go into it", "folders"),
    b(F, &["h", "<left>"], Cmd::PaneLeft, "", ""),
    b(F, &["l", "<right>"], Cmd::PaneRight, "", ""),
    b(F, &["d"], Cmd::DiscardMenu, "Discard…", "discard"),
    b(F, &["D"], Cmd::ResetMenu, "Reset options…", ""),
    b(F, &["A"], Cmd::Amend, "Amend last commit with staged changes", "amend"),
    b(F, &["e", "o"], Cmd::OpenFile, "Open file", ""),
    b(F, &["y"], Cmd::CopyMenu, "Copy to clipboard…", ""),
    b(F, &["<c-o>"], Cmd::CopyPath, "Copy path", ""),
    b(F, &["f"], Cmd::Fetch, "Fetch", ""),
    b(F, &["r"], Cmd::Refresh, "Refresh", ""),
    b(F, &["s"], Cmd::Stash, "Stash all changes", "stash"),
    b(F, &["S"], Cmd::StashMenu, "Stash options…", ""),
    // A stash, open in the file list.
    b(Some(Pane::Stash), &["<space>"], Cmd::Apply, "Apply stash", "apply"),
    b(Some(Pane::Stash), &["g"], Cmd::Pop, "Pop stash", "pop"),
    b(Some(Pane::Stash), &["d"], Cmd::Drop, "Drop stash…", "drop"),
    b(Some(Pane::Stash), &["n"], Cmd::NewBranch, "New branch from stash", "branch"),
    b(Some(Pane::Stash), &["r"], Cmd::Rename, "Rename stash", "rename"),
    b(Some(Pane::Stash), &["<enter>"], Cmd::ToggleFolder, "Open / close folder", "open / close"),
    b(Some(Pane::Stash), &["<enter>"], Cmd::Enter, "Into its diff", "diff"),
    b(Some(Pane::Stash), &["h", "<left>"], Cmd::FolderLeft, "Close folder, or go up to the one it's in", "folders"),
    b(Some(Pane::Stash), &["l", "<right>"], Cmd::FolderRight, "Open folder, or go into it", "folders"),
    b(Some(Pane::Stash), &["h", "<left>"], Cmd::PaneLeft, "", ""),
    b(Some(Pane::Stash), &["l", "<right>"], Cmd::PaneRight, "", ""),
    b(Some(Pane::Stash), &["<esc>"], Cmd::Back, "Close the stash", "close"),
    // The diff: select lines (like clicking them), then stage or discard them.
    b(M, &["h", "<left>"], Cmd::PrevHunk, "Previous hunk", "hunks"),
    b(M, &["l", "<right>"], Cmd::NextHunk, "Next hunk", "hunks"),
    b(M, &["<space>"], Cmd::SelectLine, "Select / unselect line", "select"),
    b(M, &["a"], Cmd::SelectHunk, "Select / unselect hunk", "select hunk"),
    b(M, &["<s-down>"], Cmd::RangeDown, "Select while moving down", ""),
    b(M, &["<s-up>"], Cmd::RangeUp, "Select while moving up", ""),
    b(M, &["s"], Cmd::Stage, "Stage / unstage selection (or the line)", "stage"),
    b(M, &["d"], Cmd::Discard, "Discard selection (or the line)", "discard"),
    b(M, &["<tab>"], Cmd::SwitchSide, "Switch between unstaged and staged", "other side"),
    b(M, &["<esc>"], Cmd::Back, "Clear the selection, then back", "back"),
    b(M, &["e", "o"], Cmd::OpenFile, "Open file", ""),
    b(M, &["H"], Cmd::ScrollLeft, "Scroll left", ""),
    b(M, &["L"], Cmd::ScrollRight, "Scroll right", ""),
    // Commits.
    b(C, &["<space>"], Cmd::Checkout, "Check out commit (detached)", "checkout"),
    b(C, &["<enter>"], Cmd::Enter, "View changed files", "files"),
    b(C, &["n"], Cmd::NewBranch, "New branch here", "branch"),
    b(C, &["T"], Cmd::NewTag, "Tag commit", "tag"),
    b(C, &["g"], Cmd::ResetMenu, "Reset current branch to commit…", "reset"),
    b(C, &["t"], Cmd::Revert, "Revert commit", "revert"),
    b(C, &["C"], Cmd::CopyCommit, "Copy commit (cherry-pick)", "copy"),
    b(C, &["V"], Cmd::PasteCommits, "Paste copied commits", "paste"),
    b(C, &["<c-R>"], Cmd::ClearCopied, "Clear copied commits", ""),
    b(C, &["<c-j>", "<a-down>"], Cmd::MoveDown, "Move commit down", "move"),
    b(C, &["<c-k>", "<a-up>"], Cmd::MoveUp, "Move commit up", "move"),
    b(C, &["s"], Cmd::Squash, "Squash into the commit below", "squash"),
    b(C, &["f"], Cmd::Fixup, "Fixup into the commit below", "fixup"),
    b(C, &["d"], Cmd::Drop, "Drop commit", "drop"),
    b(C, &["r"], Cmd::Reword, "Reword commit", "reword"),
    b(C, &["A"], Cmd::Amend, "Amend staged changes into commit", ""),
    b(C, &["y"], Cmd::CopyMenu, "Copy to clipboard…", ""),
    // Branches (and the workspace rows above them).
    b(B, &["<space>"], Cmd::Checkout, "Check out", "checkout"),
    b(B, &["<enter>"], Cmd::Enter, "View commits", "commits"),
    b(B, &["n"], Cmd::NewBranch, "New branch from here", "new"),
    b(B, &["d"], Cmd::Delete, "Delete", "delete"),
    b(B, &["M"], Cmd::Merge, "Merge into current branch…", "merge"),
    b(B, &["r"], Cmd::Rebase, "Rebase current branch onto this", "rebase"),
    b(B, &["f"], Cmd::FastForward, "Fast-forward from upstream", "fast-forward"),
    b(B, &["u"], Cmd::Upstream, "Set upstream…", "upstream"),
    b(B, &["R"], Cmd::Rename, "Rename branch", "rename"),
    b(B, &["T"], Cmd::NewTag, "New tag here", "tag"),
    b(B, &["P"], Cmd::PushTag, "Push (on a tag: push the tag)", ""),
    b(B, &["g"], Cmd::ResetMenu, "Reset current branch to this…", "reset"),
    b(B, &["-"], Cmd::CheckoutPrev, "Check out previous branch", "previous"),
    b(B, &["c"], Cmd::CheckoutByName, "Check out by name", ""),
    b(B, &["y", "<c-o>"], Cmd::CopyMenu, "Copy name", ""),
    // Everywhere.
    b(None, &["j", "<down>"], Cmd::Down, "", ""),
    b(None, &["k", "<up>"], Cmd::Up, "", ""),
    b(None, &["."], Cmd::PageDown, "", ""),
    b(None, &[","], Cmd::PageUp, "", ""),
    b(None, &["<", "<home>"], Cmd::Top, "", ""),
    b(None, &[">", "<end>"], Cmd::Bottom, "", ""),
    b(None, &["h", "<left>"], Cmd::PaneLeft, "", ""),
    b(None, &["l", "<right>"], Cmd::PaneRight, "", ""),
    b(None, &["<tab>"], Cmd::NextPane, "", ""),
    b(None, &["<s-tab>"], Cmd::PrevPane, "", ""),
    b(None, &["1"], Cmd::Jump(1), "", "panes"),
    b(None, &["2"], Cmd::Jump(2), "", "panes"),
    b(None, &["3"], Cmd::Jump(3), "", "panes"),
    b(None, &["4"], Cmd::Jump(4), "", "panes"),
    b(None, &["5"], Cmd::StashList, "Stashes…", "stashes"),
    b(None, &["<enter>"], Cmd::Enter, "", ""),
    b(None, &["<esc>"], Cmd::Back, "", ""),
    b(None, &["]"], Cmd::NextTab, "Next tab", ""),
    b(None, &["["], Cmd::PrevTab, "Previous tab", ""),
    b(None, &["J", "<pgdown>", "<c-d>"], Cmd::ScrollMainDown, "Scroll diff down", ""),
    b(None, &["K", "<pgup>", "<c-u>"], Cmd::ScrollMainUp, "Scroll diff up", ""),
    b(None, &["c"], Cmd::Commit, "Commit", "commit"),
    b(None, &["P"], Cmd::Push, "Push", "push"),
    b(None, &["p"], Cmd::Pull, "Pull", "pull"),
    b(None, &["R"], Cmd::Refresh, "Refresh", ""),
    b(None, &["z"], Cmd::Undo, "Undo", "undo"),
    b(None, &["<c-z>"], Cmd::Redo, "", ""),
    b(None, &["m"], Cmd::OpMenu, "Merge / rebase options…", "continue / abort"),
    b(None, &["/"], Cmd::Search, "Search (command palette)", "search"),
    b(None, &["w"], Cmd::ToggleWide, "Wide diff view on / off", "wide"),
    b(None, &["+"], Cmd::Wide(true), "", ""),
    b(None, &["_"], Cmd::Wide(false), "", ""),
    b(None, &["}"], Cmd::Context(true), "More diff context", ""),
    b(None, &["{"], Cmd::Context(false), "Less diff context", ""),
    b(None, &["<c-w>"], Cmd::Whitespace, "Toggle whitespace in diffs", ""),
    b(None, &["v", "|"], Cmd::Split, "Split / unified diff", "split"),
    b(None, &["<c-r>"], Cmd::RecentRepos, "Recent repositories", ""),
    b(None, &["?"], Cmd::Help, "", ""),
];

/// What a pane offers, in order: its own bindings, then the global ones it doesn't take over
/// (`Ui::key` gives a key to the first binding offered that has it).
pub fn offers(pane: Pane) -> impl Iterator<Item = &'static Binding> {
    let taken = move |key: &str| BINDINGS.iter().any(|b| b.pane == Some(pane) && b.keys.contains(&key));
    let own = BINDINGS.iter().filter(move |b| b.pane == Some(pane));
    own.chain(BINDINGS.iter().filter(move |b| b.pane.is_none() && !b.keys.iter().all(|k| taken(k))))
}

/// A key the way the bar and menus show it: "space", "ctrl-j", "shift-↓", "⌥↑".
pub fn pretty(key: &str) -> String {
    let Some(inner) = key.strip_prefix('<').and_then(|k| k.strip_suffix('>')).filter(|_| key.len() > 2) else {
        return key.to_string();
    };
    let (mods, name) = match inner.rsplit_once('-') {
        Some((m, n)) if !n.is_empty() && m.split('-').all(|m| ["c", "s", "a", "m"].contains(&m)) => (m, n),
        _ => ("", inner),
    };
    let name = match name {
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        "pgdown" => "pgdn",
        n => n,
    };
    let mut out = String::new();
    for m in mods.split('-').filter(|m| !m.is_empty()) {
        out.push_str(match m {
            "m" if cfg!(target_os = "macos") => "⌘",
            "m" | "c" => "ctrl-",
            "s" => "shift-",
            "a" => "⌥",
            _ => "",
        });
    }
    let name = if out.ends_with('⌘') && name.chars().count() == 1 { name.to_uppercase() } else { name.to_string() };
    out + &name
}

/// Several keys for one entry; a run of digits reads as a range.
pub fn show_keys(keys: &[&str]) -> String {
    let digits = keys.len() > 2 && keys.iter().all(|k| k.len() == 1 && k.as_bytes()[0].is_ascii_digit());
    match digits {
        true => format!("{}–{}", keys[0], keys[keys.len() - 1]),
        false => keys.iter().map(|k| pretty(k)).collect::<Vec<_>>().join(" "),
    }
}

/// A key press as lazygit writes it (tests and `--keys` screenshots drive the app with these).
pub fn press(s: &str) -> KeyPress {
    let c = Chord::parse(s);
    let (logo, ctrl) = if cfg!(target_os = "macos") { (c.command, c.ctrl) } else { (false, c.ctrl || c.command) };
    KeyPress { key: c.key, mods: crate::ui::Mods { shift: c.shift, ctrl, alt: c.alt, logo } }
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
    fn panes_offer_their_own_keys_before_global_ones() {
        let offered = |pane: Pane, key: &str| offers(pane).find(|b| b.keys.contains(&key)).map(|b| b.cmd);
        assert_eq!(offered(Pane::Main, "h"), Some(Cmd::PrevHunk));
        assert_eq!(offered(Pane::Commits, "h"), Some(Cmd::PaneLeft));
        // In the file trees h / l are for folders first, then move between panes.
        let all = |pane: Pane, key: &str| offers(pane).filter(|b| b.keys.contains(&key)).map(|b| b.cmd).collect::<Vec<_>>();
        assert_eq!(all(Pane::Files, "l"), [Cmd::FolderRight, Cmd::PaneRight]);
        assert_eq!(all(Pane::Stash, "<left>"), [Cmd::FolderLeft, Cmd::PaneLeft]);
        assert_eq!((offered(Pane::Files, "f"), offered(Pane::Commits, "f"), offered(Pane::Branches, "f")), (Some(Cmd::Fetch), Some(Cmd::Fixup), Some(Cmd::FastForward)));
        // A global binding a pane takes over entirely isn't offered there at all.
        assert!(!offers(Pane::Branches).any(|b| b.cmd == Cmd::Refresh), "R renames there");
        assert!(offers(Pane::Commits).any(|b| b.cmd == Cmd::Refresh));
    }

    #[test]
    fn command_chords_and_how_keys_read() {
        let cmd_k = super::press("<m-k>");
        assert!(Chord::parse("<m-k>").matches(&cmd_k) && !Chord::parse("k").matches(&cmd_k));
        let ctrl_shift_tab = super::press("<c-s-tab>");
        assert!(Chord::parse("<c-s-tab>").matches(&ctrl_shift_tab) && !Chord::parse("<c-tab>").matches(&ctrl_shift_tab));
        let mac = cfg!(target_os = "macos");
        assert_eq!(pretty("<m-k>"), if mac { "⌘K" } else { "ctrl-k" });
        assert_eq!(pretty("<m-enter>"), if mac { "⌘enter" } else { "ctrl-enter" });
        assert_eq!((pretty("<s-down>"), pretty("<a-up>"), pretty("<c-R>"), pretty("<c-s-tab>"), pretty("J")), ("shift-↓".into(), "⌥↑".into(), "ctrl-R".into(), "ctrl-shift-tab".into(), "J".into()));
    }
}
