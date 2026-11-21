//! A history line commits its preceding scene events. A torn last step is
//! discarded as a unit. Old, unmarked envelopes remain immediately replayable.
use super::{is_journaled, ProjectState};
use crate::history::{
    file::SavedHistory,
    tree::{HistoryNode, HistoryTree, MAX_HISTORY_BYTES, MAX_NODES},
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use slicer_types::{EventEnvelope, EventId, HistoryCause, ProjectEvent};
use std::{collections::BTreeMap, io::Write, sync::Arc};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HistoryRecord<N> {
    Node(N),
    Head { head: u64 },
    Reset { at_ms: i64 },
}

#[derive(Serialize)]
struct SceneLine<'a> {
    #[serde(flatten)]
    envelope: &'a EventEnvelope,
    #[serde(skip_serializing_if = "Option::is_none")]
    step: Option<EventId>,
}
#[derive(Serialize, Deserialize)]
struct HistoryLine<N> {
    event_id: EventId,
    root_at_ms: i64,
    pruned: Option<bool>,
    history: HistoryRecord<N>,
}

pub(super) fn write(
    writer: &mut impl Write,
    events: &[EventEnvelope],
    tree: &HistoryTree,
    cause: Option<HistoryCause>,
) -> Result<()> {
    let last = events.last().expect("nonempty step");
    let root_at_ms = tree.nodes[&tree.root()].meta.at_ms;
    let record = match cause {
        Some(HistoryCause::Record) => Some(HistoryRecord::Node(tree.nodes[&tree.head()].as_ref())),
        Some(HistoryCause::Goto) => Some(HistoryRecord::Head { head: tree.head() }),
        Some(HistoryCause::Reset) => Some(HistoryRecord::Reset { at_ms: root_at_ms }),
        Some(HistoryCause::Load) | None => None,
    };
    for envelope in events.iter().filter(|e| is_journaled(&e.event)) {
        serde_json::to_writer(
            &mut *writer,
            &SceneLine {
                envelope,
                step: record.as_ref().map(|_| last.event_id),
            },
        )?;
        writer.write_all(b"\n")?;
    }
    if let Some(history) = record {
        serde_json::to_writer(
            &mut *writer,
            &HistoryLine {
                event_id: last.event_id,
                root_at_ms,
                pruned: Some(tree.pruned),
                history,
            },
        )?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

// For recovery the whole line already lives in the input buffer. Borrow the
// two compact event arrays until their node survives pruning at the final head.
#[derive(Deserialize)]
struct IndexedNode<'a> {
    id: u64,
    parent: Option<u64>,
    meta: slicer_types::NodeMeta,
    #[serde(borrow)]
    forward: &'a RawValue,
    inverse: &'a RawValue,
}

#[derive(Deserialize)]
struct Line<'a> {
    event_id: EventId,
    step: Option<EventId>,
    event: Option<Box<ProjectEvent>>,
    #[serde(borrow)]
    history: Option<HistoryRecord<IndexedNode<'a>>>,
    root_at_ms: Option<i64>,
    pruned: Option<bool>,
}

pub(super) fn replay(
    input: &[u8],
    through: EventId,
    state: &mut ProjectState,
    tree: &mut HistoryTree,
    base: &mut Option<SavedHistory>,
) -> Result<()> {
    let mut pending = Vec::new();
    let mut payloads = BTreeMap::new();
    let mut pruned = false;
    for bytes in input.split_inclusive(|b| *b == b'\n') {
        if bytes.last() != Some(&b'\n') {
            break;
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let line: Line = serde_json::from_slice(bytes).context("invalid autosave journal line")?;
        if line.event_id <= through {
            continue;
        }
        if let Some(event) = line.event {
            if let Some(commit) = line.step {
                pending.push((commit, event));
            } else {
                ensure!(pending.is_empty(), "missing history commit");
                crate::reducer::reduce(state, &event);
            }
            continue;
        }
        ensure!(
            pending.iter().all(|(id, _)| *id == line.event_id),
            "history commit out of sequence"
        );
        match line.history.context("missing history commit")? {
            HistoryRecord::Node(node) => {
                ensure!(
                    node.parent == Some(tree.head())
                        && node.id == tree.next_id
                        && node.id < u64::MAX - 1,
                    "journal node out of sequence"
                );
                if tree.next_id == 1 {
                    let root = tree.nodes.get_mut(&tree.root()).unwrap();
                    Arc::get_mut(root).unwrap().meta.at_ms =
                        line.root_at_ms.context("missing root timestamp")?;
                }
                payloads.insert(node.id, (node.forward, node.inverse));
                tree.append(HistoryNode {
                    id: node.id,
                    parent: node.parent,
                    meta: node.meta,
                    bytes: node.forward.get().len() + node.inverse.get().len(),
                    forward: vec![],
                    inverse: vec![],
                });
                // Old writers only pruned by count. Preserve their navigable
                // topology until all subsequent head moves have been replayed.
                let removed = match line.pruned {
                    Some(false) => false,
                    Some(true) => tree.prune_to(MAX_NODES, MAX_HISTORY_BYTES, true),
                    None => tree.prune_to(MAX_NODES, usize::MAX, false),
                };
                if removed {
                    payloads.retain(|id, _| tree.nodes.contains_key(id));
                    pruned = true;
                }
            }
            HistoryRecord::Head { head } => {
                ensure!(tree.nodes.contains_key(&head), "unknown journal head");
                tree.set_head(head);
            }
            HistoryRecord::Reset { at_ms } => {
                tree.reset(at_ms);
                *base = None;
                payloads.clear();
                pruned = false;
            }
        }
        for (_, event) in pending.drain(..) {
            crate::reducer::reduce(state, &event);
        }
    }
    tree.pruned = tree.enforce_limits(false) || pruned;
    for (id, (forward, inverse)) in payloads {
        if id == tree.root {
            continue;
        }
        let Some(node) = tree.nodes.get_mut(&id) else {
            continue;
        };
        let node = Arc::get_mut(node).unwrap();
        node.forward = serde_json::from_str(forward.get())?;
        node.inverse = serde_json::from_str(inverse.get())?;
        ensure!(
            node.forward
                .iter()
                .chain(&node.inverse)
                .all(crate::history::is_replayable),
            "invalid history event"
        );
    }
    Ok(())
}
