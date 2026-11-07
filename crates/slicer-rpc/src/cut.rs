//! `object.cut_with_plane`: replace an object with the sides a plane cuts it
//! into.
//!
//! A cut is an action on the project and not an op of the stack: the halves
//! are new objects over meshes of their own, with empty stacks, and their
//! lineage stops at the cut. Slicing reads STL here, so a STEP part's B-rep
//! ends at it too.

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use slicer_state::project::ObjectRecord;
use slicer_state::StateLayer;
use slicer_types::{Bounds, ObjectId, ProjectEvent, SplitPartPayload};
use ulid::Ulid;

use crate::dispatch::{
    materialize_operations, resolve_slice_part_index, transform_point, STEP_FFI_LOCK,
};
use crate::error::RpcError;

#[derive(Deserialize)]
pub(crate) struct CutWithPlaneParams {
    object_id: ObjectId,
    /// A point on the plane, in the part's own frame: XY centred on the
    /// bounding box of the mesh it prints as, lowest point at z=0.
    origin: [f32; 3],
    /// Which way the plane faces. The side it points to is the upper one.
    normal: [f32; 3],
    /// `"both"`, `"upper"` or `"lower"`.
    #[serde(default = "keep_both")]
    keep: String,
}

fn keep_both() -> String {
    "both".to_string()
}

/// The transform that leaves a half where it was on the plate.
///
/// The slicer and the window each recentre a mesh on its own bounding box
/// before its transform: XY centre to 0, lowest point to z=0. `bounds` is the
/// half's box in `parent_transform`'s frame, so the offset that recentring
/// takes off has to go back in front of it, or every half lands on the same
/// spot on the bed.
fn half_transform(parent_transform: &[f32; 16], bounds: &Bounds) -> [f32; 16] {
    let recentred_from = [
        (bounds.min[0] + bounds.max[0]) / 2.0,
        (bounds.min[1] + bounds.max[1]) / 2.0,
        bounds.min[2],
    ];
    let moved = transform_point(parent_transform, recentred_from);
    let mut transform = *parent_transform;
    transform[12..15].copy_from_slice(&moved);
    transform
}

/// What `object` carries that does not survive a cut, by name. Paint is
/// indexed by the triangles of the old mesh and height settings by its
/// heights; a recipe and a suppressed op describe a mesh that is gone. Undo
/// brings them all back.
fn left_behind(object: &ObjectRecord) -> Vec<&'static str> {
    [
        (object.recipe.is_some(), "recipe"),
        (
            object.operations.iter().any(|op| op.suppressed),
            "suppressed_operations",
        ),
        (object.seam_facets_rle.is_some(), "seam_paint"),
        (object.seam_target_point.is_some(), "seam_target_point"),
        (object.support_facets_rle.is_some(), "support_paint"),
        (object.fan_facets_rle.is_some(), "fan_paint"),
        (object.filament_facets_rle.is_some(), "filament_paint"),
        (
            !object.layer_height_profile.is_empty(),
            "layer_height_profile",
        ),
        (!object.layer_ranges.is_empty(), "layer_ranges"),
    ]
    .into_iter()
    .filter_map(|(present, name)| present.then_some(name))
    .collect()
}

/// Whether `now` is still the mesh `before` was cut from, in the place it was.
fn is_same_geometry(now: &ObjectRecord, before: &ObjectRecord) -> bool {
    now.source_hash == before.source_hash
        && now.operations == before.operations
        && now.transform == before.transform
        && now.body.is_none()
}

fn cut_error(e: impl std::fmt::Display) -> RpcError {
    RpcError::Project(format!("cut failed: {e}"))
}

/// Cut the object, and swap it for its sides in one step of history.
///
/// The mesh cut is the one the object prints as: the holes of its stack are in
/// the sides, which is why the sides need no stack. An op that stack could not
/// cut is gone with it, so the reply names those, with why. `new_object_ids`
/// are the upper side, then the lower one.
pub(crate) async fn cut_with_plane(
    state: &StateLayer,
    params: CutWithPlaneParams,
) -> Result<Value, RpcError> {
    let snapshot = state.snapshot();
    let object = snapshot
        .objects
        .get(&params.object_id)
        .ok_or_else(|| RpcError::Project(format!("object_id not found: {:?}", params.object_id)))?;
    if let Some(body) = &object.body {
        return Err(RpcError::Project(format!(
            "object {:?} shows body {} of part {:?}: its geometry belongs to the part \
             (part.set_document), so it cannot be cut as an object",
            object.id, body.body_key, body.part_id
        )));
    }

    let mesh = materialize_operations("cut_with_plane", object, &object.operations).await?;
    let part_index = resolve_slice_part_index(object, &mesh.path);

    // Named for this cut and not for the object: undo brings an object back
    // under its old id, and a second cut of it must not write over the files
    // the redo stack still names. The halves keep their paths for good, so
    // this is not scratch space either.
    let output_dir = slicer_state::object_cache_dir()
        .map_err(cut_error)?
        .join(format!("cut-{}", Ulid::new()));
    std::fs::create_dir_all(&output_dir).map_err(cut_error)?;

    // A stack's cut is in the frame of the holes already. Its own box may not
    // be the part's any more, and the window shows it as it is.
    let recenter = mesh.path == object.source_path;
    let source = mesh.path.clone();
    let directory = output_dir.to_string_lossy().into_owned();
    let (origin, normal, keep) = (params.origin, params.normal, params.keep.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        // See STEP_FFI_LOCK: the bridge may load a STEP source here.
        let _step_lock = STEP_FFI_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slicer_ffi::cut_model_with_plane(
            &source, part_index, recenter, origin, normal, &keep, &directory,
        )
    })
    .await
    .map_err(|e| RpcError::Project(format!("cut task failed: {e}")))?;
    if !outcome.success {
        let _ = std::fs::remove_dir_all(&output_dir);
        return Err(cut_error(outcome.error_message));
    }

    // Everything that can fail happens here, so that the transaction below
    // cannot stop halfway through replacing the object.
    let part_count = outcome.parts.len() as u32;
    let mut halves = Vec::with_capacity(outcome.parts.len());
    for part in &outcome.parts {
        let bytes = std::fs::read(&part.out_path).map_err(cut_error)?;
        let bounds = Bounds {
            min: part.bounds_min,
            max: part.bounds_max,
        };
        let transform = half_transform(&object.transform, &bounds);
        let object_id = ObjectId(format!("obj-{}", Ulid::new()));
        halves.push((
            object_id.clone(),
            ProjectEvent::ObjectAdded {
                object_id,
                name: format!("{} - Part {}", object.name, part.part_index + 1),
                source_path: part.out_path.clone(),
                triangle_count: part.triangle_count,
                bounds,
                source_hash: hex::encode(Sha256::digest(&bytes)),
                step_part: None,
                split_part: Some(SplitPartPayload {
                    source_object_id: params.object_id.clone(),
                    part_index: part.part_index,
                    part_count,
                }),
                transform: Some(transform),
            },
        ));
    }

    let (new_object_ids, left) = state.transaction(|txn| {
        // The cut took a while, and another connection may have got here
        // first: these halves are of the object as it was. What they inherit
        // is read here, under the lock, for the same reason.
        let current = txn
            .project()
            .objects
            .get(&params.object_id)
            .filter(|now| is_same_geometry(now, object))
            .ok_or_else(|| {
                RpcError::Project(format!(
                    "cut failed: object {:?} changed during the cut, nothing was replaced",
                    params.object_id
                ))
            })?;
        let overrides = current.overrides.clone();
        let left = left_behind(current);

        let mut ids = Vec::with_capacity(halves.len());
        for (id, added) in halves {
            txn.apply(added);
            for (key, value) in &overrides {
                txn.apply(ProjectEvent::ObjectOverrideSet {
                    object_id: id.clone(),
                    key: key.clone(),
                    new_value: Some(value.clone()),
                });
            }
            ids.push(id);
        }
        txn.apply(ProjectEvent::ObjectRemoved {
            object_id: params.object_id.clone(),
        });
        Ok::<_, RpcError>((ids, left))
    })?;

    Ok(json!({
        "source_object_id": params.object_id,
        "new_object_ids": new_object_ids,
        "skipped_op_ids": mesh.skipped_ops.iter().map(|op| op.id.clone()).collect::<Vec<_>>(),
        "skipped_ops": mesh.skipped_ops,
        "left_behind": left,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(min: [f32; 3], max: [f32; 3]) -> Bounds {
        Bounds { min, max }
    }

    #[test]
    fn a_half_at_the_centre_of_the_parent_keeps_its_transform_but_for_height() {
        let mut parent = [0.0; 16];
        (parent[0], parent[5], parent[10], parent[15]) = (1.0, 1.0, 1.0, 1.0);
        (parent[12], parent[13]) = (100.0, 50.0);

        let half = half_transform(&parent, &bounds([-5.0, -5.0, 12.0], [5.0, 5.0, 20.0]));

        assert_eq!(half[12..15], [100.0, 50.0, 12.0]);
        assert_eq!(half[0..12], parent[0..12]);
    }

    #[test]
    fn the_offset_goes_through_the_parents_turn_and_scale() {
        // A quarter turn about Z, doubled: the part's +X runs along +Y.
        let mut parent = [0.0; 16];
        (parent[1], parent[4], parent[10], parent[15]) = (2.0, -2.0, 2.0, 1.0);
        (parent[12], parent[13], parent[14]) = (10.0, 20.0, 30.0);

        let half = half_transform(&parent, &bounds([1.0, -1.0, 4.0], [5.0, 1.0, 8.0]));

        assert_eq!(half[12..15], [10.0, 26.0, 38.0]);
    }
}
