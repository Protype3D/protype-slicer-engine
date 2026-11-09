use crate::autosave::AutosaveSink;
use crate::history::{
    classify, forward_events, inverse_events, push_capped, HistoryEffect, UndoEntry,
};
use crate::project::ProjectState;
use parking_lot::Mutex;
use slicer_types::{EventEnvelope, EventId, ProjectEvent};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::broadcast;

pub mod autosave;
mod history;
pub mod host_identity;
pub mod lockfile;
pub mod paths;
pub mod persist;
pub mod process_owner;
pub mod project;
pub mod reducer;

pub use paths::{app_data_dir, object_cache_dir};
pub use project::ObjectRecord;

/// The event log keeps this many newest events. `state.events_since` serves a
/// reconnecting client what it missed; the rest of a longer gap is lost (the
/// window warns in the console and delivers what is left). The
/// `ProjectReplaced` that opened the log is never dropped: a client that
/// missed the swap would read the rest as its old project's continuation.
/// Without a cap the log kept every `SliceProgress` and paint mask until the
/// next project swap.
pub const MAX_LOG_EVENTS: usize = 10_000;

fn push_log(log: &mut VecDeque<EventEnvelope>, envelope: EventEnvelope) {
    log.push_back(envelope);
    while log.len() > MAX_LOG_EVENTS {
        let swap_opens_the_log = matches!(
            log.front().map(|e| &e.event),
            Some(ProjectEvent::ProjectReplaced { .. })
        );
        if swap_opens_the_log {
            log.remove(1);
        } else {
            log.pop_front();
        }
    }
}

#[derive(Clone)]
pub struct StateLayer {
    inner: Arc<Mutex<Inner>>,
    event_tx: broadcast::Sender<EventEnvelope>,
    autosave: Option<Arc<AutosaveSink>>,
    autosave_unavailable_reason: Option<String>,
}

struct Inner {
    next_event_id: u64,
    log: VecDeque<EventEnvelope>,
    project: ProjectState,
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    /// Path of the currently-open project file (the one we hold the lock
    /// for). `None` until project.save / project.open is called.
    current_path: Option<std::path::PathBuf>,
}

/// The id of the last event applied, 0 before the first.
fn last_event_id(inner: &Inner) -> EventId {
    EventId(inner.next_event_id.saturating_sub(1))
}

impl StateLayer {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                next_event_id: 1,
                log: VecDeque::new(),
                project: ProjectState::default(),
                undo: Vec::new(),
                redo: Vec::new(),
                current_path: None,
            })),
            event_tx,
            autosave: None,
            autosave_unavailable_reason: None,
        }
    }

    pub fn current_path(&self) -> Option<std::path::PathBuf> {
        self.inner.lock().current_path.clone()
    }

    pub fn set_current_path(&self, path: Option<std::path::PathBuf>) {
        self.inner.lock().current_path = path;
    }

    /// Attach a session-scoped autosave sink. After this call, every
    /// applied event is appended to the session's events.log on disk.
    /// Should be called once at daemon startup, before any events are
    /// applied; otherwise the recovered project would be missing the
    /// pre-attachment events.
    pub fn with_autosave(mut self, sink: AutosaveSink) -> Self {
        self.autosave = Some(Arc::new(sink));
        self
    }

    pub fn with_autosave_failure(mut self, reason: String) -> Self {
        self.autosave_unavailable_reason = Some(reason);
        self
    }

    pub fn autosave_status(&self) -> autosave::AutosaveStatus {
        self.autosave
            .as_ref()
            .map(|sink| sink.status())
            .unwrap_or(autosave::AutosaveStatus {
                working: false,
                reason: Some(
                    self.autosave_unavailable_reason
                        .clone()
                        .unwrap_or_else(|| "autosave is not attached".into()),
                ),
                path: None,
            })
    }

    pub fn autosave(&self) -> Option<Arc<AutosaveSink>> {
        self.autosave.clone()
    }

    /// Apply a user-originated event. `classify` decides what it does to the
    /// history: an undoable scene change snapshots the prior state onto the
    /// undo stack and clears redo — adding and removing an object are such
    /// steps; an event that doesn't touch the scene (slicing, preset
    /// lifecycle) leaves the history alone; an event that replaces the scene
    /// wholesale (project open/create/replace) clears both stacks, since
    /// neither direction would still be reachable.
    pub fn apply(&self, event: ProjectEvent) -> EventEnvelope {
        let mut inner = self.inner.lock();
        // NOTE: classified against the project state as it is *before*
        // `apply_internal` runs the reducer — that's what lets the `Record`
        // arms read the "old" side of each change straight off the project.
        match classify(&inner.project, &event) {
            HistoryEffect::Record(entry) => {
                push_capped(&mut inner.undo, entry);
                inner.redo.clear();
            }
            HistoryEffect::Neutral => {}
            HistoryEffect::Reset => {
                inner.undo.clear();
                inner.redo.clear();
            }
        }
        self.apply_internal(&mut inner, event)
    }

    /// Run `f` as one user action: every event it applies is broadcast as
    /// usual, but together they make a single undo step. An event that resets
    /// history anywhere in the transaction resets it for the whole step.
    ///
    /// `f` runs with the state lock held and must work through `txn` alone:
    /// any `StateLayer` call that reads or writes state, made inside it on this
    /// layer or on a clone, deadlocks. See [`Txn`].
    pub fn transaction<R>(&self, f: impl FnOnce(&mut Txn<'_>) -> R) -> R {
        let mut guard = self.inner.lock();
        let mut txn = Txn {
            layer: self,
            // The field is `&mut Inner`; the guard deref-coerces to it.
            inner: &mut guard,
            entries: Vec::new(),
            reset: false,
        };
        let out = f(&mut txn);
        let Txn { entries, reset, .. } = txn;
        if reset {
            guard.undo.clear();
            guard.redo.clear();
        } else if let Some(step) = UndoEntry::step(entries) {
            push_capped(&mut guard.undo, step);
            guard.redo.clear();
        }
        out
    }

    /// Apply an event without touching the undo/redo stacks. Used when
    /// processing an undo or redo so we don't push the inverse onto its own
    /// stack again.
    fn apply_silent(&self, inner: &mut Inner, event: ProjectEvent) -> EventEnvelope {
        self.apply_internal(inner, event)
    }

    /// Apply the events of one undo or redo step, in order and without
    /// touching the stacks. Returns the envelope of the last one, or `None`
    /// for an empty list.
    fn apply_all(&self, inner: &mut Inner, events: Vec<ProjectEvent>) -> Option<EventEnvelope> {
        let mut last = None;
        for event in events {
            last = Some(self.apply_silent(inner, event));
        }
        last
    }

    fn apply_internal(&self, inner: &mut Inner, event: ProjectEvent) -> EventEnvelope {
        reducer::reduce(&mut inner.project, &event);
        let event_id = EventId(inner.next_event_id);
        inner.next_event_id += 1;
        let envelope = EventEnvelope {
            event_id,
            event,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };
        push_log(&mut inner.log, envelope.clone());
        if let Some(sink) = &self.autosave {
            sink.record(&envelope, &inner.project);
        }
        let _ = self.event_tx.send(envelope.clone());
        envelope
    }

    /// Roll back the last history step. Returns the envelope of the last
    /// event that step applied, or `None` if there's nothing to undo.
    pub fn undo(&self) -> Option<EventEnvelope> {
        let mut inner = self.inner.lock();
        let entry = inner.undo.pop()?;
        let events = inverse_events(&entry);
        push_capped(&mut inner.redo, entry);
        self.apply_all(&mut inner, events)
    }

    /// Re-apply a previously undone history step.
    pub fn redo(&self) -> Option<EventEnvelope> {
        let mut inner = self.inner.lock();
        let entry = inner.redo.pop()?;
        let events = forward_events(&entry);
        push_capped(&mut inner.undo, entry);
        self.apply_all(&mut inner, events)
    }

    pub fn can_undo(&self) -> bool {
        !self.inner.lock().undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.inner.lock().redo.is_empty()
    }

    pub fn snapshot(&self) -> ProjectState {
        self.inner.lock().project.clone()
    }

    /// Write the project to `path` and make it the autosave's base: a crash
    /// from here restores it with the edits after it, and a session with no
    /// edits after it is not offered for recovery, since the file holds it.
    ///
    /// The file is written outside the state lock, so a slow disk does not
    /// stall every other request. An edit that lands meanwhile is not in the
    /// file; the base then stays where it was, and the log, which holds that
    /// edit, still restores the window.
    pub fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let (project, saved_at) = {
            let inner = self.inner.lock();
            (inner.project.clone(), last_event_id(&inner))
        };
        persist::save(path, &project)?;
        let inner = self.inner.lock();
        if last_event_id(&inner) == saved_at {
            self.anchor_autosave(&inner, true);
        }
        Ok(())
    }

    /// "Don't Save" from the window: drop what the live autosave session
    /// holds beyond a project file, so the next launch offers nothing for
    /// it. Under the state lock, like every journal write. `None` when no
    /// autosave is attached; an error means the refused work is still on
    /// disk and the window must not report the refusal as done.
    pub fn discard_autosave(&self) -> Option<anyhow::Result<()>> {
        let _inner = self.inner.lock();
        self.autosave.as_ref().map(|sink| sink.discard_unsaved())
    }

    /// Make the project as it is now the base the autosave restores;
    /// `on_disk` says a project file holds it. Called under the state lock,
    /// which every event is logged under, so the base and the cut log agree.
    /// A failure is logged, not returned: like any autosave write it must not
    /// fail the swap or save it follows.
    fn anchor_autosave(&self, inner: &Inner, on_disk: bool) -> bool {
        let Some(sink) = &self.autosave else {
            return false;
        };
        if let Err(e) = sink.checkpoint(&inner.project, last_event_id(inner), on_disk) {
            tracing::warn!(?e, "autosave checkpoint failed");
            return false;
        }
        true
    }

    /// Replace the entire project state atomically. Drops the event log
    /// and undo/redo history (the loaded project is the new "origin") and
    /// broadcasts a single `ProjectReplaced` event so subscribers re-fetch
    /// derived data. The numbering goes on: `ProjectReplaced` takes the next
    /// id, so every event of the replaced project — including one still on
    /// its way to a socket — is numbered below it, and a client that keeps
    /// only events past its last id never takes a stale one for new.
    ///
    /// `path` is the file the state was opened from, `None` for a recovered
    /// session. It becomes `current_path` in the same step and is named in the
    /// event, so no subscriber sees the new state paired with the old file.
    ///
    /// The new state becomes the autosave's base in the same step: no event
    /// carries the whole project, so a crash would otherwise restore the old
    /// project's log, or an empty plate, under the edits that follow.
    ///
    /// Object overrides are stored under their canonical keys, whichever file
    /// or autosave log they were loaded from.
    pub fn replace_state(
        &self,
        project: ProjectState,
        path: Option<std::path::PathBuf>,
    ) -> EventEnvelope {
        self.replace_state_checkpointed(project, path).0
    }

    /// Return whether the replacement has a durable autosave copy. Recovery
    /// may delete its source only when this is true, including with no edits.
    pub fn replace_state_checkpointed(
        &self,
        project: ProjectState,
        path: Option<std::path::PathBuf>,
    ) -> (EventEnvelope, bool) {
        let project = project.with_canonical_override_keys();
        let mut inner = self.inner.lock();
        inner.project = project.clone();
        inner.log.clear();
        inner.undo.clear();
        inner.redo.clear();
        let event_path = path.as_ref().map(|p| p.to_string_lossy().into_owned());
        let opened_from_file = path.is_some();
        inner.current_path = path;
        let envelope = EventEnvelope {
            event_id: EventId(inner.next_event_id),
            event: ProjectEvent::ProjectReplaced {
                meta: project.meta.clone(),
                path: event_path,
            },
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };
        inner.next_event_id += 1;
        push_log(&mut inner.log, envelope.clone());
        let durable = self.anchor_autosave(&inner, opened_from_file);
        let _ = self.event_tx.send(envelope.clone());
        (envelope, durable)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.event_tx.subscribe()
    }

    /// Subscribe to the broadcast and read the last event id as one cut:
    /// every event numbered above the returned id reaches the receiver, none
    /// at or below it does. `apply_internal` and `replace_state` log and send
    /// under this same lock, so nothing can land between the two.
    pub fn subscribe_from_now(&self) -> (broadcast::Receiver<EventEnvelope>, EventId) {
        let inner = self.inner.lock();
        let receiver = self.event_tx.subscribe();
        (receiver, last_event_id(&inner))
    }

    pub fn events_since(&self, since: EventId) -> Vec<EventEnvelope> {
        self.inner
            .lock()
            .log
            .iter()
            .filter(|e| e.event_id > since)
            .cloned()
            .collect()
    }

    pub fn current_event_id(&self) -> EventId {
        last_event_id(&self.inner.lock())
    }
}

impl Default for StateLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Several events applied as one user action — one step of history.
///
/// Holds the state lock for the whole closure, so no other RPC can slip an
/// event in between. That lock is a non-reentrant `parking_lot::Mutex`, so
/// inside the closure everything goes through `txn`: calling a [`StateLayer`]
/// method that reads or writes state — `apply`, `undo`, `snapshot`,
/// `can_undo`, a nested `transaction`, … — on the layer or on a clone of it
/// (a clone shares the lock) deadlocks. A helper that takes the layer as an
/// argument is the same trap.
///
/// There is no rollback: a closure checks everything it can fail on
/// *before* it applies its first event.
pub struct Txn<'a> {
    layer: &'a StateLayer,
    inner: &'a mut Inner,
    entries: Vec<UndoEntry>,
    reset: bool,
}

impl Txn<'_> {
    /// The project as the events applied so far have left it.
    pub fn project(&self) -> &ProjectState {
        &self.inner.project
    }

    /// Apply one event. It is broadcast and autosaved like any other; its
    /// undo entry joins the transaction's single step instead of becoming
    /// its own.
    pub fn apply(&mut self, event: ProjectEvent) -> EventEnvelope {
        match classify(&self.inner.project, &event) {
            HistoryEffect::Record(entry) => self.entries.push(entry),
            HistoryEffect::Neutral => {}
            HistoryEffect::Reset => self.reset = true,
        }
        self.layer.apply_internal(&mut *self.inner, event)
    }
}
