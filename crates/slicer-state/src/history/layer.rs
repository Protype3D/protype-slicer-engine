use super::files::missing_new_meshes;
use super::tag::current_tag;
use super::tree::Move;
use super::{describe, forward_events, inverse_events, UndoEntry};
use crate::{Inner, StateLayer};
use slicer_types::{EventEnvelope, HistoryCause, HistoryChanged, HistoryListView, HistoryNodeView};

#[derive(Debug, PartialEq, Eq)]
pub enum GotoError {
    UnknownNode(u64),
    FilesMissing(Vec<String>),
}
#[derive(Debug)]
pub struct GotoOutcome {
    pub applied: bool,
    pub head: u64,
    pub previous_head: u64,
    pub last: Option<EventEnvelope>,
}

impl StateLayer {
    pub(crate) fn finish_step(
        &self,
        inner: &mut Inner,
        entries: Vec<UndoEntry>,
        reset: bool,
        events: Vec<EventEnvelope>,
    ) {
        let previous = inner.history.head();
        let now = chrono::Utc::now().timestamp_millis();
        let cause = if reset {
            inner.history.reset(now);
            Some(HistoryCause::Reset)
        } else if let Some(entry) = UndoEntry::step(entries) {
            let meta = describe(&entry, &inner.project, current_tag(), now);
            inner
                .history
                .record(forward_events(&entry), inverse_events(&entry), meta);
            Some(HistoryCause::Record)
        } else {
            None
        };
        self.publish(inner, &events, previous, cause);
    }

    pub(crate) fn notify_history(&self, inner: &Inner, previous_head: u64, cause: HistoryCause) {
        let tree = &inner.history;
        let head = tree.head();
        let _ = self.history_tx.send(HistoryChanged {
            head,
            previous_head,
            pruned: tree.pruned,
            event_id: crate::last_event_id(inner).0,
            can_undo: tree.undo_target().is_some(),
            can_redo: tree.redo_target().is_some(),
            by: current_tag().author.unwrap_or_default(),
            cause,
            added: (cause == HistoryCause::Record).then(|| tree.view(head, false)),
        });
    }

    pub fn history_view(
        &self,
        filter: Option<&str>,
        limit: usize,
        before: Option<u64>,
    ) -> HistoryListView {
        self.inner.lock().history.list(filter, limit, before)
    }

    #[allow(clippy::type_complexity)]
    pub fn history_node(
        &self,
        id: u64,
    ) -> Option<(HistoryNodeView, Vec<(String, serde_json::Value)>)> {
        let inner = self.inner.lock();
        let tree = &inner.history;
        let node = tree.nodes.get(&id)?;
        let ahead = id > tree.head() && tree.current_path().contains(&id);
        Some((
            tree.view(id, ahead),
            node.recipes()
                .map(|(id, r)| (id.to_owned(), r.clone()))
                .collect(),
        ))
    }

    pub fn goto(&self, id: u64) -> Result<GotoOutcome, GotoError> {
        self.navigate(&mut self.inner.lock(), id, true)
    }
    fn navigate(
        &self,
        inner: &mut Inner,
        id: u64,
        check_files: bool,
    ) -> Result<GotoOutcome, GotoError> {
        let tree = &inner.history;
        let previous_head = tree.head();
        let path = tree
            .path(previous_head, id)
            .ok_or(GotoError::UnknownNode(id))?;
        if path.is_empty() {
            return Ok(GotoOutcome {
                applied: false,
                head: id,
                previous_head,
                last: None,
            });
        }
        let events: Vec<_> = path
            .iter()
            .flat_map(|step| match step {
                Move::Up(n) => &tree.nodes[n].inverse,
                Move::Down(n) => &tree.nodes[n].forward,
            })
            .cloned()
            .collect();
        if check_files {
            let missing = missing_new_meshes(&inner.project, &events);
            if !missing.is_empty() {
                return Err(GotoError::FilesMissing(missing));
            }
        }
        let applied: Vec<_> = events
            .into_iter()
            .map(|event| self.apply_internal(inner, event))
            .collect();
        inner.history.set_head(id);
        self.publish(inner, &applied, previous_head, Some(HistoryCause::Goto));
        Ok(GotoOutcome {
            applied: true,
            head: id,
            previous_head,
            last: applied.into_iter().last(),
        })
    }

    pub fn history_status(&self) -> (slicer_types::EventId, u64, bool, bool) {
        let inner = self.inner.lock();
        (
            crate::last_event_id(&inner),
            inner.history.head(),
            inner.history.undo_target().is_some(),
            inner.history.redo_target().is_some(),
        )
    }
    pub fn undo(&self) -> Option<EventEnvelope> {
        self.undo_redo(false).0
    }
    pub fn redo(&self) -> Option<EventEnvelope> {
        self.undo_redo(true).0
    }
    /// The legacy commands return applied/availability flags even if a mesh
    /// disappeared. Explicit goto provides the structured list of missing paths.
    pub fn undo_redo(&self, redo: bool) -> (Option<EventEnvelope>, bool, bool) {
        let mut inner = self.inner.lock();
        let target = if redo {
            inner.history.redo_target()
        } else {
            inner.history.undo_target()
        };
        let last = target.and_then(|id| self.navigate(&mut inner, id, false).ok()?.last);
        (
            last,
            inner.history.undo_target().is_some(),
            inner.history.redo_target().is_some(),
        )
    }
}
