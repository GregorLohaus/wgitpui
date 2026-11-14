//! Commit graph lane layout.
//!
//! Commits arrive in date order. Each "lane" is a column that is waiting for a specific commit
//! (the next ancestor along that line). For every row we record the node's lane and the edge
//! segments that run from this row's center down to the next row's center.
//!
//! A lane keeps its column until the commit it waits for arrives, and that commit is drawn in
//! it, so the edge from a child to a parent always runs from the child's lane into the parent's
//! lane on the child's row and then straight down the parent's lane.

use crate::git::Commit;
use std::collections::HashMap;

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
pub struct Seg {
    pub from: u16,
    pub to: u16,
    pub color: u16,
}

#[derive(Clone, Debug, Default)]
pub struct Row {
    pub lane: u16,
    pub color: u16,
    pub segs: Vec<Seg>,
}

#[derive(Default)]
pub struct Graph {
    pub rows: Vec<Row>,
    pub max_lanes: usize,
    /// Each row's first parent as an interned id (NONE for a root commit).
    first: Vec<u32>,
    /// (row, parent id) for every further parent: the merged-in side of each merge, in row order.
    merged: Vec<(u32, u32)>,
    /// The row each interned id was pushed at (NONE until that commit arrives).
    row_of: Vec<u32>,
    // Incremental layout state, so history can be streamed in batches.
    lanes: Vec<Option<(u32, u16)>>,
    pending: Vec<(usize, usize, u16)>,
    next_color: u16,
    ids: HashMap<String, u32>,
}

/// Lays out a complete history in one go.
#[allow(dead_code)]
pub fn build(commits: &[Commit]) -> Graph {
    let mut g = Graph::default();
    for c in commits {
        g.push(c);
    }
    g.finish();
    g
}

impl Graph {
    fn intern(&mut self, hash: &str) -> u32 {
        let n = self.ids.len() as u32;
        let id = *self.ids.entry(hash.to_string()).or_insert(n);
        if id == n {
            self.row_of.push(NONE);
        }
        id
    }

    fn free_slot(&mut self) -> usize {
        match self.lanes.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.lanes.push(None);
                self.lanes.len() - 1
            }
        }
    }

    fn new_color(&mut self) -> u16 {
        self.next_color = self.next_color.wrapping_add(1);
        self.next_color - 1
    }

    /// Appends the next commit (in display order). Parents may not have been seen yet.
    pub fn push(&mut self, c: &Commit) {
        let id = self.intern(&c.hash);
        let i = self.rows.len();
        self.row_of[id as usize] = i as u32;
        let waiting = |l: &Option<(u32, u16)>| l.is_some_and(|(t, _)| t == id);
        let (lane, color) = match self.lanes.iter().position(waiting) {
            Some(j) => (j, self.lanes[j].unwrap().1),
            None => {
                let j = self.free_slot();
                (j, self.new_color())
            }
        };

        // Close out the previous row's edges now that we know where this node sits.
        if i > 0 {
            let lanes = &self.lanes;
            self.rows[i - 1].segs = self
                .pending
                .drain(..)
                .map(|(j, from, col)| Seg {
                    from: from as u16,
                    to: if lanes[j].is_some_and(|(t, _)| t == id) { lane as u16 } else { j as u16 },
                    color: col,
                })
                .collect();
        }

        for l in self.lanes.iter_mut() {
            if waiting(l) {
                *l = None;
            }
        }
        let was_active: Vec<bool> = self.lanes.iter().map(Option::is_some).collect();

        // Route edges to parents: reuse a lane already waiting for that parent, else open one.
        let mut from_node: Vec<usize> = Vec::new();
        let mut first = NONE;
        for (k, p) in c.parents.iter().enumerate() {
            let pid = self.intern(p);
            if k == 0 {
                first = pid;
            } else {
                self.merged.push((i as u32, pid));
            }
            if let Some(j) = self.lanes.iter().position(|l| l.is_some_and(|(t, _)| t == pid)) {
                from_node.push(j);
            } else {
                let j = if k == 0 && self.lanes[lane].is_none() { lane } else { self.free_slot() };
                let col = if k == 0 { color } else { self.new_color() };
                self.lanes[j] = Some((pid, col));
                from_node.push(j);
            }
        }

        for (j, l) in self.lanes.iter().enumerate() {
            if let Some((_, col)) = l {
                if was_active.get(j).copied().unwrap_or(false) {
                    self.pending.push((j, j, *col));
                }
                if from_node.contains(&j) {
                    self.pending.push((j, lane, *col));
                }
            }
        }

        while self.lanes.last().is_some_and(Option::is_none) {
            self.lanes.pop();
        }
        self.max_lanes = self.max_lanes.max(self.lanes.len()).max(lane + 1);
        self.rows.push(Row { lane: lane as u16, color, segs: Vec::new() });
        self.first.push(first);
    }

    /// Flushes edges leaving the last row (call once the stream is complete).
    pub fn finish(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if let Some(last) = self.rows.last_mut() {
            last.segs = pending.into_iter().map(|(j, from, col)| Seg { from: from as u16, to: j as u16, color: col }).collect();
        }
    }

    fn row_of(&self, id: u32) -> Option<usize> {
        self.row_of.get(id as usize).filter(|&&r| r != NONE).map(|&r| r as usize)
    }

    /// The row of `row`'s first parent, once that commit has been loaded. Parents always come
    /// after their children (date order), so following this only ever moves down.
    pub fn first_parent(&self, row: usize) -> Option<usize> {
        self.row_of(*self.first.get(row)?).filter(|&p| p > row)
    }

    /// The first-parent history from `tip` down (what `git log --first-parent` follows): the line
    /// a branch's own commits and merges sit on. Rows ascending.
    pub fn first_parent_chain(&self, tip: usize) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut r = Some(tip).filter(|&t| t < self.rows.len());
        while let Some(i) = r {
            chain.push(i as u32);
            r = self.first_parent(i);
        }
        chain
    }

    /// Splits history into lines: walking first parents from each tip in turn, a row belongs to
    /// the first walk that reaches it; each walk stops at a row already taken. Rows no tip
    /// reaches (branches merged and then deleted) start walks of their own, top-down. Returns,
    /// per row, the row of the tip whose walk took it.
    pub fn owners(&self, tips: &[usize]) -> Vec<u32> {
        let n = self.rows.len();
        let mut owner = vec![NONE; n];
        for t in tips.iter().copied().chain(0..n) {
            let mut r = Some(t).filter(|&t| t < n);
            while let Some(i) = r.filter(|&i| owner[i] == NONE) {
                owner[i] = t as u32;
                r = self.first_parent(i);
            }
        }
        owner
    }

    /// Where commits of `own` (ascending rows) were merged into another line: (merge row, merged
    /// parent row) for every merge outside `chain` (ascending rows) whose merged-in side is one of
    /// them. In row order.
    pub fn merged_into(&self, chain: &[u32], own: &[u32]) -> Vec<(u32, u32)> {
        self.merged
            .iter()
            .filter_map(|&(m, pid)| Some((m, self.row_of(pid)? as u32)))
            .filter(|(m, p)| own.binary_search(p).is_ok() && chain.binary_search(m).is_err())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(hash: &str, parents: &[&str]) -> Commit {
        Commit {
            hash: hash.repeat(8),
            parents: parents.iter().map(|p| p.repeat(8)).collect(),
            author: String::new(),
            time: 0,
            refs: vec![],
            subject: String::new(),
        }
    }

    #[test]
    fn merge_layout() {
        // m merges b into a; both descend from r.
        let commits = vec![c("m", &["a", "b"]), c("a", &["r"]), c("b", &["r"]), c("r", &[])];
        let g = build(&commits);
        assert_eq!(g.rows[0].lane, 0);
        assert_eq!(g.rows[1].lane, 0);
        assert_eq!(g.rows[2].lane, 1);
        assert_eq!(g.rows[3].lane, 0);
        // The merge row fans out into two lanes.
        assert_eq!(g.rows[0].segs.len(), 2);
        // b's edge converges back into r's lane.
        assert!(g.rows[2].segs.iter().any(|s| s.from == 1 && s.to == 0));
        assert_eq!(g.max_lanes, 2);
    }

    /// main: r ← 1 ← 2 ← M; a feature branch 1 ← e ← f, merged at M and then deleted. Afterwards
    /// main was merged into a second branch (1 ← s) at S.
    fn history() -> Graph {
        build(&[
            c("S", &["s", "M"]), // 0
            c("M", &["2", "f"]), // 1
            c("s", &["1"]),      // 2
            c("f", &["e"]),      // 3  feature tip (deleted branch)
            c("2", &["1"]),      // 4
            c("e", &["1"]),      // 5
            c("1", &["r"]),      // 6
            c("r", &[]),         // 7
        ])
    }

    #[test]
    fn first_parent_chain_follows_the_line() {
        let g = history();
        assert_eq!(g.first_parent_chain(1), vec![1, 4, 6, 7]);
        assert_eq!(g.first_parent_chain(3), vec![3, 5, 6, 7]);
        assert_eq!(g.first_parent(7), None);
        assert!(g.first_parent_chain(99).is_empty());
    }

    #[test]
    fn owners_give_shared_history_to_earlier_tips() {
        let g = history();
        // main's tip (row 1) walks first and keeps its whole line; the sync-merge branch (row 0)
        // only gets its own two commits; the deleted feature branch becomes a line of its own.
        let o = g.owners(&[1, 0]);
        assert_eq!(o, vec![0, 1, 0, 3, 1, 3, 1, 1]);
        // Order matters: a tip walked first takes everything down its first parents.
        assert_eq!(g.owners(&[0])[6], 0);
    }

    #[test]
    fn merged_into_finds_merges_out_of_a_line() {
        let g = history();
        // The feature's own commits (3, 5) were merged into main at M (row 1).
        let chain = g.first_parent_chain(3);
        assert_eq!(g.merged_into(&chain, &chain[..2]), vec![(1, 3)]);
        // main's own line was merged into s at S (row 0); merges on main itself don't count.
        let chain = g.first_parent_chain(1);
        assert_eq!(g.merged_into(&chain, &chain), vec![(0, 1)]);
        // Nothing merged s anywhere.
        let chain = g.first_parent_chain(0);
        assert!(g.merged_into(&chain, &chain[..2]).is_empty());
    }

    #[test]
    fn parents_resolve_across_batches() {
        // A parent that streams in later is found once it arrives.
        let mut g = Graph::default();
        g.push(&c("b", &["a"]));
        assert_eq!(g.first_parent(0), None);
        g.push(&c("a", &[]));
        assert_eq!(g.first_parent(0), Some(1));
    }
}
