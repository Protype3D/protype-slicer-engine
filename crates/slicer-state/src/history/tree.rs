use serde::{Deserialize, Serialize};
use slicer_types::{ForkView, ProjectEvent};
pub use slicer_types::{NodeMeta, ObjectRef};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::Arc;

/// Count compact JSON without allocating an encoded copy (notably for paint masks).
pub(super) fn json_bytes<T: Serialize + ?Sized>(value: &T) -> usize {
    struct Count(usize);
    impl Write for Count {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).expect("history is JSON serializable");
    count.0
}

pub type NodeId = u64;
pub const MAX_HISTORY_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_NODES: usize = 2000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub forward: Vec<ProjectEvent>,
    pub inverse: Vec<ProjectEvent>,
    pub meta: NodeMeta,
    #[serde(skip)]
    pub bytes: usize,
}

#[derive(Debug, Clone)]
pub struct HistoryTree {
    pub(crate) nodes: BTreeMap<NodeId, Arc<HistoryNode>>,
    pub(crate) redo: BTreeMap<NodeId, NodeId>,
    pub(crate) bytes: usize,
    pub(crate) pruned: bool,
    max_nodes: usize,
    max_bytes: usize,
    pub(crate) root: NodeId,
    pub(crate) head: NodeId,
    pub(crate) next_id: NodeId,
    pub(crate) pruned_until_ms: Option<i64>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Move {
    Up(NodeId),
    Down(NodeId),
}

impl HistoryTree {
    pub fn new(now_ms: i64) -> Self {
        Self::new_with_limits(now_ms, MAX_NODES, MAX_HISTORY_BYTES)
    }
    pub fn with_limits(max_nodes: usize, max_bytes: usize) -> Self {
        Self::new_with_limits(0, max_nodes, max_bytes)
    }
    pub(crate) fn new_with_limits(now_ms: i64, max_nodes: usize, max_bytes: usize) -> Self {
        let root = HistoryNode {
            id: 0,
            parent: None,
            forward: vec![],
            inverse: vec![],
            meta: Self::root_meta(now_ms),
            bytes: 0,
        };
        Self {
            nodes: BTreeMap::from([(0, Arc::new(root))]),
            redo: BTreeMap::new(),
            bytes: 0,
            pruned: false,
            max_nodes,
            max_bytes,
            root: 0,
            head: 0,
            next_id: 1,
            pruned_until_ms: None,
        }
    }
    fn root_meta(at_ms: i64) -> NodeMeta {
        NodeMeta {
            at_ms,
            kind: "root".into(),
            kinds: vec!["root".into()],
            ..Default::default()
        }
    }
    pub fn head(&self) -> NodeId {
        self.head
    }
    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn record(
        &mut self,
        forward: Vec<ProjectEvent>,
        inverse: Vec<ProjectEvent>,
        meta: NodeMeta,
    ) -> NodeId {
        let id = self.next_id;
        let node = HistoryNode {
            id,
            parent: Some(self.head),
            bytes: json_bytes(&forward) + json_bytes(&inverse),
            forward,
            inverse,
            meta,
        };
        self.append(node);
        self.pruned = self.enforce_limits(true);
        id
    }

    /// Also used for the payload-free topology during journal recovery.
    pub(crate) fn append(&mut self, node: HistoryNode) {
        self.next_id += 1;
        self.bytes += node.bytes;
        self.redo.insert(self.head, node.id);
        self.head = node.id;
        self.nodes.insert(node.id, Arc::new(node));
    }

    pub fn path(&self, mut from: NodeId, mut to: NodeId) -> Option<Vec<Move>> {
        self.nodes.get(&from)?;
        self.nodes.get(&to)?;
        let mut up = Vec::new();
        let mut down = Vec::new();
        // Parents always have smaller ids. Advancing the newer id cannot skip the LCA.
        while from != to {
            if from > to {
                up.push(Move::Up(from));
                from = self.nodes[&from].parent?;
            } else {
                down.push(Move::Down(to));
                to = self.nodes[&to].parent?;
            }
        }
        up.extend(down.into_iter().rev());
        Some(up)
    }

    pub fn set_head(&mut self, id: NodeId) {
        if let Some(path) = self.path(self.head, id) {
            for step in path {
                let (Move::Up(child) | Move::Down(child)) = step;
                let parent = self.nodes[&child].parent.unwrap();
                self.redo.insert(parent, child);
            }
            self.head = id;
            self.pruned = false;
        }
    }

    pub fn undo_target(&self) -> Option<NodeId> {
        let node = &self.nodes[&self.head];
        let mut parent = node.parent?;
        if let Some(group) = &node.meta.group {
            if self
                .redo
                .get(&self.head)
                .copied()
                .is_some_and(|id| self.nodes[&id].meta.group.as_ref() == Some(group))
            {
                return Some(parent);
            }
            while self.nodes[&parent].meta.group.as_ref() == Some(group) {
                parent = self.nodes[&parent].parent?;
            }
        }
        Some(parent)
    }

    pub fn redo_target(&self) -> Option<NodeId> {
        let mut child = *self.redo.get(&self.head)?;
        if let Some(group) = &self.nodes[&child].meta.group {
            while let Some(next) = self
                .redo
                .get(&child)
                .copied()
                .filter(|id| self.nodes[id].meta.group.as_ref() == Some(group))
            {
                child = next;
            }
        }
        Some(child)
    }

    pub fn current_path(&self) -> Vec<NodeId> {
        std::iter::successors(Some(self.root), |id| self.redo.get(id).copied()).collect()
    }

    pub fn forks(&self, id: NodeId) -> Vec<ForkView> {
        if !self.redo.contains_key(&id) {
            return Vec::new();
        }
        self.nodes
            .values()
            .filter(|node| node.parent == Some(id) && Some(&node.id) != self.redo.get(&id))
            .map(|child| {
                let mut tip = child;
                let mut steps = 1;
                while let Some(&next) = self.redo.get(&tip.id) {
                    tip = &self.nodes[&next];
                    steps += 1;
                }
                ForkView {
                    child: child.id,
                    tip: tip.id,
                    steps,
                    tip_kind: tip.meta.kind.clone(),
                    tip_label: tip.meta.label.clone(),
                    tip_objects: tip.meta.objects.clone(),
                }
            })
            .collect()
    }

    pub fn touching(&self, object_id: &str) -> Vec<NodeId> {
        let path = self.current_path();
        let mut result = vec![];
        let mut lineage = BTreeSet::from([object_id]);
        let mut end = path.len();
        while end > 0 {
            let mut start = end - 1;
            let group = &self.nodes[&path[start]].meta.group;
            while group.is_some() && start > 0 && &self.nodes[&path[start - 1]].meta.group == group
            {
                start -= 1;
            }
            let unit = &path[start..end];
            let added = unit.iter().any(|id| {
                self.nodes[id]
                    .meta
                    .added
                    .iter()
                    .any(|x| lineage.contains(x.as_str()))
            });
            if added
                || unit.iter().any(|id| {
                    self.nodes[id]
                        .meta
                        .objects
                        .iter()
                        .any(|o| lineage.contains(o.id.as_str()))
                })
            {
                result.extend(unit.iter().rev().copied());
                if added {
                    for id in unit {
                        lineage.extend(self.nodes[id].meta.removed.iter().map(String::as_str));
                    }
                }
            }
            end = start;
        }
        result.reverse();
        result
    }

    pub(crate) fn enforce_limits(&mut self, keep_huge_step: bool) -> bool {
        self.prune_to(self.max_nodes, self.max_bytes, keep_huge_step)
    }

    pub(crate) fn reset(&mut self, now: i64) {
        *self = Self::new_with_limits(now, self.max_nodes, self.max_bytes);
    }

    pub(crate) fn prune_to(
        &mut self,
        max_nodes: usize,
        max_bytes: usize,
        keep_huge_step: bool,
    ) -> bool {
        let max_nodes = max_nodes.max(1);
        if self.nodes.len() <= max_nodes && self.bytes <= max_bytes {
            return false;
        }
        let before = self.nodes.len();
        // Index once: opening a long chain can promote hundreds of roots.
        let mut children: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for node in self.nodes.values() {
            if let Some(parent) = node.parent {
                children.entry(parent).or_default().push(node.id);
            }
        }
        while self.nodes.len() > max_nodes || self.bytes > max_bytes {
            if keep_huge_step
                && self.nodes[&self.head].bytes > max_bytes
                && self.nodes[&self.head].parent == Some(self.root)
            {
                self.nodes
                    .retain(|id, _| *id == self.root || *id == self.head);
                self.redo = BTreeMap::from([(self.root, self.head)]);
                self.bytes = self.nodes[&self.head].bytes;
                break;
            }
            let root = self.root;
            let mut branches = children.remove(&root).expect("nonempty history to prune");
            if self.head == root {
                // At the root there is no ancestor to merge. Remove the branch
                // with the oldest preferred tip, preserving the old tie break.
                let oldest = *branches
                    .iter()
                    .min_by_key(|id| {
                        let tip = std::iter::successors(Some(**id), |n| self.redo.get(n).copied())
                            .last()
                            .unwrap();
                        (self.nodes[&tip].meta.at_ms, tip)
                    })
                    .unwrap();
                branches.retain(|id| *id != oldest);
                if self.redo.get(&root) == Some(&oldest) {
                    if let Some(&next) = branches.last() {
                        self.redo.insert(root, next);
                    } else {
                        self.redo.remove(&root);
                    }
                }
                children.insert(root, branches);
                self.drop_branches(vec![oldest], &mut children);
            } else {
                let next = self.redo.remove(&root).unwrap();
                self.nodes.remove(&root);
                branches.retain(|id| *id != next);
                self.drop_branches(branches, &mut children);
                self.root = next;
                let old = &self.nodes[&next];
                self.bytes -= old.bytes;
                self.nodes.insert(
                    next,
                    Arc::new(HistoryNode {
                        id: next,
                        parent: None,
                        forward: vec![],
                        inverse: vec![],
                        meta: Self::root_meta(old.meta.at_ms),
                        bytes: 0,
                    }),
                );
            }
        }
        let pruned = self.nodes.len() < before;
        if pruned {
            self.pruned_until_ms = Some(self.nodes[&self.root].meta.at_ms);
        }
        pruned
    }

    fn drop_branches(
        &mut self,
        mut pending: Vec<NodeId>,
        children: &mut BTreeMap<NodeId, Vec<NodeId>>,
    ) {
        while let Some(id) = pending.pop() {
            self.bytes -= self.nodes.remove(&id).unwrap().bytes;
            self.redo.remove(&id);
            if let Some(branches) = children.remove(&id) {
                pending.extend(branches);
            }
        }
    }
}
