//! `machine.*` — the passport, and the collision question it exists to
//! answer.
//!
//! Lives outside `dispatch.rs` for the same reason `geometry.rs` does:
//! that file is past 4000 lines and these handlers have their own shape.
//!
//! The one design point worth stating: `machine.check_start_block`
//! reports `safe: false` when anything at all is unresolved, not only
//! when a collision is proven. A caller that wants to proceed anyway has
//! to look at `unknown` and decide, which is the whole point — the
//! accident this guards against happened because nobody was asked.

use serde::Deserialize;
use serde_json::{json, Value};
use slicer_machine::hazard::{self, Part, Severity};

use crate::error::RpcError;

#[derive(Debug, Deserialize)]
pub struct PassportParams {
    pub machine_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CheckStartBlockParams {
    pub machine_id: String,
    /// What is already standing on the bed, in machine coordinates.
    pub parts: Vec<PartParams>,
    /// Clearance below which a pass counts as a strike. Defaults to
    /// [`hazard::DEFAULT_MARGIN_MM`].
    #[serde(default)]
    pub margin_mm: Option<f64>,
    /// `[min_x, min_y, max_x, max_y]` of what the job will print.
    ///
    /// Matters because this machine's `G29` fits its bed mesh to the
    /// objects being printed. Omitting it means "the job reprints what is
    /// on the bed" — the resume case, and the one where the mesh lands on
    /// the part. Printing something *else* while a part stands there is a
    /// different question and needs this stated.
    #[serde(default)]
    pub print_area: Option<[f64; 4]>,
}

#[derive(Debug, Deserialize)]
pub struct PartParams {
    pub name: String,
    /// `[x, y, z]` minimum corner of the part's bounding box.
    pub min: [f64; 3],
    /// `[x, y, z]` maximum corner.
    pub max: [f64; 3],
}

pub fn handle_list() -> Result<Value, RpcError> {
    let machines: Vec<Value> = slicer_machine::bundled_ids()
        .into_iter()
        .filter_map(|id| slicer_machine::bundled(id).ok().map(|p| (id, p)))
        .map(|(id, p)| {
            json!({
                "machine_id": id,
                "display_name": p.display_name,
                "bed": p.bed,
            })
        })
        .collect();
    Ok(json!({ "machines": machines }))
}

pub fn handle_passport(params: Value) -> Result<Value, RpcError> {
    let p: PassportParams = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    let passport = slicer_machine::bundled(&p.machine_id)
        .map_err(|e| RpcError::Project(format!("machine passport: {e}")))?;
    serde_json::to_value(passport)
        .map_err(|e| RpcError::Internal(format!("cannot serialize passport: {e}")))
}

/// Would starting a print hit anything already on the bed?
pub fn handle_check_start_block(params: Value) -> Result<Value, RpcError> {
    let p: CheckStartBlockParams = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    let passport = slicer_machine::bundled(&p.machine_id)
        .map_err(|e| RpcError::Project(format!("machine passport: {e}")))?;

    let parts: Vec<Part> = p
        .parts
        .iter()
        .map(|part| Part::from_bounds(part.name.clone(), part.min, part.max))
        .collect();

    let margin = p.margin_mm.unwrap_or(hazard::DEFAULT_MARGIN_MM);
    let stated_area = p.print_area.map(|a| {
        (
            slicer_machine::passport::Point2 { x: a[0], y: a[1] },
            slicer_machine::passport::Point2 { x: a[2], y: a[3] },
        )
    });
    let print_area = stated_area.or_else(|| slicer_machine::bounding_box(&parts));
    let hazards = passport.hazards_for(&parts, print_area, margin);

    let collisions: Vec<&hazard::Hazard> = hazards
        .iter()
        .filter(|h| h.severity() == Severity::Collision)
        .collect();
    let unknowns: Vec<&hazard::Hazard> = hazards
        .iter()
        .filter(|h| h.severity() == Severity::Unknown)
        .collect();

    Ok(json!({
        "machine_id": p.machine_id,
        "margin_mm": margin,
        // Echoed back because it changes the answer, and a caller that did
        // not state one should see what was assumed on its behalf.
        "print_area": print_area.map(|(lo, hi)| [lo.x, lo.y, hi.x, hi.y]),
        "print_area_assumed": stated_area.is_none(),
        // Only an empty report is safe. An unresolved move is not a pass.
        "safe": hazards.is_empty(),
        "collisions": collisions,
        "unknown": unknowns,
        "summary": summarize(&collisions, &unknowns),
    }))
}

fn summarize(collisions: &[&hazard::Hazard], unknowns: &[&hazard::Hazard]) -> Vec<String> {
    collisions
        .iter()
        .chain(unknowns.iter())
        .map(|h| h.to_string())
        .collect()
}
