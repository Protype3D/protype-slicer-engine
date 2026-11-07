//! What a part's nodes ask of the geometry kernels — behind a trait, so the
//! executor runs on PR CI against a stand-in — and the cache the results are
//! kept in.

use std::path::{Path, PathBuf};

use slicer_geometry::CsgNode;
use slicer_types::{BodyGeometry, Bounds, FeatureOp, Representation};
use ulid::Ulid;

/// A file a kernel wrote, measured.
#[derive(Debug, Clone)]
pub struct KernelOutput {
    pub path: PathBuf,
    pub triangle_count: u64,
    pub bounds: Bounds,
    pub representation: Representation,
    /// The B-rep kernel was tried first and gave no answer that is the tree's
    /// doing — it ran out of time, or could not be run — so this is the mesh
    /// kernel's answer, which the next build may improve on. The executor uses
    /// it for this run only: it does not keep it, and keeps what is cut on it
    /// apart from what is cut on the real build.
    pub should_retry_brep: bool,
}

pub trait PartKernel: Send + Sync {
    /// Build a CSG tree with the best kernel available. `key` is the tree's
    /// content key; the output goes into `cache_root`. An answer that stands in
    /// for a better one the next build may get says so in
    /// [`KernelOutput::should_retry_brep`].
    fn csg(&self, tree: &CsgNode, cache_root: &Path, key: &str) -> Result<KernelOutput, String>;

    /// Cut `op` — already in the frame `apply_operations` cuts in — into
    /// part `part_index` of `input_path` and write a binary STL to `out`.
    /// `Ok(false)` when the cut left the mesh unchanged and the kernel cannot
    /// say why; an `Err` carries the reason when it can.
    ///
    /// `out` names its content and a cut is only ever moved there whole, so a
    /// file already at `out` is this cut. Another regeneration made it while
    /// this one waited its turn, and it is answered `Ok(true)` as it is, with
    /// nothing cut and the input unread.
    fn hole(
        &self,
        input_path: &str,
        part_index: i32,
        op: &FeatureOp,
        out: &Path,
    ) -> Result<bool, String>;
}

/// The real kernels: `geometry.build`'s B-rep-then-mesh choice for trees,
/// `apply_operations` (mcut) for holes.
///
/// Either call can take seconds, and a cut waits for `STEP_FFI_LOCK`: call
/// them from a blocking thread, never from an async task.
pub struct LibKernel;

impl PartKernel for LibKernel {
    fn csg(&self, tree: &CsgNode, cache_root: &Path, key: &str) -> Result<KernelOutput, String> {
        crate::geometry::build_part(tree, cache_root, key).map_err(|e| e.to_string())
    }

    fn hole(
        &self,
        input_path: &str,
        part_index: i32,
        op: &FeatureOp,
        out: &Path,
    ) -> Result<bool, String> {
        let ops_json = serde_json::json!({ "ops": [op] }).to_string();
        let staging = out.with_file_name(format!(
            "{}.{}.tmp",
            out.file_name().unwrap_or_default().to_string_lossy(),
            Ulid::new()
        ));
        // apply_operations loads its input through the same OCCT STEP reader
        // the other STEP_FFI_LOCK call sites guard. The lock is held until the
        // cut is in place, so a call that waited for it looks at `out` after
        // the call it waited for is done with it, as `materialize_operations`
        // does.
        let _step_lock = crate::dispatch::STEP_FFI_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if out.exists() {
            return Ok(true);
        }
        let outcome = slicer_ffi::apply_operations(
            input_path,
            part_index,
            &ops_json,
            &staging.to_string_lossy(),
        );
        if !outcome.success {
            let _ = std::fs::remove_file(&staging);
            return Err(format!(
                "apply_operations failed: {}",
                outcome.error_message
            ));
        }
        if outcome.applied_count == 0 {
            let _ = std::fs::remove_file(&staging);
            return match outcome.skipped_ops.into_iter().next() {
                Some(skipped) => Err(skipped.reason),
                None => Ok(false),
            };
        }
        std::fs::rename(&staging, out).map_err(|e| {
            let _ = std::fs::remove_file(&staging);
            format!("cannot keep the cut mesh: {e}")
        })?;
        Ok(true)
    }
}

/// Which kernels this build has. Cache entries are kept apart by it: a box
/// built by the mesh kernel in a test run without OCCT must never answer a
/// daemon that would have built it as B-rep — the part would lose its faces.
const FLAVOR: &str = if cfg!(feature = "with-libslic3r") {
    "brep"
} else {
    "mesh"
};

/// Built nodes, keyed by content: `node-v1-<flavor>-<key>.json` describing
/// the file the node produced. A hit needs both — a description whose file
/// is gone is a miss.
pub struct NodeCache {
    root: PathBuf,
    flavor: &'static str,
}

impl NodeCache {
    /// A cache in `root` of the flavour this build has.
    pub fn new(root: PathBuf) -> Self {
        Self::with_flavor(root, FLAVOR)
    }

    /// A cache in `root` for another flavour than this build's, so that a test
    /// can show flavours sharing a root do not meet. Nothing but a test has a
    /// reason to ask for one.
    #[doc(hidden)]
    pub fn with_flavor(root: PathBuf, flavor: &'static str) -> Self {
        Self { root, flavor }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a hole node's mesh goes.
    pub fn mesh_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("node-v1-{}-{key}.stl", self.flavor))
    }

    fn meta_path(&self, key: &str) -> PathBuf {
        self.root
            .join(format!("node-v1-{}-{key}.json", self.flavor))
    }

    /// The node kept under `key`, when its description reads and the file it
    /// describes is still there. Anything else is a miss, with the reason
    /// logged at debug level: a node never built and an entry gone bad are the
    /// same to the caller, which builds the node either way, but not to
    /// whoever wonders why a cache did not help.
    pub fn get(&self, key: &str) -> Option<BodyGeometry> {
        match self.read(key) {
            Ok(geometry) => Some(geometry),
            Err(why) => {
                tracing::debug!(key, %why, "node cache miss");
                None
            }
        }
    }

    fn read(&self, key: &str) -> Result<BodyGeometry, String> {
        let meta = self.meta_path(key);
        let bytes =
            std::fs::read(&meta).map_err(|e| format!("cannot read {}: {e}", meta.display()))?;
        let geometry: BodyGeometry = serde_json::from_slice(&bytes)
            .map_err(|e| format!("cannot parse {}: {e}", meta.display()))?;
        if !Path::new(&geometry.source_path).exists() {
            return Err(format!(
                "the file {} describes is gone: {}",
                meta.display(),
                geometry.source_path
            ));
        }
        Ok(geometry)
    }

    pub fn put(&self, key: &str, geometry: &BodyGeometry) -> Result<(), String> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| format!("cannot create the node cache: {e}"))?;
        let final_path = self.meta_path(key);
        let staging = final_path.with_extension(format!("json.{}.tmp", Ulid::new()));
        let bytes =
            serde_json::to_vec(geometry).map_err(|e| format!("cannot encode a node: {e}"))?;
        std::fs::write(&staging, bytes).map_err(|e| format!("cannot write a node: {e}"))?;
        std::fs::rename(&staging, &final_path).map_err(|e| {
            let _ = std::fs::remove_file(&staging);
            format!("cannot keep a node: {e}")
        })
    }
}
