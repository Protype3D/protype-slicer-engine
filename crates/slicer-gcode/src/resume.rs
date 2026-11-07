//! Continue a print from layer K.
//!
//! Deliberately not "cut off the head of the file". Everything that
//! makes resuming dangerous lives in the part that a naive cut leaves
//! out or leaves in:
//!
//! * The vendor start block homes Z **at the centre of the bed**. On an
//!   empty bed that is fine; over a part with sparse infill the probe
//!   drops into a cell and the nozzle follows it. This is how a resume
//!   from layer 92 destroyed a finished part.
//! * That same block crosses the bed centre at Z10 twice more.
//! * The first thing after a `;LAYER_CHANGE` is a nozzle wipe at the
//!   *previous* layer's coordinates. Resumed cold, the nozzle is not
//!   there, and those moves drag across whatever is.
//! * The body's `E` numbers mean nothing without the mode they were
//!   written in. A generated block that declares the wrong one turns the
//!   first move into a request for half a metre of filament, so the mode
//!   is read out of the file ([`crate::extrusion`]) and reproduced.
//!
//! So a plan is built rather than a suffix taken: a start block that
//! lifts first and homes without touching Z, an approach through the
//! air, and then the layer. The whole result is handed back to the same
//! hazard arithmetic the passport feeds, and a plan with a proven
//! collision in it is refused rather than returned with a warning.

use crate::error::GcodeError;
use crate::extrusion::{self, ExtrusionMode, ExtrusionState};
use crate::layers::LAYER_CHANGE_MARKER;
use serde::{Deserialize, Serialize};
use slicer_machine::hazard::{self, Hazard, Part, Severity};
use slicer_machine::passport::Point2;
use slicer_machine::start_block::axis_word;
use slicer_machine::MachinePassport;

/// How much higher than everything on the bed to travel.
pub const DEFAULT_CLEARANCE_MM: f64 = 5.0;

/// How Z zero is re-established before the print continues.
///
/// There is no safe default here, so there is no default. Getting this
/// wrong is what drove a nozzle into a part, and the crate will not pick
/// for the caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ZReference {
    /// Re-probe with a command the passport knows. Refused outright when
    /// a part stands under the probe point.
    Probe { command: String },
    /// The machine still knows Z — it was paused, not power-cycled.
    ///
    /// `evidence` is required and is written into the emitted G-code.
    /// §8 of the spec: a claim about the machine needs an observable, and
    /// an API that accepts "trust me" without recording why makes the
    /// unsafe path the convenient one.
    Retained { evidence: String },
}

#[derive(Debug, Clone)]
pub struct ResumeRequest<'a> {
    pub gcode: &'a str,
    pub passport: &'a MachinePassport,
    /// 1-based, matching `CURRENT_LAYER`.
    pub from_layer: usize,
    /// What is already standing on the bed, in machine coordinates.
    pub parts_on_bed: Vec<Part>,
    pub z_reference: ZReference,
    /// Travel height. `None` derives it from the tallest part.
    pub safe_z_mm: Option<f64>,
    pub margin_mm: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumePlan {
    pub gcode: String,
    pub from_layer: usize,
    pub total_layers: usize,
    pub layer_z: f64,
    pub safe_z_mm: f64,
    pub approach: Point2,
    /// The E mode the file was in at the cut, reproduced by the generated
    /// block. Carried out because a caller reading the plan is entitled to
    /// see which of the two readings of the body's `E` numbers was used.
    pub extrusion_mode: ExtrusionMode,
    /// Wipe moves at the previous layer's coordinates, dropped.
    pub dropped_wipe_lines: usize,
    /// Everything the collision check could not rule out. A returned
    /// plan never contains a proven collision — those are an error — so
    /// anything here is an unknown the caller must read.
    pub unresolved: Vec<Hazard>,
}

pub fn plan_resume(request: &ResumeRequest<'_>) -> Result<ResumePlan, GcodeError> {
    let lines: Vec<&str> = request.gcode.lines().collect();
    let markers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == LAYER_CHANGE_MARKER)
        .map(|(i, _)| i)
        .collect();
    if markers.is_empty() {
        return Err(GcodeError::NoLayerMarkers);
    }
    if request.from_layer == 0 || request.from_layer > markers.len() {
        return Err(GcodeError::LayerOutOfRange {
            requested: request.from_layer,
            found: markers.len(),
        });
    }

    let start = markers[request.from_layer - 1];
    let layer_z = layer_height_marker(&lines, start).ok_or(GcodeError::NoLayerHeight {
        layer: request.from_layer,
    })?;

    let (body, dropped) = strip_previous_layer_wipe(&lines[start..]);
    let approach = first_xy(&body).ok_or(GcodeError::NoApproachPoint {
        layer: request.from_layer,
    })?;
    // Read before anything is generated: a file that never says which
    // reading its `E` numbers take is refused, not guessed at.
    let extrusion =
        extrusion::state_before(&lines, start).ok_or(GcodeError::UnknownExtrusionMode {
            layer: request.from_layer,
        })?;

    let safe_z = request.safe_z_mm.unwrap_or_else(|| {
        let tallest = request
            .parts_on_bed
            .iter()
            .map(|p| p.top_z)
            .fold(layer_z, f64::max);
        tallest + DEFAULT_CLEARANCE_MM
    });

    // Provable on its own, before any scanning: if the travel height does
    // not clear what is on the bed, the approach crosses the part however
    // it is routed. The general scan would only ever call this "unknown",
    // because it cannot know where the toolhead starts — so the specific
    // check has to exist, or the one plan that is obviously wrong is the
    // one that comes back with a shrug.
    for part in &request.parts_on_bed {
        if safe_z - part.top_z <= request.margin_mm {
            return Err(GcodeError::TravelTooLow {
                safe_z_mm: safe_z,
                part: part.name.clone(),
                top_z: part.top_z,
                margin_mm: request.margin_mm,
            });
        }
    }

    let travel = build_travel_block(request, safe_z, approach, extrusion)?;
    let landing = build_landing_block(layer_z, extrusion);
    let gcode = format!("{travel}{landing}{}\n", body.join("\n"));

    // Assessed over the *travel* only. The landing move and everything
    // after it descend onto the part on purpose — that is what resuming
    // a print is — so feeding them to a collision check would flag the
    // intended contact and drown the real answer.
    let scan =
        slicer_machine::start_block::scan(&travel, request.passport.z_probe.leaves_toolhead_at);
    let mut hazards = hazard::assess(
        &scan.traverses,
        &scan.unmodelled,
        &request.parts_on_bed,
        request.margin_mm,
    );
    if let ZReference::Probe { command } = &request.z_reference {
        if request.passport.probes_z(command) {
            hazards.extend(hazard::assess_probe(
                request.passport.z_probe.point,
                &request.parts_on_bed,
            ));
        }
        // Every probing operation the passport knows, not only the single
        // `G28` point. `G29` runs an adaptive mesh that fits itself to the
        // objects being printed — on a resume, to the part already
        // standing there — so it is the operation most likely to be asked
        // for and the one least likely to be safe.
        //
        // The print area is the footprint of what is on the bed: that is
        // precisely what the adaptive mesh will target.
        let matching: Vec<_> = request
            .passport
            .probe_operations
            .iter()
            .filter(|op| normalize(&op.command) == normalize(command))
            .cloned()
            .collect();
        hazards.extend(hazard::assess_probe_operations(
            &matching,
            &request.parts_on_bed,
            slicer_machine::bounding_box(&request.parts_on_bed),
        ));
    }

    let collisions: Vec<&Hazard> = hazards
        .iter()
        .filter(|h| h.severity() == Severity::Collision)
        .collect();
    if !collisions.is_empty() {
        return Err(GcodeError::WouldCollide {
            report: collisions
                .iter()
                .map(|h| format!("  - {h}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
    }

    Ok(ResumePlan {
        gcode,
        from_layer: request.from_layer,
        total_layers: markers.len(),
        layer_z,
        safe_z_mm: safe_z,
        approach,
        extrusion_mode: extrusion.mode,
        dropped_wipe_lines: dropped,
        unresolved: hazards,
    })
}

/// Lift, then home without touching Z, then approach through the air.
///
/// The order is the safety property. Lifting first means the XY homing
/// sweep — whose path the firmware owns and this crate does not model —
/// happens above everything on the bed, which is what lets the hazard
/// check clear it on height alone.
fn build_travel_block(
    request: &ResumeRequest<'_>,
    safe_z: f64,
    approach: Point2,
    extrusion: ExtrusionState,
) -> Result<String, GcodeError> {
    let safe_home = request
        .passport
        .z_probe
        .safe_variants
        .first()
        .ok_or(GcodeError::NoSafeHoming)?;

    let mut block = String::new();
    block.push_str("; ===== resume start block, generated =====\n");
    block.push_str(&format!(
        "; resuming at layer {} of a print sliced for {}\n",
        request.from_layer, request.passport.display_name
    ));
    for part in &request.parts_on_bed {
        block.push_str(&format!(
            "; on the bed: {} spanning X{:.1}..{:.1} Y{:.1}..{:.1}, {:.2} mm tall\n",
            part.name, part.min.x, part.max.x, part.min.y, part.max.y, part.top_z
        ));
    }
    block.push_str("G90\n");
    // Whatever the file was in. `M83` unconditionally would reinterpret an
    // absolute body's `E1234.5` as an increment.
    match extrusion.mode {
        ExtrusionMode::Relative => block.push_str("M83 ; the file's own E mode\n"),
        ExtrusionMode::Absolute => block.push_str("M82 ; the file's own E mode\n"),
    }

    match &request.z_reference {
        ZReference::Retained { evidence } => {
            block.push_str(&format!("; Z reference retained. Evidence: {evidence}\n"));
            block.push_str(&format!(
                "G1 Z{safe_z:.3} F600 ; lift clear of everything already printed\n"
            ));
        }
        ZReference::Probe { command } => {
            block.push_str(&format!("; Z reference re-probed with: {command}\n"));
            block.push_str(&format!("{command}\n"));
            block.push_str(&format!("G1 Z{safe_z:.3} F600 ; lift clear\n"));
        }
    }

    block.push_str(&format!(
        "{safe_home} ; re-establish XY without touching Z — a probing home here is the accident\n"
    ));
    block.push_str(&format!(
        "G1 X{:.3} Y{:.3} F9000 ; approach through the air\n",
        approach.x, approach.y
    ));
    Ok(block)
}

/// The deliberate part: descending onto the layer being resumed.
///
/// Kept apart from the travel block because the collision check must not
/// see it. Landing on the part is the whole intent, and a check that
/// flagged it would report a hazard on every correct plan — which is how
/// a report stops being read.
fn build_landing_block(layer_z: f64, extrusion: ExtrusionState) -> String {
    // In relative mode the body's numbers are increments, so the origin is
    // free and zero is the conventional choice. In absolute mode they are
    // positions on the total the file has been accumulating since its
    // first layer, so the axis has to be told where that total stands —
    // zeroing it makes the very next move demand the whole of it.
    let e_origin = match extrusion.mode {
        ExtrusionMode::Relative => "G92 E0\n".to_string(),
        ExtrusionMode::Absolute => format!(
            "G92 E{:.5} ; the running total this file's absolute E values continue from\n",
            extrusion.e_position
        ),
    };
    format!(
        "G1 Z{layer_z:.3} F600 ; descend onto the resumed layer\n\
         {e_origin}\
         ; ===== end of generated block =====\n"
    )
}

/// Upper-case and collapse whitespace, so `g29` and `G29 ` compare equal.
fn normalize(command: &str) -> String {
    command
        .split_whitespace()
        .map(|w| w.to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `;Z:<height>` immediately after the layer marker.
fn layer_height_marker(lines: &[&str], from: usize) -> Option<f64> {
    lines
        .iter()
        .skip(from)
        .take(6)
        .find_map(|l| l.trim().strip_prefix(";Z:"))
        .and_then(|v| v.trim().parse().ok())
}

/// Drop the wipe that belongs to the layer we are *not* printing.
///
/// Between `;WIPE_START` and `;WIPE_END` the moves are at the previous
/// layer's coordinates. Resumed cold the nozzle is elsewhere, so those
/// become a diagonal drag across whatever lies between.
fn strip_previous_layer_wipe<'a>(lines: &[&'a str]) -> (Vec<&'a str>, usize) {
    let mut out = Vec::with_capacity(lines.len());
    let mut dropped = 0usize;
    let mut in_wipe = false;
    // Only the first wipe block belongs to the previous layer; later ones
    // belong to layers this plan really is printing.
    let mut first_wipe_seen = false;

    for line in lines {
        let trimmed = line.trim();
        if trimmed == ";WIPE_START" && !first_wipe_seen {
            in_wipe = true;
            first_wipe_seen = true;
            out.push("; wipe at the previous layer's coordinates, dropped on resume");
            dropped += 1;
            continue;
        }
        if in_wipe {
            dropped += 1;
            if trimmed == ";WIPE_END" {
                in_wipe = false;
            }
            continue;
        }
        out.push(line);
    }
    (out, dropped)
}

/// First move in the body that names both X and Y — where the resumed
/// layer actually begins.
///
/// Comments are cut before anything is read. A comment is free text — the
/// vendor `layer_change_gcode` of a shipped profile carries prose, in
/// whatever language its author wrote it — and reading axis words out of
/// one either invents an approach point or, when the words are split byte
/// by byte, panics on the first character that is not one byte wide.
fn first_xy(lines: &[&str]) -> Option<Point2> {
    for line in lines {
        let code = line.split(';').next().unwrap_or("").trim();
        if !(code.starts_with("G1 ") || code.starts_with("G0 ")) {
            continue;
        }
        let mut x = None;
        let mut y = None;
        for (axis, value) in code.split_whitespace().skip(1).filter_map(axis_word) {
            match axis {
                // A placeholder (`X{first_layer_print_min[0]}`) parses to
                // `None`, and that is the answer: an approach point that
                // is not a number cannot be approached, so the move is
                // passed over rather than half-read.
                'X' => x = value,
                'Y' => y = value,
                _ => {}
            }
        }
        if let (Some(x), Some(y)) = (x, y) {
            return Some(Point2 { x, y });
        }
    }
    None
}
