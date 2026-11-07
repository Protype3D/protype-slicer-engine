//! Running a validated program: each node once, from the cache when it can,
//! a failed hole skipped rather than fatal, and one status per feature.

use std::collections::BTreeMap;
use std::path::Path;

use sha2::{Digest, Sha256};
use slicer_geometry::{CsgNode, TriangleMesh};
use slicer_types::{
    BodyGeometry, Bounds, FeatureOp, FeatureState, FeatureStatus, KernelProgram, NodeOp,
    PartSource, Representation,
};

use super::frame;
use super::kernel::{KernelOutput, NodeCache, PartKernel};
use super::program::Validated;

/// What a run produced: the bodies that built, and one status per feature
/// tag, sorted by tag.
#[derive(Debug)]
pub struct Execution {
    pub outputs: BTreeMap<String, BodyGeometry>,
    pub statuses: Vec<FeatureStatus>,
}

/// One node's result. `geometry` is what the nodes reading this one see: a
/// failed hole passes its input through, so the body builds without it.
struct NodeResult {
    geometry: Option<BodyGeometry>,
    problem: Option<(FeatureState, String)>,
    /// `geometry` is not what the node's key names: a failed hole was passed
    /// through here or somewhere upstream, or a `csg` node's mesh stands in for
    /// a B-rep build that did not finish. What is cut on it is kept under keys
    /// of its own, so that it cannot be taken for a cut on the real thing: see
    /// [`rekey`].
    is_off_key: bool,
}

/// Runs `program`: each node once, in `validated.order`.
///
/// `validated` and `keys` are what [`validate`](super::program::validate) and
/// [`node_keys`](super::program::node_keys) made for this `program` and
/// `sources`; anything else panics. A node that cannot be built leaves no
/// geometry, and the holes reading it are skipped. A hole that cannot be cut
/// passes its input through, so the body builds without it. Either way the
/// run goes on: the problem is reported in [`Execution::statuses`].
///
/// Blocks for as long as the kernels take, so call it from a blocking thread.
pub fn execute(
    program: &KernelProgram,
    validated: &Validated,
    keys: &BTreeMap<String, String>,
    sources: &[PartSource],
    kernel: &dyn PartKernel,
    cache: &NodeCache,
) -> Execution {
    let mut results: BTreeMap<String, NodeResult> = BTreeMap::new();
    for id in &validated.order {
        let key = &keys[id];
        let result = match &program.nodes[id].op {
            NodeOp::Import { source } => import(sources, source),
            NodeOp::Csg { .. } => csg(&validated.trees[id], key, kernel, cache),
            NodeOp::Hole { input, op } => {
                let upstream_tag = &program.nodes[input].tag;
                hole(&results[input], upstream_tag, op, key, kernel, cache)
            }
        };
        results.insert(id.clone(), result);
    }
    let outputs = program
        .bodies
        .iter()
        .filter_map(|(key, body)| {
            let geometry = results.get(&body.node)?.geometry.clone()?;
            Some((key.clone(), geometry))
        })
        .collect();
    Execution {
        outputs,
        statuses: statuses(program, &results),
    }
}

fn import(sources: &[PartSource], source: &str) -> NodeResult {
    let Some(source) = sources.iter().find(|s| s.id == source) else {
        return failed(format!("source {source} is not recorded on this part"));
    };
    if !Path::new(&source.source_path).exists() {
        return failed(format!(
            "the source file is missing: {}",
            source.source_path
        ));
    }
    built(BodyGeometry {
        source_path: source.source_path.clone(),
        source_hash: source.source_hash.clone(),
        triangle_count: source.triangle_count,
        bounds: source.bounds.clone(),
        design_offset: [0.0; 3],
        representation: if is_step(&source.source_path) {
            Representation::Brep
        } else {
            Representation::Mesh
        },
        step_part_index: source.step_part_index,
    })
}

/// A `csg` node, from the cache or built by the kernel.
///
/// What the kernel built is kept for the next run unless it says
/// `should_retry_brep`: a mesh standing in for a B-rep build that did not
/// finish — it ran out of time, or could not be run — is used for this run
/// only. Kept, one slow build would leave the node a mesh, a part without
/// faces, until its cache entry is removed; left out of the cache, the next run
/// asks the kernel again. A decline that will recur (a refusal, a crash,
/// `hull`) is kept like any other build.
///
/// The stand-in is not what the node's key names either — the key names the
/// tree's best build. So it is off its key, and goes by a hash of its own in
/// place of the key ([`provisional_hash`]). Whatever is cut on it is kept under
/// keys that name the stand-in (see [`rekey`]), never under the ones the real
/// build's cuts are read from. Once B-rep builds the tree, its holes are cut
/// anew and its `source_hash` differs from the stand-in's, as that of a body
/// that changed does.
fn csg(tree: &CsgNode, key: &str, kernel: &dyn PartKernel, cache: &NodeCache) -> NodeResult {
    if let Some(hit) = cache.get(key) {
        return built(hit);
    }
    let out = match kernel.csg(tree, cache.root(), key) {
        Ok(out) => out,
        Err(why) => return failed(why),
    };
    if out.should_retry_brep {
        tracing::info!(
            key,
            "the B-rep kernel gave no answer this time: its mesh is used for this run only"
        );
        return stand_in(csg_geometry(out, provisional_hash(key)));
    }
    remember(cache, key, csg_geometry(out, key.to_string()))
}

/// What a kernel's answer for a tree is, as a body that goes by `source_hash`.
fn csg_geometry(out: KernelOutput, source_hash: String) -> BodyGeometry {
    BodyGeometry {
        source_path: out.path.to_string_lossy().into_owned(),
        source_hash,
        triangle_count: out.triangle_count,
        bounds: out.bounds,
        design_offset: [0.0; 3],
        representation: out.representation,
        step_part_index: None,
    }
}

/// What the stand-in mesh of the tree under `key` goes by. It is not `key`,
/// which names the tree's best build, and it is the same on every run that
/// has to make do with the mesh.
fn provisional_hash(key: &str) -> String {
    hex::encode(Sha256::digest(format!("mesh-fallback\n{key}").as_bytes()))
}

/// A hole cut into what `upstream` built — `upstream_tag` being its feature —
/// or skipped when it built nothing. A cut made on what a failed hole passed
/// through, or on a stand-in mesh, is off its key too, and is kept under a key
/// of its own: see [`rekey`].
fn hole(
    upstream: &NodeResult,
    upstream_tag: &str,
    op: &FeatureOp,
    key: &str,
    kernel: &dyn PartKernel,
    cache: &NodeCache,
) -> NodeResult {
    let Some(input) = &upstream.geometry else {
        return skipped(format!("skipped: feature {upstream_tag} did not build"));
    };
    let key = if upstream.is_off_key {
        rekey(key, &input.source_hash)
    } else {
        key.to_string()
    };
    let result = cut(input, op, &key, kernel, cache);
    NodeResult {
        is_off_key: upstream.is_off_key || result.is_off_key,
        ..result
    }
}

/// The key of a cut made on what is not the real thing: what a failed hole
/// passed through, or a mesh standing in for a B-rep build that did not finish.
/// The program's key for the node, from
/// [`node_keys`](super::program::node_keys), names a cut on the real thing — the
/// hole's result, the tree's best build. This one names a cut on `input_hash`,
/// the content the cut actually saw. Left under the program's key, the cut
/// would be served, once the real thing exists (the hole before it works again,
/// B-rep builds the tree), as a cut made on it.
fn rekey(key: &str, input_hash: &str) -> String {
    hex::encode(Sha256::digest(
        format!("off-key\n{key}\n{input_hash}").as_bytes(),
    ))
}

fn cut(
    input: &BodyGeometry,
    op: &FeatureOp,
    key: &str,
    kernel: &dyn PartKernel,
    cache: &NodeCache,
) -> NodeResult {
    if let Some(hit) = cache.get(key) {
        return built(hit);
    }
    if let Err(e) = std::fs::create_dir_all(cache.root()) {
        return passthrough(input, format!("cannot create the node cache: {e}"));
    }
    let out = cache.mesh_path(key);
    let part_index = input.step_part_index.map_or(-1, |i| i as i32);
    let in_frame = frame::hole_in_apply_frame(op, input);
    match kernel.hole(&input.source_path, part_index, &in_frame, &out) {
        Ok(true) => keep_cut(input, key, &out, cache),
        Ok(false) => passthrough(input, NOTHING_CUT.to_string()),
        Err(why) => passthrough(input, why),
    }
}

/// What a kernel's `Ok(false)` says to the reader, when it gives no reason of
/// its own. It can be two things — a hole that misses the body, and a boolean
/// that quietly changed nothing, as it does on a mesh that is not watertight —
/// so it names both.
const NOTHING_CUT: &str = "the hole does not cut the body: it misses the body, or the body's mesh \
                           is not watertight and the cut changed nothing";

/// The cut the kernel wrote to `out`, measured and kept for the next run. A cut
/// that cannot be measured is not kept: the file is removed, for the kernel
/// takes any file it finds at `out` for the cut and would answer with this one
/// on every later run.
fn keep_cut(input: &BodyGeometry, key: &str, out: &Path, cache: &NodeCache) -> NodeResult {
    let (triangle_count, bounds) = match measure(out) {
        Ok(measured) => measured,
        Err(why) => {
            discard(out);
            return passthrough(input, why);
        }
    };
    remember(
        cache,
        key,
        BodyGeometry {
            source_path: out.to_string_lossy().into_owned(),
            source_hash: key.to_string(),
            triangle_count,
            bounds,
            design_offset: frame::hole_output_offset(input),
            representation: Representation::Mesh,
            step_part_index: None,
        },
    )
}

/// Removes a cut that was rejected. A file that is already gone is what was
/// wanted; anything else that goes wrong is said, for a file left behind is
/// answered for by the kernel on every later run.
fn discard(out: &Path) {
    match std::fs::remove_file(out) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(path = %out.display(), %e, "cannot remove a cut that could not be measured");
        }
    }
}

/// Bounds and triangle count of a written STL — no welding or manifold
/// checks, which on a million triangles would cost more than the cut.
fn measure(path: &Path) -> Result<(u64, Bounds), String> {
    let mesh = TriangleMesh::from_stl_path(path).map_err(|e| e.to_string())?;
    let (min, max) = mesh
        .bounds()
        .ok_or_else(|| "the cut left no triangles".to_string())?;
    Ok((
        mesh.triangles.len() as u64,
        Bounds {
            min: [min[0] as f32, min[1] as f32, min[2] as f32],
            max: [max[0] as f32, max[1] as f32, max[2] as f32],
        },
    ))
}

/// A freshly built node, kept for the next run. A cache that cannot be
/// written costs the next run a rebuild, not this one its result.
fn remember(cache: &NodeCache, key: &str, geometry: BodyGeometry) -> NodeResult {
    if let Err(why) = cache.put(key, &geometry) {
        tracing::warn!(key, %why, "cannot keep a built node");
    }
    built(geometry)
}

fn built(geometry: BodyGeometry) -> NodeResult {
    NodeResult {
        geometry: Some(geometry),
        problem: None,
        is_off_key: false,
    }
}

fn failed(why: String) -> NodeResult {
    NodeResult {
        geometry: None,
        problem: Some((FeatureState::Failed, why)),
        is_off_key: false,
    }
}

/// A mesh standing in for a B-rep build that did not finish: it builds, and has
/// nothing to report, but it is off its key.
fn stand_in(geometry: BodyGeometry) -> NodeResult {
    NodeResult {
        geometry: Some(geometry),
        problem: None,
        is_off_key: true,
    }
}

/// A node that did not run because a node it reads built nothing.
fn skipped(why: String) -> NodeResult {
    NodeResult {
        geometry: None,
        problem: Some((FeatureState::Skipped, why)),
        is_off_key: false,
    }
}

fn passthrough(input: &BodyGeometry, why: String) -> NodeResult {
    NodeResult {
        geometry: Some(input.clone()),
        problem: Some((FeatureState::Failed, why)),
        is_off_key: true,
    }
}

fn is_step(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".step") || lower.ends_with(".stp")
}

/// One status per feature tag: the worst of its nodes, with that node's
/// reason. Nodes no body reaches did not run and report nothing.
fn statuses(program: &KernelProgram, results: &BTreeMap<String, NodeResult>) -> Vec<FeatureStatus> {
    let mut by_tag: BTreeMap<&str, FeatureStatus> = BTreeMap::new();
    for (id, node) in &program.nodes {
        let Some(result) = results.get(id) else {
            continue;
        };
        let (state, message) = match &result.problem {
            Some((state, why)) => (*state, Some(why.clone())),
            None => (FeatureState::Ok, None),
        };
        let entry = by_tag.entry(node.tag.as_str()).or_insert(FeatureStatus {
            tag: node.tag.clone(),
            state: FeatureState::Ok,
            message: None,
        });
        if severity(state) > severity(entry.state) {
            entry.state = state;
            entry.message = message;
        }
    }
    by_tag.into_values().collect()
}

fn severity(state: FeatureState) -> u8 {
    match state {
        FeatureState::Ok => 0,
        FeatureState::Skipped => 1,
        FeatureState::Failed => 2,
    }
}
