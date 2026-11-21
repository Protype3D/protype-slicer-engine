use super::{display_name, ProjectState, UndoEntry};
use slicer_types::{NodeMeta, ObjectId, ObjectRef, ProjectEvent, StepTag};

fn include(items: &mut Vec<String>, item: &str) {
    if !items.iter().any(|existing| existing == item) {
        items.push(item.to_owned());
    }
}

pub(crate) fn describe(
    entry: &UndoEntry,
    project: &ProjectState,
    tag: StepTag,
    at_ms: i64,
) -> NodeMeta {
    let mut meta = NodeMeta {
        at_ms,
        author: tag.author.unwrap_or_default(),
        turn: tag.turn,
        group: tag.group,
        label: tag.label,
        ..Default::default()
    };
    meta.kind = collect(entry, project, &mut meta).to_owned();
    meta
}

fn object(meta: &mut NodeMeta, id: &ObjectId, name: String) {
    if let Some(existing) = meta.objects.iter_mut().find(|o| o.id == id.0) {
        if existing.name == id.0 {
            existing.name = name;
        }
    } else {
        meta.objects.push(ObjectRef {
            id: id.0.clone(),
            name,
        });
    }
}

fn collect(entry: &UndoEntry, project: &ProjectState, meta: &mut NodeMeta) -> &'static str {
    use UndoEntry::*;
    let (kind, id) = match entry {
        Batch(entries) => {
            for e in entries {
                collect(e, project, meta);
            }
            return "batch";
        }
        Setting { key, .. } => {
            include(&mut meta.keys, key);
            ("setting", None)
        }
        Override { object_id, key, .. } => {
            include(&mut meta.keys, key);
            ("override", Some(object_id))
        }
        Transform { object_id, .. } => ("transform", Some(object_id)),
        Operations { object_id, .. } => ("operations", Some(object_id)),
        SeamFacets { object_id, .. } => ("paint.seam", Some(object_id)),
        SupportFacets { object_id, .. } => ("paint.support", Some(object_id)),
        FanFacets { object_id, .. } => ("paint.fan", Some(object_id)),
        FuzzyFacets {
            object_id,
            next_overrides,
            ..
        } => {
            for key in next_overrides.keys() {
                include(&mut meta.keys, key);
            }
            ("paint.fuzzy", Some(object_id))
        }
        FilamentFacets { object_id, .. } => ("paint.filament", Some(object_id)),
        LayerHeightProfile { object_id, .. } => ("layer_height", Some(object_id)),
        LayerRanges { object_id, .. } => ("layer_ranges", Some(object_id)),
        AdaptiveRuleSlot { .. } => ("adaptive_rule", None),
        ObjectAdd { object_id, event } => {
            include(&mut meta.added, &object_id.0);
            if let ProjectEvent::ObjectAdded {
                name, source_path, ..
            } = event.as_ref()
            {
                object(meta, object_id, display_name(name, source_path));
            }
            ("object.add", Some(object_id))
        }
        ObjectRemove { record } => {
            include(&mut meta.removed, &record.id.0);
            object(
                meta,
                &record.id,
                display_name(&record.name, &record.source_path),
            );
            ("object.remove", None)
        }
        PartSlot { prev, next, .. } => {
            for part in next.iter().chain(prev.iter()) {
                for id in part.bodies.iter().filter_map(|b| b.object_id.as_ref()) {
                    object(
                        meta,
                        id,
                        project
                            .objects
                            .get(id)
                            .map(|o| display_name(&o.name, &o.source_path))
                            .unwrap_or_else(|| part.name.clone()),
                    );
                }
            }
            ("part", None)
        }
        Geometry { object_id, .. } => ("geometry", Some(object_id)),
        Recipe { object_id, .. } => ("recipe", Some(object_id)),
    };
    if let Some(id) = id {
        let name = project
            .objects
            .get(id)
            .map(|o| display_name(&o.name, &o.source_path))
            .unwrap_or_else(|| id.0.clone());
        object(meta, id, name);
    }
    include(&mut meta.kinds, kind);
    kind
}
