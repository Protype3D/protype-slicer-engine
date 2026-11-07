use serde::{Deserialize, Serialize};

/// Caller-friendly per-slot inputs. `None` on `SliceParams` =
/// single-extruder slice (the existing path). `Some(_)` requires
/// `nozzle_diameter.len() == extruder_count` and the same length for
/// `extruder_offset_xy`; other populated vectors must either be empty
/// ("let libslic3r defaults fill it") or match `extruder_count`.
///
/// `slicer_ffi::run_slice` panics on mismatch because this is a developer
/// error per spec §6.4: the agent's flatten asserts the same invariant
/// before crossing the FFI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdexInputs {
    pub extruder_count: u8,
    /// Single-head print selector for IDEX. `Some(n)` means "print the whole
    /// plate with physical head `n`" (0-based): every object is routed to
    /// extruder `n` so libslic3r emits `T{n}` and `initial_extruder = n`
    /// (which feeds the `BED_MESH_CALIBRATE TOOL={initial_extruder}` macro).
    /// `None` = normal multi-tool slice (per-object/role assignment).
    ///
    /// Requires `extruder_count == 2`; ignored otherwise. Routing a single
    /// uniform object to a non-zero head needs TWO things in libslic3r — a
    /// per-object `extruder` override AND `filament_diameter.len() == 2`
    /// (ToolOrdering counts filaments off that vector; with len 1 it clamps
    /// every tool reference back to extruder 0). The bridge ensures both.
    #[serde(default)]
    pub print_head: Option<u8>,
    pub extruder_offset_xy: Vec<[f32; 2]>,
    pub nozzle_diameter: Vec<f32>,
    pub retraction_length: Vec<f32>,
    pub retraction_speed: Vec<f32>,
    pub retract_restart_extra: Vec<f32>,
    pub deretraction_speed: Vec<f32>,
    pub retraction_minimum_travel: Vec<f32>,
    pub retract_when_changing_layer: Vec<bool>,
    pub wipe: Vec<bool>,
    pub wipe_distance: Vec<f32>,
    pub retract_before_wipe_pct: Vec<f32>,
    pub retract_length_toolchange: Vec<f32>,
    pub z_hop: Vec<f32>,
    pub z_hop_types: Vec<String>,
    pub travel_slope_deg: Vec<f32>,
    pub retract_lift_above: Vec<f32>,
    pub retract_lift_below: Vec<f32>,
    /// `RetractLiftEnforceType` per slot — libslic3r spellings or the agent's
    /// snake_case; the bridge normalises and `set_deserialize_strict` rejects
    /// anything else. Defaults to empty so bundles written before the field
    /// existed (golden `idex_bundle.json`, older agents) still deserialise.
    #[serde(default)]
    pub retract_lift_enforce: Vec<String>,
    pub min_layer_height: Vec<f32>,
    pub max_layer_height: Vec<f32>,
    pub filament_type: Vec<String>,
    pub filament_colour: Vec<String>,
    pub nozzle_temperature_initial_layer: Vec<i32>,
    pub nozzle_temperature: Vec<i32>,
    pub bed_temperature_initial_layer_single: Vec<i32>,
    pub bed_temperature: Vec<i32>,
    pub change_filament_gcode: String,
    pub machine_pause_gcode: String,
    pub time_lapse_gcode: String,
    pub change_extrusion_role_gcode: String,
    pub printing_by_object_gcode: String,
}

impl IdexInputs {
    /// Build an IDEX input with no optional per-slot overrides. Callers still
    /// need to provide `nozzle_diameter` and `extruder_offset_xy` before use.
    pub fn empty_for_count(count: u8) -> Self {
        Self {
            extruder_count: count,
            print_head: None,
            extruder_offset_xy: Vec::new(),
            nozzle_diameter: Vec::new(),
            retraction_length: Vec::new(),
            retraction_speed: Vec::new(),
            retract_restart_extra: Vec::new(),
            deretraction_speed: Vec::new(),
            retraction_minimum_travel: Vec::new(),
            retract_when_changing_layer: Vec::new(),
            wipe: Vec::new(),
            wipe_distance: Vec::new(),
            retract_before_wipe_pct: Vec::new(),
            retract_length_toolchange: Vec::new(),
            z_hop: Vec::new(),
            z_hop_types: Vec::new(),
            travel_slope_deg: Vec::new(),
            retract_lift_above: Vec::new(),
            retract_lift_below: Vec::new(),
            retract_lift_enforce: Vec::new(),
            min_layer_height: Vec::new(),
            max_layer_height: Vec::new(),
            filament_type: Vec::new(),
            filament_colour: Vec::new(),
            nozzle_temperature_initial_layer: Vec::new(),
            nozzle_temperature: Vec::new(),
            bed_temperature_initial_layer_single: Vec::new(),
            bed_temperature: Vec::new(),
            change_filament_gcode: String::new(),
            machine_pause_gcode: String::new(),
            time_lapse_gcode: String::new(),
            change_extrusion_role_gcode: String::new(),
            printing_by_object_gcode: String::new(),
        }
    }

    pub fn validate_slot_lengths(&self) -> Result<(), String> {
        let n = self.extruder_count as usize;
        if self.nozzle_diameter.len() != n {
            return Err("nozzle_diameter slot count mismatch".to_string());
        }
        if self.extruder_offset_xy.len() != n {
            return Err("extruder_offset slot count mismatch".to_string());
        }

        fn assert_optional_len<T>(name: &str, values: &[T], n: usize) -> Result<(), String> {
            if !values.is_empty() && values.len() != n {
                return Err(format!("{name} slot count mismatch"));
            }
            Ok(())
        }

        assert_optional_len("retraction_length", &self.retraction_length, n)?;
        assert_optional_len("retraction_speed", &self.retraction_speed, n)?;
        assert_optional_len("retract_restart_extra", &self.retract_restart_extra, n)?;
        assert_optional_len("deretraction_speed", &self.deretraction_speed, n)?;
        assert_optional_len(
            "retraction_minimum_travel",
            &self.retraction_minimum_travel,
            n,
        )?;
        assert_optional_len(
            "retract_when_changing_layer",
            &self.retract_when_changing_layer,
            n,
        )?;
        assert_optional_len("wipe", &self.wipe, n)?;
        assert_optional_len("wipe_distance", &self.wipe_distance, n)?;
        assert_optional_len("retract_before_wipe_pct", &self.retract_before_wipe_pct, n)?;
        assert_optional_len(
            "retract_length_toolchange",
            &self.retract_length_toolchange,
            n,
        )?;
        assert_optional_len("z_hop", &self.z_hop, n)?;
        assert_optional_len("z_hop_types", &self.z_hop_types, n)?;
        assert_optional_len("travel_slope_deg", &self.travel_slope_deg, n)?;
        assert_optional_len("retract_lift_above", &self.retract_lift_above, n)?;
        assert_optional_len("retract_lift_below", &self.retract_lift_below, n)?;
        assert_optional_len("retract_lift_enforce", &self.retract_lift_enforce, n)?;
        assert_optional_len("min_layer_height", &self.min_layer_height, n)?;
        assert_optional_len("max_layer_height", &self.max_layer_height, n)?;
        assert_optional_len("filament_type", &self.filament_type, n)?;
        assert_optional_len("filament_colour", &self.filament_colour, n)?;
        assert_optional_len(
            "nozzle_temperature_initial_layer",
            &self.nozzle_temperature_initial_layer,
            n,
        )?;
        assert_optional_len("nozzle_temperature", &self.nozzle_temperature, n)?;
        assert_optional_len(
            "bed_temperature_initial_layer_single",
            &self.bed_temperature_initial_layer_single,
            n,
        )?;
        assert_optional_len("bed_temperature", &self.bed_temperature, n)?;
        Ok(())
    }

    pub(crate) fn assert_slot_lengths(&self) {
        self.validate_slot_lengths()
            .expect("invalid IDEX slot-vector lengths");
    }
}
