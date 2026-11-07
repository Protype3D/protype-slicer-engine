//! What the `import` nodes of a tree stand for: finding the file or the scene
//! object each one names, and reading it into a solid.
//!
//! The tree holds a name and nothing else, so everything that can be wrong with
//! the thing behind the name is found here, and every refusal says which name
//! it was about.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};
use slicer_geometry::import::{ImportRef, ImportedSolid, Imports};
use slicer_geometry::{
    CsgNode, GeometryError, TriangleMesh, MAX_IMPORT_FILE_BYTES, MAX_IMPORT_TRIANGLES,
};
use slicer_types::ObjectId;

use super::{TreeKey, BREP_KERNEL_COMPILED_IN};
use crate::dispatch::is_step_extension;
use crate::error::RpcError;
use crate::mesh::ObjectMeshes;

const MIB: u64 = 1024 * 1024;

/// A tree's imports, read.
pub(super) struct ResolvedImports {
    pub solids: Imports,
    /// SHA-256 of what each name read, by name. A tree's text says which file
    /// it imports and not what that file held, and a part's key has to know.
    contents: BTreeMap<String, String>,
}

impl ResolvedImports {
    /// The key of a part built from the tree `plain` is the key of. A tree that
    /// imports nothing keeps the key it has always had, so the parts already
    /// cached under it stay found.
    pub fn key_of(&self, plain: &TreeKey) -> String {
        if self.contents.is_empty() {
            return plain.hash.clone();
        }
        let keyed = json!({ "tree": plain.hash, "imports": self.contents }).to_string();
        hex::encode(Sha256::digest(keyed.as_bytes()))
    }
}

/// One mesh, read, and what its bytes hash to.
struct ReadMesh {
    mesh: TriangleMesh,
    sha256: String,
}

/// Read everything `tree` imports. `objects` holds the scene objects it names,
/// resolved to their meshes before the call came to this blocking thread; an id
/// that is not among them names no object.
///
/// The tree's limit on triangles is held to here as well as by the evaluator,
/// which counts per use: only here can it stop the reading, and a tree of many
/// large files would otherwise be read whole before anyone added them up.
pub(super) fn resolve(
    tree: &CsgNode,
    objects: &ObjectMeshes,
    cache_root: &Path,
) -> Result<ResolvedImports, RpcError> {
    let mut solids = Imports::new();
    let mut contents = BTreeMap::new();
    let mut triangles = 0;
    for reference in tree.imports() {
        let refused = |why: String| RpcError::Project(format!("cannot import {reference}: {why}"));
        let read = match &reference {
            ImportRef::Path(path) => read_file(path, cache_root),
            ImportRef::Object(id) => read_object(id, objects),
        }
        .map_err(refused)?;
        triangles += read.mesh.triangles.len();
        if triangles > MAX_IMPORT_TRIANGLES {
            let too_large = GeometryError::ImportTooLarge {
                count: triangles,
                max: MAX_IMPORT_TRIANGLES,
            };
            return Err(RpcError::Project(too_large.to_string()));
        }
        let solid = ImportedSolid::from_mesh(&read.mesh).map_err(|e| refused(e.to_string()))?;
        contents.insert(reference.to_string(), read.sha256);
        solids.insert(reference, solid);
    }
    Ok(ResolvedImports { solids, contents })
}

fn read_file(raw: &str, cache_root: &Path) -> Result<ReadMesh, String> {
    let path = checked_path(raw)?;
    let bytes = read_limited(&path)?;
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let mesh = if is_step_extension(&path.to_string_lossy()) {
        // As the caller wrote it, like every other path that reaches OCCT:
        // `path` is canonical, and on Windows that is a `\\?\` path.
        step_mesh(raw, &sha256, cache_root)?
    } else {
        TriangleMesh::from_stl_bytes(&bytes).map_err(|e| e.to_string())?
    };
    Ok(ReadMesh { mesh, sha256 })
}

/// The path of an `import`, proven to be one worth reading: absolute, because a
/// relative one resolves against wherever the daemon was started; a regular
/// file, because reading a pipe or a device would never end; and an STL or a
/// STEP by its real name, because any file can be linked under a name ending in
/// `.stl`.
fn checked_path(raw: &str) -> Result<PathBuf, String> {
    if !Path::new(raw).is_absolute() {
        return Err("the path must be absolute".to_string());
    }
    let real = std::fs::canonicalize(raw).map_err(|e| format!("cannot open it: {e}"))?;
    let metadata = std::fs::metadata(&real).map_err(|e| format!("cannot open it: {e}"))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a file", real.display()));
    }
    let is_stl = real
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("stl"));
    if !is_stl && !is_step_extension(&real.to_string_lossy()) {
        return Err(format!(
            "{} is neither an .stl nor a .step file",
            real.display()
        ));
    }
    Ok(real)
}

/// The bytes of `path`, within the limit. The length is looked at first and the
/// read is capped as well: the first refuses a file too long without touching
/// it, the second one that grew after it was looked at.
fn read_limited(path: &Path) -> Result<Vec<u8>, String> {
    let too_long = || {
        format!(
            "the file is over the {} MiB an `import` reads",
            MAX_IMPORT_FILE_BYTES / MIB
        )
    };
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open it: {e}"))?;
    let length = file
        .metadata()
        .map_err(|e| format!("cannot read it: {e}"))?
        .len();
    if length > MAX_IMPORT_FILE_BYTES {
        return Err(too_long());
    }
    if length == 0 {
        return Err("the file is empty".to_string());
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMPORT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read it: {e}"))?;
    if bytes.len() as u64 > MAX_IMPORT_FILE_BYTES {
        return Err(too_long());
    }
    Ok(bytes)
}

/// A STEP file as triangles, through the same tessellation and the same cache
/// as a STEP object of the scene. A STEP has no triangles of its own, and a tree
/// with an `import` is built by the mesh kernel.
fn step_mesh(path: &str, sha256: &str, cache_root: &Path) -> Result<TriangleMesh, String> {
    if !BREP_KERNEL_COMPILED_IN {
        return Err("this daemon was built without OCCT: it reads STL but not STEP".to_string());
    }
    let stl = crate::dispatch::step_edit_mesh(path, sha256, cache_root)?;
    TriangleMesh::from_stl_path(stl).map_err(|e| e.to_string())
}

/// An object as `mesh.*` measures it: the file its slice reads, carrying the
/// holes of its feature stack, put back in the frame of its source.
fn read_object(id: &str, objects: &ObjectMeshes) -> Result<ReadMesh, String> {
    let file = objects
        .get(&ObjectId(id.to_string()))
        .ok_or_else(|| "there is no such object in this project".to_string())?;
    let bytes = read_limited(Path::new(&file.path))?;
    let mesh = TriangleMesh::from_stl_bytes(&bytes).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let mesh = match file.offset {
        Some(offset) => {
            hasher.update(format!("{offset:?}"));
            mesh.translated(offset)
        }
        None => mesh,
    };
    Ok(ReadMesh {
        mesh,
        sha256: hex::encode(hasher.finalize()),
    })
}
