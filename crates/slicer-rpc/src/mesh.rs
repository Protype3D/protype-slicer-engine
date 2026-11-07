//! `mesh.*` — measure a mesh instead of writing a one-off numpy script
//! to do it.
//!
//! Every arm here loads triangles and answers from them. None of them
//! consults the settings, the CSG tree or the parameters an object was
//! generated from: a bore reported from the parameter that cut it will
//! report the designed diameter for a bore something is sticking into.
//!
//! Meshes are identified by `object_id` (resolved through project state)
//! or by `source_path` for a file that is not a project object — the
//! reference part being measured usually is not one.
//!
//! An object is measured as it prints: with the holes its feature-op stack
//! cuts, whenever an op is active. Its coordinates stay where its source file
//! puts it either way — see [`MeshFile::offset`].

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};
use slicer_geometry::mesh::TriangleMesh;
use slicer_geometry::{measure, planar_faces, section, thickness, MeshError, PlanarFace};
use slicer_types::ObjectId;

use crate::error::RpcError;

/// Which mesh. Exactly one of the two is required.
#[derive(Debug, Deserialize)]
pub struct MeshRef {
    #[serde(default)]
    pub object_id: Option<slicer_types::ObjectId>,
    #[serde(default)]
    pub source_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SectionParams {
    #[serde(flatten)]
    pub mesh: MeshRef,
    pub z: f64,
}

#[derive(Debug, Deserialize)]
pub struct AxisScanParams {
    #[serde(flatten)]
    pub mesh: MeshRef,
    /// `[x, y]` of the vertical axis to measure about.
    pub axis: [f64; 2],
    pub z_from: f64,
    #[serde(default)]
    pub z_to: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub rays: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct InscribedParams {
    #[serde(flatten)]
    pub mesh: MeshRef,
    pub z: f64,
    #[serde(default)]
    pub precision_mm: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct ThicknessParams {
    #[serde(flatten)]
    pub mesh: MeshRef,
    pub axis: [f64; 2],
    pub z: f64,
    #[serde(default)]
    pub rays: Option<usize>,
}

const DEFAULT_PRECISION_MM: f64 = 0.01;
const DEFAULT_STEP_MM: f64 = 1.0;

/// Where the triangles of a mesh reference are on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshFile {
    pub path: String,
    /// Added to every vertex on loading, when `path` is the cut of a
    /// feature-op stack: [`recentering_offset`], which puts the cut back
    /// where its source puts the part.
    ///
    /// `apply_operations` recenters the part on its bounding box before it
    /// cuts — the frame feature-op origins are given in — and writes the cut
    /// in that frame. An object without ops is measured where its source puts
    /// it, and an axis a caller found there has to keep pointing at the same
    /// bore once a hole is cut.
    pub offset: Option<[f64; 3]>,
}

impl MeshFile {
    /// A mesh measured where it is.
    pub fn at(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            offset: None,
        }
    }

    /// The cut of a feature-op stack, and the offset back to its source's
    /// frame.
    pub fn cut(path: impl Into<String>, offset: [f64; 3]) -> Self {
        Self {
            path: path.into(),
            offset: Some(offset),
        }
    }
}

/// The objects a call names, each resolved to its [`MeshFile`] before the
/// call goes to a blocking thread: finding an object's mesh reads project
/// state and may cut its feature ops through the FFI, neither of which
/// belongs where the triangles are loaded.
pub type ObjectMeshes = HashMap<ObjectId, MeshFile>;

/// Resolve a mesh reference to triangles. Shared with `verify.*`, which
/// takes the same reference shape.
pub fn load_mesh(meshes: &ObjectMeshes, reference: &MeshRef) -> Result<TriangleMesh, RpcError> {
    let file = mesh_file(meshes, reference)?;
    let mesh = TriangleMesh::from_stl_path(&file.path).map_err(mesh_error)?;
    Ok(match file.offset {
        Some(offset) => mesh.translated(offset),
        None => mesh,
    })
}

/// What a mesh reference names: one of the project objects the call named,
/// as resolved before it went to a blocking thread, or a file that is not a
/// project object.
pub enum Referenced<'a, T> {
    Object(&'a T),
    File(&'a str),
}

/// Look a mesh reference up in `objects`, the call's resolved project
/// objects. Shared with `slice.estimate`, which resolves an object to what a
/// slice of it is given rather than to a mesh file.
pub fn referenced<'a, T>(
    objects: &'a HashMap<ObjectId, T>,
    reference: &'a MeshRef,
) -> Result<Referenced<'a, T>, RpcError> {
    match (&reference.object_id, &reference.source_path) {
        (Some(id), _) => objects
            .get(id)
            .map(Referenced::Object)
            .ok_or_else(|| RpcError::Project(format!("object_id not found: {id:?}"))),
        (None, Some(path)) => Ok(Referenced::File(path)),
        (None, None) => Err(RpcError::Project(
            "give either object_id or source_path".to_string(),
        )),
    }
}

fn mesh_file(meshes: &ObjectMeshes, reference: &MeshRef) -> Result<MeshFile, RpcError> {
    let file = match referenced(meshes, reference)? {
        Referenced::Object(file) => file.clone(),
        Referenced::File(path) => MeshFile::at(path),
    };
    if file.path.is_empty() {
        return Err(RpcError::Project(
            "that object has no mesh on disk to measure".to_string(),
        ));
    }
    Ok(file)
}

/// How far `apply_operations` moved a part before cutting it, worked out
/// from `uncut_path` — the uncut part — the way the bridge's
/// `recenter_to_local_bbox` does it: to the XY centre and the lowest point of
/// its bounding box, rounded to `f32`. Adding it to the cut puts the cut back
/// where the uncut part is. Keep the two in step.
///
/// Exact only while `uncut_path` has the bounding box the bridge's
/// `raw_mesh()` had. It does for a STEP's edit mesh (the tessellation
/// libslic3r imports the STEP with) and for a closed STL, whose vertices
/// libslic3r's import repair leaves alone. An open STL can differ by what that
/// repair moved or dropped — snapped vertices, stray triangles — and the whole
/// cut is then off by that much: sizes stay right, positions do not.
pub fn recentering_offset(uncut_path: &str) -> Result<[f64; 3], RpcError> {
    let uncut = TriangleMesh::from_stl_path(uncut_path).map_err(mesh_error)?;
    recentering_of(&uncut)
}

fn recentering_of(uncut: &TriangleMesh) -> Result<[f64; 3], RpcError> {
    let (min, max) = uncut
        .bounds()
        .ok_or_else(|| RpcError::Project("mesh: the uncut part has no triangles".to_string()))?;
    let as_bridge = |value: f64| f64::from(value as f32);
    Ok([
        as_bridge((min[0] + max[0]) / 2.0),
        as_bridge((min[1] + max[1]) / 2.0),
        as_bridge(min[2]),
    ])
}

/// The most faces `object.query_faces` lists for a mesh. A scan has a face per
/// triangle, and an agent wants the ones it could drill into, which are the
/// large ones. `faces_total` says how many there were.
const MAX_LISTED_FACES: usize = 256;

/// Coordinates to a nanometre: all an STL's `f32` can honestly say at this
/// size, and a third the JSON.
fn rounded(point: [f64; 3]) -> [f64; 3] {
    point.map(|value| (value * 1e6).round() / 1e6)
}

fn rounded_loop(points: &[[f64; 3]]) -> Vec<[f64; 3]> {
    points.iter().map(|point| rounded(*point)).collect()
}

/// A flat face, under the names a STEP's plane face goes by as far as they
/// apply: `axis` is its outward normal, and a client's reading of `surface`,
/// `axis` and `origin` needs no second path. `tri_start` and `tri_count` do not
/// apply: an STL's face is not one run of the file's triangles, so what they
/// are read for comes ready here, as `area_mm2`, `outline` and `holes`.
fn planar_face_json(face_id: usize, face: &PlanarFace) -> Value {
    json!({
        "face_id": face_id,
        "surface": "plane",
        "axis": rounded(face.normal),
        "origin": rounded(face.origin),
        "radius_mm": Value::Null,
        "plane_offset_mm": (face.plane_offset_mm * 1e6).round() / 1e6,
        "area_mm2": (face.area_mm2 * 1e6).round() / 1e6,
        "triangle_count": face.triangle_count,
        "outline": rounded_loop(&face.outline),
        "holes": face.holes.iter().map(|hole| rounded_loop(hole)).collect::<Vec<_>>(),
    })
}

/// `object.query_faces` for an STL: its flat faces, largest first, in the
/// file's own coordinates, as the faces of a STEP are. They are the source's,
/// before any hole its operation stack cuts.
///
/// `object.set_operations` takes the part's local frame instead, so
/// `operation_frame_origin` says where that frame sits in the file's: subtract
/// it from a face's coordinates to aim a hole at the face. Reads the whole
/// file, so call it from a blocking thread.
pub fn planar_face_report(stl_path: &str) -> Result<Value, RpcError> {
    let mesh = TriangleMesh::from_stl_path(stl_path).map_err(mesh_error)?;
    let found = planar_faces(&mesh, MAX_LISTED_FACES);
    let faces: Vec<Value> = found
        .faces
        .iter()
        .enumerate()
        .map(|(face_id, face)| planar_face_json(face_id, face))
        .collect();
    Ok(json!({
        "source": "mesh",
        "faces": faces,
        "faces_total": found.total,
        "operation_frame_origin": recentering_of(&mesh)?,
    }))
}

/// Measurement failures are the caller's problem to act on — an axis in
/// the wrong place, a Z outside the part — so they keep their own text
/// rather than being flattened into "internal error".
fn mesh_error(e: MeshError) -> RpcError {
    RpcError::Project(format!("mesh: {e}"))
}

pub fn handle_measure(meshes: &ObjectMeshes, params: Value) -> Result<Value, RpcError> {
    let p: MeshRef = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    let measured = load_mesh(meshes, &p)?.measure().map_err(mesh_error)?;
    serde_json::to_value(measured)
        .map_err(|e| RpcError::Internal(format!("cannot serialize measurement: {e}")))
}

pub fn handle_section(meshes: &ObjectMeshes, params: Value) -> Result<Value, RpcError> {
    let p: SectionParams = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    let mesh = load_mesh(meshes, &p.mesh)?;
    let cut = section::section_at_z(&mesh, p.z).map_err(mesh_error)?;
    Ok(json!({
        "z": cut.z,
        "contours": cut.contours,
        // An open contour means the surface has a hole at this height.
        // Surfaced separately so a caller does not have to notice it.
        "open_contours": cut.contours.iter().filter(|c| !c.closed).count(),
    }))
}

pub fn handle_profile(meshes: &ObjectMeshes, params: Value) -> Result<Value, RpcError> {
    let p: AxisScanParams = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    let mesh = load_mesh(meshes, &p.mesh)?;
    let samples = measure::profile(
        &mesh,
        p.axis,
        p.z_from,
        p.z_to.unwrap_or(p.z_from),
        p.step.unwrap_or(DEFAULT_STEP_MM),
    )
    .map_err(mesh_error)?;
    let max_reach = samples
        .iter()
        .filter_map(|s| s.max_reach_mm)
        .fold(f64::NEG_INFINITY, f64::max);
    Ok(json!({
        "axis": p.axis,
        "samples": samples,
        "max_reach_mm": max_reach.is_finite().then_some(max_reach),
    }))
}

/// Free circle around an axis. Scans a Z range when `z_to` is given,
/// which is the form that matters: a bore measured at one height is the
/// mistake this call exists to stop.
pub fn handle_free_circle(meshes: &ObjectMeshes, params: Value) -> Result<Value, RpcError> {
    let p: AxisScanParams = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    let mesh = load_mesh(meshes, &p.mesh)?;
    let rays = p.rays.unwrap_or(measure::DEFAULT_RAY_COUNT);

    let samples = match p.z_to {
        Some(z_to) => measure::free_circle_over(
            &mesh,
            p.axis,
            p.z_from,
            z_to,
            p.step.unwrap_or(DEFAULT_STEP_MM),
            rays,
        )
        .map_err(mesh_error)?,
        None => vec![measure::free_circle(&mesh, p.axis, p.z_from, rays).map_err(mesh_error)?],
    };

    let tightest = samples
        .iter()
        .min_by(|a, b| a.diameter_mm.total_cmp(&b.diameter_mm));

    Ok(json!({
        "axis": p.axis,
        "ray_count": rays,
        // The number a caller actually wants: the narrowest point over
        // the whole depth, not the one at the height they happened to ask.
        "min_diameter_mm": tightest.map(|s| s.diameter_mm),
        "min_at_z": tightest.map(|s| s.z),
        "samples": samples,
    }))
}

/// Find a bore's axis instead of being told it.
///
/// `free_circle` loses exactly twice whatever the given axis is off by,
/// so on a part whose axis is not already known — which is the case §1
/// exists for — the answer has to be centred first.
pub fn handle_centre_bore(meshes: &ObjectMeshes, params: Value) -> Result<Value, RpcError> {
    let p: AxisScanParams = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    let mesh = load_mesh(meshes, &p.mesh)?;
    let centred = measure::centre_bore(
        &mesh,
        p.axis,
        p.z_from,
        p.rays.unwrap_or(measure::DEFAULT_RAY_COUNT),
        p.step.unwrap_or(0.01),
    )
    .map_err(mesh_error)?;
    Ok(json!({
        "seed": p.axis,
        "axis": centred.axis,
        "moved_mm": ((centred.axis[0] - p.axis[0]).powi(2)
            + (centred.axis[1] - p.axis[1]).powi(2))
        .sqrt(),
        "diameter_mm": centred.diameter_mm,
        "enclosed": centred.enclosed,
        "z": centred.z,
    }))
}

pub fn handle_inscribed(meshes: &ObjectMeshes, params: Value) -> Result<Value, RpcError> {
    let p: InscribedParams = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    let mesh = load_mesh(meshes, &p.mesh)?;
    let circle =
        measure::inscribed_circle(&mesh, p.z, p.precision_mm.unwrap_or(DEFAULT_PRECISION_MM))
            .map_err(mesh_error)?;
    serde_json::to_value(circle)
        .map_err(|e| RpcError::Internal(format!("cannot serialize circle: {e}")))
}

pub fn handle_wall_thickness(meshes: &ObjectMeshes, params: Value) -> Result<Value, RpcError> {
    let p: ThicknessParams = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    let mesh = load_mesh(meshes, &p.mesh)?;
    let measured = thickness::wall_thickness(
        &mesh,
        p.axis,
        p.z,
        p.rays.unwrap_or(measure::DEFAULT_RAY_COUNT),
    )
    .map_err(mesh_error)?;
    Ok(json!({
        "z": measured.z,
        "axis": measured.axis,
        "ray_count": measured.ray_count,
        "min_thickness_mm": measured.min_thickness_mm,
        "min_at_deg": measured.min_at_deg,
        "has_breakthrough": measured.has_breakthrough(),
        "breakthrough_at_deg": measured.breakthrough_at_deg,
    }))
}
