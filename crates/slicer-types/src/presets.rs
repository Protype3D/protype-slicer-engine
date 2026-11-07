//! Preset library: named bundles of setting values per scope. Two sources:
//! - Vendor: read-only, ships with the daemon (seeded by `seed_vendor`).
//!   In production these come from ProtypeBrain via ProtypeAgent (D31, §6.6).
//! - User: writeable, created by the user via `preset.save`.

use crate::project::PresetId;
use crate::settings::{SettingKey, SettingScope, SettingValue, SettingsSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresetSource {
    Vendor,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Preset {
    pub id: PresetId,
    pub name: String,
    pub scope: SettingScope,
    pub source: PresetSource,
    /// If this preset was derived from another (via "Save as derived"),
    /// the parent preset id is recorded here for traceability.
    #[serde(default)]
    pub derived_from: Option<PresetId>,
    /// Setting values this preset carries. Keys are `SettingKey` strings
    /// matching entries in the schema for `scope`.
    pub values: HashMap<SettingKey, SettingValue>,
}

impl Preset {
    pub fn vendor(id: &str, name: &str, scope: SettingScope) -> Self {
        Self {
            id: PresetId(id.to_string()),
            name: name.to_string(),
            scope,
            source: PresetSource::Vendor,
            derived_from: None,
            values: HashMap::new(),
        }
    }

    pub fn with(mut self, key: &str, value: SettingValue) -> Self {
        self.values.insert(key.to_string(), value);
        self
    }
}

/// Seed the vendor presets shipped with the daemon. These mirror the active
/// preset ids referenced in `PresetState::default()` so the initial project
/// state is internally consistent.
pub fn seed_vendor(schema: &SettingsSchema) -> Vec<Preset> {
    // Pull defaults from schema so vendor presets and schema defaults stay in
    // sync.
    let default_for = |scope: SettingScope, key: &str| -> Option<SettingValue> {
        schema
            .defs
            .iter()
            .find(|d| d.scope == scope && d.key == key)
            .map(|d| d.default.clone())
    };
    let proc_default = |key: &str| default_for(SettingScope::Process, key).unwrap();
    let fil_default = |key: &str| default_for(SettingScope::Filament, key).unwrap();
    let printer_default = |key: &str| default_for(SettingScope::Printer, key).unwrap();

    vec![
        // Process presets — three quality tiers
        Preset::vendor("standard", "Standard 0.2mm", SettingScope::Process)
            .with("layer_height", SettingValue::Number(0.2))
            .with("perimeter_count", proc_default("perimeter_count"))
            .with("infill_density", proc_default("infill_density"))
            .with("infill_pattern", proc_default("infill_pattern"))
            .with("support_enabled", proc_default("support_enabled"))
            .with("print_speed", proc_default("print_speed")),
        Preset::vendor("fine", "Fine 0.1mm", SettingScope::Process)
            .with("layer_height", SettingValue::Number(0.1))
            .with("perimeter_count", SettingValue::Int(4))
            .with("infill_density", SettingValue::Number(25.0))
            .with("infill_pattern", proc_default("infill_pattern"))
            .with("support_enabled", proc_default("support_enabled"))
            .with("print_speed", SettingValue::Number(45.0)),
        Preset::vendor("draft", "Draft 0.3mm", SettingScope::Process)
            .with("layer_height", SettingValue::Number(0.3))
            .with("perimeter_count", SettingValue::Int(2))
            .with("infill_density", SettingValue::Number(15.0))
            .with("infill_pattern", proc_default("infill_pattern"))
            .with("support_enabled", proc_default("support_enabled"))
            .with("print_speed", SettingValue::Number(80.0)),
        // Filament presets — six engineering thermoplastics calibrated on
        // CD400. Temps/cooling sourced from material datasheets and
        // hands-on tuning per design spec §6.6 / §3 (engineering plastics
        // first).
        Preset::vendor("cd400-pa", "CD400 PA (Nylon)", SettingScope::Filament)
            .with("filament_type", SettingValue::String("PA".to_string()))
            .with("nozzle_temp", SettingValue::Int(265))
            .with("bed_temp", SettingValue::Int(80))
            .with("fan_speed", SettingValue::Number(20.0)),
        Preset::vendor("cd400-pa-cf", "CD400 PA-CF", SettingScope::Filament)
            .with("filament_type", SettingValue::String("PA-CF".to_string()))
            .with("nozzle_temp", SettingValue::Int(280))
            .with("bed_temp", SettingValue::Int(100))
            .with("fan_speed", fil_default("fan_speed")),
        Preset::vendor("cd400-abs-cf", "CD400 ABS-CF", SettingScope::Filament)
            .with("filament_type", SettingValue::String("ABS-CF".to_string()))
            .with("nozzle_temp", SettingValue::Int(265))
            .with("bed_temp", SettingValue::Int(105))
            .with("fan_speed", SettingValue::Number(0.0)),
        Preset::vendor("cd400-asa-cf", "CD400 ASA-CF", SettingScope::Filament)
            .with("filament_type", SettingValue::String("ASA-CF".to_string()))
            .with("nozzle_temp", SettingValue::Int(260))
            .with("bed_temp", SettingValue::Int(100))
            .with("fan_speed", SettingValue::Number(15.0)),
        Preset::vendor("cd400-petg", "CD400 PETG", SettingScope::Filament)
            .with("filament_type", SettingValue::String("PETG".to_string()))
            .with("nozzle_temp", SettingValue::Int(230))
            .with("bed_temp", SettingValue::Int(75))
            .with("fan_speed", SettingValue::Number(80.0)),
        Preset::vendor("cd400-petg-cf", "CD400 PETG-CF", SettingScope::Filament)
            .with("filament_type", SettingValue::String("PETG-CF".to_string()))
            .with("nozzle_temp", SettingValue::Int(245))
            .with("bed_temp", SettingValue::Int(80))
            .with("fan_speed", SettingValue::Number(40.0)),
        Preset::vendor("cd400-pc-cf", "CD400 PC-CF", SettingScope::Filament)
            .with("filament_type", SettingValue::String("PC-CF".to_string()))
            .with("nozzle_temp", SettingValue::Int(290))
            .with("bed_temp", SettingValue::Int(110))
            .with("fan_speed", SettingValue::Number(10.0)),
        // Printer preset — CD400 build volume
        Preset::vendor("cd400-default", "CD400 (default)", SettingScope::Printer)
            .with("bed_size_x", printer_default("bed_size_x"))
            .with("bed_size_y", printer_default("bed_size_y"))
            .with("bed_size_z", printer_default("bed_size_z")),
    ]
}
