//! Slicing pipeline orchestration.
//!
//! Three callers exercise this module:
//!   * The RPC daemon's `fake_slice` background task — calls
//!     `slicer_ffi::run_slice` directly with state-derived settings,
//!     bypassing this crate. (Historical: the dispatcher predates the
//!     pipeline crate having a real impl.)
//!   * The headless CLI (`protype-slicer-daemon slice ...`) — used for
//!     industrial automation, scripted batch slicing, and the determinism
//!     CI. Goes through `run_slice_blocking` below.
//!   * The RPC `slice.estimate` — slices through `estimate::estimate`,
//!     which is `run_slice_blocking` too.
//!
//! The CLI path uses baseline settings from
//! `slicer_types::SettingsSchema::baseline()` so the output matches what
//! the RPC path would produce on a fresh project — except for every key
//! the caller's raw libslic3r `config_overrides` name, which the baseline
//! never overwrites (see `scalars`).

pub mod estimate;
mod scalars;

use thiserror::Error;

/// One object on the plate: its mesh, and what the slicer is to do with it.
#[derive(Debug, Clone)]
pub struct PlateObject {
    inputs: slicer_ffi::ObjectInputs,
}

impl PlateObject {
    /// A mesh as its file has it: the coordinates it was authored with, no
    /// paint, no settings of its own and the preset's tool — the
    /// single-material case.
    pub fn new(input_path: impl Into<String>) -> Self {
        Self::described(slicer_ffi::ObjectInputs {
            input_path: input_path.into(),
            part_index: -1,
            object_transform: slicer_ffi::IDENTITY_OBJECT_TRANSFORM,
            seam_facets_rle: String::new(),
            support_facets_rle: String::new(),
            fan_facets_rle: None,
            filament_facets_rle: None,
            layer_height_profile: Vec::new(),
            layer_ranges: Vec::new(),
            object_overrides: Vec::new(),
        })
    }

    /// The same object, printed with tool `slot`. `extruder` is libslic3r's
    /// per-`ModelObject` key and is **1-based** (tool 0 is `extruder = 1`),
    /// matching the RPC's own validation in `slicer-rpc`'s
    /// `object.set_setting`.
    pub fn with_extruder(self, slot: u32) -> Self {
        let mut inputs = self.inputs;
        inputs.object_overrides.push(slicer_ffi::ConfigOverride {
            key: "extruder".to_string(),
            value: slot.to_string(),
        });
        Self { inputs }
    }

    /// An object described in full — transform, paint, layer ranges and
    /// settings of its own — the way the RPC daemon hands a project object
    /// to the slicer.
    pub fn described(inputs: slicer_ffi::ObjectInputs) -> Self {
        Self { inputs }
    }

    /// The mesh the object is sliced from.
    pub fn input_path(&self) -> &str {
        &self.inputs.input_path
    }
}

#[derive(Debug, Clone)]
pub struct SliceRequest {
    /// Meshes to place on the plate, in order. More than one entry is what
    /// makes a multi-material slice possible from the CLI: assign different
    /// `extruder` values and libslic3r emits the tool changes.
    pub objects: Vec<PlateObject>,
    /// Where the G-code goes. A relative path is taken against the current
    /// directory, as a bare `--output out.gcode` on the command line means.
    pub output_path: String,
    /// Preset paths are accepted for forward compatibility but currently
    /// unused — the FFI bridge loads its own bundled OrcaSlicer flat
    /// preset internally. A future iteration will let callers supply
    /// alternate presets via these paths.
    pub printer_preset_path: String,
    pub process_preset_path: String,
    pub filament_preset_path: String,
    /// Optional IDEX inputs. `None` = single-extruder slice. When set,
    /// these fields take precedence for per-slot bridge inputs.
    pub idex: Option<slicer_ffi::IdexInputs>,
    /// Optional raw libslic3r config overrides. Plumbed straight to
    /// `SliceParams::config_overrides` so headless callers (CLI smoke
    /// tests, scripted batch slicing) can flip features such as
    /// `min_layer_time_strategy` without round-tripping through the RPC
    /// daemon, or pass a whole profile. A key named here wins over the
    /// schema baseline. Empty = the schema baseline over the bundled preset.
    pub config_overrides: Vec<slicer_ffi::ConfigOverride>,
    /// Header line, scene snapshot and machine family for the G-code file.
    /// `SliceContext::default()` for callers that have none (CLI, estimate).
    pub context: slicer_ffi::SliceContext,
}

#[derive(Debug, Clone)]
pub struct SliceOutcome {
    pub total_layers: u32,
    pub total_segments: u64,
    /// The written G-code: `SliceRequest::output_path`, made absolute.
    pub gcode_path: String,
}

#[derive(Debug, Error)]
pub enum SliceError {
    #[error("input not found: {0}")]
    InputNotFound(String),
    #[error("config override {key} = {value:?}: expected {expected}")]
    InvalidOverride {
        key: String,
        value: String,
        expected: String,
    },
    #[error("ffi error: {0}")]
    Ffi(String),
    #[error("slice canceled")]
    Canceled,
    #[error("cannot resolve output path {path:?}: {source}")]
    OutputPath {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use slicer_types::{SettingConstraint, SettingDef, SettingScope, SettingValue, SettingsSchema};

    #[test]
    fn support_flag_uses_profile_v2_enable_support_key() {
        let schema = SettingsSchema {
            schema_version: 1,
            defs: vec![SettingDef {
                key: "enable_support".to_string(),
                scope: SettingScope::Process,
                group: "supports".to_string(),
                label: "Enable support".to_string(),
                description: None,
                default: SettingValue::Bool(true),
                constraint: SettingConstraint::Bool,
                basic_visible: true,
            }],
        };

        assert!(super::schema_support_enabled(&schema));
    }

    #[test]
    fn schema_defaults_prefer_profile_v2_slice_keys() {
        let schema = SettingsSchema {
            schema_version: 1,
            defs: vec![
                SettingDef {
                    key: "perimeter_count".to_string(),
                    scope: SettingScope::Process,
                    group: "walls".to_string(),
                    label: "Legacy walls".to_string(),
                    description: None,
                    default: SettingValue::Int(2),
                    constraint: SettingConstraint::Int { min: 0, max: 10 },
                    basic_visible: true,
                },
                SettingDef {
                    key: "wall_loops".to_string(),
                    scope: SettingScope::Process,
                    group: "walls".to_string(),
                    label: "Wall loops".to_string(),
                    description: None,
                    default: SettingValue::Int(5),
                    constraint: SettingConstraint::Int { min: 0, max: 10 },
                    basic_visible: true,
                },
                SettingDef {
                    key: "sparse_infill_density".to_string(),
                    scope: SettingScope::Process,
                    group: "infill".to_string(),
                    label: "Sparse infill".to_string(),
                    description: None,
                    default: SettingValue::Number(33.0),
                    constraint: SettingConstraint::Number {
                        min: 0.0,
                        max: 100.0,
                        step: 1.0,
                        unit: "%".to_string(),
                    },
                    basic_visible: true,
                },
                SettingDef {
                    key: "sparse_infill_pattern".to_string(),
                    scope: SettingScope::Process,
                    group: "infill".to_string(),
                    label: "Sparse pattern".to_string(),
                    description: None,
                    default: SettingValue::String("gyroid".to_string()),
                    constraint: SettingConstraint::Enum {
                        options: Vec::new(),
                    },
                    basic_visible: true,
                },
            ],
        };

        assert_eq!(
            super::schema_default_i32(&schema, &["wall_loops", "perimeter_count"], 0),
            5
        );
        assert_eq!(
            super::schema_default_f32(&schema, &["sparse_infill_density", "infill_density"], -1.0,),
            33.0
        );
        assert_eq!(
            super::schema_default_string(&schema, &["sparse_infill_pattern", "infill_pattern"],),
            "gyroid"
        );
    }
}

fn schema_support_enabled(schema: &slicer_types::SettingsSchema) -> bool {
    schema
        .defs
        .iter()
        .find(|d| d.key == "enable_support")
        .or_else(|| schema.defs.iter().find(|d| d.key == "support_enabled"))
        .and_then(|d| match &d.default {
            slicer_types::SettingValue::Bool(v) => Some(*v),
            _ => None,
        })
        .unwrap_or(false)
}

fn schema_default_f32(schema: &slicer_types::SettingsSchema, keys: &[&str], fallback: f32) -> f32 {
    keys.iter()
        .find_map(|key| {
            schema
                .defs
                .iter()
                .find(|d| d.key == *key)
                .and_then(|d| match &d.default {
                    slicer_types::SettingValue::Number(n) => Some(*n as f32),
                    _ => None,
                })
        })
        .unwrap_or(fallback)
}

fn schema_default_i32(schema: &slicer_types::SettingsSchema, keys: &[&str], fallback: i32) -> i32 {
    keys.iter()
        .find_map(|key| {
            schema
                .defs
                .iter()
                .find(|d| d.key == *key)
                .and_then(|d| match &d.default {
                    slicer_types::SettingValue::Int(n) => Some(*n as i32),
                    _ => None,
                })
        })
        .unwrap_or(fallback)
}

fn schema_default_string(schema: &slicer_types::SettingsSchema, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            schema
                .defs
                .iter()
                .find(|d| d.key == *key)
                .and_then(|d| match &d.default {
                    slicer_types::SettingValue::String(s) => Some(s.clone()),
                    _ => None,
                })
        })
        .unwrap_or_default()
}

/// `path` made absolute against the current directory, which is what the
/// bridge has to be given.
///
/// libslic3r's `GCode::do_export` creates the output's parent directory
/// when it does not exist. The parent of a bare file name like `out.gcode`
/// is the empty path, which never exists and cannot be created, so the
/// headless slice died in `boost::filesystem::create_directory`. Every
/// relative path is resolved, not only bare names, so that
/// `SliceOutcome::gcode_path` names the written file whatever the caller's
/// current directory is afterwards. An empty path names no file at all.
fn absolute_output_path(path: &str) -> Result<String, SliceError> {
    let unresolvable = |source| SliceError::OutputPath {
        path: path.to_string(),
        source,
    };
    let absolute = std::path::absolute(path).map_err(unresolvable)?;
    // The bridge takes UTF-8. A lossy conversion would name a directory
    // that does not exist, and libslic3r would create it and write there.
    absolute.into_os_string().into_string().map_err(|_| {
        unresolvable(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the current directory is not valid UTF-8",
        ))
    })
}

/// Run a synchronous slice. Used by the headless CLI; the RPC path uses
/// `slicer_ffi::run_slice` directly so it can stream progress events.
pub fn run_slice_blocking(req: SliceRequest) -> Result<SliceOutcome, SliceError> {
    if req.objects.is_empty() {
        return Err(SliceError::InputNotFound(String::new()));
    }
    for object in &req.objects {
        if !std::path::Path::new(object.input_path()).exists() {
            return Err(SliceError::InputNotFound(object.input_path().to_string()));
        }
    }

    let req = SliceRequest {
        output_path: absolute_output_path(&req.output_path)?,
        ..req
    };

    let scalars = scalars::headless_scalars(&req.config_overrides)?;
    let idex = req.idex.clone();

    let outcome = slicer_ffi::run_slice(slicer_ffi::SliceParams {
        output_path: &req.output_path,
        layer_height: scalars.layer_height,
        wall_loops: scalars.wall_loops,
        sparse_infill_density_pct: scalars.sparse_infill_density_pct,
        sparse_infill_pattern: scalars.sparse_infill_pattern,
        enable_support: scalars.enable_support,
        print_speed_mm_s: scalars.print_speed_mm_s,
        top_shell_thickness_mm: scalars.top_shell_thickness_mm,
        wall_generator: scalars.wall_generator,
        config_overrides: req.config_overrides.clone(),
        // bbox is informational once libslic3r owns slicing; the bridge's
        // bundled preset's printable_area governs the actual build volume.
        bbox_min: [0.0, 0.0, 0.0],
        bbox_max: [256.0, 256.0, 250.0],
        // This crate places nothing. A mesh from the CLI keeps the
        // coordinates it was authored with *relative to the others*, so
        // meshes designed to mate (a head and the eyes that fill its
        // sockets) stay registered to each other; an object described in
        // full (`PlateObject::described`, the one part `slice.estimate`
        // costs) is scaled and turned by its transform, but where the
        // transform left it is not kept — a part off the bed costs what it
        // costs on it. The plate as a whole goes down on the bed in the
        // bridge (`place_authored_plate_on_bed`) — once, by one vector for
        // every object, which is what keeps that registration.
        place_plate_on_bed: true,
        calibration_test: None,
        objects: req
            .objects
            .iter()
            .map(|object| object.inputs.clone())
            .collect(),
        // Headless CLI doesn't expose a cancel hook today. SIGINT will
        // tear down the whole process, which is acceptable for the
        // scripted use case.
        cancel: None,
        idex,
        context: req.context.clone(),
    });

    if outcome.canceled {
        return Err(SliceError::Canceled);
    }
    if !outcome.success {
        return Err(SliceError::Ffi(outcome.error_message));
    }

    Ok(SliceOutcome {
        total_layers: outcome.total_layers,
        total_segments: outcome.total_segments,
        gcode_path: req.output_path,
    })
}
