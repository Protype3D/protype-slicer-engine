//! One poll of the machine, and what each field is worth.
//!
//! The fields are not equally trustworthy, and the passport says which is
//! which. Reproduced here at the point of use because this is where the
//! temptation to believe them arises:
//!
//! * `filament_detected` means nothing while `enabled` is false — observed
//!   live reading `true` on a disabled sensor.
//! * `filament_used` is commanded extrusion, not plastic delivered, so it
//!   keeps climbing through a jam.
//! * `print_duration` starts during purge, long before the first layer.
//!   Anything that treats it as "time spent printing" raises a false
//!   alarm during legitimate probing.

use serde::{Deserialize, Serialize};

/// Printer objects worth one round trip.
pub const QUERY: &str = "print_stats&virtual_sdcard&extruder&heater_bed&toolhead\
&idle_timeout&filament_switch_sensor%20filament_switch_sensor";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub state: String,
    pub filename: String,
    /// Empty for vendor errors — the reason is in the console instead.
    pub message: String,
    pub print_duration_s: f64,
    pub total_duration_s: f64,
    /// Commanded extrusion, not delivered plastic.
    pub filament_used_mm: f64,
    /// `None` when the machine reports no layer info at all.
    pub layer: Option<u32>,
    pub total_layer: Option<u32>,
    pub progress_pct: f64,
    pub nozzle_c: f64,
    pub nozzle_target_c: f64,
    pub bed_c: f64,
    pub bed_target_c: f64,
    pub position: Option<[f64; 3]>,
    pub homed_axes: String,
    pub filament_sensor_enabled: Option<bool>,
    /// Only meaningful when `filament_sensor_enabled` is `Some(true)`.
    pub filament_detected: Option<bool>,
}

impl Snapshot {
    /// Parse the `status` object of a `/printer/objects/query` response.
    pub fn from_status(status: &serde_json::Value) -> Self {
        let get = |object: &str| {
            status
                .get(object)
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        };
        let stats = get("print_stats");
        let info = stats
            .get("info")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let extruder = get("extruder");
        let bed = get("heater_bed");
        let toolhead = get("toolhead");
        let sdcard = get("virtual_sdcard");
        let sensor = get("filament_switch_sensor filament_switch_sensor");

        Self {
            state: string(&stats, "state"),
            filename: string(&stats, "filename"),
            message: string(&stats, "message"),
            print_duration_s: number(&stats, "print_duration"),
            total_duration_s: number(&stats, "total_duration"),
            filament_used_mm: number(&stats, "filament_used"),
            layer: integer(&info, "current_layer"),
            total_layer: integer(&info, "total_layer"),
            progress_pct: number(&sdcard, "progress") * 100.0,
            nozzle_c: number(&extruder, "temperature"),
            nozzle_target_c: number(&extruder, "target"),
            bed_c: number(&bed, "temperature"),
            bed_target_c: number(&bed, "target"),
            position: toolhead.get("position").and_then(|p| {
                let a = p.as_array()?;
                Some([
                    a.first()?.as_f64()?,
                    a.get(1)?.as_f64()?,
                    a.get(2)?.as_f64()?,
                ])
            }),
            homed_axes: string(&toolhead, "homed_axes"),
            filament_sensor_enabled: sensor.get("enabled").and_then(|v| v.as_bool()),
            filament_detected: sensor.get("filament_detected").and_then(|v| v.as_bool()),
        }
    }

    /// True only for the states where the machine is actively working
    /// through a file.
    pub fn is_running(&self) -> bool {
        self.state == "printing"
    }

    /// Whether the start block has got far enough to announce the layer
    /// count. Used to tell "the file has not started" from "the counter
    /// is not being emitted".
    pub fn start_block_has_run(&self) -> bool {
        self.total_layer.is_some_and(|n| n > 0)
    }

    /// What `filament_detected` is actually worth right now.
    ///
    /// Returns `None` when the sensor is off, because the field keeps
    /// reporting its last value and reads as a healthy `true`.
    pub fn trustworthy_filament_detected(&self) -> Option<bool> {
        match self.filament_sensor_enabled {
            Some(true) => self.filament_detected,
            _ => None,
        }
    }
}

fn string(object: &serde_json::Value, key: &str) -> String {
    object
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn number(object: &serde_json::Value, key: &str) -> f64 {
    object.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

fn integer(object: &serde_json::Value, key: &str) -> Option<u32> {
    object.get(key).and_then(|v| v.as_u64()).map(|v| v as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim shape from the live machine, including the disabled
    /// sensor still reporting `filament_detected: true`.
    fn live() -> serde_json::Value {
        serde_json::json!({
            "print_stats": {
                "filename": "CUP_stand_holder_PETG.gcode",
                "total_duration": 8047.0,
                "print_duration": 4812.0,
                "filament_used": 12345.6,
                "state": "printing",
                "message": "",
                "info": {"total_layer": 200, "current_layer": 81}
            },
            "virtual_sdcard": {"progress": 0.4055},
            "extruder": {"temperature": 246.8, "target": 250.0},
            "heater_bed": {"temperature": 70.0, "target": 70.0},
            "toolhead": {"position": [195.0, 195.0, 16.2, 0.0], "homed_axes": "xyz"},
            "filament_switch_sensor filament_switch_sensor": {
                "filament_detected": true, "enabled": false
            }
        })
    }

    #[test]
    fn a_live_status_parses() {
        let s = Snapshot::from_status(&live());
        assert_eq!(s.state, "printing");
        assert_eq!(s.layer, Some(81));
        assert_eq!(s.total_layer, Some(200));
        assert!((s.progress_pct - 40.55).abs() < 1e-6);
        assert_eq!(s.position, Some([195.0, 195.0, 16.2]));
        assert_eq!(s.homed_axes, "xyz");
    }

    /// The telemetry trap, as a method rather than a note someone has to
    /// remember: the sensor reads `true` while switched off.
    #[test]
    fn a_disabled_filament_sensor_reports_nothing_rather_than_true() {
        let s = Snapshot::from_status(&live());
        assert_eq!(s.filament_detected, Some(true), "the raw field says true");
        assert_eq!(
            s.trustworthy_filament_detected(),
            None,
            "and it is worth nothing, because the sensor is disabled"
        );
    }

    #[test]
    fn an_enabled_sensor_is_believed() {
        let mut raw = live();
        raw["filament_switch_sensor filament_switch_sensor"]["enabled"] = serde_json::json!(true);
        assert_eq!(
            Snapshot::from_status(&raw).trustworthy_filament_detected(),
            Some(true)
        );
    }

    #[test]
    fn a_missing_object_does_not_panic() {
        let s = Snapshot::from_status(&serde_json::json!({}));
        assert_eq!(s.state, "");
        assert_eq!(s.layer, None);
        assert!(!s.start_block_has_run());
    }

    #[test]
    fn the_start_block_is_known_to_have_run_once_the_total_is_announced() {
        let s = Snapshot::from_status(&live());
        assert!(s.start_block_has_run());
    }
}
