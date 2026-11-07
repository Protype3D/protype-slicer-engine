//! Canonical resolved slice configuration accepted by `slice.request`.
//!
//! Product-level profile inheritance and printer calibration are resolved by
//! the agent. The daemon treats this structure as an executor contract and
//! maps it to libslic3r bridge inputs.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedSliceConfig {
    pub schema_version: u16,
    pub profile_snapshot_hash: String,
    pub extruders: Vec<ResolvedExtruderConfig>,
    #[serde(default)]
    pub machine: BTreeMap<String, Value>,
    #[serde(default)]
    pub process: BTreeMap<String, Value>,
    #[serde(default)]
    pub filaments: Vec<BTreeMap<String, Value>>,
    #[serde(default)]
    pub calibration: ResolvedCalibrationConfig,
    #[serde(default)]
    pub role_mapping: Vec<ResolvedRoleMapping>,
    /// Single-head IDEX selector (0-based physical head). `Some(n)` = print the
    /// whole plate with head `n`; `None` = normal multi-tool slice. The UI sets
    /// this from the print-mode chip (`{ kind: 'single', slot: n }`) instead of
    /// collapsing the config to a single extruder, so the bridge can route the
    /// plate to head `n` and emit a real `T{n}` + `initial_extruder = n`.
    #[serde(default)]
    pub print_head: Option<u8>,
    #[serde(default)]
    pub unsupported_settings: Vec<UnsupportedResolvedSetting>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedExtruderConfig {
    pub slot_id: u8,
    pub nozzle_diameter: f32,
    #[serde(default)]
    pub nozzle_offset_xy: [f32; 2],
    #[serde(default)]
    pub retraction_length: Option<f32>,
    #[serde(default)]
    pub retraction_speed: Option<f32>,
    #[serde(default)]
    pub retract_restart_extra: Option<f32>,
    #[serde(default)]
    pub deretraction_speed: Option<f32>,
    #[serde(default)]
    pub retraction_minimum_travel: Option<f32>,
    #[serde(default)]
    pub retract_when_changing_layer: Option<bool>,
    #[serde(default)]
    pub wipe: Option<bool>,
    #[serde(default)]
    pub wipe_distance: Option<f32>,
    #[serde(default)]
    pub retract_before_wipe: Option<f32>,
    #[serde(default)]
    pub retract_length_toolchange: Option<f32>,
    #[serde(default)]
    pub z_hop: Option<f32>,
    #[serde(default)]
    pub z_hop_types: Option<String>,
    #[serde(default)]
    pub travel_slope: Option<f32>,
    #[serde(default)]
    pub retract_lift_above: Option<f32>,
    #[serde(default)]
    pub retract_lift_below: Option<f32>,
    /// Where Z-hop is enforced (`retract_lift_enforce`, per-extruder enum).
    /// libslic3r spelling ("All Surfaces", "Top Only", "Bottom Only",
    /// "Top and Bottom"); the bridge also accepts the agent's snake_case.
    #[serde(default)]
    pub retract_lift_enforce: Option<String>,
    #[serde(default)]
    pub min_layer_height: Option<f32>,
    #[serde(default)]
    pub max_layer_height: Option<f32>,
    #[serde(default)]
    pub filament_type: Option<String>,
    #[serde(default)]
    pub filament_colour: Option<String>,
    #[serde(default)]
    pub nozzle_temperature_initial_layer: Option<i32>,
    #[serde(default)]
    pub nozzle_temperature: Option<i32>,
    #[serde(default)]
    pub bed_temperature_initial_layer_single: Option<i32>,
    #[serde(default)]
    pub bed_temperature: Option<i32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedCalibrationConfig {
    #[serde(default)]
    pub z_offset_default: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedRoleMapping {
    pub role: String,
    pub extruder_slot: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UnsupportedResolvedSetting {
    pub key: String,
    pub reason: String,
}

impl ResolvedSliceConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err(format!(
                "unsupported resolved_config schema_version {}",
                self.schema_version
            ));
        }
        if self.profile_snapshot_hash.trim().is_empty() {
            return Err("profile_snapshot_hash must not be empty".to_string());
        }
        if self.extruders.is_empty() {
            return Err("resolved_config must include at least one extruder".to_string());
        }
        if let Some(z) = self.calibration.z_offset_default {
            if !z.is_finite() {
                return Err("calibration.z_offset_default must be finite".to_string());
            }
        }

        let mut slots = HashSet::with_capacity(self.extruders.len());
        for extruder in &self.extruders {
            if !slots.insert(extruder.slot_id) {
                return Err(format!("duplicate extruder slot {}", extruder.slot_id));
            }
            if !extruder.nozzle_diameter.is_finite() || extruder.nozzle_diameter <= 0.0 {
                return Err(format!(
                    "extruder {} nozzle_diameter must be positive",
                    extruder.slot_id
                ));
            }
            if !extruder.nozzle_offset_xy[0].is_finite()
                || !extruder.nozzle_offset_xy[1].is_finite()
            {
                return Err(format!(
                    "extruder {} nozzle_offset_xy must be finite",
                    extruder.slot_id
                ));
            }
        }

        for expected in 0..self.extruders.len() {
            if !slots.contains(&(expected as u8)) {
                return Err("extruders must use contiguous zero-based slot_id values".to_string());
            }
        }

        for mapping in &self.role_mapping {
            if mapping.role.trim().is_empty() {
                return Err("role_mapping role must not be empty".to_string());
            }
            if !slots.contains(&mapping.extruder_slot) {
                return Err(format!(
                    "role_mapping.{} references unknown extruder slot {}",
                    mapping.role, mapping.extruder_slot
                ));
            }
        }

        if let Some(head) = self.print_head {
            if !slots.contains(&head) {
                return Err(format!(
                    "print_head references unknown extruder slot {head}"
                ));
            }
            if self.extruders.len() < 2 {
                return Err("print_head requires an IDEX machine (>= 2 extruders)".to_string());
            }
        }

        Ok(())
    }
}
