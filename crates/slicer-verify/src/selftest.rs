//! Run each check against something that is definitely broken, and watch
//! it go red.
//!
//! The fixtures here are not "edge cases". Each one is the defect the
//! check was written after:
//!
//! * **bore** — a bore with a crossbar left poking into it by a boolean.
//!   Bounds, volume and manifoldness cannot tell it from the intact part.
//! * **fit** — a peg wider than the hole it is meant to enter. This is
//!   the measurement that has to be trusted at 0.1 mm, so it had better
//!   be able to report a negative number.
//! * **interference** — two boxes sharing a corner.
//! * **overhangs** — a horizontal ceiling, which is 90 degrees from
//!   vertical and cannot be printed unsupported by anything.
//!
//! A self-test that comes back green is itself a failure, and is reported
//! as one: it means the check cannot see its own defect, so nothing it
//! ever says is worth acting on.

use serde::{Deserialize, Serialize};
use slicer_geometry::mesh::TriangleMesh;
use slicer_geometry::CsgNode;

use crate::{checks, ledger, Check};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelfTestReport {
    pub check: Check,
    /// What the deliberately broken input was.
    pub fixture: String,
    /// What the check said about it.
    pub observed: String,
    /// True when the check reported the defect — i.e. came back `ok:
    /// false`. That, and only that, is what earns `proven_falsifiable`.
    pub passed: bool,
    /// Present when the self-test itself could not be run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Run one check's self-test. On success, the check becomes citable for
/// the rest of this process.
pub fn run(check: Check) -> SelfTestReport {
    let outcome = match check {
        Check::Bore => broken_bore(),
        Check::Fit => oversized_peg(),
        Check::Interference => overlapping_boxes(),
        Check::Overhangs => a_flat_ceiling(),
    };

    match outcome {
        Ok((fixture, verdict)) => {
            // The whole contract in one line: red earns the flag, green
            // does not.
            let passed = !verdict.ok;
            if passed {
                ledger::record(check);
            }
            SelfTestReport {
                check,
                fixture,
                observed: verdict.summary,
                passed,
                error: None,
            }
        }
        Err((fixture, error)) => SelfTestReport {
            check,
            fixture,
            observed: String::new(),
            passed: false,
            error: Some(error),
        },
    }
}

/// Run every check's self-test.
pub fn run_all() -> Vec<SelfTestReport> {
    Check::ALL.iter().map(|c| run(*c)).collect()
}

type Fixture = Result<(String, crate::Verdict), (String, String)>;

fn mesh_of(tree: serde_json::Value) -> Result<TriangleMesh, String> {
    let node: CsgNode = serde_json::from_value(tree).map_err(|e| e.to_string())?;
    let built = slicer_geometry::build_stl(&node).map_err(|e| e.to_string())?;
    TriangleMesh::from_stl_bytes(&built.stl).map_err(|e| e.to_string())
}

fn cylinder(diameter: f64, height: f64, z: f64) -> serde_json::Value {
    serde_json::json!({"op":"translate","offset":[0.0,0.0,z],
        "child":{"op":"cylinder","diameter":diameter,"height":height,"segments":128}})
}

/// A Ø53.72 bore with a bar intruding to 13 mm from the axis — the shape
/// the original defect had, and the numbers the original one-off script
/// reported.
fn broken_bore() -> Fixture {
    const FIXTURE: &str = "a Ø53.72 bore with a crossbar left protruding into it";
    let mesh = mesh_of(serde_json::json!({"op":"union","children":[
        {"op":"difference","children":[
            cylinder(68.0, 20.0, 0.0),
            cylinder(53.72, 22.0, -1.0),
        ]},
        {"op":"translate","offset":[-25.0, 13.0, 6.0],
         "child":{"op":"box","size":[50.0, 6.0, 8.0]}},
    ]}))
    .map_err(|e| (FIXTURE.to_string(), e))?;

    checks::bore(&mesh, [0.0, 0.0], 1.0, 19.0, 53.72, 1.0, 360)
        .map(|v| (FIXTURE.to_string(), v))
        .map_err(|e| (FIXTURE.to_string(), e.to_string()))
}

/// A Ø30 peg asked to enter a Ø20 hole.
fn oversized_peg() -> Fixture {
    const FIXTURE: &str = "a Ø30 peg against a Ø20 hole";
    let hole = mesh_of(serde_json::json!({"op":"difference","children":[
        cylinder(40.0, 20.0, 0.0),
        cylinder(20.0, 22.0, -1.0),
    ]}))
    .map_err(|e| (FIXTURE.to_string(), e))?;
    let peg = mesh_of(cylinder(30.0, 15.0, 0.0)).map_err(|e| (FIXTURE.to_string(), e))?;

    checks::fit(
        &hole,
        [0.0, 0.0],
        (1.0, 19.0),
        &peg,
        [0.0, 0.0],
        (1.0, 14.0),
        0.2,
        1.0,
        360,
    )
    .map(|v| (FIXTURE.to_string(), v))
    .map_err(|e| (FIXTURE.to_string(), e.to_string()))
}

fn overlapping_boxes() -> Fixture {
    const FIXTURE: &str = "two 20 mm boxes overlapping by 10 mm in every axis";
    let a = mesh_of(serde_json::json!({"op":"box","size":[20.0,20.0,20.0]}))
        .map_err(|e| (FIXTURE.to_string(), e))?;
    let b = mesh_of(
        serde_json::json!({"op":"translate","offset":[10.0,10.0,10.0],
        "child":{"op":"box","size":[20.0,20.0,20.0]}}),
    )
    .map_err(|e| (FIXTURE.to_string(), e))?;

    checks::interference(
        &[("a".to_string(), a), ("b".to_string(), b)],
        checks::DEFAULT_STEP_MM,
    )
    .map(|v| (FIXTURE.to_string(), v))
    .map_err(|e| (FIXTURE.to_string(), e.to_string()))
}

/// A slab on two legs: the underside of the slab is a horizontal ceiling,
/// 90 degrees from vertical, which nothing can print unsupported.
fn a_flat_ceiling() -> Fixture {
    const FIXTURE: &str = "a slab bridging two legs, leaving a horizontal ceiling";
    let mesh = mesh_of(serde_json::json!({"op":"union","children":[
        {"op":"box","size":[6.0, 20.0, 20.0]},
        {"op":"translate","offset":[34.0,0.0,0.0],"child":{"op":"box","size":[6.0,20.0,20.0]}},
        {"op":"translate","offset":[0.0,0.0,20.0],"child":{"op":"box","size":[40.0,20.0,6.0]}},
    ]}))
    .map_err(|e| (FIXTURE.to_string(), e))?;

    checks::overhangs(&mesh, 45.0)
        .map(|v| (FIXTURE.to_string(), v))
        .map_err(|e| (FIXTURE.to_string(), e.to_string()))
}
