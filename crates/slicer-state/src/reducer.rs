use crate::project::{ObjectRecord, ProjectState};
use slicer_types::{ObjectGeometry, ProjectEvent};

pub fn reduce(state: &mut ProjectState, event: &ProjectEvent) {
    match event {
        ProjectEvent::ProjectCreated { meta } => {
            // A new project starts on an empty plate. Every object in state
            // is handed to libslic3r, so a part kept from the previous
            // project would be printed next to the new ones. Adaptive rules
            // and CAD parts are attached to the project too. The printer
            // setup — settings, active presets, the preset library — belongs
            // to the session and carries over.
            state.meta = Some(meta.clone());
            state.objects.clear();
            state.adaptive_rules.clear();
            state.parts.clear();
        }
        ProjectEvent::ProjectOpened { meta, .. } => {
            state.meta = Some(meta.clone());
        }
        ProjectEvent::PresetActivated { kind, preset_id } => match kind {
            slicer_types::PresetKind::Printer => state.presets.active_printer = preset_id.clone(),
            slicer_types::PresetKind::Process => state.presets.active_process = preset_id.clone(),
            slicer_types::PresetKind::Filament => state.presets.active_filament = preset_id.clone(),
        },
        ProjectEvent::ObjectAdded {
            object_id,
            name,
            source_path,
            triangle_count,
            bounds,
            source_hash,
            // `step_part` carries the multi-solid STEP fan-out identity
            // (`part_index` into the originating `Slic3r::Model::objects`).
            // Persisted below as `step_part_index` so the slicer can map
            // this record back to the right libslic3r ModelObject when N
            // records share one `.stp` source_path.
            step_part,
            // `split_part` is informational UI-only metadata (same shape as
            // `step_part`); the reducer doesn't store it. `transform` is
            // consumed below so split children land at the parent's pose.
            split_part: _,
            transform,
        } => {
            state.objects.insert(
                object_id.clone(),
                ObjectRecord {
                    recipe: None,
                    id: object_id.clone(),
                    name: name.clone(),
                    // Fall back to the bare name when the UI didn't supply a
                    // path (older builds, headless tests). Such an object has
                    // no mesh on disk, and since 2026-09-17 the libslic3r
                    // build fails its slice naming that non-path instead of
                    // answering with the bridge's rectangle stub — which is
                    // what this fallback used to lean on
                    // (`slicer-rpc/tests/slice_missing_source_e2e.rs`).
                    source_path: if source_path.is_empty() {
                        name.clone()
                    } else {
                        source_path.clone()
                    },
                    source_hash: source_hash.clone(),
                    triangle_count: *triangle_count,
                    bounds: bounds.clone(),
                    transform: transform.unwrap_or_else(identity_matrix),
                    overrides: std::collections::HashMap::new(),
                    seam_facets_rle: None,
                    seam_target_point: None,
                    support_facets_rle: None,
                    fan_facets_rle: None,
                    filament_facets_rle: None,
                    layer_height_profile: Vec::new(),
                    layer_ranges: Vec::new(),
                    operations: Vec::new(),
                    step_part_index: step_part.as_ref().map(|s| s.part_index),
                    body: None,
                },
            );
        }
        ProjectEvent::ObjectRecipeSet { object_id, recipe } => {
            if let Some(obj) = state.objects.get_mut(object_id) {
                obj.recipe = recipe.clone();
            }
        }
        ProjectEvent::ObjectRemoved { object_id } => {
            state.objects.remove(object_id);
        }
        ProjectEvent::SliceRequested { .. }
        | ProjectEvent::SliceCompleted { .. }
        | ProjectEvent::SliceFailed { .. }
        | ProjectEvent::SliceCanceled { .. }
        | ProjectEvent::SliceProgress { .. } => {
            // Slicing events observed but don't mutate project state directly.
        }
        ProjectEvent::SettingChanged {
            scope,
            key,
            new_value,
            ..
        } => {
            state
                .settings
                .insert((*scope, key.clone()), new_value.clone());
        }
        ProjectEvent::PresetSaved { preset } => {
            state
                .preset_library
                .insert(preset.id.clone(), preset.clone());
        }
        ProjectEvent::PresetDeleted { preset_id } => {
            state.preset_library.remove(preset_id);
        }
        ProjectEvent::ObjectOverrideSet {
            object_id,
            key,
            new_value,
        } => {
            if let Some(obj) = state.objects.get_mut(object_id) {
                match new_value {
                    Some(v) => {
                        obj.overrides.insert(key.clone(), v.clone());
                    }
                    None => {
                        obj.overrides.remove(key);
                    }
                }
            }
        }
        ProjectEvent::ObjectSeamFacetsSet {
            object_id,
            facets_rle,
            target_point,
        } => {
            if let Some(obj) = state.objects.get_mut(object_id) {
                obj.seam_facets_rle = facets_rle.clone();
                obj.seam_target_point = target_point.clone();
            }
        }
        ProjectEvent::ObjectSupportFacetsSet {
            object_id,
            facets_rle,
        } => {
            if let Some(obj) = state.objects.get_mut(object_id) {
                obj.support_facets_rle = facets_rle.clone();
            }
        }
        ProjectEvent::ObjectFanFacetsSet {
            object_id,
            facets_rle,
        } => {
            if let Some(obj) = state.objects.get_mut(object_id) {
                obj.fan_facets_rle = facets_rle.clone();
            }
        }
        ProjectEvent::ObjectFilamentFacetsSet {
            object_id,
            facets_rle,
        } => {
            if let Some(obj) = state.objects.get_mut(object_id) {
                obj.filament_facets_rle = facets_rle.clone();
            }
        }
        ProjectEvent::ObjectLayerHeightProfileSet { object_id, profile } => {
            if let Some(obj) = state.objects.get_mut(object_id) {
                obj.layer_height_profile = profile.clone();
            }
        }
        ProjectEvent::ObjectLayerRangesSet { object_id, ranges } => {
            if let Some(obj) = state.objects.get_mut(object_id) {
                obj.layer_ranges = ranges.clone();
            }
        }
        ProjectEvent::ObjectOperationsSet {
            object_id,
            operations,
        } => {
            if let Some(obj) = state.objects.get_mut(object_id) {
                obj.operations = operations.clone();
            }
        }
        ProjectEvent::ObjectTransformed {
            object_id,
            new_transform,
            ..
        } => {
            if let Some(obj) = state.objects.get_mut(object_id) {
                obj.transform = *new_transform;
            }
        }
        ProjectEvent::ProjectReplaced { .. } => {
            // State replacement is performed atomically in StateLayer::replace_state
            // before this event is logged; the reducer only sees it in
            // historical replay where it would correctly trigger a reset.
        }
        ProjectEvent::AdaptiveRuleSet { rule } => {
            // Upsert by id: replace if present, otherwise append.
            match state.adaptive_rules.iter().position(|r| r.id == rule.id) {
                Some(i) => state.adaptive_rules[i] = rule.clone(),
                None => state.adaptive_rules.push(rule.clone()),
            }
        }
        ProjectEvent::AdaptiveRuleRemoved { rule_id } => {
            state.adaptive_rules.retain(|r| &r.id != rule_id);
        }
        ProjectEvent::PartSet { part } => {
            state.parts.insert(part.part_id.clone(), part.clone());
        }
        ProjectEvent::PartRemoved { part_id } => {
            state.parts.remove(part_id);
        }
        ProjectEvent::ObjectGeometryUpdated {
            object_id,
            geometry,
        } => {
            // Every field is named and there is no `..`: a field added to
            // `ObjectGeometry` fails to compile here until the reducer handles
            // it, so an update cannot quietly leave the new field behind.
            let ObjectGeometry {
                source_path,
                source_hash,
                triangle_count,
                bounds,
                step_part_index,
                operations,
                body,
            } = geometry;
            if let Some(obj) = state.objects.get_mut(object_id) {
                obj.source_path = source_path.clone();
                obj.source_hash = source_hash.clone();
                obj.triangle_count = *triangle_count;
                obj.bounds = bounds.clone();
                obj.step_part_index = *step_part_index;
                obj.operations = operations.clone();
                obj.body = body.clone();
            }
        }
    }
}

fn identity_matrix() -> [f32; 16] {
    [
        1.0, 0.0, 0.0, 0.0, //
        0.0, 1.0, 0.0, 0.0, //
        0.0, 0.0, 1.0, 0.0, //
        0.0, 0.0, 0.0, 1.0, //
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use slicer_types::{Bounds, ObjectId, StepPartPayload};

    fn added_event(step_part: Option<StepPartPayload>) -> ProjectEvent {
        ProjectEvent::ObjectAdded {
            object_id: ObjectId("obj-1".into()),
            name: "part".into(),
            source_path: "/tmp/assembly.stp".into(),
            triangle_count: 42,
            bounds: Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            source_hash: "h".into(),
            step_part,
            split_part: None,
            transform: None,
        }
    }

    #[test]
    fn object_added_with_step_part_persists_part_index_on_record() {
        let mut state = ProjectState::default();
        let event = added_event(Some(StepPartPayload {
            part_name: "".into(),
            part_index: 3,
        }));

        reduce(&mut state, &event);

        let obj = state
            .objects
            .get(&ObjectId("obj-1".into()))
            .expect("object inserted");
        assert_eq!(obj.step_part_index, Some(3));
    }

    #[test]
    fn object_added_without_step_part_leaves_part_index_none() {
        let mut state = ProjectState::default();
        let event = added_event(None);

        reduce(&mut state, &event);

        let obj = state
            .objects
            .get(&ObjectId("obj-1".into()))
            .expect("object inserted");
        assert_eq!(obj.step_part_index, None);
    }
}
