//! `project.apply_external_edit` — how a part made outside the daemon (by
//! the geometry worker) gets into the project.
//!
//! Shared method 2 of the thin-daemon spec: the daemon and the worker never
//! talk to each other, the client hands the daemon the worker's file, and
//! from then on the daemon owns the mesh. Each added file is copied into the
//! object cache under the SHA-256 of its bytes and the object refers to the
//! copy, so the worker may clean its own cache freely, and save, open and
//! undo — which keep meshes by path (`slicer_state::persist`) — never depend
//! on it.
//!
//! The method knows nothing about CAD or recipes: a recipe is opaque JSON
//! bounded by `validate_recipe`, as in `geometry.build`. `add` and
//! `replace_geometry` exist so far; `remove` and `attach` from the spec join
//! as variants of `Edit`.
//!
//! `replace_geometry` swaps the mesh of an object that is already there. The
//! file is in the frame of the object's current source file, so the placement
//! moves by the change of the mesh's anchor (XY centre, lowest Z) to keep the
//! part where it stood. The feature stack is baked into the new mesh and the
//! recipe no longer describes it, so both go; paint goes too unless the caller
//! keeps it. All of it is one undo step with the rest of the batch.
//!
//! Every edit is checked, copied and measured before the transaction, so a
//! refusal of any of them leaves the project untouched; what depends on the
//! project (the object exists and shows no part's body, `keep_paint` fits)
//! is checked inside the transaction, before its first event. Copies a refused
//! batch already made stay in the cache, where a later add of the same bytes
//! reuses them.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use slicer_ffi::InspectedPart;
use slicer_state::{ObjectRecord, StateLayer};
use slicer_types::{Bounds, ObjectGeometry, ObjectId, ProjectEvent};
use ulid::Ulid;

use crate::dispatch::STEP_FFI_LOCK;
use crate::error::RpcError;
use crate::part::frame;
use crate::part::reconcile::clear_paint_events;

/// Edits one call may carry. A batch is one undo step, and the bound keeps a
/// single call from holding a blocking thread for an unbounded run of copies.
pub const MAX_EDITS: usize = 64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    edits: Vec<Edit>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Edit {
    Add(AddEdit),
    ReplaceGeometry(ReplaceGeometryEdit),
}

/// A new object from a mesh file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddEdit {
    name: String,
    source_path: String,
    #[serde(default)]
    recipe: Option<Value>,
}

/// A new mesh for an object that is already in the project. The file is in
/// the frame of the object's current source file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceGeometryEdit {
    object_id: ObjectId,
    source_path: String,
    /// Keep the object's paint. Needs the same triangle count, since paint
    /// is stored per triangle.
    #[serde(default)]
    keep_paint: bool,
}

/// A file in the daemon's object cache, named by the hash of its bytes.
struct CachedCopy {
    path: PathBuf,
    hash: String,
}

/// An add whose file is cached and measured: all the transaction needs.
struct PreparedAdd {
    object_id: ObjectId,
    name: String,
    copy: CachedCopy,
    triangle_count: u64,
    bounds: Bounds,
    recipe: Option<Value>,
}

impl PreparedAdd {
    fn source_path(&self) -> String {
        self.copy.path.to_string_lossy().into_owned()
    }

    fn summary(&self) -> Value {
        json!({
            "object_id": self.object_id,
            "name": self.name,
            "source_path": self.source_path(),
            "source_hash": self.copy.hash,
            "triangle_count": self.triangle_count,
            "bounds": self.bounds,
        })
    }

    fn into_events(self) -> Vec<ProjectEvent> {
        let source_path = self.source_path();
        let mut events = vec![ProjectEvent::ObjectAdded {
            object_id: self.object_id.clone(),
            name: self.name,
            source_path,
            triangle_count: self.triangle_count,
            bounds: self.bounds,
            source_hash: self.copy.hash,
            step_part: None,
            split_part: None,
            transform: None,
        }];
        if let Some(recipe) = self.recipe {
            events.push(ProjectEvent::ObjectRecipeSet {
                object_id: self.object_id,
                recipe: Some(recipe),
            });
        }
        events
    }
}

/// A replace whose file is cached and measured. What needs the project (the
/// old object) is read inside the transaction.
struct PreparedReplace {
    object_id: ObjectId,
    copy: CachedCopy,
    triangle_count: u64,
    bounds: Bounds,
    keep_paint: bool,
}

impl PreparedReplace {
    fn source_path(&self) -> String {
        self.copy.path.to_string_lossy().into_owned()
    }

    /// The summary of the replace and its events, or why the project refuses
    /// it. Reads `old` only: nothing is applied.
    fn plan(self, index: usize, old: &ObjectRecord) -> Result<(Value, Vec<ProjectEvent>), String> {
        // A body's mesh is its part's: the next regeneration would write the
        // part's mesh back over this one without a word and leave the shifted
        // placement behind (`reject_operations_on_body` refuses a stack for
        // the same reason).
        if let Some(body) = &old.body {
            return Err(format!(
                "edits[{index}]: object {} shows body {} of part {}: a body's mesh is \
                 edited in its part (part.set_document), not on the object",
                self.object_id.0, body.body_key, body.part_id.0
            ));
        }
        if self.keep_paint && old.triangle_count != self.triangle_count {
            return Err(format!(
                "edits[{index}]: keep_paint needs the same triangle count ({} → {})",
                old.triangle_count, self.triangle_count
            ));
        }
        let source_path = self.source_path();
        let summary = json!({
            "object_id": self.object_id,
            "name": old.name,
            "source_path": source_path,
            "source_hash": self.copy.hash,
            "triangle_count": self.triangle_count,
            "bounds": self.bounds,
        });
        let delta = frame::difference(frame::anchor(&self.bounds), frame::anchor(&old.bounds));
        // Mesh and placement go in one event: undo replays them as one, so
        // a client never shows the new mesh with the old placement.
        let transform =
            (!frame::is_negligible(delta)).then(|| frame::shift_transform(&old.transform, delta));
        let mut events = vec![ProjectEvent::ObjectGeometryUpdated {
            object_id: self.object_id.clone(),
            geometry: ObjectGeometry {
                source_path,
                source_hash: self.copy.hash,
                triangle_count: self.triangle_count,
                bounds: self.bounds,
                step_part_index: None,
                operations: Vec::new(),
                body: None,
            },
            transform,
        }];
        if !self.keep_paint {
            events.extend(clear_paint_events(old));
        }
        if old.recipe.is_some() {
            events.push(ProjectEvent::ObjectRecipeSet {
                object_id: self.object_id,
                recipe: None,
            });
        }
        Ok((summary, events))
    }
}

/// An edit ready for the transaction.
enum Prepared {
    Add(PreparedAdd),
    Replace(PreparedReplace),
}

/// Apply a batch of edits as one step of history. Blocking: it copies files
/// and measures them through the bridge, so the dispatcher runs it on the
/// blocking pool.
pub fn handle_apply_external_edit(state: &StateLayer, params: Value) -> Result<Value, RpcError> {
    let p: Params = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    // Everything that needs no file comes first, for the whole batch: a bad
    // recipe in the last edit must not leave copies of the first ones behind.
    let edits = check_edits(p.edits)?;
    let cache_root = slicer_state::object_cache_dir()
        .map_err(|e| RpcError::Internal(format!("cannot resolve object cache: {e}")))?;
    std::fs::create_dir_all(&cache_root)
        .map_err(|e| RpcError::Internal(format!("cannot create object cache: {e}")))?;
    let prepared = edits
        .into_iter()
        .enumerate()
        .map(|(index, edit)| {
            prepare(edit, &cache_root)
                .map_err(|why| RpcError::Project(format!("edits[{index}]: {why}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // The closure holds the state lock: it works through `txn` alone. Every
    // edit is planned against the project as it is before the batch, and only
    // then is anything applied, so a refusal leaves no event behind.
    state.transaction(|txn| {
        let mut objects = Vec::with_capacity(prepared.len());
        let mut events = Vec::new();
        for (index, edit) in prepared.into_iter().enumerate() {
            match edit {
                Prepared::Add(add) => {
                    objects.push(add.summary());
                    events.extend(add.into_events());
                }
                Prepared::Replace(replace) => {
                    let old = txn
                        .project()
                        .objects
                        .get(&replace.object_id)
                        .ok_or_else(|| {
                            RpcError::Project(format!(
                                "edits[{index}]: no object {}",
                                replace.object_id.0
                            ))
                        })?;
                    let (summary, planned) = replace.plan(index, old).map_err(RpcError::Project)?;
                    objects.push(summary);
                    events.extend(planned);
                }
            }
        }
        for event in events {
            txn.apply(event);
        }
        Ok(json!({ "objects": objects }))
    })
}

/// The checks that read no file, for every edit of the batch.
fn check_edits(edits: Vec<Edit>) -> Result<Vec<Edit>, RpcError> {
    if edits.is_empty() {
        return Err(RpcError::Project(
            "edits must hold at least one edit".into(),
        ));
    }
    if edits.len() > MAX_EDITS {
        return Err(RpcError::Project(format!(
            "edits holds {} edits; at most {MAX_EDITS} per call",
            edits.len()
        )));
    }
    let mut replaced: Vec<&ObjectId> = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        match edit {
            Edit::Add(add) => check_add(add),
            Edit::ReplaceGeometry(replace) => {
                // Planned against the project as it is before the batch, a
                // second replace would measure the object by stale bounds.
                if replaced.contains(&&replace.object_id) {
                    Err(format!(
                        "object {} is replaced twice in one call",
                        replace.object_id.0
                    ))
                } else {
                    replaced.push(&replace.object_id);
                    mesh_extension(Path::new(&replace.source_path)).map(|_| ())
                }
            }
        }
        .map_err(|why| RpcError::Project(format!("edits[{index}]: {why}")))?;
    }
    Ok(edits)
}

fn check_add(add: &AddEdit) -> Result<(), String> {
    if add.name.trim().is_empty() {
        return Err("name must not be empty".into());
    }
    if let Some(recipe) = &add.recipe {
        slicer_state::project::validate_recipe(recipe)?;
    }
    mesh_extension(Path::new(&add.source_path))?;
    Ok(())
}

/// The source's extension in lower case. The bridge picks its reader by
/// extension, so the copy keeps it; it may hold only ASCII letters and
/// digits, so the cache name built from it stays one file in the cache.
fn mesh_extension(source: &Path) -> Result<String, String> {
    source
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            format!(
                "{} has no mesh file extension (.stl, .3mf, .step)",
                source.display()
            )
        })
}

/// A file copied into the cache and measured.
struct Measured {
    copy: CachedCopy,
    triangle_count: u64,
    bounds: Bounds,
}

/// Copy the edit's file into the cache and measure the copy.
fn copy_and_measure(source_path: &str, cache_root: &Path) -> Result<Measured, String> {
    let source = Path::new(source_path);
    let extension = mesh_extension(source)?;
    let copy = copy_into_cache(source, &extension, cache_root)?;
    let part =
        measure_one_part(&copy.path).map_err(|why| format!("{}: {why}", source.display()))?;
    Ok(Measured {
        copy,
        triangle_count: part.triangle_count,
        bounds: Bounds {
            min: part.bounds_min,
            max: part.bounds_max,
        },
    })
}

fn prepare(edit: Edit, cache_root: &Path) -> Result<Prepared, String> {
    match edit {
        Edit::Add(add) => {
            let measured = copy_and_measure(&add.source_path, cache_root)?;
            Ok(Prepared::Add(PreparedAdd {
                object_id: ObjectId(format!("obj-{}", Ulid::new())),
                name: add.name,
                copy: measured.copy,
                triangle_count: measured.triangle_count,
                bounds: measured.bounds,
                recipe: add.recipe,
            }))
        }
        Edit::ReplaceGeometry(replace) => {
            let measured = copy_and_measure(&replace.source_path, cache_root)?;
            Ok(Prepared::Replace(PreparedReplace {
                object_id: replace.object_id,
                copy: measured.copy,
                triangle_count: measured.triangle_count,
                bounds: measured.bounds,
                keep_paint: replace.keep_paint,
            }))
        }
    }
}

/// Copy `source` into the cache as `external-<sha256>.<extension>`.
///
/// The bytes are copied first and hashed from the copy, so the name is the
/// hash of exactly what the object will read, even if the writer replaces its
/// file meanwhile. The copy goes into place by rename, as in `serial.rs`, so
/// nobody reads a half-written mesh.
fn copy_into_cache(
    source: &Path,
    extension: &str,
    cache_root: &Path,
) -> Result<CachedCopy, String> {
    let temporary = cache_root.join(format!("external-{}.tmp", Ulid::new()));
    let settled = std::fs::copy(source, &temporary)
        .map_err(|e| {
            format!(
                "cannot copy {} into the object cache: {e}",
                source.display()
            )
        })
        .and_then(|_| settle(&temporary, extension, cache_root));
    if settled.is_err() {
        // Gone already when the copy never started; nothing to report then.
        let _ = std::fs::remove_file(&temporary);
    }
    settled
}

/// Put the copy at its content address. A file already there with the same
/// bytes is kept and the copy dropped; one with other bytes is not what its
/// name says, and the copy replaces it.
fn settle(temporary: &Path, extension: &str, cache_root: &Path) -> Result<CachedCopy, String> {
    let hash = sha256_file(temporary).map_err(|e| format!("cannot hash the cached copy: {e}"))?;
    let path = cache_root.join(format!("external-{hash}.{extension}"));
    match sha256_file(&path) {
        Ok(existing) if existing == hash => {
            if let Err(e) = std::fs::remove_file(temporary) {
                tracing::warn!(path = %temporary.display(), error = %e, "cannot remove a duplicate cache copy");
            }
            return Ok(CachedCopy { path, hash });
        }
        Ok(_) => {
            tracing::warn!(path = %path.display(), "cached mesh does not match its name; replacing it");
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    }
    std::fs::rename(temporary, &path)
        .map_err(|e| format!("cannot move the copy to {}: {e}", path.display()))?;
    Ok(CachedCopy { path, hash })
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

/// Measure the copy through the bridge, as `object.import_local` measures an
/// import. An add is one object, so the file must hold exactly one part: a
/// multi-part STEP would need the fan-out `import_local` does.
fn measure_one_part(copy: &Path) -> Result<InspectedPart, String> {
    let path = copy.to_string_lossy();
    let inspected = {
        // See STEP_FFI_LOCK: the reader behind `inspect_model` is not
        // thread-safe for STEP. Poisoning is safe to swallow — the lock
        // guards no data, only re-entry into the reader.
        let _step_lock = STEP_FFI_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slicer_ffi::inspect_model(&path)
    };
    if !inspected.success {
        return Err(format!("cannot read the mesh: {}", inspected.error_message));
    }
    let count = inspected.parts.len();
    let mut parts = inspected.parts.into_iter();
    match (parts.next(), parts.next()) {
        (Some(part), None) => Ok(part),
        _ => Err(format!(
            "the file holds {count} parts; an add takes exactly one"
        )),
    }
}
