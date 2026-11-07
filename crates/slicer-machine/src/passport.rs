//! The machine passport: what a machine does that no model can predict.
//!
//! Everything in here is a claim about a physical machine, so every
//! claim carries the observation that establishes it. The `derivation`
//! and `proof` fields are not documentation — they are the difference
//! between a passport and a guess, and §8 of the spec forbids acting on
//! the second kind.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Passport format this build understands. Bumped only when a change
/// would make an older reader wrong, never for additive fields.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point2 {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Point3 {
    pub fn xy(&self) -> Point2 {
        Point2 {
            x: self.x,
            y: self.y,
        }
    }

    pub fn to_array(self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }
}

/// Usable build volume in millimetres.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bed {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// Where and how the machine finds Z zero.
///
/// This is the single most expensive fact in the passport: not knowing
/// that this machine probes at the centre of the bed is what drove a
/// nozzle into a finished part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZProbe {
    /// Where the nozzle descends to touch the bed.
    pub point: Point2,
    /// How we know. Must name an observable — a config section, a
    /// recorded move — not a manual and not an expectation.
    pub derivation: String,
    /// Commands that cause a Z probe at `point`. Anything here is
    /// unsafe to issue with a part standing under `point`.
    pub triggers: Vec<String>,
    /// Commands that home *without* probing Z. These stay safe over a
    /// populated bed, which is what makes resuming a print possible at
    /// all.
    pub safe_variants: Vec<String>,
    /// Where the toolhead is left once a probing home finishes. Seeds
    /// the start-block scan, which otherwise has no idea where motion
    /// begins.
    pub leaves_toolhead_at: Point3,
}

/// One straight move the vendor start block makes across the bed, with
/// the height it makes it at.
///
/// Not hand-written: [`crate::start_block`] derives these from the
/// vendor G-code and a test locks the derivation, so a firmware or
/// profile update that changes the choreography turns the test red
/// instead of turning a print into a collision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Traverse {
    pub from: Point2,
    pub to: Point2,
    /// Height of the nozzle throughout the move. A part taller than
    /// this, anywhere under the segment, gets hit.
    pub z: f64,
    /// 1-based line in the vendor start block this came from.
    pub source_line: usize,
    /// The line itself, so a reader can go look.
    pub raw: String,
}

/// A line of the vendor start block that must not be removed, and the
/// evidence for why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MandatoryLine {
    /// What it does.
    pub reason: String,
    /// The observable effect that proves it does it. Absence from the
    /// firmware's command help is explicitly not proof — the firmware
    /// swallows unknown commands silently, so "not in help" and "does
    /// nothing" look identical from outside.
    pub proof: String,
}

/// What the firmware does with a command it does not recognise.
///
/// `Ignored` is the dangerous one: it means a typo, a renamed macro and
/// a correct command are indistinguishable from the return code, so
/// nothing may be concluded from a command "working".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownCommandPolicy {
    Ignored,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartBlock {
    /// Where the G-code came from, e.g. `profile:machine_start_gcode`.
    pub source: String,
    /// Every in-bed move the block makes, ordered as executed.
    pub traverses: Vec<Traverse>,
    /// Moves whose height or position could not be resolved. Kept in the
    /// passport rather than dropped: a blind spot that is written down
    /// can be treated as unsafe, and one that is not written down gets
    /// treated as safe by default.
    #[serde(default)]
    pub unmodelled: Vec<crate::start_block::Unmodelled>,
    /// Keyed by the command word.
    pub mandatory_lines: BTreeMap<String, MandatoryLine>,
}

/// How a tool index reaches a physical filament slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// Variable in the firmware's `save_variables` holding the slot this
    /// tool is bound to.
    pub slot_var: String,
}

/// When a telemetry field means what it appears to mean.
///
/// Every field the daemon reads from the machine needs one of these, or
/// it will eventually be believed at the wrong moment: `filament_detected`
/// reads `true` from a sensor that is switched off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrustRule {
    /// Condition under which the field carries information at all.
    /// `None` = always meaningful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_when: Option<String>,
    /// What the number actually is, as opposed to what its name suggests.
    pub means: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachinePassport {
    pub schema_version: u32,
    /// Stable slug, e.g. `qidi-xmax4`.
    pub machine_id: String,
    pub display_name: String,
    pub bed: Bed,
    pub z_probe: ZProbe,
    /// Every command that drives the nozzle down to touch the bed.
    ///
    /// `z_probe` describes only `G28`, because that is the one the
    /// incident named. It is not the only one, and it is not the worst —
    /// see [`crate::probe`]. Defaulted so an older passport still loads,
    /// but an empty list means "nothing is known", not "nothing probes".
    #[serde(default)]
    pub probe_operations: Vec<crate::probe::ProbeOperation>,
    pub start_block: StartBlock,
    pub unknown_commands: UnknownCommandPolicy,
    pub tools: BTreeMap<String, Tool>,
    pub telemetry_trust: BTreeMap<String, TrustRule>,
}

impl MachinePassport {
    /// True when a point lies within the build area in XY.
    pub fn contains_xy(&self, p: Point2) -> bool {
        p.x >= 0.0 && p.x <= self.bed.x && p.y >= 0.0 && p.y <= self.bed.y
    }

    /// The trust rule for a telemetry field, if the passport has one.
    ///
    /// A field with no rule is a field nothing is known about; callers
    /// should treat that as "do not act on it", not as "fine".
    pub fn trust(&self, field: &str) -> Option<&TrustRule> {
        self.telemetry_trust.get(field)
    }

    /// Whether issuing `command` will drive the nozzle down at
    /// [`ZProbe::point`]. Matches on the leading word so `G28 Z` and
    /// `G28` are told apart from `G28 X Y`.
    pub fn probes_z(&self, command: &str) -> bool {
        let normalized = normalize_command(command);
        if self
            .z_probe
            .safe_variants
            .iter()
            .any(|s| normalize_command(s) == normalized)
        {
            return false;
        }
        self.z_probe
            .triggers
            .iter()
            .any(|t| normalize_command(t) == normalized)
    }
}

/// Upper-case, collapse whitespace, drop comments — so `g28  z` and
/// `G28 Z ; home` compare equal.
fn normalize_command(command: &str) -> String {
    let body = command.split(';').next().unwrap_or("");
    body.split_whitespace()
        .map(|w| w.to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> ZProbe {
        ZProbe {
            point: Point2 { x: 195.0, y: 195.0 },
            derivation: "homing_override".into(),
            triggers: vec!["G28".into(), "G28 Z".into()],
            safe_variants: vec!["G28 X Y".into()],
            leaves_toolhead_at: Point3 {
                x: 195.0,
                y: 195.0,
                z: 10.0,
            },
        }
    }

    fn passport() -> MachinePassport {
        MachinePassport {
            schema_version: SCHEMA_VERSION,
            machine_id: "test".into(),
            display_name: "Test".into(),
            bed: Bed {
                x: 390.0,
                y: 390.0,
                z: 340.0,
            },
            z_probe: probe(),
            probe_operations: Vec::new(),
            start_block: StartBlock {
                source: "test".into(),
                traverses: Vec::new(),
                unmodelled: Vec::new(),
                mandatory_lines: BTreeMap::new(),
            },
            unknown_commands: UnknownCommandPolicy::Ignored,
            tools: BTreeMap::new(),
            telemetry_trust: BTreeMap::new(),
        }
    }

    #[test]
    fn bare_g28_probes_z_but_g28_xy_does_not() {
        let p = passport();
        assert!(p.probes_z("G28"));
        assert!(p.probes_z("G28 Z"));
        assert!(!p.probes_z("G28 X Y"));
    }

    #[test]
    fn command_matching_ignores_case_spacing_and_comments() {
        let p = passport();
        assert!(p.probes_z("g28   z  ; home z"));
        assert!(!p.probes_z("g28 x  y"));
    }

    #[test]
    fn an_unrelated_command_does_not_probe() {
        assert!(!passport().probes_z("G1 X10"));
    }

    #[test]
    fn a_field_with_no_trust_rule_reports_nothing_rather_than_ok() {
        assert!(passport().trust("filament_detected").is_none());
    }
}
