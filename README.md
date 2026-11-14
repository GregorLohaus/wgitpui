# wgitpui

A GPU-rendered git client in Rust. The whole UI stack is hand-rolled on top of **wgpu**:
no egui/iced, no font crates, no libgit2.

| Direct dependency | Why |
|---|---|
| `wgpu` | rendering (Metal / Vulkan / DX12, trimmed per platform) |
| `winit` | window + input (wgpu needs a window from somewhere) |
| `pollster` | block on wgpu's async init |

Everything else is in-tree:

- `font.rs`: TrueType parser (cmap 4/12, simple + composite `glyf`, `.ttc`), exact-area coverage rasterizer, shelf-packed glyph atlas. Loads system fonts (SF / Menlo / DejaVu / Segoe; override with `WGITPUI_UI_FONT`, `WGITPUI_MONO_FONT`).
- `gpu.rs`: one pipeline, one vertex format. Rounded rects, rings, AA lines, whole graph edges and soft glows are analytic SDFs in the fragment shader; text samples the atlas.
- `ui.rs`: immediate-mode painter, layout rects, input, smooth scrolling, text editing.
- `git.rs`: drives the `git` CLI (porcelain formats), builds partial patches for line staging.
- `app/load.rs`: what opening a repository and each refresh read from git, as plain data any thread can read (tabs are read on workers, and so is the refresh when you come back), and taking it in without undoing anything newer.
- `graph.rs`: incremental lane layout for the commit graph, plus first-parent lines, which branch each commit belongs to, and where a line was merged into others.
- `rebase.rs`: turns drag-and-drop gestures into `git rebase -i` todo lists.
- `keys.rs`, `app/keyboard.rs`: the lazygit-style keymap (panes, key chords, the `?` menu and the key bar) and what each key does. A key can only be read through `Ui::key` with a `Binding` (its keys, its `?` text, its bar label, whether it applies right now), and the bar and `?` list are made from exactly the bindings offered that frame, so they can't show a key that does nothing or miss one that works.
- `filetree.rs`: changed files as a folder tree with status letters and lines added / removed; the working copy's unstaged and staged lists, a commit's, a stash's and a comparison's files all use it.
- `config.rs`: the settings file (`key = value` lines).
- `chrome.rs`: macOS title-bar integration via the Objective-C runtime.
- `diffview.rs`, `fileview.rs`, `palette.rs`: the diff, blame/file/tree and palette views.
- `syntax.rs`: per-line syntax highlighter and LCS word diff.
- `png.rs`: tiny PNG encoder for headless screenshots.

## Features

- **Tabs**: several repositories open at once, each with its own state; background work (history loading, push/fetch) keeps running in inactive tabs. Open with **+** (native folder picker; ⌘T lists recent repos), by dropping a folder on the window, by typing a path in ⌘K, or as command-line arguments. Tabs show branch plus a status dot (yellow = changes, red = merge/rebase in progress), reorder by dragging, and are restored on the next launch. Opening a repository never holds up the window: worker threads read it while its tab says "opening…" (at launch the visible tab first, then its neighbours; a tab you switch to jumps the queue), and coming back to the window or to a tab catches up with changes made elsewhere in the background too. Keys: ⌘T, ⌘O, ⌘W, ⌘1–⌘9, ctrl-tab / ⌘⇧[ ].
- **Edge-to-edge window** (macOS): transparent title bar, the native traffic lights re-centered inside the top bar (raw Objective-C runtime calls, no crates). Drag empty top-bar space to move, double-click to zoom.
- **Commit graph** with bezier lane edges, ref pills, and a working-copy node tethered to HEAD.
- **Branch lineage**: the selected branch's history (its first-parent line, all the way down) is drawn in one color with a soft glow while the rest of the graph steps back. Selecting a branch, or a commit it points at, picks that branch; any other commit picks the line it sits on (main keeps its own line, a merged and deleted branch still forms one); the working copy picks HEAD's branch. Every merge that took the branch into another one gets a glowing ring.
- **Streaming history**: `git log` is streamed on a background thread into an incremental lane layout, so the first screen paints immediately (100k commits fully load in under a second). Refreshes are skipped entirely when no ref or HEAD moved.
- **Minimap scrollbar**: branch, tag, HEAD and selection markers across the whole history, plus the selected branch: a rail over its own commits and a glowing dot at every merge into another branch. Drag to scrub.
- **File trees**: changed files are listed by folder, everywhere (unstaged, staged, a commit's, a stash's, a comparison's): folders first, a folder that only holds one folder shares its row (`src/app`), and every row shows its status and the lines it adds and removes (a folder: the sum). Click a folder to close or open it; hover a row for *Stage* / *Unstage* / *Discard*, which on a folder act on everything in it. Closed folders stay closed as the files change. A commit's diff follows the tree's order, so going down the list goes down the diff.
- **Line-level staging**: click or drag across `+`/`-` lines, then *Stage N lines* / *Unstage N lines*. A selection can span hunks (any of their buttons acts on all of it) and survives refreshes. Hunk and file staging too.
- **Stashes**: *Stash…* in the working copy's header (or `s` / `S` in its files) puts changes away: all of them, with untracked files, only what's staged, or all but the staged ones. The stashes stay out of the way until there are some: then *Stashes · N* (or `5`) lists them, and one opens in place of the working copy with its message, branch, files and diff, to apply, pop, branch from, rename or drop. Stashing and dropping are undoable.
- **Discard with undo**: discard files, hunks or selected lines (or delete untracked files). In the staged diff, *Discard* takes staged hunks or lines out of both the index and the working tree. Every destructive action goes on an undo stack (`⌘Z`, or *Undo* in the toast), and a copy is written to `.git/wgitpui-trash/` so nothing is ever truly lost.
- **Undoable commits**: commit, amend and rebase record the previous HEAD; undo resets back to it.
- **Drag-to-rebase**: drag a commit row in the graph. Drop on the top/bottom edge of another commit to reorder, or on its middle to squash (fixup) into it. It warns when the rewrite touches pushed history. `⌥↑`/`⌥↓` move the selected commit from the keyboard; *Squash into parent* and *Drop commit* are in the palette. Conflicts show a banner with Continue / Abort.
- **Blame** with an age heatmap: newer lines glow orange, older ones fade to blue. Click a chunk to jump to its commit.
- **Time machine** (*Files* tab, `]`): the repository tree at any commit with that commit's changes highlighted, plus a history scrubber. Drag it or press ▶ to replay history. An open file follows the scrubber, so you can watch a single file (and its blame) evolve.
- **Compare any two commits**: ⌘-click (or shift-click) a second commit in the graph, ⌘-click a branch in the sidebar, or use the palette (*Compare main ↔ feature*, *Compare with working tree*). The graph marks the endpoints A/B and highlights the commits in between. Toggle *exact diff* (`A B`) vs *since merge-base* (`A...B`, what a pull request shows), swap sides, see ahead/behind counts.
- **Diff viewer** (used everywhere): unified or side-by-side, word-level change highlighting, syntax colors (Rust, C-family/JS/TS/Go/Java/Swift…, Python, shell, TOML/YAML/JSON), context lines 3 → 10 → 25 → whole file, ignore whitespace, collapsible files, a file index that tracks your scroll position, and a wide mode that hides the graph. In wide mode the working copy and commits become two columns: the file lists and commit message (or the commit's details and files) on the left, the diff on the right. A commit's *Files* tab does the same with its tree and the opened file. Drag the divider to resize the column; both views share its width.
- **Who changed each line**: diffs, comparisons and the conflict resolver show author · age · short commit id per line (once per run of lines from the same commit, with an age-colored bar). Removed lines are attributed on the old side, added/context lines on the new side (commit vs. parent, working tree vs. index, B vs. A or the merge-base); uncommitted lines say so. Hover for the commit subject and exact date, click to jump to the commit. Loaded in the background, only for files on screen; *Authors* toggles it.
- **Merge, rebase, cherry-pick, revert, reset**: right-click a commit or branch (ctrl-click on macOS) for a context menu, or drag one branch label onto another (graph pill or sidebar) to merge / rebase / fast-forward. *Merge* always makes a merge commit; when the target could simply move forward, *Fast-forward* is offered right below it. Every menu shows a dry-run preview (`git merge-tree`): how many commits come in and whether they conflict, before anything is touched. All of these are undoable, including `reset --hard` (local changes are saved first) and branch deletion.
- **Conflict resolver**: one view of the file, numbered like the result. Each conflict sits in place as its lines from both sides: yours (blue) then incoming (orange), each with a checkbox. Checked lines are kept and numbered; unchecked lines are struck through and dropped. Per-conflict buttons: *Yours*, *Incoming*, *Both* (again to flip order), *Neither*, *Edit*. *Preview result* (`v`) hides everything that won't be saved. Unchecking everything puts a conflict back to undecided (removing both sides is an explicit *Neither*). Opens automatically when a merge / rebase / cherry-pick / revert stops, or via the conflict list / palette. Keys: `1` `2` `3` `0` `e`, `n`/`p`, `⌘S`. *Save & mark resolved* writes + stages the file, moves on to the next conflicted file, and is undoable (git's resolve-undo restores the conflict). Modify/delete conflicts get explicit Keep / Delete choices.
- **Hide branches**: *Hide branch* in a branch's context menu (or the palette) drops it from the graph, minimap and branch lists; commits only that branch reaches disappear (`git log --exclude`). Hidden branches live in a HIDDEN section below REMOTES with *Show* / *Show all*. Saved per repository in `.git/wgitpui-hidden-refs`.
- **Worktrees**: the repository's other worktrees are listed under WORKSPACE with their branch and a changed-file count; click one to open it in its own tab, where staging, line staging, committing and everything else work as in the main working copy. Create one with the **+** next to WORKSPACE, *New worktree here…* on a commit, *Check out in new worktree…* on a branch, or *New worktree…* in ⌘K: type a new branch name (the folder goes next to the main worktree as `<repo>-<branch>`) or a path. Branches checked out in another worktree show its name in the sidebar, and checking one out switches to that worktree's tab instead of failing. Adding and removing run in the background (the worktree's row shows a spinner) and are undoable; removing refuses while the worktree has uncommitted changes, and closes its tab. Stale entries (folder deleted) can be pruned.
- **Push to any remote**: *Push* opens a remote picker: upstream first, each remote with ↑ahead / ↓behind for this branch (or *new branch*). The first push sets the upstream; pushing to another remote leaves it alone. A diverged remote gets a warning plus *Force push (with lease)*, and *Push all tags to <remote>* is at the bottom. Also *Push main to <remote>* in the palette.
- **Upstreams**: right-click a branch → *Set upstream…* / *Change upstream…* to pick the remote branch it tracks (same name first), or stop tracking. A branch whose tracked branch was deleted on the remote says *gone* in the sidebar. Pushes always go to the branch's own name, whatever it tracks. Undoable.
- **Tags**: *Create tag here…* on a commit or branch, or *New tag…* in ⌘K: a name makes a lightweight tag, a name followed by a message an annotated one, and the second choice pushes it right away. Right-click a tag to push it to a remote or delete it (locally, undoable).
- **Pull from any remote**: *Pull* opens the same kind of picker, listing each remote that has your branch (the upstream's branch for the upstream remote) with ↓behind / ↑ahead as of the last fetch. Picking one fast-forwards; when it has diverged, *Rebase onto* and *Merge* are offered too. *Pull with rebase* / *Pull with merge* (upstream) are in the palette.
- **Fetch from any remote**: *Fetch* picks one remote, or all of them at once (with `--prune`).
- **Command palette** (`⌘K` / `Ctrl+K`): fuzzy search over actions, branches (checkout), commits (subject or hash), *create branch*, and *open repository* (type a path).
- Fetch / pull / push run in the background. Double-click a branch to switch.
- Redraws only on input or animation (idle = 0% CPU).

## Keys

The keyboard works like [lazygit](https://github.com/jesseduffield/lazygit)'s defaults, mapped onto this
layout: the sidebar is lazygit's branches panel, the graph its commits, the file list on the right its
files (or an open stash), and the diff its staging view. While you use the keys, a ring shows the pane
they go to; clicking a pane hands it the keys too. `?` lists every key of the focused pane (pick one to
run it).

A bar along the bottom always shows the keys for what has focus (and while you type a commit message,
or a menu, the palette or the conflict resolver is open, theirs); click one to run it. Its × hides it,
and so does `key_bar = false` in the settings file (below); ⌘K → *Show the key bar* brings it back.

| Key | Anywhere |
|---|---|
| `1` `2` `3` `4` | the panes left to right: sidebar / commits / files / diff |
| `h` `l` (`←` `→`), `tab` | pane to the left / right, or round |
| `j` `k` (`↓` `↑`), `.` `,`, `>` `<` | next / previous, a page, last / first |
| `enter` / `esc` | into a file's diff, a commit's files, a branch's commits / back out |
| `]` `[` | sections of the branch list; a commit's Changes / Files (time machine) tabs |
| `J` `K` (`pgdn` `pgup`, `ctrl-d` `ctrl-u`) | scroll the diff |
| `P` / `p` | push / pull: straight to the upstream when it's a plain push or a fast-forward, else the remote picker |
| `c` | write the commit message: `enter` commits the summary, `tab` adds a description, `⌘↵` commits from anywhere |
| `z` (`⌘Z`) | undo the last discard / commit / merge / rebase / reset / branch delete / tag / upstream change |
| `m` | continue or abort a merge / rebase in progress |
| `/` (`⌘K`, `⌘P`) | command palette |
| `ctrl-r` | recent repositories |
| `w` (`+` / `_`) | wide diff view on / off |
| `}` / `{` | more / less diff context |
| `ctrl-w` | ignore whitespace on / off |
| `v` (`\|`) | split / unified diff |
| `5` | stashes |
| `R` (`⌘R`) | refresh (it also refreshes when the window gets focus) |

| Pane | Keys |
|---|---|
| Files | `j` `k` files and folders · `h` close the folder, or up to the one it's in · `l` open the folder, or into it (where there's no folder to do that with, `h` `l` move between panes as everywhere) · `space` stage / unstage the file, or everything in the folder · `a` everything · `enter` into the diff (on a folder: close / open it; on a conflict: the resolver) · `d` discard… (a folder: all of it) · `D` reset… · `A` amend the last commit · `s` stash · `S` stash options… · `e` `o` open · `y` copy… · `f` fetch |
| Diff | `j` `k` line · `h` `l` hunk · `space` select the line · `a` select the hunk · shift-`↓` `↑` select while moving · `s` stage / unstage the selection · `d` discard it (with nothing selected: the line under the cursor) · `tab` unstaged ↔ staged · `esc` clear the selection, then back to the files |
| Commits | `space` check out · `enter` its files · `n` branch · `T` tag · `g` reset… · `t` revert · `C` / `V` copy / paste (cherry-pick) · `ctrl-j` `ctrl-k` (`⌥↓` `⌥↑`) move · `s` squash · `f` fixup · `d` drop · `r` reword · `A` amend · `y` copy… |
| Branches | `space` check out · `enter` its commits · `n` new · `d` delete… · `M` merge… · `r` rebase onto · `f` fast-forward · `u` upstream · `R` rename · `T` tag · `P` push the tag · `g` reset to… · `-` previous branch · `c` by name · `y` copy |
| Stash | `space` apply · `g` pop · `d` drop… · `n` branch from it · `r` rename · `enter` its diff · `h` `l` folders, as in Files · `esc` close |

With the mouse: `⌘`/shift-click a commit to compare it with the selection; right-click (or ctrl-click) for a
context menu. Not here (yet): bisect and custom patches; `q` doesn't quit (it's a window: `⌘Q`).

## Settings

`~/Library/Application Support/wgitpui/config` on macOS (`$XDG_CONFIG_HOME/wgitpui/config` or
`~/.config/wgitpui/config` on Linux, `%APPDATA%\wgitpui\config` on Windows), next to the saved session.
`key = value` lines, `#` for comments; read again whenever the window gets focus.

| Setting | Default | |
|---|---|---|
| `key_bar` | `true` | the key bar along the bottom |

## Install (macOS)

With [Homebrew](https://brew.sh):

```sh
brew install --cask GregorLohaus/tap/wgitpui
```

This puts **Wgitpui** in Applications and the `wgitpui` command on your PATH; `brew upgrade`
picks up new releases.

Or download `Wgitpui-<version>-macos.dmg` from the GitHub release (or the workflow artifacts), drag
**Wgitpui** into Applications, then put the command on your PATH, either with
*Install command line tool.command* from the disk image or with *Install 'wgitpui' command* in
the app's ⌘K palette.

Release builds are ad-hoc signed, not notarized. The cask clears the quarantine flag, so a Homebrew
install opens right away. A downloaded app has to be allowed once: open it, then choose
*Open Anyway* in System Settings → Privacy & Security.

## Command line

```sh
cd ~/code/project && wgitpui          # opens this repository, returns your prompt
wgitpui ~/code/a ~/code/b             # each path becomes a tab
wgitpui --foreground .                # stay attached (logs, debugging)
```

From a terminal, `wgitpui` hands the repository to the running app (it opens as a tab and the
window comes forward) through a per-user socket. If the app isn't running, it starts it in the
background: via LaunchServices when installed as `Wgitpui.app`, otherwise as a detached process
in its own session. Either way the shell is usable immediately.

## SSH and your shell environment

The app isn't sandboxed: git and ssh read `~/.ssh` (config, keys, known_hosts) just as they do in a
terminal. The environment is what can differ. Started with `wgitpui`, the app inherits your
terminal's; started from the Dock, Finder or Spotlight, it would get launchd's: a bare PATH and
macOS's own SSH agent. So in that case it first takes the environment of your login shell, and if
that still leaves ssh with an empty macOS agent, it uses the `SSH_AUTH_SOCK` your fish, zsh or bash
profile sets (Bitwarden, 1Password, Secretive, gpg-agent…). An `IdentityAgent` in `~/.ssh/config`
beats both, in the app and in your terminal.

ssh can't ask you anything from inside the app: keys have to come from an agent, the macOS keychain
(`UseKeychain yes`) or have no passphrase, and a new host needs one connection from a terminal to
get into known_hosts.

## Building the app bundle

```sh
scripts/bundle-macos.sh               # dist/Wgitpui.app, .dmg, .zip for this Mac's architecture
scripts/bundle-macos.sh --universal   # arm64 + x86_64
```

The icon is `assets/icon.svg`; the script renders it into `AppIcon.icns` (with `rsvg-convert`
if installed, otherwise macOS Quick Look). `.github/workflows/macos.yml` runs the tests and the
universal bundle on every push, and publishes the `.dmg` / `.zip` to a GitHub release for `v*` tags.
It then points the Homebrew cask at that release: `scripts/homebrew-cask.sh` writes
`Casks/wgitpui.rb`, which the workflow pushes to `GregorLohaus/homebrew-tap`. That step needs a
`HOMEBREW_TAP_TOKEN` repository secret, a fine-grained token with *Contents: Read and write* on
the tap repository only; without it the release still goes out and the cask stays where it was.

## Run

```sh
cargo run --release -- ~/code/some-repo [~/code/other-repo ...]
cargo test

# Headless render to PNG (no window), handy for UI work and CI:
cargo run -- --screenshot shot.png --size 1440x900 --scale 2 [--commit N] [--files] [--open PATH] [--palette QUERY] \
    [--compare A,B] [--split|--unified] [--wide] [--menu BRANCH] ~/code/some-repo
```
