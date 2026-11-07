//! Project save/open. MVP scope: write a ZIP file with a `Protype/`
//! namespace per spec §6.7. We do NOT write `3D/3dmodel.model` — meshes
//! live on disk via source paths until libslic3r FFI lands and the daemon
//! owns canonical geometry. The file therefore won't open in OrcaSlicer
//! (bidirectional compat is in backlog), but Protype roundtrips cleanly.

use crate::project::{display_name, ObjectRecord, ProjectState};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use slicer_types::{
    AdaptiveRule, BodyRef, Bounds, FeatureOp, LayerConfigRange, ObjectId, PartSnapshot, Preset,
    PresetState, ProjectMeta, SettingKey, SettingScope, SettingValue,
};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::Path;

const PROTYPE_VERSION_FILE: &str = "Protype/version.json";
const PROTYPE_PROJECT_FILE: &str = "Protype/project.json";

/// Highest on-disk format this daemon reads. v2 = a project carrying CAD parts
/// or part bodies. A project with neither is still written as v1, so older
/// daemons keep opening everything they can open correctly; one with either is
/// written as v2, so they refuse it instead of silently dropping the parts and
/// the body bindings on their next save.
const SCHEMA_VERSION: u32 = 2;
/// Written for a project with no parts and no bodies, so older daemons keep opening it.
const SCHEMA_VERSION_WITHOUT_PARTS: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct VersionFile {
    schema_version: u32,
    produced_by: String,
    produced_at_unix_ms: i64,
    /// Identifier carried into telemetry once opt-in lands; `None` until then.
    machine_id_hash: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectFile {
    meta: Option<ProjectMeta>,
    presets: PresetState,
    objects: Vec<SerializedObject>,
    /// Settings stored as a flat list because tuple keys don't survive JSON.
    settings: Vec<SerializedSetting>,
    preset_library: Vec<Preset>,
    /// Adaptive Print rules attached to this project. Empty for projects
    /// saved by older builds — the deserializer treats it as `Vec::new()`.
    #[serde(default)]
    adaptive_rules: Vec<AdaptiveRule>,
    /// CAD parts. Absent from files written before parts existed.
    #[serde(default)]
    parts: Vec<PartSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedObject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recipe: Option<serde_json::Value>,
    id: ObjectId,
    /// Display name. Older project files predate it; `load` names such an
    /// object after its source file (see `display_name`).
    #[serde(default)]
    name: String,
    source_path: String,
    source_hash: String,
    triangle_count: u64,
    /// Mesh-local bbox in mm. Older project files saved before this
    /// landed are deserialized with a degenerate placeholder so
    /// loading doesn't error — they'll just have no slice bbox until
    /// re-imported.
    #[serde(default = "default_bounds")]
    bounds: Bounds,
    transform: [f32; 16],
    overrides: HashMap<SettingKey, SettingValue>,
    /// Base64-encoded Orca-compatible RLE seam-paint blob. Older project
    /// files predate seam paint, so `serde(default)` keeps them loadable.
    #[serde(default)]
    seam_facets_rle: Option<String>,
    /// JSON-encoded seam target point. `serde(default)` for back-compat.
    #[serde(default)]
    seam_target_point: Option<String>,
    /// Base64-encoded Orca-compatible RLE support-paint blob. Older
    /// project files predate support paint, so `serde(default)` keeps
    /// them loadable.
    #[serde(default)]
    support_facets_rle: Option<String>,
    /// Base64 RLE fan-paint blob. Older project files predate fan paint, so
    /// `serde(default)` keeps them loadable.
    #[serde(default)]
    fan_facets_rle: Option<String>,
    #[serde(default)]
    filament_facets_rle: Option<String>,
    /// Per-object variable-layer-height profile as a flat
    /// `[z, height, ...]` list (mm). Older project files predate this
    /// field; `serde(default)` keeps them loadable.
    #[serde(default)]
    layer_height_profile: Vec<f32>,
    /// Prusa-style per-Z setting overrides. Older project files predate
    /// this field; `serde(default)` keeps them loadable.
    #[serde(default)]
    layer_ranges: Vec<LayerConfigRange>,
    /// STEP part index (`object.import_local` fan-out over a multi-solid
    /// STEP). Older project files predate this field; `serde(default)`
    /// keeps them loadable.
    #[serde(default)]
    step_part_index: Option<u32>,
    /// Parametric feature-operation stack (Phase 2 CAD edit mode). Older
    /// project files predate this field; `serde(default)` keeps them
    /// loadable — they open with an empty op stack, same as an object that
    /// never had one.
    #[serde(default)]
    operations: Vec<FeatureOp>,
    /// Which part body this object shows. Absent from older files.
    #[serde(default)]
    body: Option<BodyRef>,
}

fn default_bounds() -> Bounds {
    Bounds {
        min: [0.0, 0.0, 0.0],
        max: [0.0, 0.0, 0.0],
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedSetting {
    scope: SettingScope,
    key: SettingKey,
    value: SettingValue,
}

/// The format `project` is written as: v2 when it carries a part or an object
/// bound to a part body, which is what a v1 daemon would drop on its next save,
/// v1 otherwise.
fn schema_version_for(project: &ProjectState) -> u32 {
    let carries_parts =
        !project.parts.is_empty() || project.objects.values().any(|o| o.body.is_some());
    if carries_parts {
        SCHEMA_VERSION
    } else {
        SCHEMA_VERSION_WITHOUT_PARTS
    }
}

/// Write the project state into a .3mf-style ZIP at `path`. Atomic via
/// write-then-rename so a crash mid-write doesn't corrupt the destination.
pub fn save(path: &Path, project: &ProjectState) -> Result<()> {
    let tmp = tmp_alongside(path);
    {
        let f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let mut zip = zip::ZipWriter::new(f);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        // Protype/version.json
        let version = VersionFile {
            schema_version: schema_version_for(project),
            produced_by: format!("protype-slicer-daemon/{}", env!("CARGO_PKG_VERSION")),
            produced_at_unix_ms: chrono::Utc::now().timestamp_millis(),
            machine_id_hash: None,
        };
        zip.start_file(PROTYPE_VERSION_FILE, opts)?;
        zip.write_all(&serde_json::to_vec_pretty(&version)?)?;

        // Protype/project.json
        let project_file = ProjectFile {
            meta: project.meta.clone(),
            presets: project.presets.clone(),
            objects: project
                .objects
                .values()
                .map(|o| SerializedObject {
                    recipe: o.recipe.clone(),
                    id: o.id.clone(),
                    name: o.name.clone(),
                    source_path: o.source_path.clone(),
                    source_hash: o.source_hash.clone(),
                    triangle_count: o.triangle_count,
                    bounds: o.bounds.clone(),
                    transform: o.transform,
                    overrides: o.overrides.clone(),
                    seam_facets_rle: o.seam_facets_rle.clone(),
                    seam_target_point: o.seam_target_point.clone(),
                    support_facets_rle: o.support_facets_rle.clone(),
                    fan_facets_rle: o.fan_facets_rle.clone(),
                    filament_facets_rle: o.filament_facets_rle.clone(),
                    layer_height_profile: o.layer_height_profile.clone(),
                    layer_ranges: o.layer_ranges.clone(),
                    step_part_index: o.step_part_index,
                    operations: o.operations.clone(),
                    body: o.body.clone(),
                })
                .collect(),
            settings: project
                .settings
                .iter()
                .map(|((scope, key), value)| SerializedSetting {
                    scope: *scope,
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
            preset_library: project.preset_library.values().cloned().collect(),
            adaptive_rules: project.adaptive_rules.clone(),
            parts: project.parts.values().cloned().collect(),
        };
        zip.start_file(PROTYPE_PROJECT_FILE, opts)?;
        zip.write_all(&serde_json::to_vec_pretty(&project_file)?)?;

        zip.finish()?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn tmp_alongside(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("project"));
    name.push(".tmp");
    path.with_file_name(name)
}

/// Read a Protype-format .3mf back into a ProjectState. Schema migrations
/// run between read and reconstruction so older files keep opening.
pub fn load(path: &Path) -> Result<ProjectState> {
    let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(f)?;

    let version: VersionFile = read_json(&mut zip, PROTYPE_VERSION_FILE)
        .context("missing or invalid Protype/version.json — not a Protype project")?;

    if version.schema_version > SCHEMA_VERSION {
        return Err(anyhow!(
            "project written by a newer Protype Slicer (schema v{}); current daemon supports v{}",
            version.schema_version,
            SCHEMA_VERSION
        ));
    }
    // Future: dispatch migrate_v1_to_v2(json) etc here.

    let project_file: ProjectFile = read_json(&mut zip, PROTYPE_PROJECT_FILE)
        .context("missing or invalid Protype/project.json")?;

    let mut state = ProjectState {
        meta: project_file.meta,
        presets: project_file.presets,
        objects: BTreeMap::new(),
        settings: HashMap::new(),
        preset_library: HashMap::new(),
        adaptive_rules: project_file.adaptive_rules,
        parts: project_file
            .parts
            .into_iter()
            .map(|part| (part.part_id.clone(), part))
            .collect(),
    };
    for o in project_file.objects {
        let name = display_name(&o.name, &o.source_path);
        if let Some(recipe) = &o.recipe {
            crate::project::validate_recipe(recipe).map_err(|e| anyhow!(e))?;
        }
        state.objects.insert(
            o.id.clone(),
            ObjectRecord {
                recipe: o.recipe,
                id: o.id,
                name,
                source_path: o.source_path,
                source_hash: o.source_hash,
                triangle_count: o.triangle_count,
                bounds: o.bounds,
                transform: o.transform,
                overrides: o.overrides,
                seam_facets_rle: o.seam_facets_rle,
                seam_target_point: o.seam_target_point,
                support_facets_rle: o.support_facets_rle,
                fan_facets_rle: o.fan_facets_rle,
                filament_facets_rle: o.filament_facets_rle,
                layer_height_profile: o.layer_height_profile,
                layer_ranges: o.layer_ranges,
                step_part_index: o.step_part_index,
                operations: o.operations,
                body: o.body,
            },
        );
    }
    for s in project_file.settings {
        state.settings.insert((s.scope, s.key), s.value);
    }
    for p in project_file.preset_library {
        state.preset_library.insert(p.id.clone(), p);
    }
    Ok(state)
}

fn read_json<R: Read + std::io::Seek, T: for<'de> Deserialize<'de>>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<T> {
    let mut f = zip
        .by_name(name)
        .with_context(|| format!("entry {name} not found in archive"))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectState;
    use slicer_types::{ObjectId, SettingScope, SettingValue};

    #[test]
    fn save_then_load_round_trip() {
        let mut original = ProjectState::default();
        // Mutate some values so we can detect the roundtrip.
        original.settings.insert(
            (SettingScope::Process, "layer_height".into()),
            SettingValue::Number(0.42),
        );
        original.objects.insert(
            ObjectId("obj-x".into()),
            ObjectRecord {
                recipe: None,
                id: ObjectId("obj-x".into()),
                name: String::new(),
                source_path: "/tmp/foo.stl".into(),
                source_hash: "abc".into(),
                triangle_count: 99,
                bounds: Bounds {
                    min: [-10.0, -10.0, 0.0],
                    max: [10.0, 10.0, 20.0],
                },
                transform: [
                    1.0, 0.0, 0.0, 0.0, //
                    0.0, 1.0, 0.0, 0.0, //
                    0.0, 0.0, 1.0, 0.0, //
                    50.0, 25.0, 0.0, 1.0,
                ],
                overrides: HashMap::new(),
                seam_facets_rle: Some("AAEC".to_string()),
                seam_target_point: Some(
                    "{\"x\":1.0,\"y\":2.0,\"z\":3.0,\"facet_idx\":42}".to_string(),
                ),
                support_facets_rle: None,
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                step_part_index: None,
                operations: Vec::new(),
                body: None,
            },
        );

        let dir = tempdir();
        let path = dir.join("test.3mf");
        save(&path, &original).unwrap();
        let loaded = load(&path).unwrap();

        assert_eq!(loaded.objects.len(), 1);
        assert_eq!(
            loaded
                .settings
                .get(&(SettingScope::Process, "layer_height".to_string())),
            Some(&SettingValue::Number(0.42))
        );
        let obj = loaded.objects.get(&ObjectId("obj-x".into())).unwrap();
        assert_eq!(obj.triangle_count, 99);
        assert_eq!(obj.seam_facets_rle.as_deref(), Some("AAEC"));
        assert_eq!(
            obj.seam_target_point.as_deref(),
            Some("{\"x\":1.0,\"y\":2.0,\"z\":3.0,\"facet_idx\":42}")
        );

        std::fs::remove_dir_all(dir).ok();
    }

    fn tempdir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "protype-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
