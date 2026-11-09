//! `geometry.build` — backend-2 of the pluggable geometry contract.
//!
//! Lives outside `dispatch.rs` on purpose: that file is already past 4000
//! lines, and this handler has enough of its own shape (cache addressing,
//! kernel choice, error mapping) to be read on its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use slicer_geometry::import::{ImportRef, ImportedSolid, Imports};
use slicer_geometry::{build_stl_with, CsgNode, TriangleMesh};
use slicer_state::StateLayer;
use slicer_types::{Bounds, ObjectId, ProjectEvent, Representation};
use ulid::Ulid;

use crate::brep_worker::BrepFailure;
use crate::error::RpcError;
use crate::mesh::ObjectMeshes;
use crate::part::kernel::KernelOutput;

mod imports;

#[derive(Debug, Deserialize)]
pub struct GeometryBuildParams {
    pub name: String,
    pub tree: CsgNode,
    #[serde(default)]
    pub recipe: Option<Value>,
}

/// Was the B-rep kernel compiled into *this crate*?
///
/// Not the same question as whether OCCT is linked into the process, and
/// telling them apart is worth a public constant: a binary can link all
/// of OCCT through `slicer-ffi` and still answer every `geometry.build`
/// with the mesh kernel, because the branch that calls OCCT is behind
/// this crate's own feature. That happened — `slicer-daemon` forwarded
/// `with-libslic3r` to `slicer-ffi` and `slicer-pipeline` but not here,
/// and the shipped daemon quietly built meshes while every test passed.
/// Callers that link both crates should check this against
/// `slicer_ffi::version()` rather than assume they agree.
pub const BREP_KERNEL_COMPILED_IN: bool = cfg!(feature = "with-libslic3r");

/// A finished part, and which kernel finished it.
///
/// OCCT is preferred because a B-rep part answers `object.query_faces`,
/// which is the whole reason this backend exists. The mesh kernel stays
/// as the fallback: it has no `hull` gap and it survives near-coincident
/// geometry that OCCT refuses. Losing a part that used to build would be
/// the worst possible outcome of adding a second kernel.
enum Built {
    Brep(Part),
    Mesh {
        part: Part,
        /// Why the B-rep kernel did not build this tree first.
        declined: BrepFailure,
    },
}

/// What both kernels have to produce, whatever they produce it from: the
/// bytes are already on disk by the time this exists, because the two
/// kernels write very differently (one returns a buffer, the other writes
/// its own file) and everything downstream only wants the numbers.
struct Part {
    volume_mm3: f64,
    triangle_count: u64,
    bounds: Bounds,
}

impl Built {
    fn part(&self) -> &Part {
        match self {
            Built::Brep(part) | Built::Mesh { part, .. } => part,
        }
    }

    /// A B-rep part carries topology, so `object.query_faces` will answer
    /// it with real faces. A mesh part will honestly answer "none".
    fn has_faces(&self) -> bool {
        matches!(self, Built::Brep(_))
    }

    fn kernel(&self) -> &'static str {
        match self {
            Built::Brep(_) => "brep",
            Built::Mesh { .. } => "mesh",
        }
    }

    /// The part as a node of a program takes it: where its file is, how it was
    /// measured, which kernel made it, and whether the next build of the tree
    /// is worth another B-rep try. It is when the mesh is only here because
    /// the B-rep kernel missed its deadline or could not be run, which may go
    /// differently the next time; a refusal or a crash will not.
    fn into_kernel_output(self, path: PathBuf) -> KernelOutput {
        let should_retry_brep =
            matches!(&self, Built::Mesh { declined, .. } if declined.is_transient());
        let (representation, part) = match self {
            Built::Brep(part) => (Representation::Brep, part),
            Built::Mesh { part, .. } => (Representation::Mesh, part),
        };
        KernelOutput {
            path,
            triangle_count: part.triangle_count,
            bounds: part.bounds,
            representation,
            should_retry_brep,
        }
    }
}

/// Build a part from a CSG tree, cache it, and register it as a project
/// object.
///
/// The cache is content-addressed by the canonical JSON of the tree, and by
/// what its `import` nodes read, so the same tree built twice reuses one file
/// on disk while still producing two independent objects — that is how a client
/// places the same generated part more than once.
///
/// `objects` holds the scene objects the tree imports, resolved before the call
/// came to this blocking thread, as for `mesh.*`.
pub fn handle_build(
    state: &StateLayer,
    objects: &ObjectMeshes,
    params: Value,
) -> Result<Value, RpcError> {
    let p: GeometryBuildParams = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;

    if let Some(recipe) = &p.recipe {
        slicer_state::project::validate_recipe(recipe).map_err(RpcError::Project)?;
    }

    let written = tree_key(&p.tree)
        .map_err(|e| RpcError::Internal(format!("cannot canonicalize tree: {e}")))?;

    // The cache root comes first only so that a refusal has somewhere to
    // be written down; nothing is put in it until something succeeds.
    let cache_root = slicer_state::object_cache_dir()
        .map_err(|e| RpcError::Internal(format!("cannot resolve object cache: {e}")))?;
    std::fs::create_dir_all(&cache_root)
        .map_err(|e| RpcError::Internal(format!("cannot create object cache: {e}")))?;

    // Validation and building share one exit, so that whatever refuses
    // the tree, the tree is kept. Both refusals reach the client as one
    // line of text and no more, and a tree nobody wrote down cannot be
    // reproduced from that line.
    let outcome = (|| {
        // Before the kernel choice, not inside it. The limits are
        // shared, so a tree illegal for one kernel is illegal for both;
        // validating inside whichever kernel happened to answer would
        // create input whose legality depends on how the daemon was
        // compiled.
        p.tree
            .validate()
            .map_err(|e| RpcError::Project(e.to_string()))?;
        // Before anything is read: this refusal is about the tree, and no file
        // could change it.
        slicer_geometry::kernel_need(&p.tree).map_err(|e| RpcError::Project(e.to_string()))?;
        let resolved = imports::resolve(&p.tree, objects, &cache_root)?;
        let source_hash = resolved.key_of(&written);
        let (built, path) =
            build_with_best_kernel(&p.tree, resolved.solids, &cache_root, &source_hash)?;
        Ok((built, path, source_hash))
    })();

    let (built, source_path, source_hash) = outcome.inspect_err(|why| {
        keep_the_failed_recipe(&cache_root, &written.hash, &written.canonical, why)
    })?;
    let part = built.part();

    let bounds = part.bounds.clone();
    let object_id = ObjectId(format!("obj-{}", Ulid::new()));
    let source_ref = source_path.to_string_lossy().into_owned();

    let mut events = vec![ProjectEvent::ObjectAdded {
        object_id: object_id.clone(),
        name: p.name.clone(),
        source_path: source_ref.clone(),
        triangle_count: part.triangle_count,
        bounds: bounds.clone(),
        source_hash: source_hash.clone(),
        step_part: None,
        split_part: None,
        transform: None,
    }];
    if let Some(recipe) = p.recipe {
        events.push(ProjectEvent::ObjectRecipeSet {
            object_id: object_id.clone(),
            recipe: Some(recipe),
        });
    }
    state.transaction(|txn| {
        for event in events {
            txn.apply(event);
        }
    });

    Ok(json!({
        "object_id": object_id,
        "name": p.name,
        "triangle_count": part.triangle_count,
        "bounds": bounds,
        "source_hash": source_hash,
        // `mesh_path` predates the second kernel and has consumers, so it
        // stays and keeps meaning "the file this object was made from" —
        // which is now a STEP as often as an STL. `source_ref` is the
        // same string under the name the contract gives it.
        "mesh_path": source_ref,
        "source_ref": source_ref,
        "volume_mm3": part.volume_mm3,
        "kernel": built.kernel(),
        "has_faces": built.has_faces(),
    }))
}

/// A CSG tree as the cache addresses it: the text it is hashed from, and the
/// hash.
pub(crate) struct TreeKey {
    /// The parsed tree written back out — defaults filled in, fields in
    /// declaration order — not the text it was read from, so two spellings
    /// of one tree are one tree. A tree that will not build is kept under
    /// this text.
    pub canonical: String,
    /// SHA-256 of `canonical`, in hex: the name of the cache file a built
    /// part is written to, and its object's `source_hash`.
    pub hash: String,
}

/// The one place a tree's key is made. `geometry.build` caches a part by it,
/// and a part's `csg` node is keyed by it, so a part and a generated object
/// built from one tree share their file.
pub(crate) fn tree_key(tree: &CsgNode) -> Result<TreeKey, serde_json::Error> {
    let canonical = serde_json::to_string(tree)?;
    let hash = if canonical.contains(LOFT_TAG) {
        hex::encode(Sha256::digest(
            format!("{LOFT_BUILD}\n{canonical}").as_bytes(),
        ))
    } else {
        hex::encode(Sha256::digest(canonical.as_bytes()))
    };
    Ok(TreeKey { canonical, hash })
}

/// How a loft node reads in a canonical tree. Text inside a tree could not
/// match by accident: its quotes come out escaped.
const LOFT_TAG: &str = r#""op":"loft""#;

/// Which build of a loft a tree's key stands for: the same tree comes out a
/// different solid since lofts went straight between sections. Only trees with
/// a loft carry it, so every other part keeps its key and its cache entries, and
/// the old lofts stop being served from `part-<hash>` and `editmesh-<hash>`.
const LOFT_BUILD: &str = "loft-v2";

/// Write down the tree that would not build, next to the parts that did.
///
/// Nothing else records it. A part that builds becomes an `ObjectAdded`
/// event in the autosave log and a file in the cache; a part that does
/// not leaves one RPC error on its way to a dialog box, and once that is
/// closed the recipe is gone. A failure report then arrives as the text
/// of the error and nothing else — which is how a build that came back
/// enclosing -11799791 mm³ on 2026-08-21 could not be reproduced, or
/// even read, after the fact.
///
/// Best-effort by design: this runs on the way out of a failure, and a
/// second failure here must not replace the first one in the reply.
fn keep_the_failed_recipe(cache_root: &Path, source_hash: &str, tree: &str, why: &RpcError) {
    let path = cache_path(cache_root, source_hash, "failed.json");
    let record = json!({ "error": why.to_string(), "tree": tree });
    tracing::warn!(
        source_hash,
        error = %why,
        recipe = %path.display(),
        "geometry.build failed; the tree that failed is kept beside the cache"
    );
    if let Err(e) = std::fs::write(&path, record.to_string()) {
        tracing::warn!(?e, recipe = %path.display(), "cannot keep the failed recipe");
    }
}

/// OCCT first, mesh second, and the caller is told which answered.
///
/// A tree carrying `hull` or `import` skips OCCT outright rather than
/// failing into the fallback: OCCT has no convex hull and takes no mesh, so
/// the attempt would cost time and end in the same place. What such a tree
/// holds that only OCCT can build is built apart: [`join_brep_islands`].
fn build_with_best_kernel(
    tree: &CsgNode,
    imports: Imports,
    cache_root: &Path,
    source_hash: &str,
) -> Result<(Built, PathBuf), RpcError> {
    // A B-rep operation over a mesh is impossible rather than merely hard,
    // so it is refused before either kernel runs — and refused as bad input,
    // because no fallback can rescue it.
    let need =
        slicer_geometry::kernel_need(tree).map_err(|mixed| RpcError::Project(mixed.to_string()))?;

    let declined = if need == slicer_geometry::KernelNeed::MeshOnly {
        BrepFailure::Refused(
            "the tree contains a mesh-only operation (`hull`, `surface_relief`, `import` or a twisted `extrude`)"
                .to_string(),
        )
    } else {
        let path = cache_path(cache_root, source_hash, "step");
        match write_brep_atomically(&path, |partial| try_brep(tree, partial)) {
            Ok(part) => return Ok((Built::Brep(part), path)),
            Err(why) => why,
        }
    };

    tracing::info!(
        source_hash,
        reason = %declined,
        "B-rep kernel declined this tree; building it with the mesh kernel instead"
    );

    let (tree, imports) = join_brep_islands(tree, imports, cache_root)?;
    let path = cache_path(cache_root, source_hash, "stl");
    let mesh = write_atomically(
        &path,
        |partial| try_mesh(&tree, &imports, partial),
        std::convert::identity,
    );
    let part = mesh.map_err(|mesh_why| {
        // Both texts, because they are different reasons and the second
        // explains the first more often than it looks like it will.
        RpcError::Project(format!("no kernel could build this part: {mesh_why} (the B-rep kernel declined first: {declined})"))
    })?;
    Ok((Built::Mesh { part, declined }, path))
}

/// The tree the mesh kernel can build, and what it imports: `tree` with every
/// subtree only OCCT can build ([`slicer_geometry::brep_islands`]) built by OCCT
/// and put back as an `import` of its tessellation.
///
/// The kernels can only meet at a mesh, and the mesh is the same one
/// `object.query_faces` derives and a slice is cut from. No mesh fallback here:
/// the mesh kernel has none of these operations, so OCCT's refusal is the answer.
fn join_brep_islands(
    tree: &CsgNode,
    mut imports: Imports,
    cache_root: &Path,
) -> Result<(CsgNode, Imports), RpcError> {
    let mut meshes = HashMap::new();
    for island in slicer_geometry::brep_islands(tree) {
        let (stl, solid) = island_mesh(island, cache_root).map_err(|why| {
            RpcError::Project(format!(
                "`{}` needs the B-rep kernel, and it gave no part to join to the mesh: {why}",
                island.op_name()
            ))
        })?;
        imports.insert(ImportRef::Path(stl.clone()), solid);
        let import = CsgNode::Import {
            source_path: Some(stl),
            object_id: None,
        };
        meshes.insert(std::ptr::from_ref(island), import);
    }
    Ok((with_islands_replaced(tree, &meshes), imports))
}

/// One island as a mesh: where its file is, and the solid the mesh kernel takes.
fn island_mesh(island: &CsgNode, cache_root: &Path) -> Result<(String, ImportedSolid), String> {
    let key = tree_key(island)
        .map_err(|e| format!("cannot canonicalize the subtree: {e}"))?
        .hash;
    let step = cache_path(cache_root, &key, "step");
    write_brep_atomically(&step, |partial| try_brep(island, partial))
        .map_err(|why| why.to_string())?;
    let stl = crate::dispatch::step_edit_mesh(&step.to_string_lossy(), &key, cache_root)?;
    let mesh = TriangleMesh::from_stl_path(&stl).map_err(|e| e.to_string())?;
    let solid = ImportedSolid::from_mesh(&mesh).map_err(|e| e.to_string())?;
    Ok((stl.to_string_lossy().into_owned(), solid))
}

/// `node` with each island, known by where it stands in the tree, swapped for
/// its mesh.
fn with_islands_replaced(node: &CsgNode, meshes: &HashMap<*const CsgNode, CsgNode>) -> CsgNode {
    match meshes.get(&std::ptr::from_ref(node)) {
        Some(mesh) => mesh.clone(),
        None => node.map_children(|child| with_islands_replaced(child, meshes)),
    }
}

/// A tree built and cached exactly as `geometry.build` builds it, without
/// registering an object — what a part's `csg` node needs.
///
/// The answer says whether to build the tree again some other time
/// ([`KernelOutput::should_retry_brep`]); `geometry.build` has no use for that,
/// since it builds the tree anew on every call.
pub(crate) fn build_part(
    tree: &CsgNode,
    cache_root: &Path,
    tree_hash: &str,
) -> Result<KernelOutput, RpcError> {
    tree.validate()
        .map_err(|e| RpcError::Project(e.to_string()))?;
    std::fs::create_dir_all(cache_root)
        .map_err(|e| RpcError::Internal(format!("cannot create object cache: {e}")))?;
    let (built, path) = build_with_best_kernel(tree, Imports::new(), cache_root, tree_hash)?;
    Ok(built.into_kernel_output(path))
}

/// Run `build` against a temporary path and rename its result into place.
///
/// Written through a temporary and renamed, not straight to the final
/// path.
///
/// The content is addressed by its hash, so rewriting is semantically a
/// no-op and the old code did it unconditionally to self-heal a truncated
/// file. But writing in place truncates *first*: two clients building the
/// same tree at once — the same part placed twice, two sessions, a test
/// suite — leave a window where a third reader sees a half-written file.
/// Found exactly that way, as an intermittent "this does not parse as
/// binary or ASCII STL" from `mesh.measure` roughly one run in four.
///
/// A rename is atomic within a filesystem, so a reader now sees either
/// the previous complete file or the new one. The B-rep kernel writes its
/// own file rather than handing back bytes, which changes nothing here:
/// it is pointed at the temporary and the rename happens the same way.
///
/// Failing to move the finished part into place is the machine's doing, not the
/// tree's, and is made into an `E` by `unwritable`: what that is depends on who
/// asked — for the B-rep kernel it is [`BrepFailure::Unavailable`].
fn write_atomically<E>(
    final_path: &Path,
    build: impl FnOnce(&Path) -> Result<Part, E>,
    unwritable: impl FnOnce(String) -> E,
) -> Result<Part, E> {
    // Unique per *call*, not per process: two builds inside one process
    // sharing a temporary name is the same race one level down, and one
    // renames the other's file out from under it.
    //
    // The real extension stays on the end, and that is not cosmetic:
    // libslic3r picks its reader by extension (`load_model_for_path`), so
    // a STEP measured while it is still called `.partial-…` comes back
    // unreadable and the part silently falls through to the mesh kernel.
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let extension = final_path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let partial = final_path.with_extension(format!(
        "partial-{}-{}.{extension}",
        std::process::id(),
        SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));

    let part = build(&partial).inspect_err(|_| {
        let _ = std::fs::remove_file(&partial);
    })?;
    std::fs::rename(&partial, final_path).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        unwritable(format!("cannot write the generated part: {e}"))
    })?;
    Ok(part)
}

/// [`write_atomically`] for what the B-rep kernel builds: a part that cannot be
/// put in place is [`BrepFailure::Unavailable`], because nothing was learned
/// about the tree and the next try may go differently.
fn write_brep_atomically(
    final_path: &Path,
    build: impl FnOnce(&Path) -> Result<Part, BrepFailure>,
) -> Result<Part, BrepFailure> {
    write_atomically(final_path, build, BrepFailure::Unavailable)
}

/// Ask OCCT for the part, writing it to `out` as STEP.
///
/// The triangle count and bounds come back through `inspect_model` — the
/// same reader `object.import_local` measures an imported STEP with — so
/// a generated STEP and an imported one are described by the same numbers
/// from the same code, rather than by two estimates that could disagree.
#[cfg(feature = "with-libslic3r")]
fn try_brep(tree: &CsgNode, out: &Path) -> Result<Part, BrepFailure> {
    // Through `brep_worker`, not straight into the bridge: OCCT can take
    // the process down on a legal part, and in the daemon that call runs
    // in a child so it takes the child down instead. See that module.
    let volume_mm3 = crate::brep_worker::build_step(tree, out)?;

    let read = slicer_ffi::inspect_model(&out.to_string_lossy());
    if !read.success {
        return Err(BrepFailure::Refused(format!(
            "OCCT wrote a STEP it could not read back: {}",
            read.error_message
        )));
    }
    let measured = read
        .parts
        .first()
        .ok_or_else(|| BrepFailure::Refused("OCCT wrote a STEP with no parts in it".to_string()))?;

    Ok(Part {
        volume_mm3,
        triangle_count: measured.triangle_count,
        bounds: Bounds {
            min: measured.bounds_min,
            max: measured.bounds_max,
        },
    })
}

/// Without OCCT there is no B-rep kernel to try — the bridge's B-rep
/// entry points are all present but all answer "failed", so calling them
/// would produce a confusing refusal instead of a plain one.
#[cfg(not(feature = "with-libslic3r"))]
fn try_brep(_tree: &CsgNode, _out: &Path) -> Result<Part, BrepFailure> {
    Err(BrepFailure::Refused(
        "this daemon was built without OCCT".to_string(),
    ))
}

fn try_mesh(tree: &CsgNode, imports: &Imports, out: &Path) -> Result<Part, String> {
    let built = build_stl_with(tree, imports).map_err(|e| e.to_string())?;
    std::fs::write(out, &built.stl).map_err(|e| format!("cannot write generated mesh: {e}"))?;
    Ok(Part {
        volume_mm3: built.volume_mm3,
        // `BuiltGeometry` keeps this u32 because an STL header cannot
        // express more; project state counts in u64.
        triangle_count: u64::from(built.triangle_count),
        bounds: Bounds {
            min: built.bounds_min,
            max: built.bounds_max,
        },
    })
}

/// `<object_cache>/generated-<hash>.<ext>` — the same cache root
/// `object.split_to_parts` and the STEP edit-mesh path already use, with a
/// `generated-` prefix so the three kinds of derived artifact stay
/// tellable apart on disk. The extension names the kernel's output, and
/// is what makes `object.query_faces` answer a generated part with real
/// faces without a line of change in that arm.
fn cache_path(cache_root: &Path, source_hash: &str, extension: &str) -> PathBuf {
    cache_root.join(format!("generated-{source_hash}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(tree: serde_json::Value) -> CsgNode {
        serde_json::from_value(tree).unwrap()
    }

    /// A loft built by this daemon is not the solid an older one cached under
    /// the same tree, and must not be read back from that entry.
    #[test]
    fn a_tree_with_a_loft_has_a_key_of_its_own() {
        let square = serde_json::json!([[0.0, 0.0], [9.0, 0.0], [9.0, 9.0], [0.0, 9.0]]);
        let loft = parsed(serde_json::json!({
            "op": "loft",
            "sections": [{ "outline": square, "z": 0.0 }, { "outline": square, "z": 5.0 }]
        }));

        let key = tree_key(&loft).unwrap();

        let unversioned = hex::encode(Sha256::digest(key.canonical.as_bytes()));
        assert_ne!(key.hash, unversioned);
    }

    /// Every other part keeps its key, and with it its cache entries.
    #[test]
    fn a_tree_without_a_loft_keeps_its_key() {
        let cube = parsed(serde_json::json!({ "op": "box", "size": [1.0, 2.0, 3.0] }));

        let key = tree_key(&cube).unwrap();

        assert_eq!(
            key.hash,
            hex::encode(Sha256::digest(key.canonical.as_bytes()))
        );
    }

    fn a_part() -> Part {
        Part {
            volume_mm3: 1000.0,
            triangle_count: 12,
            bounds: Bounds {
                min: [0.0; 3],
                max: [10.0; 3],
            },
        }
    }

    fn a_mesh_built_after(declined: BrepFailure) -> Built {
        Built::Mesh {
            part: a_part(),
            declined,
        }
    }

    #[test]
    fn a_b_rep_part_describes_itself_and_is_not_sent_back_for_a_retry() {
        let out = Built::Brep(a_part()).into_kernel_output(PathBuf::from("part.step"));
        assert_eq!(out.path, PathBuf::from("part.step"));
        assert_eq!(out.triangle_count, 12);
        assert_eq!(out.bounds, a_part().bounds);
        assert_eq!(out.representation, Representation::Brep);
        assert!(!out.should_retry_brep);
    }

    #[test]
    fn a_mesh_built_after_the_b_rep_kernel_ran_out_of_time_or_could_not_be_run_asks_for_a_retry() {
        for declined in [
            BrepFailure::TimedOut("too slow".to_string()),
            BrepFailure::Unavailable("cannot run the worker".to_string()),
        ] {
            let built = a_mesh_built_after(declined.clone());
            let out = built.into_kernel_output(PathBuf::from("part.stl"));
            assert_eq!(out.representation, Representation::Mesh);
            assert!(
                out.should_retry_brep,
                "{declined:?} says nothing about the tree, and the next try may go differently"
            );
        }
    }

    #[test]
    fn a_b_rep_part_that_cannot_be_put_in_place_is_unavailable_not_refused() {
        // The build writes nothing, and the directory it should have written to
        // is not there: the move into place cannot succeed.
        let why =
            write_brep_atomically(Path::new("/no/such/dir/generated-k.step"), |_| Ok(a_part()))
                .err()
                .expect("there is nothing to move");
        assert!(matches!(why, BrepFailure::Unavailable(_)), "got: {why:?}");
        assert!(
            why.to_string().contains("cannot write the generated part"),
            "got: {why}"
        );
        assert!(why.is_transient());
    }

    #[test]
    fn a_mesh_built_after_a_refusal_or_a_crash_does_not() {
        for declined in [
            BrepFailure::Refused("no such shape".to_string()),
            BrepFailure::Crashed("the worker died".to_string()),
        ] {
            let built = a_mesh_built_after(declined.clone());
            let out = built.into_kernel_output(PathBuf::from("part.stl"));
            assert_eq!(out.representation, Representation::Mesh);
            assert!(
                !out.should_retry_brep,
                "{declined:?} comes back every time, so the mesh is the answer"
            );
        }
    }
}
