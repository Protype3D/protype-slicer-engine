//! The scalars a headless slice hands the bridge.
//!
//! Headless callers have no project to take them from, so they come from
//! `slicer_types::SettingsSchema::baseline()` — the same source the UI
//! seeds its Settings panel from, which keeps CLI output matching the
//! GUI's "fresh project" baseline.
//!
//! A schema default is what a fresh project starts at, not an instruction,
//! but the bridge treats every scalar as one: it applies `config_overrides`
//! first and then forces the scalars over them, unless a scalar holds its
//! "keep" value. So a key the caller's overrides name never goes out as its
//! baseline. It goes as its keep value, or — `layer_height` and
//! `wall_generator` have none — as the caller's own value, which the bridge
//! then forces unchanged. Before that, a profile passed as overrides lost
//! its layer height, walls, infill and wall generator without a word: a
//! 20 mm cube estimated at 100 % infill cost exactly what it costs at 20 %.
//! `top_shell_thickness` needs nothing here: the bridge already skips its
//! shell rebuild whenever the overrides name a shell key.
//!
//! The RPC path (`fake_slice` in `slicer-rpc`) derives its scalars from the
//! project and never comes through here.

use slicer_ffi::ConfigOverride;

use crate::{schema_default_f32, schema_default_i32, schema_default_string};
use crate::{schema_support_enabled, SliceError};

/// What `run_slice_blocking` passes as `SliceParams`' scalar fields.
#[derive(Debug, PartialEq)]
pub(crate) struct HeadlessScalars {
    pub(crate) layer_height: f32,
    pub(crate) wall_loops: u32,
    pub(crate) sparse_infill_density_pct: f32,
    pub(crate) sparse_infill_pattern: String,
    pub(crate) enable_support: bool,
    pub(crate) print_speed_mm_s: f32,
    pub(crate) top_shell_thickness_mm: f32,
    pub(crate) wall_generator: String,
}

/// The values at which the bridge leaves a key to the config — the preset
/// with the overrides applied — instead of forcing it.
const KEEP_WALL_LOOPS: u32 = 0;
const KEEP_INFILL_DENSITY_PCT: f32 = -1.0;
const KEEP_INFILL_PATTERN: &str = "";
/// Headless never forces a print speed, named or not. `print_speed`
/// defaults to 60 mm/s in the schema, and the bridge treats any positive
/// value as "fan this out over outer wall, inner wall and sparse infill" —
/// so taking the default manufactured a user request nobody made and
/// overwrote whatever speeds the profile had. That is what took a 6h17
/// print to 8h30.
///
/// The RPC path uses the scalar only for legacy snapshots where a user
/// really did set it; headless has no such setting at all. Callers who want
/// a specific speed pass the real libslic3r keys through `--overrides`,
/// which is both more precise than one knob and, since the guard in
/// bridge.cpp, authoritative over it.
const KEEP_PRINT_SPEED: f32 = 0.0;
/// The finest layer height the bridge slices: `run_slice` in
/// `protype_slicer_bridge.cpp` clamps anything below up to it.
const MIN_LAYER_HEIGHT_MM: f32 = 0.01;

pub(crate) fn headless_scalars(
    overrides: &[ConfigOverride],
) -> Result<HeadlessScalars, SliceError> {
    let schema = slicer_types::SettingsSchema::baseline();
    let named = |key: &str| last_value(overrides, key);
    Ok(HeadlessScalars {
        // Carried as f32, as the window's layer heights always are: 0.12
        // reaches libslic3r as 0.1199999973, and the G-code still says 0.12.
        layer_height: match named("layer_height") {
            Some(value) => layer_height_mm(value)?,
            None => schema_default_f32(&schema, &["layer_height"], 0.2),
        },
        wall_loops: match named("wall_loops") {
            Some(_) => KEEP_WALL_LOOPS,
            None => {
                schema_default_i32(&schema, &["wall_loops", "perimeter_count"], 0).max(0) as u32
            }
        },
        sparse_infill_density_pct: match named("sparse_infill_density") {
            Some(_) => KEEP_INFILL_DENSITY_PCT,
            None => schema_default_f32(&schema, &["sparse_infill_density", "infill_density"], -1.0),
        },
        sparse_infill_pattern: match named("sparse_infill_pattern") {
            Some(_) => KEEP_INFILL_PATTERN.to_string(),
            None => schema_default_string(&schema, &["sparse_infill_pattern", "infill_pattern"]),
        },
        // The schema default is off; a caller's `enable_support` flips it.
        enable_support: named("enable_support")
            .map(|value| matches!(value, "1" | "true" | "on"))
            .unwrap_or_else(|| schema_support_enabled(&schema)),
        print_speed_mm_s: KEEP_PRINT_SPEED,
        top_shell_thickness_mm: schema_default_f32(&schema, &["top_shell_thickness"], 0.0).max(0.0),
        wall_generator: named("wall_generator")
            .map(str::to_string)
            .unwrap_or_else(|| schema_default_string(&schema, &["wall_generator"])),
    })
}

/// The value the config ends on for `key`. The bridge applies the
/// overrides in order, so of a repeated key the last value is the one that
/// sticks.
fn last_value<'a>(overrides: &'a [ConfigOverride], key: &str) -> Option<&'a str> {
    overrides
        .iter()
        .rev()
        .find(|o| o.key == key)
        .map(|o| o.value.as_str())
}

/// A layer height to carry to the bridge. The bridge forces whatever it is
/// given, clamping anything finer than `MIN_LAYER_HEIGHT_MM` up to it, and
/// falling back to the baseline would be this module's bug over again — so
/// a value the bridge could not slice as given is refused here.
fn layer_height_mm(value: &str) -> Result<f32, SliceError> {
    value
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|mm| mm.is_finite() && *mm >= MIN_LAYER_HEIGHT_MM)
        .ok_or_else(|| SliceError::InvalidOverride {
            key: "layer_height".to_string(),
            value: value.to_string(),
            expected: format!("a layer height of at least {MIN_LAYER_HEIGHT_MM} mm"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(pairs: &[(&str, &str)]) -> Vec<ConfigOverride> {
        pairs
            .iter()
            .map(|(key, value)| ConfigOverride {
                key: key.to_string(),
                value: value.to_string(),
            })
            .collect()
    }

    fn scalars(pairs: &[(&str, &str)]) -> HeadlessScalars {
        headless_scalars(&overrides(pairs)).expect("the overrides are valid")
    }

    /// Naming nothing, a headless slice asks for what a fresh project
    /// starts at.
    #[test]
    fn with_no_overrides_the_scalars_are_the_schema_baseline() {
        assert_eq!(
            scalars(&[]),
            HeadlessScalars {
                layer_height: 0.2,
                wall_loops: 3,
                sparse_infill_density_pct: 20.0,
                sparse_infill_pattern: "gyroid".to_string(),
                enable_support: false,
                print_speed_mm_s: 0.0,
                top_shell_thickness_mm: 1.0,
                wall_generator: "arachne".to_string(),
            }
        );
    }

    /// The bridge applies the overrides first and forces the scalars after
    /// them, skipping a scalar only at its "keep" value — so that is what a
    /// key the caller named has to reach it as.
    #[test]
    fn a_named_key_with_a_keep_value_is_left_to_the_config() {
        let s = scalars(&[
            ("wall_loops", "5"),
            ("sparse_infill_density", "100%"),
            ("sparse_infill_pattern", "grid"),
        ]);
        assert_eq!(s.wall_loops, KEEP_WALL_LOOPS);
        assert_eq!(s.sparse_infill_density_pct, KEEP_INFILL_DENSITY_PCT);
        assert_eq!(s.sparse_infill_pattern, KEEP_INFILL_PATTERN);
    }

    /// No "keep" value exists for the generator — the bridge reads an
    /// empty one as Arachne — so the scalar carries the caller's own.
    #[test]
    fn a_named_wall_generator_is_carried_as_asked() {
        assert_eq!(
            scalars(&[("wall_generator", "classic")]).wall_generator,
            "classic"
        );
    }

    /// Nor for the layer height, which the bridge always forces.
    #[test]
    fn a_named_layer_height_is_carried_as_asked() {
        assert_eq!(scalars(&[("layer_height", "0.12")]).layer_height, 0.12);
    }

    /// The bridge applies the overrides in order, so the config ends on a
    /// key's last value. A carried scalar that took an earlier one would
    /// force the config back to it.
    #[test]
    fn a_repeated_key_is_carried_with_its_last_value() {
        let s = scalars(&[
            ("layer_height", "0.1"),
            ("wall_generator", "arachne"),
            ("enable_support", "1"),
            ("layer_height", "0.28"),
            ("wall_generator", "classic"),
            ("enable_support", "0"),
        ]);
        assert_eq!(s.layer_height, 0.28);
        assert_eq!(s.wall_generator, "classic");
        assert!(!s.enable_support);
    }

    /// Only the keys the caller named leave the baseline.
    #[test]
    fn keys_the_overrides_do_not_name_keep_the_baseline() {
        assert_eq!(
            scalars(&[("sparse_infill_density", "100%")]),
            HeadlessScalars {
                sparse_infill_density_pct: KEEP_INFILL_DENSITY_PCT,
                ..scalars(&[])
            }
        );
    }

    /// A layer height that cannot be carried must not quietly turn into
    /// the baseline's 0.2 — that is the bug itself — nor reach the bridge,
    /// which would clamp it up to 0.01 mm: `0.005` is a unit slip, not a
    /// request for a slice twenty times finer than 0.2.
    #[test]
    fn a_layer_height_the_bridge_cannot_slice_as_given_is_refused() {
        for bad in ["abc", "", "0", "-0.2", "NaN", "inf", "0.005"] {
            let err = headless_scalars(&overrides(&[("layer_height", bad)]))
                .expect_err("an unusable layer height must be refused");
            assert!(
                matches!(err, SliceError::InvalidOverride { ref key, .. } if key == "layer_height"),
                "{bad:?} gave {err}"
            );
        }
    }

    #[test]
    fn a_named_enable_support_is_carried() {
        assert!(scalars(&[("enable_support", "1")]).enable_support);
    }
}
