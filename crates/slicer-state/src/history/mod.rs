//! Undo / redo history: what an event does to it ([`classify`]), what one step
//! is made of ([`UndoEntry`]) and which events walk a step back and forth
//! ([`inverse_events`], [`forward_events`]).
//!
//! The stacks, the lock around them and the way a transaction gathers its
//! entries stay in the crate root, next to `StateLayer`; they call in here.

use crate::project::{display_name, ObjectRecord, ProjectState};
use slicer_types::{
    AdaptiveRule, FeatureOp, LayerConfigRange, ObjectGeometry, ObjectId, PartId, PartSnapshot,
    ProjectEvent, SettingKey, SettingScope, SettingValue, StepPartPayload,
};

/// Maximum number of entries kept on the undo / redo stacks. Older entries
/// are dropped when the cap is exceeded — long sessions don't grow without
/// bound, but the user still gets a comfortable history.
const HISTORY_CAP: usize = 200;

/// One undoable mutation. Symmetric: `prev` returns the project to the state
/// before the change, `next` re-applies it. Each entry maps to exactly one
/// project event for both directions, except a `Batch`, which maps to the
/// events of every entry it holds, and an `ObjectRemove`, whose undo is the
/// several events that put the object back (`restore_events`).
#[derive(Debug, Clone)]
pub(crate) enum UndoEntry {
    Setting {
        scope: SettingScope,
        key: SettingKey,
        prev: Option<SettingValue>,
        next: SettingValue,
    },
    Transform {
        object_id: ObjectId,
        prev: [f32; 16],
        next: [f32; 16],
    },
    Override {
        object_id: ObjectId,
        key: SettingKey,
        prev: Option<SettingValue>,
        next: Option<SettingValue>,
    },
    Operations {
        object_id: ObjectId,
        prev: Vec<FeatureOp>,
        next: Vec<FeatureOp>,
    },
    /// Seam paint. The RLE blob and the target point are written together by
    /// the painter at the end of each stroke, so they undo together too.
    SeamFacets {
        object_id: ObjectId,
        prev_facets_rle: Option<String>,
        prev_target_point: Option<String>,
        next_facets_rle: Option<String>,
        next_target_point: Option<String>,
    },
    SupportFacets {
        object_id: ObjectId,
        prev: Option<String>,
        next: Option<String>,
    },
    FanFacets {
        object_id: ObjectId,
        prev: Option<String>,
        next: Option<String>,
    },
    FilamentFacets {
        object_id: ObjectId,
        prev: Option<String>,
        next: Option<String>,
    },
    LayerHeightProfile {
        object_id: ObjectId,
        prev: Vec<f32>,
        next: Vec<f32>,
    },
    LayerRanges {
        object_id: ObjectId,
        prev: Vec<LayerConfigRange>,
        next: Vec<LayerConfigRange>,
    },
    /// One Adaptive Print rule slot, keyed by `rule_id`. `None` on a side
    /// means the rule is absent there — so this single entry covers both
    /// `AdaptiveRuleSet` (upsert; `next` is `Some`) and `AdaptiveRuleRemoved`
    /// (`next` is `None`), and undoing either direction just restores the
    /// other slot value.
    AdaptiveRuleSlot {
        rule_id: String,
        prev: Option<AdaptiveRule>,
        next: Option<AdaptiveRule>,
    },
    /// An object put on the plate. Keeps the whole `ObjectAdded`, so redo
    /// re-emits exactly what the first add did. Undoing it assumes the id was
    /// new (every emitter mints a fresh ULID): a record the add replaced
    /// would not come back.
    ObjectAdd {
        object_id: ObjectId,
        event: Box<ProjectEvent>,
    },
    /// An object taken off the plate, with everything it carried.
    ObjectRemove { record: Box<ObjectRecord> },
    /// One part, keyed by id. `None` on a side means no such part there, so
    /// one entry covers creating, editing and removing it.
    PartSlot {
        part_id: PartId,
        prev: Option<Box<PartSnapshot>>,
        next: Option<Box<PartSnapshot>>,
    },
    /// An object's geometry replaced in place.
    Geometry {
        object_id: ObjectId,
        prev: Box<ObjectGeometry>,
        next: Box<ObjectGeometry>,
    },
    /// Every entry one transaction recorded, in the order they were applied.
    Batch(Vec<UndoEntry>),
    Recipe {
        object_id: ObjectId,
        prev: Option<serde_json::Value>,
        next: Option<serde_json::Value>,
    },
}

impl UndoEntry {
    /// One history step from the entries of one transaction: nothing for no
    /// entry, the entry itself for one, a batch for more.
    pub(crate) fn step(mut entries: Vec<UndoEntry>) -> Option<UndoEntry> {
        match entries.len() {
            0 => None,
            1 => entries.pop(),
            _ => Some(UndoEntry::Batch(entries)),
        }
    }
}

/// What an event does to the undo / redo history.
///
/// Three outcomes, not two. "This event isn't undoable" and "this event makes
/// the existing history unreachable" used to be the same answer (`None` from
/// the old `derive_undo_entry`), and a catch-all `_ => None` handed that one
/// destructive answer to every variant nobody had thought about. That is why
/// *every* `SliceProgress` tick wiped both stacks — slicing never touches the
/// scene, but it was unrecognised, and unrecognised meant "erase".
///
/// `classify` therefore lists every `ProjectEvent` variant by name and has no
/// catch-all: a new variant must fail to compile until its author picks a
/// bucket on purpose.
// `Record` is as large as an `UndoEntry` and the other two are empty, which
// trips `large_enum_variant`. Boxing would buy nothing: a `HistoryEffect` is
// returned by `classify`, matched on in `apply` and dropped — it is never
// stored, so the only effect would be an allocation per undoable event, while
// the `UndoEntry` behind it has to be moved onto the stack unboxed anyway.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum HistoryEffect {
    /// Changes the scene reversibly: snapshot onto the undo stack, drop redo.
    Record(UndoEntry),
    /// Doesn't change the scene — leave both stacks exactly as they are.
    Neutral,
    /// Makes the recorded history unreachable: clear both stacks.
    Reset,
}

pub(crate) fn push_capped(stack: &mut Vec<UndoEntry>, entry: UndoEntry) {
    stack.push(entry);
    if stack.len() > HISTORY_CAP {
        // Drop the oldest entry (front).
        stack.remove(0);
    }
}

/// Sort one event into a history bucket. See [`HistoryEffect`] for why there
/// are three and why this match must stay exhaustive — **do not add a `_`
/// arm**: the compile error a new variant produces here is the whole point.
///
/// `project` is the state *before* this event's reducer arm runs (see
/// [`StateLayer::apply`]), so every `Record` arm can read the "old" side of
/// the pair directly off the project.
///
/// The comment above isn't what stops a `_` arm from coming back — a comment
/// doesn't compile to anything. These two clippy lints do: both are
/// *restriction*-tier (off even under `clippy::all`/`pedantic`), so they have
/// to be turned on right here to bite, but with them on, `-D warnings` in the
/// `cargo clippy --workspace --all-targets` gate turns any future `_ => ...`
/// on this match into a build failure instead of a silent merge. Both are
/// needed, not just one: clippy's wildcard check picks between them by how
/// many variants the `_` would swallow — `wildcard_enum_match_arm` fires
/// when it's 2+, `match_wildcard_for_single_variants` fires when it's
/// exactly 1. A single freshly-added `ProjectEvent` variant caught by a
/// catch-all — the realistic way this regresses — hits the *single*-variant
/// case, so `wildcard_enum_match_arm` alone silently misses it (confirmed by
/// mutation testing this fix). That silent miss is exactly how the bug
/// shipped the first time: an unrecognised `ProjectEvent` fell into a
/// catch-all that answered `Reset`, so every `SliceProgress` tick — which
/// never touches the scene — wiped undo/redo. Together these two lints
/// close that gap from either direction (catch-all-as-Reset or
/// catch-all-as-Neutral), for one missing variant or many.
///
/// [`StateLayer::apply`]: crate::StateLayer::apply
#[warn(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
pub(crate) fn classify(project: &ProjectState, event: &ProjectEvent) -> HistoryEffect {
    match event {
        ProjectEvent::ObjectRecipeSet { object_id, recipe } => match project.objects.get(object_id)
        {
            Some(record) => HistoryEffect::Record(UndoEntry::Recipe {
                object_id: object_id.clone(),
                prev: record.recipe.clone(),
                next: recipe.clone(),
            }),
            None => HistoryEffect::Neutral,
        },
        // ---- Reset: whatever the stacks describe is no longer reachable ----
        //
        // A different project (or a wholesale state replacement) means the
        // object ids and setting values the entries name may not even exist.
        ProjectEvent::ProjectCreated { .. }
        | ProjectEvent::ProjectOpened { .. }
        | ProjectEvent::ProjectReplaced { .. } => HistoryEffect::Reset,

        // ---- Neutral: the scene is untouched, so the history still holds ----
        //
        // Slicing reads the scene and reports on it. Every one of these used
        // to erase the stacks, and `SliceProgress` fires dozens of times per
        // job — one slice was enough to lose the whole session's undo.
        ProjectEvent::SliceRequested { .. }
        | ProjectEvent::SliceProgress { .. }
        | ProjectEvent::SliceCompleted { .. }
        | ProjectEvent::SliceFailed { .. }
        | ProjectEvent::SliceCanceled { .. } => HistoryEffect::Neutral,

        // Preset activation and the preset library live in slices of
        // `ProjectState` that no `UndoEntry` names, so they can neither be
        // undone by an entry nor invalidate one. Ctrl+Z after switching a
        // preset keeps walking back the scene edits, which is what the user
        // expects; making preset changes themselves undoable is a separate
        // decision, not this task's.
        ProjectEvent::PresetActivated { .. }
        | ProjectEvent::PresetSaved { .. }
        | ProjectEvent::PresetDeleted { .. } => HistoryEffect::Neutral,

        // ---- Record: reversible scene changes ----

        // Add and remove are reversible: an add is undone by removing the
        // object, a removal by restoring the record it had (`restore_events`).
        ProjectEvent::ObjectAdded { object_id, .. } => {
            HistoryEffect::Record(UndoEntry::ObjectAdd {
                object_id: object_id.clone(),
                event: Box::new(event.clone()),
            })
        }
        ProjectEvent::ObjectRemoved { object_id } => match project.objects.get(object_id) {
            Some(record) => HistoryEffect::Record(UndoEntry::ObjectRemove {
                record: Box::new(record.clone()),
            }),
            None => HistoryEffect::Neutral,
        },
        ProjectEvent::SettingChanged {
            scope,
            key,
            old_value,
            new_value,
        } => {
            // Trust the event's old_value (it was computed against current
            // state at apply time in dispatch); fall back to project lookup.
            let prev = old_value
                .clone()
                .or_else(|| project.settings.get(&(*scope, key.clone())).cloned());
            HistoryEffect::Record(UndoEntry::Setting {
                scope: *scope,
                key: key.clone(),
                prev,
                next: new_value.clone(),
            })
        }
        ProjectEvent::ObjectTransformed {
            object_id,
            old_transform,
            new_transform,
        } => HistoryEffect::Record(UndoEntry::Transform {
            object_id: object_id.clone(),
            prev: *old_transform,
            next: *new_transform,
        }),
        ProjectEvent::ObjectOverrideSet {
            object_id,
            key,
            new_value,
        } => {
            let prev = project
                .objects
                .get(object_id)
                .and_then(|o| o.overrides.get(key).cloned());
            HistoryEffect::Record(UndoEntry::Override {
                object_id: object_id.clone(),
                key: key.clone(),
                prev,
                next: new_value.clone(),
            })
        }
        ProjectEvent::ObjectOperationsSet {
            object_id,
            operations,
        } => {
            // `project` here is the state BEFORE this event's reducer arm
            // runs (see `apply`, which classifies ahead of `apply_internal`)
            // — so the object's current `operations` is exactly the "old"
            // side of the undo pair, same trick `ObjectOverrideSet` uses for
            // `overrides`. An unknown object_id (shouldn't happen — the RPC
            // arm validates first) degrades to an empty prev rather than
            // panicking.
            let prev = project
                .objects
                .get(object_id)
                .map(|o| o.operations.clone())
                .unwrap_or_default();
            HistoryEffect::Record(UndoEntry::Operations {
                object_id: object_id.clone(),
                prev,
                next: operations.clone(),
            })
        }
        ProjectEvent::ObjectSeamFacetsSet {
            object_id,
            facets_rle,
            target_point,
        } => {
            let prev = project.objects.get(object_id);
            HistoryEffect::Record(UndoEntry::SeamFacets {
                object_id: object_id.clone(),
                prev_facets_rle: prev.and_then(|o| o.seam_facets_rle.clone()),
                prev_target_point: prev.and_then(|o| o.seam_target_point.clone()),
                next_facets_rle: facets_rle.clone(),
                next_target_point: target_point.clone(),
            })
        }
        ProjectEvent::ObjectSupportFacetsSet {
            object_id,
            facets_rle,
        } => HistoryEffect::Record(UndoEntry::SupportFacets {
            object_id: object_id.clone(),
            prev: project
                .objects
                .get(object_id)
                .and_then(|o| o.support_facets_rle.clone()),
            next: facets_rle.clone(),
        }),
        ProjectEvent::ObjectFanFacetsSet {
            object_id,
            facets_rle,
        } => HistoryEffect::Record(UndoEntry::FanFacets {
            object_id: object_id.clone(),
            prev: project
                .objects
                .get(object_id)
                .and_then(|o| o.fan_facets_rle.clone()),
            next: facets_rle.clone(),
        }),
        ProjectEvent::ObjectFilamentFacetsSet {
            object_id,
            facets_rle,
        } => HistoryEffect::Record(UndoEntry::FilamentFacets {
            object_id: object_id.clone(),
            prev: project
                .objects
                .get(object_id)
                .and_then(|o| o.filament_facets_rle.clone()),
            next: facets_rle.clone(),
        }),
        ProjectEvent::ObjectLayerHeightProfileSet { object_id, profile } => {
            HistoryEffect::Record(UndoEntry::LayerHeightProfile {
                object_id: object_id.clone(),
                prev: project
                    .objects
                    .get(object_id)
                    .map(|o| o.layer_height_profile.clone())
                    .unwrap_or_default(),
                next: profile.clone(),
            })
        }
        ProjectEvent::ObjectLayerRangesSet { object_id, ranges } => {
            HistoryEffect::Record(UndoEntry::LayerRanges {
                object_id: object_id.clone(),
                prev: project
                    .objects
                    .get(object_id)
                    .map(|o| o.layer_ranges.clone())
                    .unwrap_or_default(),
                next: ranges.clone(),
            })
        }
        // Upsert by id: `prev` is whatever occupied the slot (None when this
        // is a brand-new rule, in which case undo removes it again).
        ProjectEvent::AdaptiveRuleSet { rule } => {
            HistoryEffect::Record(UndoEntry::AdaptiveRuleSlot {
                rule_id: rule.id.clone(),
                prev: find_adaptive_rule(project, &rule.id),
                next: Some(rule.clone()),
            })
        }
        ProjectEvent::AdaptiveRuleRemoved { rule_id } => {
            HistoryEffect::Record(UndoEntry::AdaptiveRuleSlot {
                rule_id: rule_id.clone(),
                prev: find_adaptive_rule(project, rule_id),
                next: None,
            })
        }
        // A part's snapshot is replaced whole, so its entry is the slot: what
        // was there before (nothing, for a new part) and what is there now.
        ProjectEvent::PartSet { part } => HistoryEffect::Record(UndoEntry::PartSlot {
            part_id: part.part_id.clone(),
            prev: project.parts.get(&part.part_id).cloned().map(Box::new),
            next: Some(Box::new(part.clone())),
        }),
        ProjectEvent::PartRemoved { part_id } => match project.parts.get(part_id) {
            Some(prev) => HistoryEffect::Record(UndoEntry::PartSlot {
                part_id: part_id.clone(),
                prev: Some(Box::new(prev.clone())),
                next: None,
            }),
            None => HistoryEffect::Neutral,
        },
        ProjectEvent::ObjectGeometryUpdated {
            object_id,
            geometry,
        } => match project.objects.get(object_id) {
            Some(record) => HistoryEffect::Record(UndoEntry::Geometry {
                object_id: object_id.clone(),
                prev: Box::new(record.geometry()),
                next: Box::new(geometry.clone()),
            }),
            None => HistoryEffect::Neutral,
        },
    }
}

fn find_adaptive_rule(project: &ProjectState, rule_id: &str) -> Option<AdaptiveRule> {
    project
        .adaptive_rules
        .iter()
        .find(|r| r.id == rule_id)
        .cloned()
}

/// The event that puts one Adaptive Print rule slot into `slot`: setting the
/// rule when it's occupied, removing it when it's empty.
fn adaptive_rule_event(rule_id: &str, slot: &Option<AdaptiveRule>) -> ProjectEvent {
    match slot {
        Some(rule) => ProjectEvent::AdaptiveRuleSet { rule: rule.clone() },
        None => ProjectEvent::AdaptiveRuleRemoved {
            rule_id: rule_id.to_string(),
        },
    }
}

/// The event that puts one part slot into `slot`: setting the part when it
/// is occupied, removing it when it is empty.
fn part_slot_event(part_id: &PartId, slot: &Option<Box<PartSnapshot>>) -> ProjectEvent {
    match slot {
        Some(part) => ProjectEvent::PartSet {
            part: (**part).clone(),
        },
        None => ProjectEvent::PartRemoved {
            part_id: part_id.clone(),
        },
    }
}

/// The events that take one history step back. A batch is undone from its
/// last entry to its first: each entry's inverse assumes the state the later
/// entries had not touched yet.
pub(crate) fn inverse_events(entry: &UndoEntry) -> Vec<ProjectEvent> {
    match entry {
        UndoEntry::Recipe {
            object_id, prev, ..
        } => vec![ProjectEvent::ObjectRecipeSet {
            object_id: object_id.clone(),
            recipe: prev.clone(),
        }],
        UndoEntry::Batch(entries) => entries.iter().rev().flat_map(inverse_events).collect(),
        UndoEntry::ObjectAdd { object_id, .. } => vec![ProjectEvent::ObjectRemoved {
            object_id: object_id.clone(),
        }],
        UndoEntry::ObjectRemove { record } => restore_events(record),
        UndoEntry::PartSlot { part_id, prev, .. } => vec![part_slot_event(part_id, prev)],
        UndoEntry::Geometry {
            object_id, prev, ..
        } => vec![ProjectEvent::ObjectGeometryUpdated {
            object_id: object_id.clone(),
            geometry: (**prev).clone(),
        }],
        UndoEntry::Setting {
            scope, key, prev, ..
        } => vec![ProjectEvent::SettingChanged {
            scope: *scope,
            key: key.clone(),
            old_value: None,
            // If we never had a value, fall back to current; in practice
            // settings are always seeded so prev is always Some.
            new_value: prev.clone().unwrap_or(SettingValue::Bool(false)),
        }],
        UndoEntry::Transform {
            object_id,
            prev,
            next,
        } => vec![ProjectEvent::ObjectTransformed {
            object_id: object_id.clone(),
            old_transform: *next,
            new_transform: *prev,
        }],
        UndoEntry::Override {
            object_id,
            key,
            prev,
            ..
        } => vec![ProjectEvent::ObjectOverrideSet {
            object_id: object_id.clone(),
            key: key.clone(),
            new_value: prev.clone(),
        }],
        UndoEntry::Operations {
            object_id, prev, ..
        } => vec![ProjectEvent::ObjectOperationsSet {
            object_id: object_id.clone(),
            operations: prev.clone(),
        }],
        UndoEntry::SeamFacets {
            object_id,
            prev_facets_rle,
            prev_target_point,
            ..
        } => vec![ProjectEvent::ObjectSeamFacetsSet {
            object_id: object_id.clone(),
            facets_rle: prev_facets_rle.clone(),
            target_point: prev_target_point.clone(),
        }],
        UndoEntry::SupportFacets {
            object_id, prev, ..
        } => vec![ProjectEvent::ObjectSupportFacetsSet {
            object_id: object_id.clone(),
            facets_rle: prev.clone(),
        }],
        UndoEntry::FanFacets {
            object_id, prev, ..
        } => vec![ProjectEvent::ObjectFanFacetsSet {
            object_id: object_id.clone(),
            facets_rle: prev.clone(),
        }],
        UndoEntry::FilamentFacets {
            object_id, prev, ..
        } => vec![ProjectEvent::ObjectFilamentFacetsSet {
            object_id: object_id.clone(),
            facets_rle: prev.clone(),
        }],
        UndoEntry::LayerHeightProfile {
            object_id, prev, ..
        } => vec![ProjectEvent::ObjectLayerHeightProfileSet {
            object_id: object_id.clone(),
            profile: prev.clone(),
        }],
        UndoEntry::LayerRanges {
            object_id, prev, ..
        } => vec![ProjectEvent::ObjectLayerRangesSet {
            object_id: object_id.clone(),
            ranges: prev.clone(),
        }],
        UndoEntry::AdaptiveRuleSlot { rule_id, prev, .. } => {
            vec![adaptive_rule_event(rule_id, prev)]
        }
    }
}

/// The events that re-apply one history step, a batch in its original order.
pub(crate) fn forward_events(entry: &UndoEntry) -> Vec<ProjectEvent> {
    match entry {
        UndoEntry::Recipe {
            object_id, next, ..
        } => vec![ProjectEvent::ObjectRecipeSet {
            object_id: object_id.clone(),
            recipe: next.clone(),
        }],
        UndoEntry::Batch(entries) => entries.iter().flat_map(forward_events).collect(),
        UndoEntry::ObjectAdd { event, .. } => vec![(**event).clone()],
        UndoEntry::ObjectRemove { record } => vec![ProjectEvent::ObjectRemoved {
            object_id: record.id.clone(),
        }],
        UndoEntry::PartSlot { part_id, next, .. } => vec![part_slot_event(part_id, next)],
        UndoEntry::Geometry {
            object_id, next, ..
        } => vec![ProjectEvent::ObjectGeometryUpdated {
            object_id: object_id.clone(),
            geometry: (**next).clone(),
        }],
        UndoEntry::Setting {
            scope, key, next, ..
        } => vec![ProjectEvent::SettingChanged {
            scope: *scope,
            key: key.clone(),
            old_value: None,
            new_value: next.clone(),
        }],
        UndoEntry::Transform {
            object_id,
            prev,
            next,
        } => vec![ProjectEvent::ObjectTransformed {
            object_id: object_id.clone(),
            old_transform: *prev,
            new_transform: *next,
        }],
        UndoEntry::Override {
            object_id,
            key,
            next,
            ..
        } => vec![ProjectEvent::ObjectOverrideSet {
            object_id: object_id.clone(),
            key: key.clone(),
            new_value: next.clone(),
        }],
        UndoEntry::Operations {
            object_id, next, ..
        } => vec![ProjectEvent::ObjectOperationsSet {
            object_id: object_id.clone(),
            operations: next.clone(),
        }],
        UndoEntry::SeamFacets {
            object_id,
            next_facets_rle,
            next_target_point,
            ..
        } => vec![ProjectEvent::ObjectSeamFacetsSet {
            object_id: object_id.clone(),
            facets_rle: next_facets_rle.clone(),
            target_point: next_target_point.clone(),
        }],
        UndoEntry::SupportFacets {
            object_id, next, ..
        } => vec![ProjectEvent::ObjectSupportFacetsSet {
            object_id: object_id.clone(),
            facets_rle: next.clone(),
        }],
        UndoEntry::FanFacets {
            object_id, next, ..
        } => vec![ProjectEvent::ObjectFanFacetsSet {
            object_id: object_id.clone(),
            facets_rle: next.clone(),
        }],
        UndoEntry::FilamentFacets {
            object_id, next, ..
        } => vec![ProjectEvent::ObjectFilamentFacetsSet {
            object_id: object_id.clone(),
            facets_rle: next.clone(),
        }],
        UndoEntry::LayerHeightProfile {
            object_id, next, ..
        } => vec![ProjectEvent::ObjectLayerHeightProfileSet {
            object_id: object_id.clone(),
            profile: next.clone(),
        }],
        UndoEntry::LayerRanges {
            object_id, next, ..
        } => vec![ProjectEvent::ObjectLayerRangesSet {
            object_id: object_id.clone(),
            ranges: next.clone(),
        }],
        UndoEntry::AdaptiveRuleSlot { rule_id, next, .. } => {
            vec![adaptive_rule_event(rule_id, next)]
        }
    }
}

/// The events that put a removed object back exactly as it was: `ObjectAdded`
/// with its own id, name, source and transform, then — for a part body — the
/// `ObjectGeometryUpdated` that binds it again, then one existing setter per
/// field that is not at its default. Only that geometry event is new on the
/// wire, and only a body's restore sends it: a plain object comes back through
/// events a client already understands.
fn restore_events(record: &ObjectRecord) -> Vec<ProjectEvent> {
    // Every field is named and there is no `..`: a field added to
    // `ObjectRecord` fails to compile here until the restore handles it, so
    // an undone removal cannot quietly come back without it.
    let ObjectRecord {
        id,
        name,
        source_path,
        source_hash,
        triangle_count,
        bounds,
        transform,
        overrides,
        seam_facets_rle,
        seam_target_point,
        support_facets_rle,
        fan_facets_rle,
        filament_facets_rle,
        layer_height_profile,
        layer_ranges,
        operations,
        step_part_index,
        recipe,
        body,
    } = record;
    let mut events = vec![ProjectEvent::ObjectAdded {
        object_id: id.clone(),
        name: display_name(name, source_path),
        source_path: source_path.clone(),
        triangle_count: *triangle_count,
        bounds: bounds.clone(),
        source_hash: source_hash.clone(),
        step_part: step_part_index.map(|part_index| StepPartPayload {
            part_name: String::new(),
            part_index,
        }),
        split_part: None,
        transform: Some(*transform),
    }];
    if recipe.is_some() {
        events.push(ProjectEvent::ObjectRecipeSet {
            object_id: id.clone(),
            recipe: recipe.clone(),
        });
    }
    // A body's binding rides on a geometry event, right after the add, so the
    // object is a body again before anything else lands on it.
    if body.is_some() {
        events.push(ProjectEvent::ObjectGeometryUpdated {
            object_id: id.clone(),
            geometry: record.geometry(),
        });
    }
    let mut keys: Vec<&SettingKey> = overrides.keys().collect();
    keys.sort();
    for key in keys {
        events.push(ProjectEvent::ObjectOverrideSet {
            object_id: id.clone(),
            key: key.clone(),
            new_value: Some(overrides[key].clone()),
        });
    }
    if seam_facets_rle.is_some() || seam_target_point.is_some() {
        events.push(ProjectEvent::ObjectSeamFacetsSet {
            object_id: id.clone(),
            facets_rle: seam_facets_rle.clone(),
            target_point: seam_target_point.clone(),
        });
    }
    if support_facets_rle.is_some() {
        events.push(ProjectEvent::ObjectSupportFacetsSet {
            object_id: id.clone(),
            facets_rle: support_facets_rle.clone(),
        });
    }
    if fan_facets_rle.is_some() {
        events.push(ProjectEvent::ObjectFanFacetsSet {
            object_id: id.clone(),
            facets_rle: fan_facets_rle.clone(),
        });
    }
    if filament_facets_rle.is_some() {
        events.push(ProjectEvent::ObjectFilamentFacetsSet {
            object_id: id.clone(),
            facets_rle: filament_facets_rle.clone(),
        });
    }
    if !layer_height_profile.is_empty() {
        events.push(ProjectEvent::ObjectLayerHeightProfileSet {
            object_id: id.clone(),
            profile: layer_height_profile.clone(),
        });
    }
    if !layer_ranges.is_empty() {
        events.push(ProjectEvent::ObjectLayerRangesSet {
            object_id: id.clone(),
            ranges: layer_ranges.clone(),
        });
    }
    if !operations.is_empty() {
        events.push(ProjectEvent::ObjectOperationsSet {
            object_id: id.clone(),
            operations: operations.clone(),
        });
    }
    events
}

#[cfg(test)]
mod tests;
