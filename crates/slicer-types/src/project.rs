use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
pub struct ProjectId(pub String);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
pub struct PlateId(pub u32);

#[derive(
    Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct ObjectId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
pub struct PresetId(pub String);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
pub enum PresetKind {
    Printer,
    Process,
    Filament,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectMeta {
    pub title: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub modified_at: i64,
    pub schema_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PresetState {
    pub active_printer: PresetId,
    pub active_process: PresetId,
    pub active_filament: PresetId,
}

/// Highest encoded fan paint value: `1 + (lines - 1) * 21 + speed_pct / 5`
/// with `lines` in 1..=7 and `speed_pct` in 0..=100 (multiples of 5); 0 is
/// unpainted. The same encoding as a facet of `fan_facets_rle` and as
/// libslic3r's `FanPaint.hpp`.
pub const MAX_FAN_FACET_VALUE: u8 = 147;

/// Exact part-fan speed for a height range, on the lines it names — the zone
/// the engine builds from painted facets, over whole layers.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct LayerRangeFan {
    /// 0..=100, a multiple of 5.
    pub speed_pct: u8,
    /// Bit mask: 1 outer wall, 2 inner walls, 4 infill/bridges/top/bottom; 1..=7.
    pub lines: u8,
}

impl LayerRangeFan {
    pub fn validate(&self) -> Result<(), String> {
        if self.speed_pct > 100 || !self.speed_pct.is_multiple_of(5) {
            return Err(format!(
                "fan speed_pct must be 0..=100 in steps of 5, got {}",
                self.speed_pct
            ));
        }
        if !(1..=7).contains(&self.lines) {
            return Err(format!(
                "fan lines must be a mask in 1..=7, got {}",
                self.lines
            ));
        }
        Ok(())
    }

    /// The encoded value (see `MAX_FAN_FACET_VALUE`); 0 — no band — for a fan
    /// that does not validate.
    pub fn encoded_value(&self) -> u8 {
        if self.validate().is_err() {
            return 0;
        }
        1 + (self.lines - 1) * 21 + self.speed_pct / 5
    }
}

/// Per-object Z-range setting overrides — Prusa-style "Layer ranges modifier".
/// Maps onto libslic3r's `ModelObject::layer_config_ranges`. Each entry
/// overrides the listed settings only for layers whose Z falls inside
/// `[z_min, z_max]`.
///
/// Ranges must be non-overlapping with `z_min < z_max`; the daemon validates
/// on `object.set_layer_ranges` and rejects invalid sets.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct LayerConfigRange {
    pub id: String,
    pub z_min: f64,
    pub z_max: f64,
    /// Override keys must belong to the per-range whitelist, and values be
    /// numbers libslic3r applies; see `is_layer_range_overridable()` and
    /// `validate_layer_range_value()` in `crates/slicer-types/src/settings.rs`.
    /// A project stores ranges as saved, unchecked: at slice time the bridge
    /// skips a value libslic3r does not take and names it in the warnings.
    pub overrides: std::collections::BTreeMap<String, crate::settings::SettingValue>,
    /// Exact fan speed for the range's layers; absent in projects saved before
    /// height-band fan. A range may carry a fan and no overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan: Option<LayerRangeFan>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_range_fan_encodes_like_a_fan_paint_facet() {
        let fan = |speed_pct, lines| LayerRangeFan { speed_pct, lines };
        assert_eq!(fan(0, 1).encoded_value(), 1);
        assert_eq!(fan(80, 1).encoded_value(), 17);
        assert_eq!(fan(0, 7).encoded_value(), 127);
        assert_eq!(fan(100, 7).encoded_value(), MAX_FAN_FACET_VALUE);
        assert_eq!(fan(7, 1).encoded_value(), 0, "an invalid fan is no band");
    }

    #[test]
    fn layer_range_fan_accepts_multiples_of_five_and_every_line_mask() {
        for speed_pct in [0, 5, 100] {
            for lines in 1..=7 {
                assert_eq!(
                    LayerRangeFan { speed_pct, lines }.validate(),
                    Ok(()),
                    "{speed_pct} % on lines {lines}"
                );
            }
        }
    }

    #[test]
    fn layer_range_fan_refuses_other_speeds_and_line_masks() {
        let error = |speed_pct, lines| LayerRangeFan { speed_pct, lines }.validate().unwrap_err();
        assert!(error(7, 1).starts_with("fan speed_pct"), "{}", error(7, 1));
        assert!(
            error(105, 1).starts_with("fan speed_pct"),
            "{}",
            error(105, 1)
        );
        assert!(error(50, 0).starts_with("fan lines"), "{}", error(50, 0));
        assert!(error(50, 8).starts_with("fan lines"), "{}", error(50, 8));
    }

    #[test]
    fn layer_config_range_without_fan_keeps_its_json() {
        let json = serde_json::json!({ "id": "r1", "z_min": 5.0, "z_max": 10.0, "overrides": {} });
        let range: LayerConfigRange = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(range.fan, None);
        assert_eq!(serde_json::to_value(&range).unwrap(), json);
    }

    #[test]
    fn layer_config_range_explicit_null_fan_deserializes_to_none() {
        // The UI omits `fan` entirely when the fan is off, but another
        // client may send an explicit `null` — both must land on `None`,
        // and re-serializing drops the key either way (`skip_serializing_if`).
        let json = serde_json::json!({
            "id": "r1",
            "z_min": 5.0,
            "z_max": 10.0,
            "overrides": {},
            "fan": null
        });
        let range: LayerConfigRange = serde_json::from_value(json).unwrap();
        assert_eq!(range.fan, None);
        assert_eq!(
            serde_json::to_value(&range).unwrap(),
            serde_json::json!({ "id": "r1", "z_min": 5.0, "z_max": 10.0, "overrides": {} })
        );
    }

    #[test]
    fn layer_config_range_fan_round_trips_through_json() {
        let json = serde_json::json!({
            "id": "r1",
            "z_min": 5.0,
            "z_max": 10.0,
            "overrides": {},
            "fan": { "speed_pct": 80, "lines": 3 }
        });
        let range: LayerConfigRange = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            range.fan,
            Some(LayerRangeFan {
                speed_pct: 80,
                lines: 3
            })
        );
        assert_eq!(serde_json::to_value(&range).unwrap(), json);
    }
}
