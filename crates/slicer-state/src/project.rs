use slicer_types::*;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone)]
pub struct ProjectState {
    pub meta: Option<ProjectMeta>,
    pub presets: PresetState,
    pub objects: BTreeMap<ObjectId, ObjectRecord>,
    /// Live setting values keyed by (scope, key). Initialized from
    /// `SettingsSchema::baseline()` defaults; mutated by `SettingChanged`
    /// reducer arm.
    pub settings: HashMap<(SettingScope, SettingKey), SettingValue>,
    /// Preset library (vendor + user). Mutated by `PresetSaved` /
    /// `PresetDeleted` reducer arms; seeded with vendor presets on init.
    pub preset_library: HashMap<PresetId, Preset>,
    /// Adaptive Print rules attached to this project. Manual editor
    /// (Phase 5 Track A) populates these; the agent's rule engine
    /// fires them at runtime. Persisted in Protype/project.json.
    pub adaptive_rules: Vec<AdaptiveRule>,
    /// CAD parts by id. Each part's bodies are shown by objects in `objects`
    /// (`ObjectRecord::body` points back here).
    pub parts: BTreeMap<PartId, PartSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectRecord {
    /// Generator recipe (including exact formula/template). Absent on imports
    /// and projects created before generator metadata persistence.
    pub recipe: Option<serde_json::Value>,
    pub id: ObjectId,
    /// Display name the object was added under. Kept so that undoing a
    /// removal can put it back under the same name: a client drops an
    /// `ObjectAdded` without one.
    pub name: String,
    pub source_path: String,
    pub source_hash: String,
    pub triangle_count: u64,
    /// Mesh-local bounding box in millimeters. Sent by the UI in
    /// `object.add` and used by the slicer to compute the plate bbox so
    /// the layer count reflects the real model height instead of a
    /// hardcoded 20mm cube heuristic.
    pub bounds: Bounds,
    pub transform: [f32; 16],
    /// Per-object overrides for process-scope settings. Subset only; see
    /// `OVERRIDABLE_KEYS` in slicer-types.
    pub overrides: HashMap<SettingKey, SettingValue>,
    /// Base64-encoded Orca-compatible RLE seam-paint blob. `None` means no
    /// per-triangle seam paint has been applied to this object.
    pub seam_facets_rle: Option<String>,
    /// JSON-encoded seam target point `{x,y,z,facet_idx}`. `None` means no
    /// target point is set.
    pub seam_target_point: Option<String>,
    /// Base64-encoded Orca-compatible RLE support-paint blob. `None` means
    /// no per-triangle support paint has been applied to this object. Wire
    /// format is the same as `seam_facets_rle` so the same C++ decoder
    /// applies — values 0/1/2 map to none/enforcer/blocker respectively
    /// and feed `ModelVolume::supported_facets` on the libslic3r side.
    pub support_facets_rle: Option<String>,
    /// Base64 RLE fan-paint blob (`ProjectEvent::ObjectFanFacetsSet`). `None`
    /// means no facet of this object has a painted fan speed.
    pub fan_facets_rle: Option<String>,
    pub filament_facets_rle: Option<String>,
    /// Per-object variable-layer-height profile: a flat list of
    /// `[z0, h0, z1, h1, ...]` (z mm, height mm), ascending z. Maps 1:1
    /// onto libslic3r's `ModelObject::layer_height_profile.data`. Empty
    /// = uniform layer height.
    pub layer_height_profile: Vec<f32>,
    /// Prusa-style per-Z setting overrides. Default empty. Reaches
    /// libslic3r via `BridgeLayerConfigRange` (added in Phase 2 T8) and
    /// `ModelObject::layer_config_ranges` (T9).
    pub layer_ranges: Vec<slicer_types::project::LayerConfigRange>,
    /// Parametric feature-operation stack (Phase 2 CAD edit mode —
    /// currently cylindrical holes only). Default empty. Set wholesale by
    /// `object.set_operations`, which also materializes it via
    /// `apply_operations` (mcut boolean) into a content-addressed cache
    /// STL (`ops-<source_hash>-<ops_hash>.stl`); the slice reads that
    /// materialized geometry instead of `source_path` when non-empty.
    pub operations: Vec<slicer_types::FeatureOp>,
    /// STEP part index this record came from (`object.import_local`
    /// fan-out over a multi-solid STEP). `None` for STL/3MF and
    /// split-derived parts. Lets the slicer map each record to the right
    /// libslic3r ModelObject when N records share one `.stp` source_path.
    pub step_part_index: Option<u32>,
    /// Which part body this object shows; `None` for a plain object. Set by
    /// `ObjectGeometryUpdated`, never by `ObjectAdded`.
    pub body: Option<BodyRef>,
}

/// The name an object is shown under: the one it was added with, or, when
/// that is empty, its source file's name. A record saved before names were
/// kept has an empty one, and so does one added under an empty name.
pub(crate) fn display_name(name: &str, source_path: &str) -> String {
    if !name.is_empty() {
        return name.to_string();
    }
    std::path::Path::new(source_path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("object")
        .to_string()
}

impl ProjectState {
    /// The same project with every object override under its canonical key
    /// (`canonical_override_key`). A project file or autosave log can predate a
    /// rename; when both spellings are stored, the canonical one wins.
    pub fn with_canonical_override_keys(mut self) -> Self {
        for object in self.objects.values_mut() {
            object.overrides = canonical_overrides(std::mem::take(&mut object.overrides));
        }
        self
    }
}

impl ObjectRecord {
    /// The fields `ObjectGeometryUpdated` replaces, as they are now.
    pub fn geometry(&self) -> ObjectGeometry {
        ObjectGeometry {
            source_path: self.source_path.clone(),
            source_hash: self.source_hash.clone(),
            triangle_count: self.triangle_count,
            bounds: self.bounds.clone(),
            step_part_index: self.step_part_index,
            operations: self.operations.clone(),
            body: self.body.clone(),
        }
    }
}

fn canonical_overrides(
    overrides: HashMap<SettingKey, SettingValue>,
) -> HashMap<SettingKey, SettingValue> {
    let (legacy, mut canonical): (HashMap<_, _>, HashMap<_, _>) = overrides
        .into_iter()
        .partition(|(key, _)| canonical_override_key(key) != key.as_str());
    for (key, value) in legacy {
        canonical
            .entry(canonical_override_key(&key).to_string())
            .or_insert(value);
    }
    canonical
}

impl Default for ProjectState {
    fn default() -> Self {
        let schema = SettingsSchema::baseline();
        let mut settings = HashMap::new();
        for def in &schema.defs {
            settings.insert((def.scope, def.key.clone()), def.default.clone());
        }
        let mut preset_library = HashMap::new();
        for p in seed_vendor(&schema) {
            preset_library.insert(p.id.clone(), p);
        }
        Self {
            meta: None,
            presets: PresetState {
                active_printer: PresetId("cd400-default".into()),
                active_process: PresetId("standard".into()),
                active_filament: PresetId("cd400-pa-cf".into()),
            },
            objects: BTreeMap::new(),
            settings,
            preset_library,
            adaptive_rules: Vec::new(),
            parts: BTreeMap::new(),
        }
    }
}

/// Bound opaque JSON without interpreting or changing generator fields. The
/// same guard runs on incoming builds and project load. Iterative traversal
/// avoids recursion on client-supplied metadata.
pub fn validate_recipe(recipe: &serde_json::Value) -> Result<(), String> {
    const MAX_BYTES: usize = 1024 * 1024;
    const MAX_NODES: usize = 100_000;
    const MAX_DEPTH: usize = 64;
    if !recipe.is_object() {
        return Err("recipe must be a JSON object".into());
    }
    let mut pending = vec![(recipe, 0usize)];
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if depth > MAX_DEPTH || nodes > MAX_NODES || pending.len() > MAX_NODES {
            return Err("recipe exceeds 64 levels or 100000 JSON values".into());
        }
        match value {
            serde_json::Value::Object(fields) => {
                if fields.len() + pending.len() + nodes > MAX_NODES {
                    return Err("recipe exceeds 100000 JSON values".into());
                }
                bytes = bytes.saturating_add(fields.keys().map(String::len).sum::<usize>());
                pending.extend(fields.values().map(|v| (v, depth + 1)));
            }
            serde_json::Value::Array(values) => {
                if values.len() + pending.len() + nodes > MAX_NODES {
                    return Err("recipe exceeds 100000 JSON values".into());
                }
                pending.extend(values.iter().map(|v| (v, depth + 1)));
            }
            serde_json::Value::String(s) => bytes = bytes.saturating_add(s.len()),
            _ => {}
        }
        if bytes > MAX_BYTES {
            return Err("recipe exceeds 1 MiB".into());
        }
    }
    if serde_json::to_vec(recipe).map_err(|e| e.to_string())?.len() > MAX_BYTES {
        return Err("recipe exceeds 1 MiB".into());
    }
    Ok(())
}
