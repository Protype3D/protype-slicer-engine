//! CAD parts: a parametric document the UI owns, the kernel program the
//! daemon executes for it, and the bodies that program produces. The daemon
//! never reads `document` — it stores and returns it. It executes `program`.

use crate::{Bounds, FeatureOp, ObjectId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The program version this daemon executes.
pub const PROGRAM_VERSION: u32 = 1;

#[derive(
    Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct PartId(pub String);

/// Which body of which part an object on the plate shows.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct BodyRef {
    pub part_id: PartId,
    /// `<feature id>:<role>`, as the program's `bodies` names it.
    pub body_key: String,
}

/// The graph the daemon executes for a part. Node ids are the UI's to
/// choose; the daemon keys its cache by node content, not by id.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct KernelProgram {
    pub version: u32,
    pub nodes: BTreeMap<String, ProgramNode>,
    pub bodies: BTreeMap<String, ProgramBody>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ProgramBody {
    /// The node whose output this body is.
    pub node: String,
    /// Name the body's object gets when it first appears on the plate.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ProgramNode {
    /// Id of the feature this node was compiled from; statuses come back per
    /// tag.
    pub tag: String,
    pub op: NodeOp,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeOp {
    /// One of the part's recorded sources (`PartSnapshot::sources`), as is.
    Import { source: String },
    /// A `geometry.build` CSG tree, built by the best kernel available.
    Csg { tree: serde_json::Value },
    /// One cylindrical hole cut into `input`. Unlike an object's op stack,
    /// `op.origin`/`op.axis` are in the part's design frame; the executor
    /// moves them into the frame `apply_operations` cuts in.
    Hole { input: String, op: FeatureOp },
}

/// A source file a part was created from, recorded at creation so a program
/// can only import what the project already holds.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct PartSource {
    pub id: String,
    pub source_path: String,
    pub source_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_part_index: Option<u32>,
    pub triangle_count: u64,
    pub bounds: Bounds,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Representation {
    /// Real faces: a STEP file the face queries answer for.
    Brep,
    Mesh,
}

/// One built body: the file the slicer reads and where it sits in the part.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct BodyGeometry {
    pub source_path: String,
    pub source_hash: String,
    pub triangle_count: u64,
    /// Bounds in the file's own coordinates.
    pub bounds: Bounds,
    /// File coordinates + `design_offset` = the part's design coordinates.
    pub design_offset: [f32; 3],
    pub representation: Representation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_part_index: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct PartBody {
    pub key: String,
    /// The object showing this body. `None` until the body first builds. An
    /// id whose object is gone means the user removed it, and it is not
    /// rebuilt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_id: Option<ObjectId>,
    /// The last geometry that built — kept when a later run fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<BodyGeometry>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeatureState {
    Ok,
    Failed,
    /// Did not run: something it depends on failed.
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct FeatureStatus {
    pub tag: String,
    pub state: FeatureState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A whole part, as `PartSet` carries it and the project stores it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct PartSnapshot {
    pub part_id: PartId,
    pub name: String,
    /// Opaque id (a ULID) of this snapshot. An edit names the revision it
    /// was made on and is refused when the part has moved on.
    pub revision: String,
    /// The UI's document — stored and returned, never interpreted.
    pub document: serde_json::Value,
    pub program: KernelProgram,
    pub sources: Vec<PartSource>,
    pub bodies: Vec<PartBody>,
    pub statuses: Vec<FeatureStatus>,
}

/// What `ObjectGeometryUpdated` puts on an object: its geometry, its op
/// stack and which part body it shows.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ObjectGeometry {
    pub source_path: String,
    pub source_hash: String,
    pub triangle_count: u64,
    pub bounds: Bounds,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_part_index: Option<u32>,
    #[serde(default)]
    pub operations: Vec<FeatureOp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<BodyRef>,
}
