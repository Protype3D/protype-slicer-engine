//! `verify.*` — checks that must have been seen failing to be cited.
//!
//! Every result carries `proven_falsifiable`. It is not a parameter and
//! there is no way to set it over the wire: it comes from the session
//! ledger, and the only thing that writes to that ledger is
//! `verify.selftest` actually watching the check go red on a
//! deliberately broken fixture.
//!
//! `verify.status` exists so an agent can ask what it is allowed to cite
//! before it starts, rather than discovering afterwards that it quoted a
//! check nobody had tested.

use serde::Deserialize;
use serde_json::{json, Value};
use slicer_geometry::mesh::TriangleMesh;
use slicer_verify::{checks, ledger, selftest, Check};

use crate::error::RpcError;
use crate::mesh::{MeshRef, ObjectMeshes};

const DEFAULT_RAYS: usize = 360;

#[derive(Debug, Deserialize)]
pub struct BoreParams {
    #[serde(flatten)]
    pub mesh: MeshRef,
    pub axis: [f64; 2],
    pub z_from: f64,
    pub z_to: f64,
    pub need_diameter_mm: f64,
    #[serde(default)]
    pub step_mm: Option<f64>,
    #[serde(default)]
    pub rays: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct FitParams {
    pub hole: MeshRef,
    pub hole_axis: [f64; 2],
    pub hole_z: [f64; 2],
    pub peg: MeshRef,
    pub peg_axis: [f64; 2],
    pub peg_z: [f64; 2],
    #[serde(default)]
    pub wanted_clearance_mm: Option<f64>,
    #[serde(default)]
    pub step_mm: Option<f64>,
    #[serde(default)]
    pub rays: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct InterferenceParams {
    pub parts: Vec<NamedMesh>,
    #[serde(default)]
    pub step_mm: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct NamedMesh {
    pub name: String,
    #[serde(flatten)]
    pub mesh: MeshRef,
}

#[derive(Debug, Deserialize)]
pub struct OverhangParams {
    #[serde(flatten)]
    pub mesh: MeshRef,
    #[serde(default = "default_threshold")]
    pub threshold_deg: f64,
    /// Height treated as the build plate. Defaults to the mesh's own
    /// lowest point — "the part sits on the plate".
    #[serde(default)]
    pub bed_z: Option<f64>,
}

fn default_threshold() -> f64 {
    45.0
}

#[derive(Debug, Deserialize, Default)]
pub struct SelfTestParams {
    /// One check, or all of them when absent.
    #[serde(default)]
    pub check: Option<Check>,
}

pub fn handle_bore(meshes: &ObjectMeshes, params: Value) -> Result<Value, RpcError> {
    let p: BoreParams = parse(params)?;
    let mesh = crate::mesh::load_mesh(meshes, &p.mesh)?;
    let verdict = checks::bore(
        &mesh,
        p.axis,
        p.z_from,
        p.z_to,
        p.need_diameter_mm,
        p.step_mm.unwrap_or(checks::DEFAULT_STEP_MM),
        p.rays.unwrap_or(DEFAULT_RAYS),
    )
    .map_err(mesh_error)?;
    Ok(render(&verdict))
}

pub fn handle_fit(meshes: &ObjectMeshes, params: Value) -> Result<Value, RpcError> {
    let p: FitParams = parse(params)?;
    let hole = crate::mesh::load_mesh(meshes, &p.hole)?;
    let peg = crate::mesh::load_mesh(meshes, &p.peg)?;
    let verdict = checks::fit(
        &hole,
        p.hole_axis,
        (p.hole_z[0], p.hole_z[1]),
        &peg,
        p.peg_axis,
        (p.peg_z[0], p.peg_z[1]),
        p.wanted_clearance_mm.unwrap_or(0.0),
        p.step_mm.unwrap_or(checks::DEFAULT_STEP_MM),
        p.rays.unwrap_or(DEFAULT_RAYS),
    )
    .map_err(mesh_error)?;
    Ok(render(&verdict))
}

pub fn handle_interference(meshes: &ObjectMeshes, params: Value) -> Result<Value, RpcError> {
    let p: InterferenceParams = parse(params)?;
    let parts: Vec<(String, TriangleMesh)> = p
        .parts
        .iter()
        .map(|part| {
            crate::mesh::load_mesh(meshes, &part.mesh).map(|mesh| (part.name.clone(), mesh))
        })
        .collect::<Result<_, _>>()?;
    let verdict = checks::interference(&parts, p.step_mm.unwrap_or(checks::DEFAULT_STEP_MM))
        .map_err(mesh_error)?;
    Ok(render(&verdict))
}

pub fn handle_overhangs(meshes: &ObjectMeshes, params: Value) -> Result<Value, RpcError> {
    let p: OverhangParams = parse(params)?;
    let mesh = crate::mesh::load_mesh(meshes, &p.mesh)?;
    let verdict = match p.bed_z {
        Some(bed_z) => checks::overhangs_above(&mesh, p.threshold_deg, bed_z),
        None => checks::overhangs(&mesh, p.threshold_deg),
    }
    .map_err(mesh_error)?;
    Ok(render(&verdict))
}

/// Run the self-tests. The only thing that can make a check citable.
pub fn handle_selftest(params: Value) -> Result<Value, RpcError> {
    let p: SelfTestParams = serde_json::from_value(params).unwrap_or_default();
    let reports = match p.check {
        Some(check) => vec![selftest::run(check)],
        None => selftest::run_all(),
    };
    let all_red = reports.iter().all(|r| r.passed);
    Ok(json!({
        // Every self-test going red is the *good* outcome: it means each
        // check can see the defect it exists for.
        "all_checks_proven_falsifiable": all_red,
        "reports": reports,
        "proven": ledger::proven_checks(),
        "unproven": ledger::unproven_checks(),
    }))
}

/// What may currently be cited.
pub fn handle_status() -> Result<Value, RpcError> {
    let unproven = ledger::unproven_checks();
    Ok(json!({
        "proven": ledger::proven_checks(),
        "unproven": unproven,
        "ready": unproven.is_empty(),
        "note": "a pass from an unproven check is not evidence; run verify.selftest first",
    }))
}

fn render(verdict: &slicer_verify::Verdict) -> Value {
    let mut value = serde_json::to_value(verdict).unwrap_or(Value::Null);
    if let Some(caveat) = verdict.caveat() {
        value["caveat"] = json!(caveat);
    }
    // Spelled out separately from `ok` so a caller reading quickly cannot
    // mistake one for the other.
    value["credible_pass"] = json!(verdict.is_credible_pass());
    value
}

fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::Project(format!("invalid params: {e}")))
}

fn mesh_error(e: slicer_geometry::MeshError) -> RpcError {
    RpcError::Project(format!("verify: {e}"))
}
