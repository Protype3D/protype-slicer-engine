//! Settings system: schema-described configurable parameters for printer,
//! process, and filament presets. The schema is exposed to UI via codegen
//! (`protype-slicer-daemon schema`) so forms can be auto-generated.
//!
//! Phase 2 scope: ~10 core parameters across three categories. The full
//! ~250-parameter set (per design spec Section 6.6) is a Phase 2.x expansion.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One settings value, either a number, an int, a bool, or a string enum.
/// Stays as a tagged union so the codegen produces a TypeScript discriminated
/// union the UI can switch over.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "kind", content = "value")]
pub enum SettingValue {
    Number(f64),
    Int(i64),
    Bool(bool),
    String(String),
}

impl SettingValue {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            SettingValue::Number(v) => Some(*v),
            SettingValue::Int(v) => Some(*v as f64),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            SettingValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            SettingValue::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

/// Setting category; determines which preset the value belongs to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SettingScope {
    Process,
    Filament,
    Printer,
}

/// Stable key identifying a setting field. Strings are used (not enums) so the
/// schema is forward-compatible with new parameters added later.
pub type SettingKey = String;

/// Subset of process-scope settings that can be overridden per-object. Per
/// design spec §6.6 — the slicer must support overriding these on a single
/// object without affecting the rest of the plate. Real per-object slicing
/// is wired when libslic3r FFI lands (T1A.8); for now the daemon tracks
/// the override and the UI displays it.
pub const OVERRIDABLE_KEYS: &[&str] = &[
    "layer_height",
    // `wall_loops` is the canonical T1 schema key; `perimeter_count` is the
    // legacy alias still accepted for backward compatibility with older UI
    // builds and projects that stored the override under the old name. Both
    // resolve to the same libslic3r `wall_loops` config option in the bridge.
    "wall_loops",
    "perimeter_count",
    "infill_density",
    "infill_pattern",
    "support_enabled",
    "ironing_type",
    "ironing_flow",
    "ironing_speed",
    "ironing_spacing",
    "ironing_inset",
    "seam_position",
    "seam_slope_type",
    "seam_slope_conditional",
    "scarf_angle_threshold",
    "seam_slope_start_height",
    "seam_slope_min_length",
    "seam_slope_steps",
    "seam_slope_inner_walls",
    "scarf_joint_speed",
    "scarf_joint_flow_ratio",
    "scarf_overhang_threshold",
    "seam_slope_entire_loop",
    "seam_facets_rle",
    "seam_target_point",
    // Per-object extruder/tool assignment (IDEX multi-material). Maps to
    // libslic3r's `ModelObject.config.extruder` (1-based) via the generic
    // object-override path in the bridge, so ToolOrdering emits T{n} for the
    // object. It is NOT a schema Process setting, so `object.set_override`
    // special-cases its validation (see dispatch.rs) instead of the schema lookup.
    "extruder",
    // libslic3r `PrintObjectConfig` option, offered by the UI's override panel.
    // Validated by `object_override_constraint`, not the plate schema.
    "support_interface_spacing",
    // `support_facets_rle` is intentionally NOT listed: it is mutated via the
    // dedicated `object.set_support_facets` RPC, not the generic
    // `object.set_override` path. The UI's OVERRIDABLE_KEYS does include it
    // (see packages/ui/src/store/objectStore.ts) for per-object editor
    // bookkeeping — this desync is by design.
    //
    // `layer_height_profile` is also intentionally NOT listed: it lives as a
    // top-level field on ObjectRecord (not in the settings overrides map) and
    // is mutated via the dedicated `object.set_layer_height_profile` RPC.
];

pub fn is_overridable(key: &str) -> bool {
    OVERRIDABLE_KEYS.contains(&key)
}

/// The one key an override is stored under. The UI reads a stored
/// `perimeter_count` as `wall_loops` and clears it under that name, so the
/// daemon keeps no second spelling that could outlive a cleared override.
pub fn canonical_override_key(key: &str) -> &str {
    match key {
        "perimeter_count" => "wall_loops",
        _ => key,
    }
}

/// Value constraint for an overridable key that `SettingsSchema::baseline`
/// does not define. Adding such a key to the plate schema instead would make
/// `setting.set` accept it plate-wide, where nothing reads it. Ranges mirror
/// the UI's override panel (`packages/ui/src/components/overrides/overrideDefs.ts`
/// in Slicer).
pub fn object_override_constraint(key: &str) -> Option<SettingConstraint> {
    match key {
        // Canonical name of the schema's `perimeter_count`.
        "wall_loops" => Some(SettingConstraint::Int { min: 1, max: 12 }),
        "support_interface_spacing" => Some(SettingConstraint::Number {
            min: 0.0,
            max: 10.0,
            step: 0.05,
            unit: "mm".to_string(),
        }),
        _ => None,
    }
}

/// Keys overridable per layer-config-range. Strictly narrower than the
/// object-wide `OVERRIDABLE_KEYS` — libslic3r only honours a subset of
/// the process config inside `layer_config_ranges`. Mirrors PrusaSlicer's
/// "Layer ranges modifier" UI scope.
///
/// A range reaches the print only as `layer_height` or through
/// `PrintRegionConfig`; any other key is accepted by libslic3r and silently
/// ignored. That is why cooling is absent: part-fan speeds are per-extruder
/// `PrintConfig` options. The gate is
/// `slicer-rpc/tests/layer_range_keys_reach_libslic3r.rs`. What a key's value
/// must be: `validate_layer_range_value`.
///
/// If you add a key here, also update `LAYER_RANGE_OVERRIDABLE_KEYS` in
/// `packages/ui/src/store/layerRangeWhitelist.ts` (UI mirror).
pub const LAYER_RANGE_OVERRIDABLE_KEYS: &[&str] = &[
    // Layer height per band — the headline use case.
    "layer_height",
    // Speed family.
    "outer_wall_speed",
    "inner_wall_speed",
    "sparse_infill_speed",
    "internal_solid_infill_speed",
    "top_surface_speed",
    "bridge_speed",
    "gap_infill_speed",
    // Wall / infill density.
    "wall_loops",
    "sparse_infill_density",
];

pub fn is_layer_range_overridable(key: &str) -> bool {
    LAYER_RANGE_OVERRIDABLE_KEYS.contains(&key)
}

/// `Ok` when libslic3r applies `value` as the layer-range override `key` (a
/// key on `LAYER_RANGE_OVERRIDABLE_KEYS`), else what is wrong with it. Every
/// such key is a numeric libslic3r option and the bridge hands the value over
/// as text (`set_deserialize_strict`), so anything but a number is lost or
/// misread: `handle_legacy` drops `outer_wall_speed = 50%` and still reports
/// success, text that is not a number does not parse and the bridge skips it,
/// and `true` reads as 1 mm/s. The kinds are the ones the Slicer's range
/// panel sends (`LAYER_RANGE_OVERRIDE_DEFS` in
/// `packages/ui/src/store/layerRangeWhitelist.ts`), held as strictly as
/// `object.set_override` holds its keys': a `Number`, and for `wall_loops` —
/// an `int` option — an `Int` that fits one; 3.5 would be read as 3 walls.
/// No bounds beyond that: the panel does not clamp what is typed, and
/// libslic3r applies any number. A key of another type needs its own arm.
pub fn validate_layer_range_value(key: &str, value: &SettingValue) -> Result<(), String> {
    match (key, value) {
        ("wall_loops", SettingValue::Int(n)) if i32::try_from(*n).is_ok() => Ok(()),
        ("wall_loops", _) => Err(format!(
            "{key} takes an Int that fits a 32-bit integer, got {value:?}"
        )),
        (_, SettingValue::Number(_)) => Ok(()),
        _ => Err(format!("{key} takes a Number, got {value:?}")),
    }
}

/// Constraint metadata published in JSON Schema for UI form generation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum SettingConstraint {
    Number {
        min: f64,
        max: f64,
        step: f64,
        unit: String,
    },
    Int {
        min: i64,
        max: i64,
    },
    Bool,
    Enum {
        options: Vec<EnumOption>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EnumOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SettingDef {
    pub key: SettingKey,
    pub scope: SettingScope,
    /// Group within the scope (e.g. "Quality", "Walls", "Infill", "Cooling").
    pub group: String,
    pub label: String,
    pub description: Option<String>,
    pub default: SettingValue,
    pub constraint: SettingConstraint,
    /// Whether the field is shown in Basic mode (D21 schema annotation).
    pub basic_visible: bool,
}

/// The schema: list of all available settings with metadata. Versioned so
/// migrations work later (D25).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SettingsSchema {
    pub schema_version: u32,
    pub defs: Vec<SettingDef>,
}

impl SettingsSchema {
    /// Phase 2 baseline: ~10 core parameters.
    pub fn baseline() -> Self {
        Self {
            schema_version: 1,
            defs: vec![
                // -------- Process (How to print) --------
                SettingDef {
                    key: "layer_height".to_string(),
                    scope: SettingScope::Process,
                    group: "Quality".to_string(),
                    label: "Layer height".to_string(),
                    description: Some(
                        "Vertical resolution of each printed layer. Smaller \
                         is finer but slower."
                            .to_string(),
                    ),
                    default: SettingValue::Number(0.2),
                    constraint: SettingConstraint::Number {
                        min: 0.05,
                        max: 0.6,
                        step: 0.01,
                        unit: "mm".to_string(),
                    },
                    basic_visible: true,
                },
                SettingDef {
                    key: "top_shell_thickness".to_string(),
                    scope: SettingScope::Process,
                    group: "Quality".to_string(),
                    label: "Top shell thickness".to_string(),
                    description: Some(
                        "Solid skin thickness on the top of the print. Larger \
                         is more sealed but takes longer. The slicer auto-rounds \
                         to a whole number of layers based on Layer height."
                            .to_string(),
                    ),
                    default: SettingValue::Number(1.0),
                    constraint: SettingConstraint::Number {
                        min: 0.2,
                        max: 5.0,
                        step: 0.1,
                        unit: "mm".to_string(),
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "perimeter_count".to_string(),
                    scope: SettingScope::Process,
                    group: "Walls".to_string(),
                    label: "Wall count".to_string(),
                    description: Some(
                        "Number of perimeter walls. More walls = stronger.".to_string(),
                    ),
                    default: SettingValue::Int(3),
                    constraint: SettingConstraint::Int { min: 1, max: 10 },
                    basic_visible: true,
                },
                SettingDef {
                    key: "wall_generator".to_string(),
                    scope: SettingScope::Process,
                    group: "Walls".to_string(),
                    label: "Wall generator".to_string(),
                    description: Some(
                        "Classic fixes the wall width to nozzle size; Arachne \
                         uses variable widths so thin features (chimneys, \
                         small text, sharp corners) print cleanly without \
                         gap-filling tricks."
                            .to_string(),
                    ),
                    default: SettingValue::String("arachne".to_string()),
                    constraint: SettingConstraint::Enum {
                        options: vec![
                            EnumOption {
                                value: "classic".to_string(),
                                label: "Classic".to_string(),
                            },
                            EnumOption {
                                value: "arachne".to_string(),
                                label: "Arachne (variable width)".to_string(),
                            },
                        ],
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "infill_density".to_string(),
                    scope: SettingScope::Process,
                    group: "Infill".to_string(),
                    label: "Infill density".to_string(),
                    description: Some("Internal fill percentage.".to_string()),
                    default: SettingValue::Number(20.0),
                    constraint: SettingConstraint::Number {
                        min: 0.0,
                        max: 100.0,
                        step: 1.0,
                        unit: "%".to_string(),
                    },
                    basic_visible: true,
                },
                SettingDef {
                    key: "infill_pattern".to_string(),
                    scope: SettingScope::Process,
                    group: "Infill".to_string(),
                    label: "Infill pattern".to_string(),
                    description: None,
                    default: SettingValue::String("gyroid".to_string()),
                    constraint: SettingConstraint::Enum {
                        options: vec![
                            EnumOption {
                                value: "grid".to_string(),
                                label: "Grid".to_string(),
                            },
                            EnumOption {
                                value: "gyroid".to_string(),
                                label: "Gyroid".to_string(),
                            },
                            EnumOption {
                                value: "honeycomb".to_string(),
                                label: "Honeycomb".to_string(),
                            },
                            EnumOption {
                                value: "lines".to_string(),
                                label: "Lines".to_string(),
                            },
                            EnumOption {
                                value: "triangles".to_string(),
                                label: "Triangles".to_string(),
                            },
                        ],
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "support_enabled".to_string(),
                    scope: SettingScope::Process,
                    group: "Supports".to_string(),
                    label: "Generate supports".to_string(),
                    description: Some("Enable support material for overhangs.".to_string()),
                    default: SettingValue::Bool(false),
                    constraint: SettingConstraint::Bool,
                    basic_visible: true,
                },
                SettingDef {
                    key: "print_speed".to_string(),
                    scope: SettingScope::Process,
                    group: "Speed".to_string(),
                    label: "Print speed".to_string(),
                    description: None,
                    default: SettingValue::Number(60.0),
                    constraint: SettingConstraint::Number {
                        min: 10.0,
                        max: 250.0,
                        step: 5.0,
                        unit: "mm/s".to_string(),
                    },
                    basic_visible: true,
                },
                // -------- Process / Seam --------
                SettingDef {
                    key: "seam_position".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Seam position".to_string(),
                    description: Some(
                        "Where each perimeter loop starts and ends. \
                         Aligned stacks the seam vertically; Aligned back \
                         biases it toward the rear; Random scatters it; \
                         Nearest minimizes travel."
                            .to_string(),
                    ),
                    default: SettingValue::String("aligned_back".to_string()),
                    constraint: SettingConstraint::Enum {
                        options: vec![
                            EnumOption {
                                value: "aligned_back".to_string(),
                                label: "Aligned back".to_string(),
                            },
                            EnumOption {
                                value: "aligned".to_string(),
                                label: "Aligned".to_string(),
                            },
                            EnumOption {
                                value: "nearest".to_string(),
                                label: "Nearest".to_string(),
                            },
                            EnumOption {
                                value: "back".to_string(),
                                label: "Rear".to_string(),
                            },
                            EnumOption {
                                value: "random".to_string(),
                                label: "Random".to_string(),
                            },
                        ],
                    },
                    basic_visible: true,
                },
                SettingDef {
                    key: "seam_slope_type".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Scarf seam".to_string(),
                    description: Some(
                        "Scarf joint ramps the seam over multiple layers to \
                         hide it as a gradient instead of a step."
                            .to_string(),
                    ),
                    default: SettingValue::String("none".to_string()),
                    constraint: SettingConstraint::Enum {
                        options: vec![
                            EnumOption {
                                value: "none".to_string(),
                                label: "Off".to_string(),
                            },
                            EnumOption {
                                value: "external".to_string(),
                                label: "External walls".to_string(),
                            },
                            EnumOption {
                                value: "all".to_string(),
                                label: "All walls".to_string(),
                            },
                        ],
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "seam_slope_conditional".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Conditional scarf".to_string(),
                    description: Some(
                        "Apply scarf only on smooth perimeters; sharp \
                         corners hide the seam well enough on their own."
                            .to_string(),
                    ),
                    default: SettingValue::Bool(true),
                    constraint: SettingConstraint::Bool,
                    basic_visible: false,
                },
                SettingDef {
                    key: "scarf_angle_threshold".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Scarf angle threshold".to_string(),
                    description: Some(
                        "Maximum interior angle (degrees) on the perimeter \
                         loop above which scarf is applied. Default 155°."
                            .to_string(),
                    ),
                    default: SettingValue::Int(155),
                    constraint: SettingConstraint::Int { min: 90, max: 180 },
                    basic_visible: false,
                },
                // NOTE: libslic3r declares this as coFloatOrPercent and defaults to
                // 0 (height-based ramp disabled). We default to 0.4 mm so enabling
                // scarf yields a visible ramp without further tuning. Same
                // FloatOrPercent caveat as scarf_joint_speed applies.
                SettingDef {
                    key: "seam_slope_start_height".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Scarf start height".to_string(),
                    description: Some(
                        "Vertical distance over which the scarf ramps the \
                         flow up from zero. Larger = smoother but longer seam."
                            .to_string(),
                    ),
                    default: SettingValue::Number(0.4),
                    constraint: SettingConstraint::Number {
                        min: 0.0,
                        max: 2.0,
                        step: 0.05,
                        unit: "mm".to_string(),
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "seam_slope_min_length".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Scarf min length".to_string(),
                    description: Some(
                        "Minimum loop length (mm) for scarf to engage. \
                         Small loops are too short to ramp smoothly."
                            .to_string(),
                    ),
                    default: SettingValue::Number(20.0),
                    constraint: SettingConstraint::Number {
                        min: 0.0,
                        max: 200.0,
                        step: 1.0,
                        unit: "mm".to_string(),
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "seam_slope_steps".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Scarf steps".to_string(),
                    description: Some(
                        "Number of discrete flow steps along the ramp. \
                         More steps = smoother gradient but more G-code."
                            .to_string(),
                    ),
                    default: SettingValue::Int(10),
                    constraint: SettingConstraint::Int { min: 2, max: 64 },
                    basic_visible: false,
                },
                SettingDef {
                    key: "seam_slope_inner_walls".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Scarf inner walls".to_string(),
                    description: Some(
                        "Apply scarf to inner walls too. Off by default — \
                         outer wall is the visible one."
                            .to_string(),
                    ),
                    default: SettingValue::Bool(false),
                    constraint: SettingConstraint::Bool,
                    basic_visible: false,
                },
                // NOTE: libslic3r declares this as coFloatOrPercent (accepts "80%"
                // or absolute mm/s). The current SettingConstraint::Number encoding
                // forces absolute values only. If UI later wants percentage entry,
                // introduce a FloatOrPercent constraint variant first.
                SettingDef {
                    key: "scarf_joint_speed".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Scarf joint speed".to_string(),
                    description: Some(
                        "Print speed in the scarf region. Slower = cleaner seam.".to_string(),
                    ),
                    default: SettingValue::Number(20.0),
                    constraint: SettingConstraint::Number {
                        min: 5.0,
                        max: 250.0,
                        step: 5.0,
                        unit: "mm/s".to_string(),
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "scarf_joint_flow_ratio".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Scarf flow ratio".to_string(),
                    description: Some(
                        "Multiplier on extrusion flow inside the scarf ramp.".to_string(),
                    ),
                    default: SettingValue::Number(1.0),
                    constraint: SettingConstraint::Number {
                        min: 0.5,
                        max: 1.5,
                        step: 0.05,
                        unit: "x".to_string(),
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "scarf_overhang_threshold".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Scarf overhang threshold".to_string(),
                    description: Some(
                        "Skip scarf on perimeter segments whose overhang \
                         exceeds this percentage of nozzle diameter."
                            .to_string(),
                    ),
                    default: SettingValue::Number(60.0),
                    constraint: SettingConstraint::Number {
                        min: 0.0,
                        max: 100.0,
                        step: 5.0,
                        unit: "%".to_string(),
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "seam_slope_entire_loop".to_string(),
                    scope: SettingScope::Process,
                    group: "Seam".to_string(),
                    label: "Scarf entire loop".to_string(),
                    description: Some(
                        "Spread the scarf flow ramp across the entire \
                         perimeter loop instead of just the start segment."
                            .to_string(),
                    ),
                    default: SettingValue::Bool(false),
                    constraint: SettingConstraint::Bool,
                    basic_visible: false,
                },
                // -------- Process / Ironing --------
                SettingDef {
                    key: "ironing_type".to_string(),
                    scope: SettingScope::Process,
                    group: "Ironing".to_string(),
                    label: "Ironing".to_string(),
                    description: Some(
                        "Smooth top surfaces with an extra hot pass after \
                         the layer is printed."
                            .to_string(),
                    ),
                    default: SettingValue::String("none".to_string()),
                    constraint: SettingConstraint::Enum {
                        options: vec![
                            EnumOption {
                                value: "none".to_string(),
                                label: "Off".to_string(),
                            },
                            EnumOption {
                                value: "top".to_string(),
                                label: "Top surfaces".to_string(),
                            },
                            EnumOption {
                                value: "topmost".to_string(),
                                label: "Topmost layer only".to_string(),
                            },
                            EnumOption {
                                value: "all_solid".to_string(),
                                label: "All solid layers".to_string(),
                            },
                        ],
                    },
                    basic_visible: true,
                },
                SettingDef {
                    key: "ironing_flow".to_string(),
                    scope: SettingScope::Process,
                    group: "Ironing".to_string(),
                    label: "Ironing flow".to_string(),
                    description: Some(
                        "Percent of normal extrusion flow during the ironing \
                         pass. Lower = less material, smoother finish."
                            .to_string(),
                    ),
                    default: SettingValue::Number(10.0),
                    constraint: SettingConstraint::Number {
                        min: 0.0,
                        max: 100.0,
                        step: 1.0,
                        unit: "%".to_string(),
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "ironing_speed".to_string(),
                    scope: SettingScope::Process,
                    group: "Ironing".to_string(),
                    label: "Ironing speed".to_string(),
                    description: Some("Speed of the ironing pass.".to_string()),
                    default: SettingValue::Number(20.0),
                    constraint: SettingConstraint::Number {
                        min: 1.0,
                        max: 500.0,
                        step: 1.0,
                        unit: "mm/s".to_string(),
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "ironing_spacing".to_string(),
                    scope: SettingScope::Process,
                    group: "Ironing".to_string(),
                    label: "Ironing spacing".to_string(),
                    description: Some("Distance between ironing pass lines.".to_string()),
                    default: SettingValue::Number(0.1),
                    constraint: SettingConstraint::Number {
                        min: 0.05,
                        max: 1.0,
                        step: 0.01,
                        unit: "mm".to_string(),
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "ironing_inset".to_string(),
                    scope: SettingScope::Process,
                    group: "Ironing".to_string(),
                    label: "Ironing inset".to_string(),
                    description: Some(
                        "Distance kept from the outer wall during ironing.".to_string(),
                    ),
                    default: SettingValue::Number(0.25),
                    constraint: SettingConstraint::Number {
                        min: 0.0,
                        max: 10.0,
                        step: 0.01,
                        unit: "mm".to_string(),
                    },
                    basic_visible: false,
                },
                // -------- Filament --------
                SettingDef {
                    key: "filament_type".to_string(),
                    scope: SettingScope::Filament,
                    group: "Material".to_string(),
                    label: "Material".to_string(),
                    description: None,
                    default: SettingValue::String("PA-CF".to_string()),
                    constraint: SettingConstraint::Enum {
                        options: vec![
                            EnumOption {
                                value: "PA".to_string(),
                                label: "PA (Nylon)".to_string(),
                            },
                            EnumOption {
                                value: "PA-CF".to_string(),
                                label: "PA-CF".to_string(),
                            },
                            EnumOption {
                                value: "ABS-CF".to_string(),
                                label: "ABS-CF".to_string(),
                            },
                            EnumOption {
                                value: "ASA-CF".to_string(),
                                label: "ASA-CF".to_string(),
                            },
                            EnumOption {
                                value: "PETG".to_string(),
                                label: "PETG".to_string(),
                            },
                            EnumOption {
                                value: "PETG-CF".to_string(),
                                label: "PETG-CF".to_string(),
                            },
                            EnumOption {
                                value: "PC-CF".to_string(),
                                label: "PC-CF".to_string(),
                            },
                        ],
                    },
                    basic_visible: true,
                },
                SettingDef {
                    key: "nozzle_temp".to_string(),
                    scope: SettingScope::Filament,
                    group: "Temperature".to_string(),
                    label: "Nozzle temperature".to_string(),
                    description: None,
                    default: SettingValue::Int(280),
                    constraint: SettingConstraint::Int { min: 150, max: 320 },
                    basic_visible: true,
                },
                SettingDef {
                    key: "bed_temp".to_string(),
                    scope: SettingScope::Filament,
                    group: "Temperature".to_string(),
                    label: "Bed temperature".to_string(),
                    description: None,
                    default: SettingValue::Int(100),
                    constraint: SettingConstraint::Int { min: 0, max: 130 },
                    basic_visible: true,
                },
                SettingDef {
                    key: "fan_speed".to_string(),
                    scope: SettingScope::Filament,
                    group: "Cooling".to_string(),
                    label: "Fan speed".to_string(),
                    description: None,
                    default: SettingValue::Number(30.0),
                    constraint: SettingConstraint::Number {
                        min: 0.0,
                        max: 100.0,
                        step: 5.0,
                        unit: "%".to_string(),
                    },
                    basic_visible: false,
                },
                // -------- Printer --------
                SettingDef {
                    key: "bed_size_x".to_string(),
                    scope: SettingScope::Printer,
                    group: "Build volume".to_string(),
                    label: "Bed X".to_string(),
                    description: None,
                    default: SettingValue::Number(400.0),
                    constraint: SettingConstraint::Number {
                        min: 100.0,
                        max: 600.0,
                        step: 10.0,
                        unit: "mm".to_string(),
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "bed_size_y".to_string(),
                    scope: SettingScope::Printer,
                    group: "Build volume".to_string(),
                    label: "Bed Y".to_string(),
                    description: None,
                    default: SettingValue::Number(400.0),
                    constraint: SettingConstraint::Number {
                        min: 100.0,
                        max: 600.0,
                        step: 10.0,
                        unit: "mm".to_string(),
                    },
                    basic_visible: false,
                },
                SettingDef {
                    key: "bed_size_z".to_string(),
                    scope: SettingScope::Printer,
                    group: "Build volume".to_string(),
                    label: "Bed Z".to_string(),
                    description: None,
                    default: SettingValue::Number(400.0),
                    constraint: SettingConstraint::Number {
                        min: 100.0,
                        max: 600.0,
                        step: 10.0,
                        unit: "mm".to_string(),
                    },
                    basic_visible: false,
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seam_group_contains_position_and_scarf_keys() {
        let schema = SettingsSchema::baseline();
        let seam_keys: Vec<&str> = schema
            .defs
            .iter()
            .filter(|d| d.group == "Seam")
            .map(|d| d.key.as_str())
            .collect();
        assert_eq!(
            seam_keys.len(),
            12,
            "expected exactly 12 entries in Seam group (1 strategy + 11 scarf), got {}",
            seam_keys.len()
        );
        for expected in &[
            "seam_position",
            "seam_slope_type",
            "seam_slope_conditional",
            "scarf_angle_threshold",
            "seam_slope_start_height",
            "seam_slope_min_length",
            "seam_slope_steps",
            "seam_slope_inner_walls",
            "scarf_joint_speed",
            "scarf_joint_flow_ratio",
            "scarf_overhang_threshold",
            "seam_slope_entire_loop",
        ] {
            assert!(
                seam_keys.contains(expected),
                "expected key {expected} in Seam group, got {:?}",
                seam_keys
            );
        }
        let pos = schema
            .defs
            .iter()
            .find(|d| d.key == "seam_position")
            .unwrap();
        assert_eq!(
            pos.default,
            SettingValue::String("aligned_back".to_string())
        );
    }

    #[test]
    fn seam_keys_are_overridable_per_object() {
        for key in &[
            "seam_position",
            "seam_slope_type",
            "seam_slope_conditional",
            "scarf_angle_threshold",
            "seam_slope_start_height",
            "seam_slope_min_length",
            "seam_slope_steps",
            "seam_slope_inner_walls",
            "scarf_joint_speed",
            "scarf_joint_flow_ratio",
            "scarf_overhang_threshold",
            "seam_slope_entire_loop",
        ] {
            assert!(is_overridable(key), "expected {key} to be overridable");
        }
    }

    #[test]
    fn seam_paint_keys_are_overridable() {
        assert!(is_overridable("seam_facets_rle"));
        assert!(is_overridable("seam_target_point"));
    }
}
