use super::*;
use crate::StateLayer;
use slicer_types::{
    AdaptiveAction, AdaptiveTrigger, Bounds, LayerConfigRange, Preset, PresetId, PresetKind,
    ProjectId, ProjectMeta,
};

const IDENTITY: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0,
];

const MOVED: [f32; 16] = [
    1.0, 0.0, 0.0, 10.0, //
    0.0, 1.0, 0.0, 20.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0,
];

/// Put one object on the plate and return its id. The add is an undoable
/// step of its own; tests undo only the steps they applied after it.
fn add_test_object(state: &StateLayer) -> ObjectId {
    let object_id = ObjectId("obj-1".into());
    state.apply(ProjectEvent::ObjectAdded {
        object_id: object_id.clone(),
        name: "part".into(),
        source_path: "/tmp/part.stl".into(),
        triangle_count: 12,
        bounds: Bounds {
            min: [0.0, 0.0, 0.0],
            max: [10.0, 10.0, 10.0],
        },
        source_hash: "hash".into(),
        step_part: None,
        split_part: None,
        transform: None,
    });
    object_id
}

fn move_object(state: &StateLayer, object_id: &ObjectId) {
    state.apply(ProjectEvent::ObjectTransformed {
        object_id: object_id.clone(),
        old_transform: IDENTITY,
        new_transform: MOVED,
    });
}

fn object(state: &StateLayer, object_id: &ObjectId) -> ObjectRecord {
    state
        .snapshot()
        .objects
        .get(object_id)
        .cloned()
        .expect("object on the plate")
}

fn test_rule(id: &str, name: &str) -> AdaptiveRule {
    AdaptiveRule {
        id: id.to_string(),
        name: name.to_string(),
        enabled: true,
        trigger: AdaptiveTrigger::ManualEvent {
            event_label: "Stringing visible".into(),
        },
        actions: vec![AdaptiveAction::PausePrint],
        priority: 0,
    }
}

fn slice_events_sample() -> Vec<ProjectEvent> {
    vec![
        ProjectEvent::SliceRequested {
            job_id: "j1".into(),
        },
        ProjectEvent::SliceProgress {
            job_id: "j1".into(),
            stage: "perimeters".into(),
            percent: 42.0,
            layer: Some(7),
            total_layers: Some(100),
        },
        ProjectEvent::SliceCompleted {
            job_id: "j1".into(),
            gcode_path: "/tmp/out.gcode".into(),
            total_layers: 100,
            total_segments: 9000,
            suggested_filename: "out.gcode".into(),
            warnings: Vec::new(),
        },
        ProjectEvent::SliceFailed {
            job_id: "j1".into(),
            error: "boom".into(),
        },
        ProjectEvent::SliceCanceled {
            job_id: "j1".into(),
        },
    ]
}

#[test]
fn slicing_does_not_touch_history() {
    let state = StateLayer::new();
    let object_id = add_test_object(&state);
    move_object(&state, &object_id);
    assert!(state.can_undo(), "a transform has to be undoable");

    state.apply(ProjectEvent::SliceRequested {
        job_id: "j1".into(),
    });
    for percent in [10.0, 50.0, 90.0] {
        state.apply(ProjectEvent::SliceProgress {
            job_id: "j1".into(),
            stage: "infill".into(),
            percent,
            layer: None,
            total_layers: None,
        });
    }
    state.apply(ProjectEvent::SliceCompleted {
        job_id: "j1".into(),
        gcode_path: "/tmp/out.gcode".into(),
        total_layers: 100,
        total_segments: 9000,
        suggested_filename: "out.gcode".into(),
        warnings: Vec::new(),
    });

    assert!(
        state.can_undo(),
        "slicing doesn't change the scene and must not erase the history"
    );
}

#[test]
fn opening_a_project_resets_history() {
    let state = StateLayer::new();
    let object_id = add_test_object(&state);
    move_object(&state, &object_id);
    assert!(state.can_undo());

    state.apply(ProjectEvent::ProjectOpened {
        project_id: ProjectId("p1".into()),
        meta: ProjectMeta {
            title: "other".into(),
            description: None,
            created_at: 0,
            modified_at: 0,
            schema_version: 1,
        },
    });

    assert!(
        !state.can_undo(),
        "someone else's project makes the recorded history unreachable"
    );
    assert!(!state.can_redo());
}

#[test]
fn slice_events_are_neutral_not_reset() {
    // Guard against regression: if anyone puts a catch-all back, the most
    // likely casualty is Slice*, which would quietly slide into Reset and
    // start losing history exactly as invisibly as before this change.
    let project = ProjectState::default();
    for event in slice_events_sample() {
        assert!(
            matches!(classify(&project, &event), HistoryEffect::Neutral),
            "{event:?} has to be neutral"
        );
    }
}

#[test]
fn preset_events_are_neutral() {
    // Presets live in a slice of ProjectState no UndoEntry names, so they
    // neither invalidate an entry nor need one.
    let project = ProjectState::default();
    let events = [
        ProjectEvent::PresetActivated {
            kind: PresetKind::Process,
            preset_id: PresetId("standard".into()),
        },
        ProjectEvent::PresetSaved {
            preset: Preset::vendor("p1", "P1", SettingScope::Process),
        },
        ProjectEvent::PresetDeleted {
            preset_id: PresetId("p1".into()),
        },
    ];
    for event in events {
        assert!(
            matches!(classify(&project, &event), HistoryEffect::Neutral),
            "{event:?} has to be neutral"
        );
    }
}

#[test]
fn adding_and_removing_an_object_are_undoable() {
    let state = StateLayer::new();
    let object_id = add_test_object(&state);
    move_object(&state, &object_id);
    state.apply(ProjectEvent::ObjectRemoved {
        object_id: object_id.clone(),
    });

    state.undo().expect("undo the removal");
    assert_eq!(object(&state, &object_id).transform, MOVED);
    state.undo().expect("undo the move");
    state.undo().expect("undo the add");
    assert!(!state.snapshot().objects.contains_key(&object_id));
    assert!(!state.can_undo());
}

#[test]
fn transform_undo_then_redo_walks_the_object_back_and_forth() {
    let state = StateLayer::new();
    let object_id = add_test_object(&state);
    move_object(&state, &object_id);
    assert_eq!(object(&state, &object_id).transform, MOVED);

    state.undo().expect("undo available");
    assert_eq!(object(&state, &object_id).transform, IDENTITY);

    state.redo().expect("redo available");
    assert_eq!(object(&state, &object_id).transform, MOVED);
}

#[test]
fn seam_paint_undo_restores_the_previous_blob_and_target() {
    let state = StateLayer::new();
    let object_id = add_test_object(&state);
    state.apply(ProjectEvent::ObjectSeamFacetsSet {
        object_id: object_id.clone(),
        facets_rle: Some("first".into()),
        target_point: Some("{\"x\":1}".into()),
    });
    state.apply(ProjectEvent::ObjectSeamFacetsSet {
        object_id: object_id.clone(),
        facets_rle: Some("second".into()),
        target_point: None,
    });

    state.undo().expect("undo available");
    let obj = object(&state, &object_id);
    assert_eq!(obj.seam_facets_rle.as_deref(), Some("first"));
    assert_eq!(obj.seam_target_point.as_deref(), Some("{\"x\":1}"));

    state.redo().expect("redo available");
    let obj = object(&state, &object_id);
    assert_eq!(obj.seam_facets_rle.as_deref(), Some("second"));
    assert_eq!(obj.seam_target_point, None);
}

#[test]
fn support_paint_undo_restores_the_previous_blob() {
    let state = StateLayer::new();
    let object_id = add_test_object(&state);
    state.apply(ProjectEvent::ObjectSupportFacetsSet {
        object_id: object_id.clone(),
        facets_rle: Some("blob".into()),
    });

    state.undo().expect("undo available");
    assert_eq!(object(&state, &object_id).support_facets_rle, None);

    state.redo().expect("redo available");
    assert_eq!(
        object(&state, &object_id).support_facets_rle.as_deref(),
        Some("blob")
    );
}

#[test]
fn fan_paint_undo_restores_the_previous_blob() {
    let state = StateLayer::new();
    let object_id = add_test_object(&state);
    state.apply(ProjectEvent::ObjectFanFacetsSet {
        object_id: object_id.clone(),
        facets_rle: Some("AAAA".into()),
    });
    state.apply(ProjectEvent::ObjectFanFacetsSet {
        object_id: object_id.clone(),
        facets_rle: Some("BBBB".into()),
    });

    state.undo().expect("undo available");
    assert_eq!(
        object(&state, &object_id).fan_facets_rle.as_deref(),
        Some("AAAA")
    );

    state.redo().expect("redo available");
    assert_eq!(
        object(&state, &object_id).fan_facets_rle.as_deref(),
        Some("BBBB")
    );
}

#[test]
fn filament_paint_undo_restores_the_previous_blob() {
    let state = StateLayer::new();
    let object_id = add_test_object(&state);
    state.apply(ProjectEvent::ObjectFilamentFacetsSet {
        object_id: object_id.clone(),
        facets_rle: Some("AAAA".into()),
    });
    state.apply(ProjectEvent::ObjectFilamentFacetsSet {
        object_id: object_id.clone(),
        facets_rle: Some("BBBB".into()),
    });

    state.undo().expect("undo available");
    assert_eq!(
        object(&state, &object_id).filament_facets_rle.as_deref(),
        Some("AAAA")
    );

    state.redo().expect("redo available");
    assert_eq!(
        object(&state, &object_id).filament_facets_rle.as_deref(),
        Some("BBBB")
    );
}

#[test]
fn layer_height_profile_undo_restores_the_previous_profile() {
    let state = StateLayer::new();
    let object_id = add_test_object(&state);
    state.apply(ProjectEvent::ObjectLayerHeightProfileSet {
        object_id: object_id.clone(),
        profile: vec![0.0, 0.2, 10.0, 0.1],
    });

    state.undo().expect("undo available");
    assert!(object(&state, &object_id).layer_height_profile.is_empty());

    state.redo().expect("redo available");
    assert_eq!(
        object(&state, &object_id).layer_height_profile,
        vec![0.0, 0.2, 10.0, 0.1]
    );
}

#[test]
fn layer_ranges_undo_restores_the_previous_ranges() {
    let state = StateLayer::new();
    let object_id = add_test_object(&state);
    let range = LayerConfigRange {
        id: "r1".into(),
        z_min: 0.0,
        z_max: 5.0,
        overrides: Default::default(),
        fan: None,
    };
    state.apply(ProjectEvent::ObjectLayerRangesSet {
        object_id: object_id.clone(),
        ranges: vec![range.clone()],
    });

    state.undo().expect("undo available");
    assert!(object(&state, &object_id).layer_ranges.is_empty());

    state.redo().expect("redo available");
    let ranges = object(&state, &object_id).layer_ranges;
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].id, "r1");
}

#[test]
fn adaptive_rule_undo_removes_a_freshly_added_rule() {
    let state = StateLayer::new();
    state.apply(ProjectEvent::AdaptiveRuleSet {
        rule: test_rule("r1", "first"),
    });
    assert_eq!(state.snapshot().adaptive_rules.len(), 1);

    state.undo().expect("undo available");
    assert!(state.snapshot().adaptive_rules.is_empty());

    state.redo().expect("redo available");
    assert_eq!(state.snapshot().adaptive_rules[0].name, "first");
}

#[test]
fn adaptive_rule_undo_restores_the_overwritten_and_the_removed_rule() {
    let state = StateLayer::new();
    state.apply(ProjectEvent::AdaptiveRuleSet {
        rule: test_rule("r1", "first"),
    });
    state.apply(ProjectEvent::AdaptiveRuleSet {
        rule: test_rule("r1", "second"),
    });
    state.apply(ProjectEvent::AdaptiveRuleRemoved {
        rule_id: "r1".into(),
    });
    assert!(state.snapshot().adaptive_rules.is_empty());

    state.undo().expect("undo of the removal");
    assert_eq!(state.snapshot().adaptive_rules[0].name, "second");

    state.undo().expect("undo of the overwrite");
    assert_eq!(state.snapshot().adaptive_rules[0].name, "first");
}

#[test]
fn a_new_change_drops_the_redo_stack() {
    let state = StateLayer::new();
    let object_id = add_test_object(&state);
    move_object(&state, &object_id);
    state.undo().expect("undo available");
    assert!(state.can_redo());

    state.apply(ProjectEvent::ObjectSupportFacetsSet {
        object_id,
        facets_rle: Some("blob".into()),
    });
    assert!(
        !state.can_redo(),
        "a fresh change makes the undone branch unreachable"
    );
}
