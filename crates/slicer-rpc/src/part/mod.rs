//! `part.*` — CAD parts: a document the UI owns, a program this daemon runs
//! for it, and the objects its bodies are shown by.
//!
//! Every call that changes a part is one transaction. The program runs
//! first, outside the state lock — on a large mesh it takes seconds. The
//! events go in afterwards, under the lock.
//!
//! An edit made on a revision the part has moved on from is refused, and its
//! result is dropped rather than applied over a newer one. That is checked
//! twice, and the two checks are not the same:
//!
//! - `part.set_document` compares its `base_revision` with the part as it is
//!   when the call arrives, before anything runs. A courtesy to the client,
//!   which hears "stale edit" at once instead of after the whole program, and
//!   no more: the part can still move on while the program runs.
//! - `commit` compares it again under the state lock, at the moment the events
//!   would be applied (`check_base`), and applies nothing if the part moved on
//!   meanwhile. That is the guarantee. `part.create_from_object` has no
//!   revision to compare, and checks in the same two places that its object
//!   still exists and is not already a body.

pub mod executor;
pub mod frame;
pub mod kernel;
pub mod program;
pub mod reconcile;

use serde::Deserialize;
use serde_json::{json, Value};
use slicer_state::project::ProjectState;
use slicer_state::{ObjectRecord, StateLayer, Txn};
use slicer_types::{
    KernelProgram, ObjectId, PartBody, PartId, PartSnapshot, PartSource, ProjectEvent,
};
use ulid::Ulid;

use crate::error::RpcError;
use executor::Execution;
use kernel::{LibKernel, NodeCache, PartKernel};
use program::Validated;
use reconcile::{reconcile, ReconcileInput};

/// The longest a part's name may be. Counted in characters, not bytes, like a
/// body's name, which has the same limit.
pub const MAX_PART_NAME_LEN: usize = 256;

#[derive(Debug, Deserialize)]
struct CreateParams {
    name: String,
    document: Value,
    program: KernelProgram,
}

#[derive(Debug, Deserialize)]
struct CreateFromObjectParams {
    object_id: ObjectId,
    body_key: String,
    name: String,
    document: Value,
    program: KernelProgram,
}

#[derive(Debug, Deserialize)]
struct SetDocumentParams {
    part_id: PartId,
    base_revision: String,
    document: Value,
    program: KernelProgram,
}

#[derive(Debug, Deserialize)]
struct PartIdParams {
    part_id: PartId,
}

/// Where a regeneration starts from — checked again under the state lock.
enum Base {
    New,
    Existing {
        revision: String,
    },
    Adopt {
        object_id: ObjectId,
        body_key: String,
    },
}

impl Base {
    /// What `part.create_from_object` adopts: the key of the body, and the
    /// existing object that comes to show it.
    fn adopted(&self) -> Option<(&str, &ObjectId)> {
        match self {
            Base::Adopt {
                object_id,
                body_key,
            } => Some((body_key.as_str(), object_id)),
            Base::New | Base::Existing { .. } => None,
        }
    }
}

struct Regeneration {
    part_id: PartId,
    name: String,
    document: Value,
    program: KernelProgram,
    sources: Vec<PartSource>,
    base: Base,
}

pub fn handle_create(state: &StateLayer, params: Value) -> Result<Value, RpcError> {
    handle_create_with(state, params, &LibKernel, &default_cache()?)
}

pub fn handle_create_from_object(state: &StateLayer, params: Value) -> Result<Value, RpcError> {
    handle_create_from_object_with(state, params, &LibKernel, &default_cache()?)
}

pub fn handle_set_document(state: &StateLayer, params: Value) -> Result<Value, RpcError> {
    handle_set_document_with(state, params, &LibKernel, &default_cache()?)
}

pub fn handle_create_with(
    state: &StateLayer,
    params: Value,
    kernel: &dyn PartKernel,
    cache: &NodeCache,
) -> Result<Value, RpcError> {
    let p: CreateParams = parse(params)?;
    check_part_name(&p.name)?;
    regenerate(
        state,
        Regeneration {
            part_id: new_part_id(),
            name: p.name,
            document: p.document,
            program: p.program,
            sources: Vec::new(),
            base: Base::New,
        },
        kernel,
        cache,
    )
}

pub fn handle_create_from_object_with(
    state: &StateLayer,
    params: Value,
    kernel: &dyn PartKernel,
    cache: &NodeCache,
) -> Result<Value, RpcError> {
    let p: CreateFromObjectParams = parse(params)?;
    check_part_name(&p.name)?;
    let snap = state.snapshot();
    let object = snap
        .objects
        .get(&p.object_id)
        .ok_or_else(|| object_not_found(&p.object_id))?;
    if object.body.is_some() {
        return Err(already_a_body(&p.object_id));
    }
    // The object's own source, as imported — never a materialized cut. Its
    // op stack, if any, comes back as features of the document.
    let source = PartSource {
        id: p.object_id.0.clone(),
        source_path: object.source_path.clone(),
        source_hash: object.source_hash.clone(),
        step_part_index: object.step_part_index,
        triangle_count: object.triangle_count,
        bounds: object.bounds.clone(),
    };
    regenerate(
        state,
        Regeneration {
            part_id: new_part_id(),
            name: p.name,
            document: p.document,
            program: p.program,
            sources: vec![source],
            base: Base::Adopt {
                object_id: p.object_id,
                body_key: p.body_key,
            },
        },
        kernel,
        cache,
    )
}

pub fn handle_set_document_with(
    state: &StateLayer,
    params: Value,
    kernel: &dyn PartKernel,
    cache: &NodeCache,
) -> Result<Value, RpcError> {
    let p: SetDocumentParams = parse(params)?;
    let snap = state.snapshot();
    let part = snap
        .parts
        .get(&p.part_id)
        .ok_or_else(|| part_not_found(&p.part_id))?;
    let base = Base::Existing {
        revision: p.base_revision,
    };
    // The first of two checks, so that a client on a revision the part has left
    // does not wait out a whole program (seconds a B-rep node) to hear "stale
    // edit". Only a courtesy: the program runs outside the state lock, and the
    // part can move on while it does. `commit` makes the check again under the
    // lock, and that one is the guarantee.
    check_base(&snap, &p.part_id, &base)?;
    regenerate(
        state,
        Regeneration {
            part_id: p.part_id.clone(),
            name: part.name.clone(),
            document: p.document,
            program: p.program,
            sources: part.sources.clone(),
            base,
        },
        kernel,
        cache,
    )
}

/// Remove a part and every object its bodies are shown by — one step.
pub fn handle_remove(state: &StateLayer, params: Value) -> Result<Value, RpcError> {
    let p: PartIdParams = parse(params)?;
    state.transaction(|txn| {
        let part = txn
            .project()
            .parts
            .get(&p.part_id)
            .cloned()
            .ok_or_else(|| part_not_found(&p.part_id))?;
        let live: Vec<ObjectId> = part
            .bodies
            .iter()
            .filter_map(|b| b.object_id.clone())
            .filter(|id| txn.project().objects.contains_key(id))
            .collect();
        for object_id in &live {
            txn.apply(ProjectEvent::ObjectRemoved {
                object_id: object_id.clone(),
            });
        }
        txn.apply(ProjectEvent::PartRemoved {
            part_id: p.part_id.clone(),
        });
        Ok(json!({ "removed_object_ids": live }))
    })
}

pub fn handle_get(state: &StateLayer, params: Value) -> Result<Value, RpcError> {
    let p: PartIdParams = parse(params)?;
    let snap = state.snapshot();
    let part = snap
        .parts
        .get(&p.part_id)
        .ok_or_else(|| part_not_found(&p.part_id))?;
    serde_json::to_value(part).map_err(|e| RpcError::Internal(format!("cannot encode part: {e}")))
}

pub fn handle_list(state: &StateLayer) -> Result<Value, RpcError> {
    let snap = state.snapshot();
    let parts: Vec<&PartSnapshot> = snap.parts.values().collect();
    Ok(json!({ "parts": parts }))
}

/// `object.set_operations` on an object that shows a body of a part: refused.
///
/// A body's holes are `hole` nodes of its part's program, and every
/// regeneration writes an empty stack into the geometry of its bodies'
/// objects. A stack set on such an object directly would be gone at the next
/// edit of the part without a word, so it is refused up front.
pub(crate) fn reject_operations_on_body(object: &ObjectRecord) -> Result<(), RpcError> {
    let Some(body) = &object.body else {
        return Ok(());
    };
    Err(RpcError::Project(format!(
        "object {:?} shows body {} of part {:?}: a body's holes are edited in its part \
         (part.set_document), not on the object",
        object.id, body.body_key, body.part_id
    )))
}

/// The name a request gives a new part: 1 to [`MAX_PART_NAME_LEN`] characters,
/// not blank. Only `part.create` and `part.create_from_object` name a part; an
/// edit keeps the stored name, so a stored one is never held to this again.
fn check_part_name(name: &str) -> Result<(), RpcError> {
    if program::is_valid_name(name, MAX_PART_NAME_LEN) {
        return Ok(());
    }
    Err(RpcError::Project(format!(
        "the part name must be 1 to {MAX_PART_NAME_LEN} characters"
    )))
}

/// Runs the program, then puts its bodies on the plate.
fn regenerate(
    state: &StateLayer,
    r: Regeneration,
    kernel: &dyn PartKernel,
    cache: &NodeCache,
) -> Result<Value, RpcError> {
    let validated = check_request(&r)?;
    let execution = run_program(&r, &validated, kernel, cache)?;
    state.transaction(|txn| commit(txn, r, execution))
}

/// What a request is refused for before any kernel runs: a document that is
/// too large, a program that does not validate, an adopted body the program
/// does not have.
fn check_request(r: &Regeneration) -> Result<Validated, RpcError> {
    check_document_size(&r.document)?;
    let validated = program::validate(&r.program, &r.sources).map_err(RpcError::Project)?;
    if let Some((body_key, _)) = r.base.adopted() {
        if !r.program.bodies.contains_key(body_key) {
            return Err(RpcError::Project(format!(
                "body {body_key} is not in the program"
            )));
        }
    }
    Ok(validated)
}

fn check_document_size(document: &Value) -> Result<(), RpcError> {
    let bytes = serde_json::to_vec(document)
        .map_err(|e| RpcError::Project(format!("invalid document: {e}")))?
        .len();
    if bytes > program::MAX_DOCUMENT_BYTES {
        return Err(RpcError::Project(format!(
            "the document is {bytes} bytes; a part's document is at most {} bytes",
            program::MAX_DOCUMENT_BYTES
        )));
    }
    Ok(())
}

/// Runs the program outside the state lock, and refuses a run that left the
/// request without the bodies it needs.
fn run_program(
    r: &Regeneration,
    validated: &Validated,
    kernel: &dyn PartKernel,
    cache: &NodeCache,
) -> Result<Execution, RpcError> {
    let keys = program::node_keys(&r.program, validated, &r.sources);
    let execution = executor::execute(&r.program, validated, &keys, &r.sources, kernel, cache);
    if has_needed_bodies(&r.base, &execution) {
        Ok(execution)
    } else {
        Err(did_not_build(&execution))
    }
}

/// A new part needs a body to show and an adopted object the body it is to
/// show. An edit of a part needs none: a body that did not build leaves its
/// object as it was.
fn has_needed_bodies(base: &Base, execution: &Execution) -> bool {
    match base {
        Base::New => !execution.outputs.is_empty(),
        Base::Adopt { body_key, .. } => execution.outputs.contains_key(body_key),
        Base::Existing { .. } => true,
    }
}

/// Applies a run to the project: the events that bring the plate in line,
/// then the part itself. Runs under the state lock.
fn commit(txn: &mut Txn<'_>, r: Regeneration, execution: Execution) -> Result<Value, RpcError> {
    // Everything that can refuse the edit is checked before the first
    // event: a transaction has no rollback.
    let previous = check_base(txn.project(), &r.part_id, &r.base)?;
    let reconciled = reconcile(
        &ReconcileInput {
            project: txn.project(),
            part_id: &r.part_id,
            program: &r.program,
            previous: &previous,
            outputs: &execution.outputs,
            adopt: r.base.adopted(),
        },
        new_object_id,
    );
    for event in reconciled.events {
        txn.apply(event);
    }
    let part = PartSnapshot {
        part_id: r.part_id,
        name: r.name,
        revision: Ulid::new().to_string(),
        document: r.document,
        program: r.program,
        sources: r.sources,
        bodies: reconciled.bodies,
        statuses: execution.statuses,
    };
    let reply = response(&part, &reconciled.new_objects, &reconciled.paint_cleared);
    txn.apply(ProjectEvent::PartSet { part });
    Ok(reply)
}

/// What the part's bodies were before this edit — or why the edit no
/// longer applies. The call in `commit`, under the state lock, is the
/// guarantee: an edit another one overtook during the run is refused there.
/// `part.set_document` also calls it on a snapshot before the run, only so
/// that an edit already stale fails fast — never move the lock-time call.
fn check_base(
    project: &ProjectState,
    part_id: &PartId,
    base: &Base,
) -> Result<Vec<PartBody>, RpcError> {
    match base {
        Base::New => {
            if project.parts.contains_key(part_id) {
                return Err(RpcError::Project(format!(
                    "part {part_id:?} already exists"
                )));
            }
            Ok(Vec::new())
        }
        Base::Existing { revision } => {
            let part = project
                .parts
                .get(part_id)
                .ok_or_else(|| part_not_found(part_id))?;
            if &part.revision != revision {
                return Err(RpcError::Project(format!(
                    "stale edit: part {part_id:?} is at revision {}, the edit was made on {revision}",
                    part.revision
                )));
            }
            Ok(part.bodies.clone())
        }
        Base::Adopt { object_id, .. } => {
            let object = project
                .objects
                .get(object_id)
                .ok_or_else(|| object_not_found(object_id))?;
            if object.body.is_some() {
                return Err(already_a_body(object_id));
            }
            Ok(Vec::new())
        }
    }
}

fn did_not_build(execution: &Execution) -> RpcError {
    let reasons: Vec<String> = execution
        .statuses
        .iter()
        .filter_map(|s| s.message.as_ref().map(|m| format!("{}: {m}", s.tag)))
        .collect();
    RpcError::Project(format!("the part did not build — {}", reasons.join("; ")))
}

fn response(
    snapshot: &PartSnapshot,
    new_objects: &[ObjectId],
    paint_cleared: &[ObjectId],
) -> Value {
    let bodies: Vec<Value> = snapshot
        .bodies
        .iter()
        .map(|b| {
            json!({
                "key": b.key,
                "object_id": b.object_id,
                "representation": b.geometry.as_ref().map(|g| g.representation),
                "triangle_count": b.geometry.as_ref().map(|g| g.triangle_count),
                "source_path": b.geometry.as_ref().map(|g| g.source_path.clone()),
            })
        })
        .collect();
    json!({
        "part_id": snapshot.part_id,
        "revision": snapshot.revision,
        "bodies": bodies,
        "statuses": snapshot.statuses,
        "new_object_ids": new_objects,
        "paint_cleared": paint_cleared,
    })
}

fn default_cache() -> Result<NodeCache, RpcError> {
    let root = slicer_state::object_cache_dir()
        .map_err(|e| RpcError::Internal(format!("cannot resolve object cache: {e}")))?;
    Ok(NodeCache::new(root))
}

fn new_part_id() -> PartId {
    PartId(format!("part-{}", Ulid::new()))
}

fn new_object_id() -> ObjectId {
    ObjectId(format!("obj-{}", Ulid::new()))
}

fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::Project(format!("invalid params: {e}")))
}

fn part_not_found(part_id: &PartId) -> RpcError {
    RpcError::Project(format!("part not found: {part_id:?}"))
}

fn object_not_found(object_id: &ObjectId) -> RpcError {
    RpcError::Project(format!("object_id not found: {object_id:?}"))
}

fn already_a_body(object_id: &ObjectId) -> RpcError {
    RpcError::Project(format!(
        "object {object_id:?} is already the body of a part"
    ))
}
