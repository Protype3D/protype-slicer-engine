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
//! bounded by `validate_recipe`, as in `geometry.build`. Only `add` exists
//! so far; `replace_geometry`, `remove` and `attach` from the spec join as
//! variants of `Edit`.
//!
//! Every edit is checked, copied and measured before the transaction, so a
//! refusal of any of them leaves the project untouched. Copies a refused
//! batch already made stay in the cache, where a later add of the same bytes
//! reuses them.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use slicer_ffi::InspectedPart;
use slicer_state::StateLayer;
use slicer_types::{Bounds, ObjectId, ProjectEvent};
use ulid::Ulid;

use crate::dispatch::STEP_FFI_LOCK;
use crate::error::RpcError;

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

/// Apply a batch of edits as one step of history. Blocking: it copies files
/// and measures them through the bridge, so the dispatcher runs it on the
/// blocking pool.
pub fn handle_apply_external_edit(state: &StateLayer, params: Value) -> Result<Value, RpcError> {
    let p: Params = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    // Everything that needs no file comes first, for the whole batch: a bad
    // recipe in the last edit must not leave copies of the first ones behind.
    let adds = check_edits(p.edits)?;
    let cache_root = slicer_state::object_cache_dir()
        .map_err(|e| RpcError::Internal(format!("cannot resolve object cache: {e}")))?;
    std::fs::create_dir_all(&cache_root)
        .map_err(|e| RpcError::Internal(format!("cannot create object cache: {e}")))?;
    let prepared = adds
        .into_iter()
        .enumerate()
        .map(|(index, add)| {
            prepare_add(add, &cache_root)
                .map_err(|why| RpcError::Project(format!("edits[{index}]: {why}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let objects: Vec<Value> = prepared.iter().map(PreparedAdd::summary).collect();
    let events: Vec<ProjectEvent> = prepared
        .into_iter()
        .flat_map(PreparedAdd::into_events)
        .collect();
    // The closure holds the state lock: it works through `txn` alone.
    state.transaction(|txn| {
        for event in events {
            txn.apply(event);
        }
    });
    Ok(json!({ "objects": objects }))
}

/// The checks that read no file, for every edit of the batch.
fn check_edits(edits: Vec<Edit>) -> Result<Vec<AddEdit>, RpcError> {
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
    edits
        .into_iter()
        .enumerate()
        .map(|(index, edit)| {
            let Edit::Add(add) = edit;
            check_add(&add)
                .map(|()| add)
                .map_err(|why| RpcError::Project(format!("edits[{index}]: {why}")))
        })
        .collect()
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

/// Copy the add's file into the cache and measure the copy.
fn prepare_add(add: AddEdit, cache_root: &Path) -> Result<PreparedAdd, String> {
    let source = Path::new(&add.source_path);
    let extension = mesh_extension(source)?;
    let copy = copy_into_cache(source, &extension, cache_root)?;
    let part =
        measure_one_part(&copy.path).map_err(|why| format!("{}: {why}", source.display()))?;
    Ok(PreparedAdd {
        object_id: ObjectId(format!("obj-{}", Ulid::new())),
        name: add.name,
        copy,
        triangle_count: part.triangle_count,
        bounds: Bounds {
            min: part.bounds_min,
            max: part.bounds_max,
        },
        recipe: add.recipe,
    })
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
