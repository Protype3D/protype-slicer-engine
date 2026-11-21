use super::*;

#[test]
fn goto_other_branch_equals_replaying_it() {
    let state = StateLayer::new();
    let id = add_test_object(&state);
    move_object(&state, &id);
    state.goto(1).unwrap();
    state.apply(ProjectEvent::SettingChanged {
        scope: SettingScope::Process,
        key: "layer_height".into(),
        old_value: None,
        new_value: SettingValue::Number(0.35),
    });
    let target = state.snapshot();
    state.goto(2).unwrap();
    state.goto(3).unwrap();
    let replay = state.snapshot();
    assert_eq!(target.objects, replay.objects);
    assert_eq!(target.settings, replay.settings);
}

// Head 1 references a real mesh. Head 2 removes the object, then its file disappears.
fn removed_mesh(state: &StateLayer) -> (ObjectId, std::path::PathBuf) {
    let path = std::env::temp_dir().join(ulid::Ulid::new().to_string());
    std::fs::write(&path, b"mesh").unwrap();
    let id = ObjectId("mesh".into());
    state.apply(ProjectEvent::ObjectAdded {
        object_id: id.clone(),
        name: "mesh".into(),
        source_path: path.to_string_lossy().into_owned(),
        bounds: Bounds {
            min: [0.; 3],
            max: [1.; 3],
        },
        triangle_count: 1,
        source_hash: "hash".into(),
        step_part: None,
        split_part: None,
        transform: None,
    });
    state.apply(ProjectEvent::ObjectRemoved {
        object_id: id.clone(),
    });
    std::fs::remove_file(&path).unwrap();
    (id, path)
}

#[test]
fn goto_refuses_when_mesh_file_missing() {
    let state = StateLayer::new();
    let (_, path) = removed_mesh(&state);
    state.apply(ProjectEvent::SettingChanged {
        scope: SettingScope::Process,
        key: "layer_height".into(),
        old_value: None,
        new_value: SettingValue::Number(0.45),
    });
    let before = state.snapshot();
    let mut rx = state.subscribe();
    let mut history = state.subscribe_history();
    let before_id = state.current_event_id();
    assert!(
        matches!(state.goto(1),Err(crate::GotoError::FilesMissing(paths)) if paths==vec![path.to_string_lossy()])
    );
    assert_eq!(state.history_view(None, 10, None).head, 3);
    assert_eq!(state.snapshot().settings, before.settings);
    assert!(state.snapshot().objects.is_empty());
    assert_eq!(state.current_event_id(), before_id);
    assert!(rx.try_recv().is_err());
    assert!(history.try_recv().is_err());
}

#[test]
fn goto_passes_through_state_with_missing_file_when_target_does_not_need_it() {
    let state = StateLayer::new();
    removed_mesh(&state);
    // The path 2 -> 1 -> 0 restores the object and removes it again.
    assert!(state.goto(0).unwrap().applied);
    assert_eq!(state.history_view(None, 10, None).head, 0);
    assert!(state.snapshot().objects.is_empty());
}

#[test]
fn undo_restores_object_even_if_its_file_is_gone() {
    let state = StateLayer::new();
    let (id, path) = removed_mesh(&state);
    state.undo().unwrap();
    assert_eq!(
        state.snapshot().objects[&id].source_path,
        path.to_string_lossy()
    );
    assert_eq!(state.history_view(None, 10, None).head, 1);
}

#[test]
fn reset_inside_transaction_starts_new_tree() {
    let state = StateLayer::new();
    add_test_object(&state);
    let mut history = state.subscribe_history();
    state.transaction(|tx| {
        tx.apply(ProjectEvent::ProjectCreated {
            meta: ProjectMeta {
                title: "new".into(),
                description: None,
                created_at: 0,
                modified_at: 0,
                schema_version: 1,
            },
        });
    });
    let list = state.history_view(None, 10, None);
    assert_eq!((list.head, list.nodes.len()), (0, 1));
    assert_eq!(
        history.try_recv().unwrap().cause,
        slicer_types::HistoryCause::Reset
    );
}

#[test]
fn history_changed_follows_every_record_and_goto() {
    let state = StateLayer::new();
    let mut rx = state.subscribe_history();
    add_test_object(&state);
    state.goto(0).unwrap();
    let mut changes = vec![];
    while let Ok(change) = rx.try_recv() {
        changes.push((change.previous_head, change.head, change.cause));
    }
    use slicer_types::HistoryCause::*;
    assert_eq!(changes, vec![(0, 1, Record), (1, 0, Goto)]);
}

#[test]
fn inverse_uses_the_scene_under_the_lock_when_another_client_has_moved_the_object() {
    let state = StateLayer::new();
    let id = add_test_object(&state);
    move_object(&state, &id);
    let mut next = MOVED;
    next[12] = 90.;
    state.apply(ProjectEvent::ObjectTransformed {
        object_id: id.clone(),
        old_transform: IDENTITY,
        new_transform: next,
    });
    state.goto(2).unwrap();
    assert_eq!(state.snapshot().objects[&id].transform, MOVED);
}

#[test]
fn record_that_prunes_says_so() {
    let state = StateLayer::new();
    state.inner.lock().history = crate::history::tree::HistoryTree::with_limits(2, usize::MAX);
    let mut rx = state.subscribe_history();
    let id = add_test_object(&state);
    assert!(!rx.try_recv().unwrap().pruned);
    move_object(&state, &id);
    assert!(rx.try_recv().unwrap().pruned);
    state.undo().unwrap();
    assert!(!rx.try_recv().unwrap().pruned);
}

#[test]
fn history_changed_carries_event_id() {
    let state = StateLayer::new();
    let mut rx = state.subscribe_history();
    let id = add_test_object(&state);
    assert_eq!(rx.try_recv().unwrap().event_id, state.current_event_id().0);
    move_object(&state, &id);
    state.goto(0).unwrap();
    assert!(rx.try_recv().unwrap().event_id < state.current_event_id().0);
    assert_eq!(rx.try_recv().unwrap().event_id, state.current_event_id().0);
}
