//! Bodies → objects on the plate. Pure: reads the project and a run's
//! outputs and returns the events that bring the objects in line, so every
//! rule is tested without a kernel or a dispatcher.

use std::collections::BTreeMap;

use slicer_state::project::ProjectState;
use slicer_state::ObjectRecord;
use slicer_types::{
    BodyGeometry, BodyRef, KernelProgram, ObjectGeometry, ObjectId, PartBody, PartId, ProgramBody,
    ProjectEvent, StepPartPayload,
};

use super::frame;

pub struct ReconcileInput<'a> {
    pub project: &'a ProjectState,
    pub part_id: &'a PartId,
    pub program: &'a KernelProgram,
    /// The part's bodies before this run; empty for a new part.
    pub previous: &'a [PartBody],
    pub outputs: &'a BTreeMap<String, BodyGeometry>,
    /// `part.create_from_object`: this body key is shown by this existing
    /// object, whose current geometry sits at design offset zero.
    pub adopt: Option<(&'a str, &'a ObjectId)>,
}

#[derive(Debug, Default)]
pub struct Reconciled {
    /// In the order to apply them: per body in key order — for a new object
    /// `ObjectAdded` before the `ObjectGeometryUpdated` that binds it — then
    /// the removals.
    pub events: Vec<ProjectEvent>,
    /// The part's bodies after this run: one per body of the program, in key
    /// order.
    pub bodies: Vec<PartBody>,
    /// The ids `new_id` handed out, one per `ObjectAdded` in `events`.
    pub new_objects: Vec<ObjectId>,
    /// Objects whose paint `events` clear because their mesh changed.
    pub paint_cleared: Vec<ObjectId>,
}

/// Where the part's design frame sits on the plate: the transform of a live
/// object and the design anchor of the body it shows.
type Placement = ([f32; 16], [f64; 3]);

/// Brings the plate in line with one run of the part's program: each body of
/// the program in key order, then the objects of bodies it dropped.
///
/// `new_id` is called once for each object added.
pub fn reconcile(input: &ReconcileInput<'_>, mut new_id: impl FnMut() -> ObjectId) -> Reconciled {
    let mut out = Reconciled::default();
    let placement = placement_reference(input);
    for (key, body) in &input.program.bodies {
        reconcile_body(&mut out, input, placement.as_ref(), key, body, &mut new_id);
    }
    remove_dropped(&mut out, input);
    out
}

/// One body of the program: the [`PartBody`] it leaves and the events its
/// object needs.
fn reconcile_body(
    out: &mut Reconciled,
    input: &ReconcileInput<'_>,
    placement: Option<&Placement>,
    key: &str,
    body: &ProgramBody,
    new_id: &mut impl FnMut() -> ObjectId,
) {
    let previous = input.previous.iter().find(|b| b.key == key);
    let previous_geometry = previous.and_then(|b| b.geometry.as_ref());
    let mapped = previous
        .and_then(|b| b.object_id.clone())
        .or_else(|| adopted_object(input, key));

    let Some(geometry) = input.outputs.get(key) else {
        // Did not build this run: the plate keeps what it shows.
        out.bodies.push(part_body(key, mapped, previous_geometry));
        return;
    };
    let body_ref = BodyRef {
        part_id: input.part_id.clone(),
        body_key: key.to_owned(),
    };
    let object_id = match mapped {
        Some(object_id) => {
            // An id without an object is one the user took off the plate:
            // the mapping stays, nothing is rebuilt for it.
            if let Some(object) = input.project.objects.get(&object_id) {
                update_object(out, object, previous_geometry, geometry, &body_ref);
            }
            object_id
        }
        None => {
            let object_id = new_id();
            add_object(out, &object_id, body, geometry, &body_ref, placement);
            object_id
        }
    };
    out.bodies
        .push(part_body(key, Some(object_id), Some(geometry)));
}

fn part_body(key: &str, object_id: Option<ObjectId>, geometry: Option<&BodyGeometry>) -> PartBody {
    PartBody {
        key: key.to_owned(),
        object_id,
        geometry: geometry.cloned(),
    }
}

/// The object `part.create_from_object` named, when it names this body.
fn adopted_object(input: &ReconcileInput<'_>, key: &str) -> Option<ObjectId> {
    match input.adopt {
        Some((adopted_key, object_id)) if adopted_key == key => Some(object_id.clone()),
        _ => None,
    }
}

/// A body that builds for the first time: its object, then the binding to the
/// body, which `ObjectAdded` cannot carry. The object goes where the design
/// puts it next to what the plate already shows of the part; with none of it
/// there, it has no transform of its own and the UI places it.
fn add_object(
    out: &mut Reconciled,
    object_id: &ObjectId,
    body: &ProgramBody,
    geometry: &BodyGeometry,
    body_ref: &BodyRef,
    placement: Option<&Placement>,
) {
    let transform = placement.map(|(transform, anchor)| {
        frame::shift_transform(
            transform,
            frame::difference(frame::design_anchor(geometry), *anchor),
        )
    });
    out.events.push(ProjectEvent::ObjectAdded {
        object_id: object_id.clone(),
        name: body.name.clone(),
        source_path: geometry.source_path.clone(),
        triangle_count: geometry.triangle_count,
        bounds: geometry.bounds.clone(),
        source_hash: geometry.source_hash.clone(),
        step_part: geometry.step_part_index.map(|part_index| StepPartPayload {
            part_name: body.name.clone(),
            part_index,
        }),
        split_part: None,
        transform,
    });
    out.events.push(ProjectEvent::ObjectGeometryUpdated {
        object_id: object_id.clone(),
        geometry: object_geometry(geometry, body_ref),
    });
    out.new_objects.push(object_id.clone());
}

/// A body the program no longer has takes its object off the plate — unless
/// the user already did.
fn remove_dropped(out: &mut Reconciled, input: &ReconcileInput<'_>) {
    let dropped = input
        .previous
        .iter()
        .filter(|b| !input.program.bodies.contains_key(&b.key));
    for object_id in dropped
        .filter_map(|b| b.object_id.as_ref())
        .filter(|id| input.project.objects.contains_key(*id))
    {
        out.events.push(ProjectEvent::ObjectRemoved {
            object_id: object_id.clone(),
        });
    }
}

fn update_object(
    out: &mut Reconciled,
    object: &ObjectRecord,
    previous: Option<&BodyGeometry>,
    geometry: &BodyGeometry,
    body_ref: &BodyRef,
) {
    let wanted = object_geometry(geometry, body_ref);
    if object.geometry() != wanted {
        let mesh_changed = object.source_hash != geometry.source_hash
            || object.triangle_count != geometry.triangle_count;
        out.events.push(ProjectEvent::ObjectGeometryUpdated {
            object_id: object.id.clone(),
            geometry: wanted,
        });
        if mesh_changed && clear_paint(out, object) {
            out.paint_cleared.push(object.id.clone());
        }
    }
    // Every consumer recentres a mesh on its own bounds, so a body whose
    // anchor moved in the design must move its placement by as much to stay
    // where it was in the world.
    let old_anchor = previous.map_or_else(|| frame::anchor(&object.bounds), frame::design_anchor);
    let delta = frame::difference(frame::design_anchor(geometry), old_anchor);
    if !frame::is_negligible(delta) {
        out.events.push(ProjectEvent::ObjectTransformed {
            object_id: object.id.clone(),
            old_transform: object.transform,
            new_transform: frame::shift_transform(&object.transform, delta),
        });
    }
}

/// Paint is stored per triangle, so a different mesh makes it meaningless.
/// Cleared with events — undone with the rest of the step.
fn clear_paint(out: &mut Reconciled, object: &ObjectRecord) -> bool {
    let mut cleared = false;
    if object.support_facets_rle.is_some() {
        out.events.push(ProjectEvent::ObjectSupportFacetsSet {
            object_id: object.id.clone(),
            facets_rle: None,
        });
        cleared = true;
    }
    if object.seam_facets_rle.is_some() || object.seam_target_point.is_some() {
        out.events.push(ProjectEvent::ObjectSeamFacetsSet {
            object_id: object.id.clone(),
            facets_rle: None,
            target_point: None,
        });
        cleared = true;
    }
    if object.fan_facets_rle.is_some() {
        out.events.push(ProjectEvent::ObjectFanFacetsSet {
            object_id: object.id.clone(),
            facets_rle: None,
        });
        cleared = true;
    }
    if object.filament_facets_rle.is_some() {
        out.events.push(ProjectEvent::ObjectFilamentFacetsSet {
            object_id: object.id.clone(),
            facets_rle: None,
        });
        cleared = true;
    }
    cleared
}

fn object_geometry(geometry: &BodyGeometry, body_ref: &BodyRef) -> ObjectGeometry {
    ObjectGeometry {
        source_path: geometry.source_path.clone(),
        source_hash: geometry.source_hash.clone(),
        triangle_count: geometry.triangle_count,
        bounds: geometry.bounds.clone(),
        step_part_index: geometry.step_part_index,
        operations: Vec::new(),
        body: Some(body_ref.clone()),
    }
}

/// The part's design frame on the plate: the transform and design anchor of
/// a body some live object already shows, or of the object being adopted.
/// `None` when nothing of the part is on the plate yet — then the UI places
/// the new objects itself.
fn placement_reference(input: &ReconcileInput<'_>) -> Option<Placement> {
    let from_bodies = input.previous.iter().find_map(|b| {
        let object = input.project.objects.get(b.object_id.as_ref()?)?;
        Some((object.transform, frame::design_anchor(b.geometry.as_ref()?)))
    });
    from_bodies.or_else(|| {
        let (_, object_id) = input.adopt?;
        let object = input.project.objects.get(object_id)?;
        Some((object.transform, frame::anchor(&object.bounds)))
    })
}
