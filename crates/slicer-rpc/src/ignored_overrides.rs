//! Plate overrides libslic3r would drop without applying them, on the
//! `slice.request` path.
//!
//! libslic3r's `PrintConfigDef::handle_legacy` clears a key it does not accept
//! and the setter still reports success, so the bridge refuses a slice that
//! carries such an override. A GUI slice leaves them out instead — the profile
//! is the Slicer's, not something its user typed — and names them in
//! `SliceCompleted.warnings`. Except the ones that lose nothing: the Slicer
//! sends several settings under two names on every slice, and a warning shown
//! on every slice is one nobody reads.

use slicer_ffi::{ConfigOverride, IdexInputs};
use slicer_types::ResolvedSliceConfig;
use std::collections::BTreeSet;

/// Names the Slicer's schema sends next to the libslic3r spelling of the same
/// setting, with the same value, on every slice (`apply_libslic3r_aliases`,
/// schema at Slicer `main` on 2026-09-25): PrusaSlicer's name, filed there as
/// the `libslic3r_key` — or, for the filament rows, the Slicer's own key, with
/// libslic3r's name as the alias. libslic3r drops the name on the left.
pub(crate) const SLICER_ALIASES: &[(&str, &str)] = &[
    ("perimeters", "wall_loops"),
    ("top_solid_layers", "top_shell_layers"),
    ("bottom_solid_layers", "bottom_shell_layers"),
    ("fill_density", "sparse_infill_density"),
    ("infill_every_layers", "infill_combination"),
    ("external_perimeter_speed", "outer_wall_speed"),
    ("perimeter_speed", "inner_wall_speed"),
    ("top_solid_infill_speed", "top_surface_speed"),
    ("first_layer_speed", "initial_layer_speed"),
    ("gap_fill_speed", "gap_infill_speed"),
    ("support_material", "enable_support"),
    ("support_material_threshold", "support_threshold_angle"),
    (
        "support_material_contact_distance",
        "support_top_z_distance",
    ),
    ("skirts", "skirt_loops"),
    (
        "filament_min_layer_time_strategy",
        "min_layer_time_strategy",
    ),
    (
        "filament_park_and_wait_retract_length",
        "park_and_wait_retract_length",
    ),
    ("filament_park_and_wait_z_hop", "park_and_wait_z_hop"),
];

/// A plate override libslic3r would drop, and how the bridge names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IgnoredOverride {
    pub key: String,
    pub description: String,
}

/// Split `overrides` into those libslic3r applies, in order, and those it
/// would drop. `describe` is `slicer_ffi::describe_ignored_config_override`,
/// injected for tests.
pub(crate) fn split(
    overrides: Vec<ConfigOverride>,
    describe: fn(&str, &str) -> Option<String>,
) -> (Vec<ConfigOverride>, Vec<IgnoredOverride>) {
    let mut applied = Vec::with_capacity(overrides.len());
    let mut ignored = Vec::new();
    for entry in overrides {
        match describe(&entry.key, &entry.value) {
            None => applied.push(entry),
            Some(description) => ignored.push(IgnoredOverride {
                key: entry.key,
                description,
            }),
        }
    }
    (applied, ignored)
}

/// The `SliceCompleted` warning naming the dropped overrides that lose
/// something, each once, or `None` when none does.
pub(crate) fn warning(
    ignored: &[IgnoredOverride],
    config: Option<&ResolvedSliceConfig>,
    idex: Option<&IdexInputs>,
) -> Option<String> {
    let mut seen = BTreeSet::new();
    let named: Vec<&str> = ignored
        .iter()
        .filter(|dropped| !loses_nothing(&dropped.key, config, idex))
        .map(|dropped| dropped.description.as_str())
        .filter(|description| seen.insert(*description))
        .collect();
    (!named.is_empty()).then(|| {
        format!(
            "libslic3r does not recognise these settings and ignored them: {}",
            named.join(", ")
        )
    })
}

/// Whether dropping `key` loses nothing: a [`SLICER_ALIASES`] name that every
/// bag carrying it gives the value of its libslic3r spelling, or a flat bed
/// temperature — which the daemon's own mapping produces, knowing libslic3r
/// ignores the name — when the slot DTO carries one, since the bridge sets the
/// plate temperatures from that (`hot_plate_temp`). For bed temperatures that
/// is presence, not value: the slot's temperature is the one printed whatever
/// the flat one says. The Slicer fills both from one filament bag; a client
/// sending them apart is not told the flat one lost.
fn loses_nothing(
    key: &str,
    config: Option<&ResolvedSliceConfig>,
    idex: Option<&IdexInputs>,
) -> bool {
    match key {
        "bed_temperature" => idex.is_some_and(|idex| !idex.bed_temperature.is_empty()),
        "bed_temperature_initial_layer" => {
            idex.is_some_and(|idex| !idex.bed_temperature_initial_layer_single.is_empty())
        }
        _ => SLICER_ALIASES
            .iter()
            .find(|(alias, _)| *alias == key)
            .zip(config)
            .is_some_and(|((alias, spelling), config)| {
                paired_in_every_bag(config, alias, spelling)
            }),
    }
}

/// Values compare as JSON, the form both names arrive in, not as the text
/// libslic3r would parse.
fn paired_in_every_bag(config: &ResolvedSliceConfig, alias: &str, spelling: &str) -> bool {
    let bags = [&config.machine, &config.process]
        .into_iter()
        .chain(&config.filaments);
    let mut carrying = bags.filter(|bag| bag.contains_key(alias)).peekable();
    carrying.peek().is_some()
        && carrying.all(|bag| {
            bag.get(spelling)
                .is_some_and(|value| Some(value) == bag.get(alias))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ov(key: &str, value: &str) -> ConfigOverride {
        ConfigOverride {
            key: key.to_string(),
            value: value.to_string(),
        }
    }

    /// Stands in for `slicer_ffi::describe_ignored_config_override`, which the
    /// stub build answers `None` for everything.
    fn describe_typo(key: &str, _value: &str) -> Option<String> {
        (key == "sparse_infil_density").then(|| format!("{key} (unknown or obsolete key)"))
    }

    fn ignored(key: &str) -> IgnoredOverride {
        IgnoredOverride {
            key: key.to_string(),
            description: format!("{key} (unknown or obsolete key)"),
        }
    }

    fn config(process: serde_json::Value, filaments: serde_json::Value) -> ResolvedSliceConfig {
        serde_json::from_value(json!({
            "schema_version": 1,
            "profile_snapshot_hash": "ignored-overrides",
            "extruders": [{ "slot_id": 0, "nozzle_diameter": 0.4 }],
            "process": process,
            "filaments": filaments
        }))
        .expect("a valid resolved config")
    }

    fn bed_temperatures_on_the_slot() -> IdexInputs {
        IdexInputs {
            bed_temperature: vec![60],
            bed_temperature_initial_layer_single: vec![65],
            ..IdexInputs::empty_for_count(1)
        }
    }

    #[test]
    fn an_override_libslic3r_would_drop_is_split_out_with_its_description() {
        let (applied, dropped) = split(
            vec![
                ov("wall_loops", "3"),
                ov("sparse_infil_density", "100"),
                ov("skirt_loops", "2"),
            ],
            describe_typo,
        );

        let applied_keys: Vec<&str> = applied.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(applied_keys, ["wall_loops", "skirt_loops"]);
        assert_eq!(dropped, [ignored("sparse_infil_density")]);
    }

    #[test]
    fn nothing_dropped_is_no_warning() {
        assert_eq!(
            warning(&[], Some(&config(json!({}), json!([]))), None),
            None
        );
    }

    #[test]
    fn the_warning_names_every_dropped_override() {
        let text = warning(
            &[ignored("sparse_infil_density"), ignored("brim_widht")],
            Some(&config(json!({}), json!([]))),
            None,
        )
        .expect("a warning");

        assert!(
            text.starts_with("libslic3r does not recognise these settings and ignored them: "),
            "{text}"
        );
        assert!(text.contains("sparse_infil_density (unknown or obsolete key)"));
        assert!(text.contains("brim_widht (unknown or obsolete key)"));
    }

    /// Machine and process entries are collected without merging
    /// (`collect_config_overrides_with`), so one key can be dropped twice.
    #[test]
    fn a_key_dropped_twice_is_named_once() {
        let text = warning(
            &[
                ignored("sparse_infil_density"),
                ignored("sparse_infil_density"),
            ],
            Some(&config(json!({}), json!([]))),
            None,
        )
        .expect("a warning");

        assert_eq!(text.matches("sparse_infil_density").count(), 1, "{text}");
    }

    /// What the Slicer sends on every slice: the setting under PrusaSlicer's
    /// name and under Orca's, one value.
    #[test]
    fn an_alias_carrying_the_value_of_its_libslic3r_spelling_is_not_worth_a_warning() {
        let paired = config(json!({ "wall_loops": 3, "perimeters": 3 }), json!([]));

        assert_eq!(warning(&[ignored("perimeters")], Some(&paired), None), None);
    }

    /// Edited under the name libslic3r ignores: the edit is lost.
    #[test]
    fn an_alias_with_another_value_is_named() {
        let diverged = config(json!({ "wall_loops": 3, "perimeters": 4 }), json!([]));

        let text = warning(&[ignored("perimeters")], Some(&diverged), None).expect("a warning");
        assert!(text.contains("perimeters"), "{text}");
    }

    #[test]
    fn an_alias_alone_is_named() {
        let alone = config(json!({ "perimeters": 4 }), json!([]));

        assert!(warning(&[ignored("perimeters")], Some(&alone), None).is_some());
    }

    /// Filament keys come the other way round — the Slicer's own name is the
    /// one libslic3r does not know — and per slot: every slot must agree.
    #[test]
    fn a_filament_alias_loses_nothing_only_when_every_slot_agrees() {
        let slot = |strategy: &str| {
            json!({
                "filament_min_layer_time_strategy": "slowdown",
                "min_layer_time_strategy": strategy
            })
        };
        let agree = config(json!({}), json!([slot("slowdown"), slot("slowdown")]));
        let disagree = config(json!({}), json!([slot("slowdown"), slot("nothing")]));
        let dropped = [ignored("filament_min_layer_time_strategy")];

        assert_eq!(warning(&dropped, Some(&agree), None), None);
        assert!(warning(&dropped, Some(&disagree), None).is_some());
    }

    /// The daemon itself sends `bed_temperature*` flat, and libslic3r ignores
    /// both names; the bridge sets the plate temperatures from the slot DTO.
    #[test]
    fn a_flat_bed_temperature_is_not_worth_a_warning_when_the_slot_carries_one() {
        let dropped = [
            ignored("bed_temperature"),
            ignored("bed_temperature_initial_layer"),
        ];
        let resolved = config(json!({}), json!([{ "bed_temperature_normal": 60 }]));

        assert_eq!(
            warning(
                &dropped,
                Some(&resolved),
                Some(&bed_temperatures_on_the_slot())
            ),
            None
        );
        let text = warning(
            &dropped,
            Some(&resolved),
            Some(&IdexInputs::empty_for_count(1)),
        )
        .expect("without slot temperatures the flat ones are all there is");
        assert!(text.contains("bed_temperature (unknown"), "{text}");
        assert!(text.contains("bed_temperature_initial_layer"), "{text}");
    }

    /// The table's claim, held against libslic3r: it drops the name on the
    /// left and applies the one on the right. A pin that teaches libslic3r an
    /// alias, or forgets a spelling, fails here instead of going quiet.
    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn libslic3r_drops_each_alias_and_applies_its_spelling() {
        use slicer_ffi::describe_ignored_config_override as describe;
        for (alias, spelling) in SLICER_ALIASES {
            assert!(describe(alias, "1").is_some(), "libslic3r applies {alias}");
            assert_eq!(describe(spelling, "1"), None, "libslic3r drops {spelling}");
        }
    }
}
