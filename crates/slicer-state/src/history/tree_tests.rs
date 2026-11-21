use super::tree::*;

fn step(t: &mut HistoryTree, group: Option<&str>) -> NodeId {
    let meta = NodeMeta {
        group: group.map(str::to_owned),
        at_ms: t.next_id as i64,
        ..Default::default()
    };
    t.record(vec![], vec![], meta)
}

#[test]
fn record_after_going_back_keeps_old_branch() {
    let mut t = HistoryTree::new(0);
    let a = step(&mut t, None);
    let b = step(&mut t, None);
    t.set_head(a);
    step(&mut t, None);
    let forks = t.forks(a);
    assert_eq!(forks.len(), 1);
    assert_eq!((forks[0].child, forks[0].tip, forks[0].steps), (b, b, 1));
}

#[test]
fn path_goes_up_to_common_ancestor_then_down() {
    let mut t = HistoryTree::new(0);
    let a = step(&mut t, None);
    let b = step(&mut t, None);
    let c = step(&mut t, None);
    t.set_head(a);
    let d = step(&mut t, None);
    assert_eq!(
        t.path(c, d),
        Some(vec![Move::Up(c), Move::Up(b), Move::Down(d)])
    );
}

fn grouped() -> HistoryTree {
    let mut t = HistoryTree::new(0);
    step(&mut t, None);
    for _ in 0..3 {
        step(&mut t, Some("g"));
    }
    t
}

#[test]
fn undo_jumps_over_whole_group() {
    let mut t = grouped();
    assert_eq!(t.undo_target(), Some(1));
    t.set_head(1);
    assert_eq!(t.redo_target(), Some(4));
}

#[test]
fn undo_inside_group_steps_one_node() {
    let mut t = grouped();
    t.set_head(3);
    assert_eq!(t.undo_target(), Some(2));
}

#[test]
fn redo_follows_child_we_came_from() {
    let mut t = grouped();
    t.set_head(1);
    let d = step(&mut t, None);
    t.set_head(4);
    t.set_head(d);
    t.set_head(1);
    assert_eq!(t.redo_target(), Some(d));
}

#[test]
fn touching_follows_rebuild_lineage() {
    let mut t = HistoryTree::new(0);
    for (group, id, added, removed) in [
        (None, "A", true, false),
        (Some("g"), "A", false, true),
        (Some("g"), "B", true, false),
        (None, "B", false, false),
    ] {
        let meta = NodeMeta {
            group: group.map(str::to_owned),
            objects: vec![ObjectRef {
                id: id.into(),
                name: id.into(),
            }],
            added: if added { vec![id.into()] } else { vec![] },
            removed: if removed { vec![id.into()] } else { vec![] },
            ..Default::default()
        };
        t.record(vec![], vec![], meta);
    }
    assert_eq!(t.touching("B"), vec![1, 2, 3, 4]);
}

#[test]
fn prune_collapses_oldest_into_root() {
    let mut t = HistoryTree::new(0);
    for _ in 0..2005 {
        step(&mut t, None);
    }
    assert_eq!((t.nodes.len(), t.root(), t.head()), (2000, 6, 2005));
    assert_eq!(t.pruned_until_ms, Some(6));
}

fn paint(tree: &mut HistoryTree, size: usize) -> NodeId {
    tree.record(
        vec![slicer_types::ProjectEvent::ObjectSupportFacetsSet {
            object_id: slicer_types::ObjectId("a".into()),
            facets_rle: Some("x".repeat(size)),
        }],
        vec![],
        NodeMeta {
            at_ms: tree.next_id as i64,
            ..Default::default()
        },
    )
}

#[test]
fn memory_limit_collapses_oldest_paint_steps() {
    let mut tree = HistoryTree::with_limits(2000, 3 << 20);
    for _ in 0..10 {
        paint(&mut tree, 1 << 20);
    }
    assert_eq!((tree.head(), tree.root()), (10, 8));
    assert!(tree.bytes <= 3 << 20);
    assert_eq!(
        tree.bytes,
        tree.nodes.values().map(|n| n.bytes).sum::<usize>()
    );
    assert_eq!(tree.pruned_until_ms, Some(8));
}

#[test]
fn one_huge_step_keeps_itself() {
    let mut tree = HistoryTree::with_limits(2000, 1024);
    paint(&mut tree, 100);
    paint(&mut tree, 2048);
    assert_eq!((tree.root(), tree.head(), tree.nodes.len()), (1, 2, 2));
    assert!(tree.bytes > 1024);
    assert_eq!(tree.undo_target(), Some(1));
    paint(&mut tree, 100);
    assert_eq!(tree.root(), 2);
    assert!(tree.bytes <= 1024);
}

#[test]
fn cloning_tree_shares_nodes() {
    let mut tree = HistoryTree::new(0);
    let id = paint(&mut tree, 1024);
    let copy = tree.clone();
    tree.set_head(0);
    step(&mut tree, None);
    assert!(std::sync::Arc::ptr_eq(&copy.nodes[&id], &tree.nodes[&id]));
    assert_eq!(copy.current_path(), vec![0, id]);
}

#[test]
fn opening_never_expands_beyond_limit() {
    let mut tree = HistoryTree::new(0);
    paint(&mut tree, 1 << 20);
    let mut disk: serde_json::Value =
        serde_json::from_slice(&super::file::encode(&tree, b"p").unwrap()).unwrap();
    let nodes = disk["nodes"].as_array_mut().unwrap();
    for id in 2..=50 {
        let mut node = nodes.last().unwrap().clone();
        nodes.last_mut().unwrap()["redo_child"] = id.into();
        node["id"] = id.into();
        node["parent"] = (id - 1).into();
        nodes.push(node);
    }
    disk["head"] = 50.into();
    disk["next_id"] = 51.into();
    let tree =
        super::file::decode_with_limits(&serde_json::to_vec(&disk).unwrap(), b"p", 2000, 8 << 20)
            .into_tree()
            .unwrap();
    assert_eq!((tree.root(), tree.head()), (43, 50));
    assert!(tree.bytes <= 8 << 20);
    assert_eq!(super::file::EXPANDED_BYTES.get(), tree.bytes);
    let state = crate::StateLayer::new();
    let mut rx = state.subscribe_history();
    let (env, _) = state.replace_state_with_history(Default::default(), None, Some(tree));
    let msg = rx.try_recv().unwrap();
    assert!(msg.pruned);
    assert_eq!(msg.event_id, env.event_id.0);
}

#[test]
fn opening_v1_keeps_literal_markers_in_opaque_blobs() {
    let mut tree = HistoryTree::new(0);
    let recipe = serde_json::json!({"nested":{"$protype_blob":"user data"}});
    tree.record(
        vec![slicer_types::ProjectEvent::ObjectRecipeSet {
            object_id: slicer_types::ObjectId("a".into()),
            recipe: Some(recipe.clone()),
        }],
        vec![],
        NodeMeta::default(),
    );
    let mut disk: serde_json::Value =
        serde_json::from_slice(&super::file::encode(&tree, b"p").unwrap()).unwrap();
    let key = super::blobs::hash_value(&recipe);
    disk["format"] = 1.into();
    disk["blobs"] = serde_json::json!({&key:recipe});
    disk["nodes"][1]["forward"][0]["recipe"] = serde_json::json!({"$protype_blob":key});
    let tree = super::file::decode(&serde_json::to_vec(&disk).unwrap(), b"p")
        .into_tree()
        .unwrap();
    assert_eq!(tree.nodes[&1].recipes().next().unwrap().1, &recipe);
}
