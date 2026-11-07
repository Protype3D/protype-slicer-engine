//! Checking a part's kernel program before anything runs, and the content
//! keys its nodes are cached under. Pure Rust: a malformed program is refused
//! by code PR CI tests, before it can reach a kernel.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;
use sha2::{Digest, Sha256};
use slicer_geometry::CsgNode;
use slicer_types::{FeatureOp, HeadKind, KernelProgram, NodeOp, PartSource, PROGRAM_VERSION};

use crate::geometry::tree_key;

pub const MAX_NODES: usize = 4096;
pub const MAX_BODIES: usize = 256;
/// Counted in characters, not bytes, so a limit stated in letters holds in
/// any script.
pub const MAX_TAG_LEN: usize = 128;
/// Counted in characters, not bytes. The limit of a body key and of a body name.
pub const MAX_BODY_KEY_LEN: usize = 256;
pub const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
/// The largest radius a hole may have, in mm.
pub const MAX_HOLE_RADIUS_MM: f32 = 5_000.0;
/// The deepest a blind hole may go, in mm.
pub const MAX_HOLE_DEPTH_MM: f32 = 10_000.0;
/// The shortest axis that still names a direction. `make_cutter` in
/// `crates/slicer-ffi/cpp/protype_slicer_bridge.cpp` aims a hole's cutter along
/// its axis, and below this length (`a.norm() < 1e-6f` there) it drops the axis
/// and aims along +Z instead: a shorter axis would be cut somewhere other than
/// where it points.
pub const MIN_HOLE_AXIS_LEN: f32 = 1e-6;
/// The largest magnitude any component of a hole's origin or axis may have: in
/// mm for the origin and, the axis having no unit, in the same number for the
/// axis. The bridge works in `f32`, and past this it goes wrong without a word.
/// `make_cutter` takes the axis's `norm()` in `f32`, which overflows for a
/// component beyond about 1.8e19 (the square root of `f32::MAX`) and leaves the
/// cutter aimed along +Z with the hole reported as cut; an origin near
/// `f32::MAX` gives the cutter an infinite vertex. A million mm is a kilometre —
/// far past any design, whose sizes are at most 10⁴ mm, and far short of both.
pub const MAX_HOLE_COORD_MM: f32 = 1_000_000.0;

/// Which layout of hashed content the keys of imports and holes follow, and
/// which cut a hole's key stands for. Bump it when that layout changes or when
/// the same hole on the same input can come out differently, so keys made
/// under the old one stop matching. 2: cutters turned off each other's rays
/// (`cutter_spin` in the bridge), as `ops_cache_path`'s token was.
const KEY_VERSION: u32 = 2;

/// A program that passed [`validate`]: the nodes the bodies reach, each
/// input before the node reading it, and every `csg` tree already parsed.
#[derive(Debug)]
pub struct Validated {
    pub order: Vec<String>,
    pub trees: BTreeMap<String, CsgNode>,
}

/// Check `program` against the `sources` its part was created from, before
/// anything runs it.
///
/// `Ok` guarantees that:
///
/// - the version is [`PROGRAM_VERSION`], there are at most [`MAX_NODES`]
///   nodes, and there are one to [`MAX_BODIES`] bodies;
/// - every body key reads `<feature>:<role>`, every body name and every node
///   tag is not empty and within its length limit, and every body shows a node
///   the program has;
/// - every `import` reads one of `sources`, and every `csg` tree parses and
///   passes [`CsgNode::validate`], parsed trees being returned in
///   [`Validated::trees`];
/// - every hole reads a node the program has, and is a plain `hole` that is
///   switched on, with finite numbers, a radius above 0 and at most
///   [`MAX_HOLE_RADIUS_MM`], an origin and an axis whose every component is at
///   most [`MAX_HOLE_COORD_MM`] in magnitude, an axis at least
///   [`MIN_HOLE_AXIS_LEN`] long and, when blind, a depth above 0 and at most
///   [`MAX_HOLE_DEPTH_MM`];
/// - the nodes the bodies reach have no cycle, and come back in
///   [`Validated::order`] with each node after the one it reads.
///
/// That is all it guarantees. It runs nothing: a tree that parses can still
/// fail in a kernel, and a hole can still miss the body it cuts. A node no
/// body reaches is checked on its own but neither ordered nor searched for a
/// cycle. A refusal is one line of text, naming the body or node at fault when
/// there is one.
pub fn validate(program: &KernelProgram, sources: &[PartSource]) -> Result<Validated, String> {
    check_header(program)?;
    check_bodies(program)?;
    let trees = check_nodes(program, sources)?;
    let order = topological_order(program)?;
    Ok(Validated { order, trees })
}

/// Content key per node: the operation, its numbers and the keys of its
/// inputs — never the node id, the node's tag or a feature's id. Two programs
/// describing the same geometry share cache entries whatever they call their
/// nodes.
///
/// These are the keys of the program as written. A hole cut into what is not the
/// real thing — what a failed hole passed through, or a mesh standing in for a
/// B-rep build that did not finish — is kept under a key of the executor's own,
/// made from the one here by `rekey` in `executor.rs`; a change to how keys are
/// made must be read against that function too.
///
/// # Panics
///
/// When `validated`, `program` and `sources` are not the ones [`validate`] was
/// given: an import whose source is not in `sources`, an `order` naming a node
/// the program lacks or putting a node before the one it reads, and a `csg`
/// node without a parsed tree all panic instead of getting a made-up key.
pub fn node_keys(
    program: &KernelProgram,
    validated: &Validated,
    sources: &[PartSource],
) -> BTreeMap<String, String> {
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    for id in &validated.order {
        let key = match &program.nodes[id].op {
            NodeOp::Import { source } => {
                let source = sources
                    .iter()
                    .find(|s| &s.id == source)
                    .expect("validate checked every import's source");
                hash(&json!({
                    "v": KEY_VERSION,
                    "kind": "import",
                    "source_hash": source.source_hash,
                    "step_part_index": source.step_part_index,
                }))
            }
            // The key `geometry.build` gives the same tree, so a part and a
            // plain generated object built from one tree share their file.
            NodeOp::Csg { .. } => {
                tree_key(&validated.trees[id])
                    .expect("a parsed CSG tree is plain data and always serializes")
                    .hash
            }
            NodeOp::Hole { input, op } => {
                let mut content = json!({
                    "v": KEY_VERSION,
                    "kind": "hole",
                    "input": keys[input],
                    "origin": op.origin,
                    "axis": op.axis,
                    "radius_mm": op.radius_mm,
                    "depth_mm": op.depth_mm,
                    "through": op.through,
                });
                // Only with a head: a plain hole keeps the key its cut is
                // cached under.
                if op.head_kind != HeadKind::None {
                    content["head"] = json!({
                        "kind": op.head_kind,
                        "dia_mm": op.head_dia_mm,
                        "depth_mm": op.head_depth_mm,
                        "angle_deg": op.head_angle_deg,
                    });
                }
                hash(&content)
            }
        };
        keys.insert(id.clone(), key);
    }
    keys
}

/// SHA-256 of the value's compact JSON text, in hex.
///
/// The text writes an object's keys in sorted order, because no crate in the
/// workspace turns on `serde_json`'s `preserve_order`. If one ever does, every
/// key made here changes: a cache miss, never a wrong part.
fn hash(value: &serde_json::Value) -> String {
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

/// The program's version and the size of its two maps.
fn check_header(program: &KernelProgram) -> Result<(), String> {
    if program.version != PROGRAM_VERSION {
        return Err(format!(
            "program version {} is not supported; this daemon runs version {PROGRAM_VERSION}",
            program.version
        ));
    }
    if program.nodes.len() > MAX_NODES {
        return Err(format!(
            "a program has at most {MAX_NODES} nodes, got {}",
            program.nodes.len()
        ));
    }
    if program.bodies.is_empty() {
        return Err("a program needs at least one body".to_string());
    }
    if program.bodies.len() > MAX_BODIES {
        return Err(format!(
            "a program has at most {MAX_BODIES} bodies, got {}",
            program.bodies.len()
        ));
    }
    Ok(())
}

/// Each body's key and name, and that the node it shows exists.
fn check_bodies(program: &KernelProgram) -> Result<(), String> {
    for (key, body) in &program.bodies {
        check_body_key(key)?;
        if !is_valid_name(&body.name, MAX_BODY_KEY_LEN) {
            return Err(format!(
                "body {key}: the name must be 1 to {MAX_BODY_KEY_LEN} characters"
            ));
        }
        if !program.nodes.contains_key(&body.node) {
            return Err(format!(
                "body {key} names node {}, which the program does not have",
                body.node
            ));
        }
    }
    Ok(())
}

/// Whether `name` reads as a name: not blank, and at most `max_chars`
/// characters. Counted in characters, not bytes, so a limit stated in letters
/// holds in any script. The rule of a body's name and of a part's.
pub(crate) fn is_valid_name(name: &str, max_chars: usize) -> bool {
    !name.trim().is_empty() && name.chars().count() <= max_chars
}

fn check_body_key(key: &str) -> Result<(), String> {
    let well_formed = key.chars().count() <= MAX_BODY_KEY_LEN
        && key
            .split_once(':')
            .is_some_and(|(tag, role)| !tag.is_empty() && !role.is_empty());
    if well_formed {
        Ok(())
    } else {
        Err(format!(
            "body key {key:?} must read <feature>:<role> and be at most {MAX_BODY_KEY_LEN} characters"
        ))
    }
}

/// Each node's tag and operation. Returns the parsed tree of every `csg` node.
fn check_nodes(
    program: &KernelProgram,
    sources: &[PartSource],
) -> Result<BTreeMap<String, CsgNode>, String> {
    let source_ids: BTreeSet<&str> = sources.iter().map(|s| s.id.as_str()).collect();
    let mut trees = BTreeMap::new();
    for (id, node) in &program.nodes {
        if node.tag.is_empty() || node.tag.chars().count() > MAX_TAG_LEN {
            return Err(format!(
                "node {id}: the tag must be 1 to {MAX_TAG_LEN} characters"
            ));
        }
        match &node.op {
            NodeOp::Import { source } => check_import(id, source, &source_ids)?,
            NodeOp::Csg { tree } => {
                trees.insert(id.clone(), parse_tree(id, tree)?);
            }
            NodeOp::Hole { input, op } => check_hole_node(program, id, input, op)?,
        }
    }
    Ok(trees)
}

fn check_import(id: &str, source: &str, source_ids: &BTreeSet<&str>) -> Result<(), String> {
    if source_ids.contains(source) {
        return Ok(());
    }
    Err(format!(
        "node {id} imports source {source}, which this part was not created from"
    ))
}

/// A `csg` node's tree as the parser reads it, held to the limits
/// `geometry.build` holds a tree to.
fn parse_tree(id: &str, tree: &serde_json::Value) -> Result<CsgNode, String> {
    let parsed: CsgNode = serde_json::from_value(tree.clone())
        .map_err(|e| format!("node {id}: invalid CSG tree: {e}"))?;
    parsed.validate().map_err(|e| format!("node {id}: {e}"))?;
    // A node's key is made from the tree's text alone, and that says nothing
    // of what a file behind it holds. A program has its own `import` node,
    // which reads a recorded source and is keyed by its content.
    if let Some(reference) = parsed.imports().first() {
        return Err(format!(
            "node {id}: a `csg` tree cannot import {reference}: add it as a source of the part \
             and read it with an `import` node of the program"
        ));
    }
    Ok(parsed)
}

fn check_hole_node(
    program: &KernelProgram,
    id: &str,
    input: &str,
    op: &FeatureOp,
) -> Result<(), String> {
    if !program.nodes.contains_key(input) {
        return Err(format!(
            "node {id} cuts into node {input}, which the program does not have"
        ));
    }
    check_hole(id, op)
}

/// What `apply_operations` will cut: a plain, switched-on `hole` whose numbers
/// a cutter can be built from.
fn check_hole(id: &str, op: &FeatureOp) -> Result<(), String> {
    if op.kind != "hole" {
        return Err(format!(
            "node {id}: a hole node carries kind \"hole\", got {:?}",
            op.kind
        ));
    }
    // `apply_operations` skips a suppressed op without a word, and the node's
    // key would not tell it from the same hole unsuppressed. The compiler
    // leaves a suppressed feature out of the program, so this is a client
    // that did not.
    if op.suppressed {
        return Err(format!(
            "node {id}: a program carries no suppressed hole; leave the feature out of it"
        ));
    }
    check_hole_numbers(id, op)
}

fn check_hole_numbers(id: &str, op: &FeatureOp) -> Result<(), String> {
    let finite = op
        .origin
        .iter()
        .chain(op.axis.iter())
        .all(|v| v.is_finite())
        && op.radius_mm.is_finite()
        && op.depth_mm.is_finite();
    if !finite {
        return Err(format!("node {id}: every hole number must be finite"));
    }
    check_hole_coordinates(id, op)?;
    if op.radius_mm <= 0.0 || op.radius_mm > MAX_HOLE_RADIUS_MM {
        return Err(format!(
            "node {id}: the hole radius must be above 0 and at most {MAX_HOLE_RADIUS_MM} mm, got {}",
            op.radius_mm
        ));
    }
    if axis_length(&op.axis) < f64::from(MIN_HOLE_AXIS_LEN) {
        return Err(format!(
            "node {id}: the hole axis must be at least {MIN_HOLE_AXIS_LEN} long, got {:?}",
            op.axis
        ));
    }
    if !op.through && (op.depth_mm <= 0.0 || op.depth_mm > MAX_HOLE_DEPTH_MM) {
        return Err(format!(
            "node {id}: a blind hole needs a depth above 0 and at most {MAX_HOLE_DEPTH_MM} mm, got {}",
            op.depth_mm
        ));
    }
    Ok(())
}

/// The origin and the axis of a hole within [`MAX_HOLE_COORD_MM`], component by
/// component: finite is not enough for the bridge's `f32`.
fn check_hole_coordinates(id: &str, op: &FeatureOp) -> Result<(), String> {
    for (what, vector) in [("origin", &op.origin), ("axis", &op.axis)] {
        if vector.iter().any(|v| v.abs() > MAX_HOLE_COORD_MM) {
            return Err(format!(
                "node {id}: every component of the hole {what} must be at most \
                 {MAX_HOLE_COORD_MM} in magnitude, got {vector:?}"
            ));
        }
    }
    Ok(())
}

/// The length of `axis`, summed in `f64` so that no component's square can
/// overflow or vanish.
fn axis_length(axis: &[f32; 3]) -> f64 {
    axis.iter()
        .map(|v| f64::from(*v).powi(2))
        .sum::<f64>()
        .sqrt()
}

/// Where a node stands in the walk of [`topological_order`].
enum Mark {
    /// On the way down from a body: its input is not ordered yet.
    Visiting,
    /// Ordered, along with everything it reads.
    Done,
}

/// One thing the walk still has to do.
enum Step<'a> {
    /// Reach the node, and first whatever it reads.
    Enter(&'a str),
    /// Everything the node reads is ordered: order the node itself.
    Exit(&'a str),
}

/// The nodes the bodies reach, every input before the node reading it.
/// Refuses a cycle rather than looping on it.
///
/// The walk keeps its own stack instead of recursing. A chain as long as
/// [`MAX_NODES`] is a legal program, and a recursive walk of it takes over a
/// megabyte of call stack in a debug build: a request must be refused or run,
/// never overflow the thread that reads it.
fn topological_order(program: &KernelProgram) -> Result<Vec<String>, String> {
    let mut marks = BTreeMap::new();
    let mut order = Vec::new();
    for body in program.bodies.values() {
        walk_from(&body.node, program, &mut marks, &mut order)?;
    }
    Ok(order)
}

/// Orders `root` and what it reads, the nodes earlier walks did not reach yet.
fn walk_from<'a>(
    root: &'a str,
    program: &'a KernelProgram,
    marks: &mut BTreeMap<&'a str, Mark>,
    order: &mut Vec<String>,
) -> Result<(), String> {
    let mut steps = vec![Step::Enter(root)];
    while let Some(step) = steps.pop() {
        match step {
            Step::Enter(id) => enter(id, program, marks, &mut steps)?,
            Step::Exit(id) => {
                marks.insert(id, Mark::Done);
                order.push(id.to_string());
            }
        }
    }
    Ok(())
}

/// Reaches `id`: marks it, and puts the node it reads on the stack above its
/// own exit, so that node is ordered first.
fn enter<'a>(
    id: &'a str,
    program: &'a KernelProgram,
    marks: &mut BTreeMap<&'a str, Mark>,
    steps: &mut Vec<Step<'a>>,
) -> Result<(), String> {
    match marks.get(id) {
        Some(Mark::Done) => {}
        Some(Mark::Visiting) => return Err(format!("the program has a cycle through node {id}")),
        None => {
            marks.insert(id, Mark::Visiting);
            steps.push(Step::Exit(id));
            steps.extend(input_of(program, id).map(Step::Enter));
        }
    }
    Ok(())
}

/// The node `id` reads, if it reads one. Every kind is named, so a new kind
/// that reads nodes cannot be passed over here without the compiler saying so.
fn input_of<'a>(program: &'a KernelProgram, id: &str) -> Option<&'a str> {
    match &program.nodes.get(id)?.op {
        NodeOp::Hole { input, .. } => Some(input.as_str()),
        NodeOp::Import { .. } | NodeOp::Csg { .. } => None,
    }
}
