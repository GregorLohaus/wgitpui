//! Commit graph lane layout.
//!
//! Commits arrive in date order. Each "lane" is a column that is waiting for a specific commit
//! (the next ancestor along that line). For every row we record the node's lane and the edge
//! segments that run from this row's center down to the next row's center.

use crate::git::Commit;
use std::collections::HashMap;

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
        *self.ids.entry(hash.to_string()).or_insert(n)
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
        for (k, p) in c.parents.iter().enumerate() {
            let pid = self.intern(p);
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
    }

    /// Flushes edges leaving the last row (call once the stream is complete).
    pub fn finish(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if let Some(last) = self.rows.last_mut() {
            last.segs = pending.into_iter().map(|(j, from, col)| Seg { from: from as u16, to: j as u16, color: col }).collect();
        }
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
}
