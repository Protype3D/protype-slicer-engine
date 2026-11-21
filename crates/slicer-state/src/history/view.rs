use super::tree::{HistoryNode, HistoryTree};
use serde_json::Value;
use slicer_types::{HistoryListView, HistoryNodeView, ProjectEvent};

impl HistoryNode {
    pub(super) fn recipes(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.forward.iter().filter_map(|event| match event {
            ProjectEvent::ObjectRecipeSet {
                object_id,
                recipe: Some(recipe),
            } => Some((object_id.0.as_str(), recipe)),
            _ => None,
        })
    }
}

impl HistoryTree {
    pub(super) fn view(&self, id: u64, ahead: bool) -> HistoryNodeView {
        let n = &self.nodes[&id];
        let forks = self.forks(id);
        HistoryNodeView {
            id,
            parent: n.parent,
            meta: n.meta.clone(),
            children: forks.len() as u32 + u32::from(self.redo.contains_key(&id)),
            redo_child: self.redo.get(&id).copied(),
            ahead,
            has_recipe: n.recipes().next().is_some(),
            forks,
        }
    }

    pub(super) fn list(
        &self,
        filter: Option<&str>,
        limit: usize,
        before: Option<u64>,
    ) -> HistoryListView {
        let path = filter.map_or_else(|| self.current_path(), |object| self.touching(object));
        let mut nodes: Vec<_> = path
            .into_iter()
            .rev()
            .filter(|id| before.is_none_or(|cursor| *id < cursor))
            .take(limit.min(super::tree::MAX_NODES))
            .map(|id| self.view(id, id > self.head))
            .collect();
        nodes.reverse();
        HistoryListView {
            head: self.head,
            root: self.root,
            can_undo: self.undo_target().is_some(),
            can_redo: self.redo_target().is_some(),
            pruned_until_ms: self.pruned_until_ms,
            nodes,
        }
    }
}
