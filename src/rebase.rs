//! Plans an interactive rebase from a drag-and-drop gesture on the commit graph.
//!
//! The "chain" is the linear first-parent history from HEAD down (newest first, no merges).
//! A plan rewrites only as deep as the gesture reaches, onto that commit's parent.

#[derive(Clone, Copy, Debug)]
pub struct ChainItem<'a> {
    pub hash: &'a str,
    pub subject: &'a str,
    /// None for a root commit (rebase with `--root`).
    pub parent: Option<&'a str>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RebaseOp {
    /// Place the dragged commit directly above (newer than) the target.
    Above,
    /// Place it directly below (older than) the target.
    Below,
    /// Fold it into the target, keeping the target's message (`fixup`).
    Squash,
    /// Remove it from history.
    Drop,
}

#[derive(Debug)]
pub struct Plan {
    /// Commit to rebase onto; None = `--root`.
    pub base: Option<String>,
    /// Contents of git-rebase-todo, oldest first.
    pub todo: String,
    pub label: String,
}

fn quote(s: &str) -> String {
    let s: String = s.chars().take(40).collect();
    format!("“{s}”")
}

pub fn plan(chain: &[ChainItem], src: usize, target: usize, op: RebaseOp) -> Option<Plan> {
    let n = chain.len();
    if src >= n {
        return None;
    }
    let mut order: Vec<(usize, &str)> = (0..n).map(|i| (i, "pick")).collect();
    let deepest = if op == RebaseOp::Drop {
        order.remove(src);
        src
    } else {
        if target == src || target >= n {
            return None;
        }
        let item = order.remove(src);
        let t = order.iter().position(|o| o.0 == target)?;
        match op {
            RebaseOp::Above => order.insert(t, item),
            RebaseOp::Below => order.insert(t + 1, item),
            RebaseOp::Squash => order.insert(t, (item.0, "fixup")),
            RebaseOp::Drop => unreachable!(),
        }
        src.max(target)
    };
    if matches!(op, RebaseOp::Above | RebaseOp::Below) && order.iter().map(|o| o.0).eq(0..n) {
        return None; // dropped back where it was
    }
    let mut todo: String = order
        .iter()
        .filter(|o| o.0 <= deepest)
        .rev()
        .map(|&(i, verb)| format!("{verb} {} {}\n", chain[i].hash, chain[i].subject))
        .collect();
    if op == RebaseOp::Drop {
        todo.push_str(&format!("drop {} {}\n", chain[src].hash, chain[src].subject));
    }
    let label = match op {
        RebaseOp::Above | RebaseOp::Below => format!("move {}", quote(chain[src].subject)),
        RebaseOp::Squash => format!("squash {} into {}", quote(chain[src].subject), quote(chain[target].subject)),
        RebaseOp::Drop => format!("drop {}", quote(chain[src].subject)),
    };
    Some(Plan { base: chain[deepest].parent.map(String::from), todo, label })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{run, Repo};

    fn chain() -> Vec<ChainItem<'static>> {
        vec![
            ChainItem { hash: "a", subject: "A", parent: Some("b") },
            ChainItem { hash: "b", subject: "B", parent: Some("c") },
            ChainItem { hash: "c", subject: "C", parent: Some("r") },
        ]
    }

    #[test]
    fn move_below_rewrites_down_to_target() {
        let p = plan(&chain(), 0, 2, RebaseOp::Below).unwrap();
        assert_eq!(p.base.as_deref(), Some("r"));
        assert_eq!(p.todo, "pick a A\npick c C\npick b B\n");
    }

    #[test]
    fn move_only_touches_needed_depth() {
        let p = plan(&chain(), 1, 0, RebaseOp::Above).unwrap();
        assert_eq!(p.base.as_deref(), Some("c"));
        assert_eq!(p.todo, "pick a A\npick b B\n");
    }

    #[test]
    fn squash_places_fixup_after_target() {
        let p = plan(&chain(), 0, 2, RebaseOp::Squash).unwrap();
        assert_eq!(p.todo, "pick c C\nfixup a A\npick b B\n");
        let p = plan(&chain(), 2, 0, RebaseOp::Squash).unwrap();
        assert_eq!(p.todo, "pick b B\npick a A\nfixup c C\n");
    }

    #[test]
    fn noop_moves_are_rejected() {
        assert!(plan(&chain(), 1, 0, RebaseOp::Below).is_none());
        assert!(plan(&chain(), 1, 1, RebaseOp::Above).is_none());
    }

    #[test]
    fn real_rebase_reorders_and_squashes() {
        let dir = std::env::temp_dir().join(format!("wgitpui-rebase-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| run(&dir, args, None).unwrap();
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@example.com"]);
        g(&["config", "user.name", "T"]);
        for name in ["one", "two", "three", "four"] {
            std::fs::write(dir.join(name), name).unwrap();
            g(&["add", "."]);
            g(&["commit", "-qm", name]);
        }
        let repo = Repo::open(&dir).unwrap();
        let log = repo.log().unwrap(); // newest first: four three two one
        let items: Vec<ChainItem> = log
            .iter()
            .map(|c| ChainItem { hash: &c.hash, subject: &c.subject, parent: c.parents.first().map(String::as_str) })
            .collect();
        // Move "four" below "two".
        let p = plan(&items, 0, 2, RebaseOp::Below).unwrap();
        repo.rebase_with_todo(p.base.as_deref(), &p.todo).unwrap();
        let subjects = || repo.log().unwrap().iter().map(|c| c.subject.clone()).collect::<Vec<_>>();
        assert_eq!(subjects(), ["three", "two", "four", "one"]);

        // Squash the newest ("three") into the root commit ("one") — needs --root.
        let log = repo.log().unwrap();
        let items: Vec<ChainItem> = log
            .iter()
            .map(|c| ChainItem { hash: &c.hash, subject: &c.subject, parent: c.parents.first().map(String::as_str) })
            .collect();
        let p = plan(&items, 0, 3, RebaseOp::Squash).unwrap();
        assert!(p.base.is_none());
        repo.rebase_with_todo(p.base.as_deref(), &p.todo).unwrap();
        assert_eq!(subjects(), ["two", "four", "one"]);
        assert!(repo.git(&["show", "--stat", "--format=", "HEAD~2"]).unwrap().contains("three"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
