use crate::build_info;
use crate::error::RpcError;
use crate::estimate::SlicedObjects;
use crate::mesh::{MeshFile, ObjectMeshes};
use crate::methods::{capabilities, METHODS};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use slicer_ffi::gcode_header;
use slicer_ffi::CancelHandle;
use slicer_ffi::OptionJoinKind;
use slicer_state::{ObjectRecord, StateLayer};
use slicer_types::{
    canonical_override_key, is_overridable, object_override_constraint, AdaptiveRule, Bounds,
    ObjectId, Preset, PresetId, PresetKind, PresetSource, ProjectEvent, ProjectMeta,
    ResolvedExtruderConfig, ResolvedRoleMapping, ResolvedSliceConfig, SettingConstraint,
    SettingKey, SettingScope, SettingValue, SettingsSchema,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tracing;
use ulid::Ulid;

/// Bookkeeping for the most recently completed slice. Used by
/// `project.export_gcode` to look up the daemon-side G-code path
/// without trusting client-supplied paths (defence in depth — keeps the
/// RPC from being abused as a generic file-copy primitive).
#[derive(Clone, Debug)]
pub struct LastSliceInfo {
    pub job_id: String,
    pub gcode_path: String,
    pub suggested_filename: String,
}

#[derive(Clone)]
pub struct Dispatcher {
    pub state: StateLayer,
    /// In-flight slice jobs keyed by job_id, holding their cancel
    /// handles. Inserted by `slice.request` immediately before spawning
    /// the slicer task; flipped by `slice.cancel`; removed by the
    /// slicer task itself when it finishes (success / fail / canceled).
    /// Wrapped in `Arc<StdMutex<…>>` so Dispatcher stays `Clone` and so
    /// the spawned slicer task can take its handle out without going
    /// through `&mut self`.
    pub cancel_tokens: Arc<StdMutex<HashMap<String, CancelHandle>>>,
    /// Latest successful slice. Updated by the slicer task on
    /// SliceCompleted; read by `project.export_gcode`. Reset to None
    /// only at process start — we keep stale entries around so a user
    /// can re-export the last slice even after starting a fresh one
    /// that fails (the failure leaves the previous successful slice in
    /// place, which is what they'd expect from any other tool).
    pub last_slice: Arc<StdMutex<Option<LastSliceInfo>>>,
}

impl Dispatcher {
    pub fn new(state: StateLayer) -> Self {
        Self {
            state,
            cancel_tokens: Arc::new(StdMutex::new(HashMap::new())),
            last_slice: Arc::new(StdMutex::new(None)),
        }
    }
}

#[derive(Deserialize)]
struct ProjectNewParams {
    title: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct PresetActivateParams {
    kind: PresetKind,
    preset_id: PresetId,
}

#[derive(Deserialize, Default)]
struct EventsSinceParams {
    #[serde(default)]
    since: u64,
}

#[derive(Deserialize)]
struct ObjectAddParams {
    name: String,
    triangle_count: u64,
    bounds: Bounds,
    /// SHA-256 hex of the canonical mesh encoding (vertices+indices). The mesh
    /// itself is uploaded out-of-band via streaming binary frames in production;
    /// for Phase 1, only the metadata is sent.
    source_hash: String,
    /// Absolute filesystem path to the source mesh on the daemon-visible disk.
    /// The FFI bridge needs this to actually read the STL/3MF. Optional for
    /// backward compatibility with older UI builds — empty string means "no
    /// path known", and slicing such an object now FAILS in the libslic3r
    /// build ("object has no mesh path"). It used to come back as the bridge's
    /// rectangle stub reported as a successful slice; see
    /// `tests/slice_missing_source_e2e.rs`.
    #[serde(default)]
    source_path: String,
}

#[derive(Deserialize)]
struct ObjectImportLocalParams {
    source_path: String,
}

#[derive(Deserialize)]
struct ObjectRemoveParams {
    object_id: ObjectId,
}

#[derive(Deserialize)]
struct ObjectSplitToPartsParams {
    object_id: ObjectId,
}

#[derive(Deserialize)]
struct ObjectQueryFacesParams {
    object_id: ObjectId,
}

#[derive(Deserialize)]
struct ObjectGetEditMeshParams {
    object_id: ObjectId,
}

#[derive(Deserialize)]
struct ObjectGetMaterializedMeshParams {
    object_id: ObjectId,
}

#[derive(Deserialize, Default)]
struct SliceRequestParams {
    #[serde(default)]
    plate_id: Option<u32>,
    #[serde(default)]
    traceability: Option<crate::serial::SerialSlice>,
    #[serde(default)]
    idex: Option<slicer_ffi::IdexInputs>,
    #[serde(default)]
    resolved_config: Option<ResolvedSliceConfig>,
    /// Part fan spin-up and spin-down times for the paint fan lead pass.
    /// The agent strips these `agent_only` machine keys from
    /// `resolved_config`, so the window reads them off the resolved profile
    /// and sends them here. Absent = the printer defaults, 1 s and 2 s.
    #[serde(default)]
    fan_lead: Option<slicer_gcode::FanLeadParams>,
    /// A calibration test (retraction / pressure-advance / outer-wall speed
    /// by print height) the slice builds into the G-code. Absent = a plain
    /// slice.
    #[serde(default)]
    calibration_test: Option<slicer_ffi::CalibrationTest>,
    /// Line written under the generation banner of the G-code header. Must
    /// start with `Protype Slicer `. Absent = the engine's own line.
    #[serde(default)]
    producer_line: Option<String>,
    /// Scene snapshot (PNG, optionally a data URL) the G-code thumbnails
    /// are made from, with its size. Absent or empty = no thumbnails.
    #[serde(default)]
    preview_png_base64: Option<String>,
    #[serde(default)]
    preview_width: Option<u32>,
    #[serde(default)]
    preview_height: Option<u32>,
    /// The machine is a Bambu one and keeps the Bambu keys of the built-in
    /// preset.
    #[serde(default)]
    is_bbl_machine: bool,
}

#[derive(Deserialize)]
struct SettingSetParams {
    scope: SettingScope,
    key: SettingKey,
    value: SettingValue,
}

#[derive(Deserialize)]
struct SettingGetParams {
    scope: SettingScope,
    key: SettingKey,
}

#[derive(Deserialize, Default)]
struct PresetListParams {
    #[serde(default)]
    scope: Option<SettingScope>,
}

#[derive(Deserialize)]
struct PresetSaveParams {
    name: String,
    scope: SettingScope,
    /// Optional explicit value bundle. If omitted, the daemon snapshots the
    /// current `state.settings` for this scope and uses that.
    #[serde(default)]
    values: Option<HashMap<SettingKey, SettingValue>>,
    /// Optional parent preset id for traceability.
    #[serde(default)]
    derived_from: Option<PresetId>,
    /// If true (default), activate the new preset immediately.
    #[serde(default = "default_true")]
    activate: bool,
}

fn default_true() -> bool {
    true
}

/// Case-insensitive check for STEP/STP source-path extension. Used by
/// `object.import_local` to decide whether to attach a `step_part`
/// payload to each emitted `ObjectAdded` event — for STL/3MF imports
/// `step_part` stays `None` and is omitted from the JSON envelope. Also
/// used by `object.query_faces` and `object.get_edit_mesh` to decide
/// whether an object has a derived STEP edit mesh / B-rep face map to
/// materialize at all, vs. just echoing the source path back untouched.
pub(crate) fn is_step_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".step") || lower.ends_with(".stp")
}

/// Every distinct `object_id` in a params tree, in the order met. See
/// `Dispatcher::measured_meshes`, which is the only caller and explains why
/// the search is recursive.
fn collect_object_ids(params: &Value, out: &mut Vec<String>) {
    match params {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("object_id") {
                if !out.contains(id) {
                    out.push(id.clone());
                }
            }
            for value in map.values() {
                collect_object_ids(value, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_object_ids(item, out)),
        _ => {}
    }
}

/// Cache paths for the tessellated STL + facemap JSON that
/// `ensure_step_edit_mesh` derives from one part of a STEP object,
/// shared by `object.query_faces` and `object.get_edit_mesh`. Lives
/// under `cache_root` (callers pass `slicer_state::object_cache_dir()` —
/// the same root `object.split_to_parts` uses for its derived part STLs,
/// see that arm below), keyed by `source_hash` (content-addressed, so a
/// re-import of the same bytes reuses the cache) and `part_index` (which
/// STEP part within that source — `None` collapses to `0`, matching the
/// `-1 → 0` normalization `query_model_faces` does on the FFI side for
/// non-STEP/legacy callers).
///
/// Pure path computation — does not touch the filesystem or create the
/// directory; callers are responsible for that (mirrors
/// `slicer_state::paths::object_cache_dir`'s contract).
fn edit_mesh_cache_paths(
    cache_root: &std::path::Path,
    source_hash: &str,
    part_index: Option<u32>,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let stem = format!("editmesh-{}-{}", source_hash, part_index.unwrap_or(0));
    (
        cache_root.join(format!("{stem}.stl")),
        cache_root.join(format!("{stem}.facemap.json")),
    )
}

/// Cache path for what `object.query_faces` found on the STL at `source_path`,
/// or `None` when the file is not there to be stamped. The window asks on every
/// selection of a part, which is why the answer is kept: growing the faces of a
/// large scan takes seconds.
///
/// Keyed by the file as it stands, not by `source_hash`: the window's
/// `object.add` makes that hash from a name, a triangle count and bounds, so
/// two different STLs can share one, and the second would be answered with the
/// first one's faces for good. `v1` versions the search and its answer's shape.
fn mesh_faces_cache_path(
    cache_root: &std::path::Path,
    source_path: &str,
) -> Option<std::path::PathBuf> {
    let file = std::fs::metadata(source_path).ok()?;
    let stamp = format!("{source_path}\n{}\n{:?}", file.len(), file.modified().ok()?);
    let key = hex::encode(Sha256::digest(stamp.as_bytes()));
    Some(cache_root.join(format!("meshfaces-v1-{key}.json")))
}

/// The largest STL `object.query_faces` searches: 3 million triangles at 50
/// bytes each. The window waits for this answer before it shows a part, and
/// gives up after 30 s, which would cost it the part's whole edit mesh over
/// faces it does not read. A scan this size has no flat faces worth listing.
const MAX_FACE_SEARCH_BYTES: u64 = 150_000_000;

fn is_stl_extension(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("stl"))
}

/// `object.query_faces` for a source with no B-rep. Never an error: the window
/// asks on every selection, and a file it cannot read is a part with no faces
/// to show, said in the log.
async fn mesh_face_report(obj: &ObjectRecord) -> Value {
    let nothing = json!({ "source": "none", "faces": [] });
    if !is_stl_extension(&obj.source_path) {
        return nothing;
    }
    let file_bytes = std::fs::metadata(&obj.source_path).map_or(0, |file| file.len());
    if file_bytes > MAX_FACE_SEARCH_BYTES {
        return json!({
            "source": "none",
            "faces": [],
            "reason": format!(
                "the mesh is {file_bytes} bytes, over the {MAX_FACE_SEARCH_BYTES} the face search \
                 takes"
            ),
        });
    }
    let cached = slicer_state::object_cache_dir()
        .ok()
        .and_then(|root| mesh_faces_cache_path(&root, &obj.source_path));
    let hit = cached
        .as_ref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    if let Some(report) = hit {
        return report;
    }
    let source_path = obj.source_path.clone();
    let report =
        tokio::task::spawn_blocking(move || crate::mesh::planar_face_report(&source_path)).await;
    match report {
        Ok(Ok(report)) => {
            if let Some(path) = cached {
                keep_mesh_faces(&path, &report);
            }
            report
        }
        Ok(Err(why)) => {
            tracing::warn!(source = %obj.source_path, %why, "cannot read the faces of an STL");
            nothing
        }
        Err(why) => {
            tracing::warn!(source = %obj.source_path, %why, "the face search did not finish");
            nothing
        }
    }
}

/// Keep a face report for the next selection, through a private file renamed
/// into place like every other cache entry. A cache that cannot be written
/// costs the next selection a search, not this one its answer.
fn keep_mesh_faces(path: &std::path::Path, report: &Value) {
    let staging = staging_path(path);
    let kept = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&staging, report.to_string()))
        .and_then(|()| std::fs::rename(&staging, path));
    if let Err(why) = kept {
        let _ = std::fs::remove_file(&staging);
        tracing::warn!(path = %path.display(), %why, "cannot keep the faces of an STL");
    }
}

/// Read and validate a facemap JSON written by
/// `slicer_ffi::query_model_faces` (`{"faces":[...]}`). Returns the
/// `faces` array on success, or a caller-facing description of what is
/// wrong (unreadable file, invalid JSON, missing/non-array `faces` —
/// e.g. a write truncated by a killed process). Callers decide whether
/// a bad facemap is a hard error (freshly generated) or a cache miss
/// worth regenerating (pre-existing file).
fn read_facemap_faces(map_path: &std::path::Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(map_path).map_err(|e| format!("facemap read: {e}"))?;
    let mut map: Value = serde_json::from_str(&text).map_err(|e| format!("facemap parse: {e}"))?;
    match map.get_mut("faces") {
        Some(faces @ Value::Array(_)) => Ok(faces.take()),
        _ => Err("facemap parse: missing `faces` array".to_string()),
    }
}

/// Whether the edit-mesh cache entry at `stl_path` + `map_path` is usable:
/// both files exist and the facemap parses. A facemap that does not parse
/// (e.g. one a daemon from before these writes were staged left truncated
/// when it was killed) counts as a miss, so the entry is tessellated again —
/// self-healing rather than a permanently poisoned cache entry.
fn edit_mesh_cached(stl_path: &std::path::Path, map_path: &std::path::Path) -> bool {
    stl_path.exists() && map_path.exists() && read_facemap_faces(map_path).is_ok()
}

/// The tessellated STL of the STEP at `source_path`, whose content hashes to
/// `source_hash`: the cache entry a STEP object of that content has, made here
/// when it is missing. For callers already on a blocking thread, which is where
/// `Dispatcher::ensure_step_edit_mesh` goes for the same thing.
pub(crate) fn step_edit_mesh(
    source_path: &str,
    source_hash: &str,
    cache_root: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let (stl_path, map_path) = edit_mesh_cache_paths(cache_root, source_hash, None);
    // See STEP_FFI_LOCK: OCCT's STEP reader is not thread-safe. Held across the
    // check, as in `ensure_step_edit_mesh`, so that two cold calls tessellate
    // once.
    let _step_lock = STEP_FFI_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !edit_mesh_cached(&stl_path, &map_path) {
        tessellate_edit_mesh(source_path, -1, &stl_path, &map_path)?;
    }
    Ok(stl_path)
}

/// Tessellate one part of a STEP source into the edit-mesh cache entry at
/// `stl_path` + `map_path`. The FFI writes both files to private staging
/// paths that are then renamed into place, the facemap last: a cache path
/// only ever names a whole file, and a facemap in place means its mesh is
/// in place too. A caller already reading the entry keeps the files it
/// opened, and a write that fails or is killed leaves at worst a stray
/// `.tmp`, never a truncated file at a cache path. Crosses into OCCT, so
/// the caller holds `STEP_FFI_LOCK`.
fn tessellate_edit_mesh(
    source_path: &str,
    part: i32,
    stl_path: &std::path::Path,
    map_path: &std::path::Path,
) -> Result<(), String> {
    let staged_stl = staging_path(stl_path);
    let staged_map = staging_path(map_path);
    let outcome = slicer_ffi::query_model_faces(
        source_path,
        part,
        &staged_stl.to_string_lossy(),
        &staged_map.to_string_lossy(),
    );
    let placed = if outcome.success {
        std::fs::rename(&staged_stl, stl_path)
            .and_then(|()| std::fs::rename(&staged_map, map_path))
            .map_err(|e| format!("cache write: {e}"))
    } else {
        Err(outcome.error_message)
    };
    if placed.is_err() {
        let _ = std::fs::remove_file(&staged_stl);
        let _ = std::fs::remove_file(&staged_map);
    }
    placed
}

/// A private path beside `target` to write it under before renaming it into
/// place: `<name>.<ulid>.tmp`, fresh per call, so two writers of one cache
/// entry never share a file.
fn staging_path(target: &std::path::Path) -> std::path::PathBuf {
    target.with_file_name(format!(
        "{}.{}.tmp",
        target.file_name().unwrap_or_default().to_string_lossy(),
        Ulid::new()
    ))
}

/// Cache path for the mesh `object.set_operations` materializes via
/// `apply_operations`. Content-addressed by the object's source bytes
/// (`source_hash`, same content-hash `object.import_local` computes), WHICH
/// part of that source (`part_index` — same discriminator and same
/// `unwrap_or(0)` normalization `edit_mesh_cache_paths` uses, since a
/// multi-solid STEP fan-out gives every sibling object the same
/// `source_hash`; see `object.import_local`), and the op stack that
/// produced this mesh (`ops_hash`, see `ops_hash` below) — so re-importing
/// identical source bytes, or re-setting an identical op list, reuses the
/// same file instead of re-running mcut, while distinct STEP parts of one
/// import never collide down to `ops_hash` alone. Lives under `cache_root`
/// (same root `edit_mesh_cache_paths` and `object.split_to_parts` use).
/// Pure path computation, mirroring `edit_mesh_cache_paths`'s contract: no
/// filesystem access, no directory creation.
///
/// The `v2` token versions the cache FORMAT independent of its inputs:
/// `apply_operations`'s cutter geometry is allowed to change (e.g. 776d00e,
/// which fixed the through-cutter's positioning) without touching
/// `ops_hash`'s inputs at all, which would otherwise let an already-cached
/// STL from before such a fix keep being served as a hit after the fix
/// lands. Bump this token whenever `apply_operations`'s output for the same
/// (source, part, ops) triple can change.
pub(crate) fn ops_cache_path(
    cache_root: &std::path::Path,
    source_hash: &str,
    part_index: Option<u32>,
    ops_hash: &str,
) -> std::path::PathBuf {
    cache_root.join(format!(
        "ops-v2-{source_hash}-{}-{ops_hash}.stl",
        part_index.unwrap_or(0)
    ))
}

/// An op a cut left out, on the wire and in the sidecar: `apply_operations`'
/// `skipped_ops`. `code` is for a program to branch on, `reason` for whoever
/// has to aim the op again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SkippedOp {
    pub(crate) id: String,
    code: String,
    reason: String,
}

impl From<slicer_ffi::SkippedOp> for SkippedOp {
    fn from(op: slicer_ffi::SkippedOp) -> Self {
        Self {
            id: op.id,
            code: op.code,
            reason: op.reason,
        }
    }
}

/// Sidecar next to a materialized ops cache file, naming the ops the cut left
/// out and why (`apply_operations`' `skipped_ops`). The FFI names them only on
/// the call that cuts; a cache hit runs none, and without the sidecar it would
/// report the stack clean — the window's "not applied" mark would vanish
/// while the part still prints without the op. Same basename,
/// `.skipped.json` appended.
fn ops_skipped_sidecar_path(cache_file: &std::path::Path) -> std::path::PathBuf {
    let mut name = cache_file.as_os_str().to_os_string();
    name.push(".skipped.json");
    std::path::PathBuf::from(name)
}

/// The ops a cached cut left out, or `None` when `cache_file` is no whole
/// cache entry: no cut on disk, or no readable sidecar beside it. A cut made
/// before the sidecar was kept cannot say what it left out, so it is cut
/// again rather than reported clean.
fn cached_cut_skips(cache_file: &std::path::Path) -> Option<Vec<SkippedOp>> {
    if !cache_file.exists() {
        return None;
    }
    let body = std::fs::read(ops_skipped_sidecar_path(cache_file)).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Write the sidecar of `cache_file` through a private file renamed into
/// place, as the cut itself is.
fn write_skipped_sidecar(
    cache_file: &std::path::Path,
    skipped_ops: &[SkippedOp],
) -> std::io::Result<()> {
    let sidecar = ops_skipped_sidecar_path(cache_file);
    let staging = sidecar.with_file_name(format!(
        "{}.{}.tmp",
        sidecar.file_name().unwrap_or_default().to_string_lossy(),
        Ulid::new()
    ));
    let written = serde_json::to_vec(skipped_ops)
        .map_err(std::io::Error::other)
        .and_then(|body| std::fs::write(&staging, body))
        .and_then(|()| std::fs::rename(&staging, &sidecar));
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    written
}

/// SHA-256 hex digest of the operations list's canonical JSON encoding.
/// `serde_json::to_vec` on a `Vec<FeatureOp>` is deterministic (struct
/// field order follows declaration order, array order is preserved), so
/// the same op list — same ids, same values, same order — always hashes
/// identically. This is the `ops_hash` half of the `ops_cache_path` key:
/// an unchanged op list is a cache hit and never touches the FFI.
pub(crate) fn ops_hash(operations: &[slicer_types::FeatureOp]) -> String {
    let canon = serde_json::to_vec(operations).unwrap_or_default();
    hex::encode(Sha256::digest(&canon))
}

/// Slice input path for an object: the ops-cache STL its stored op stack
/// cuts, or the untouched source when no op is active — the same mesh
/// `object.get_materialized_mesh` reports. `fake_slice` resolves every
/// object through this while building `objects_for_ffi`.
///
/// A cut the ops cache no longer holds is cut again here: a project opened
/// on another machine, a cleared cache, a bumped `ops-v2` token or a
/// `FeatureOp` encoding change that re-keys `ops_hash` all name a cut
/// nobody has made yet. The window re-reads cuts when it opens a project,
/// but the MCP server slices the daemon's project straight away, and a
/// slice can race the window's recut (the in-lock recheck in
/// `materialize_operations` makes the loser reuse the winner's file). A
/// stack that cannot be cut is an error, never a fallback to the source:
/// the part would print without its holes and nothing would say so.
pub(crate) async fn resolve_slice_input_path(obj: &ObjectRecord) -> Result<String, RpcError> {
    printed_mesh_path("slice", obj).await
}

/// What `obj` prints as: `resolve_slice_input_path` for a call other than a
/// slice, named by `context` in errors. `mesh.*`, `verify.*` and
/// `slice.estimate` resolve an `object_id` through it, so they measure the
/// part a slice prints — the same cut, cut again when the cache lost it.
async fn printed_mesh_path(context: &str, obj: &ObjectRecord) -> Result<String, RpcError> {
    Ok(materialize_operations(context, obj, &obj.operations)
        .await?
        .path)
}

/// Every object's slice input (`resolve_slice_input_path`), in the
/// `snap.objects` order `objects_for_ffi` is built in. The error names the
/// first object whose stack could not be cut: the slice cannot go on
/// without it.
async fn resolve_slice_input_paths(
    snap: &slicer_state::project::ProjectState,
) -> Result<Vec<String>, String> {
    let mut paths = Vec::with_capacity(snap.objects.len());
    for obj in snap.objects.values() {
        let path = resolve_slice_input_path(obj)
            .await
            .map_err(|e| cannot_cut(obj, e))?;
        if path != obj.source_path && obj.filament_facets_rle.is_some() {
            return Err(format!("{}: material paint belongs to the original mesh; clear paint or suppress geometry operations before slicing", obj.name));
        }
        paths.push(path);
    }
    Ok(paths)
}

/// Why `obj` cannot be taken as it prints: its stack would not cut. What
/// every caller that cannot go on without the cut reports, naming the object.
fn cannot_cut(obj: &ObjectRecord, e: RpcError) -> String {
    format!(
        "object {}: cannot cut its operations: {}",
        obj.id.0,
        inner_text(e)
    )
}

/// An error's text without its kind prefix, for wrapping it in another error
/// of the same kind, which adds the prefix again.
fn inner_text(e: RpcError) -> String {
    match e {
        RpcError::Project(text) => text,
        other => other.to_string(),
    }
}

/// The mesh an op stack cuts, as `object.set_operations` and `object.
/// get_materialized_mesh` report it.
pub(crate) struct MaterializedMesh {
    /// What slicing reads for the stack: the ops-cache STL, or the untouched
    /// source when no op is active.
    pub(crate) path: String,
    /// Ops the FFI applied on THIS call. 0 on a cache hit: no FFI ran, so
    /// nothing was newly applied by the call. The file itself is unchanged
    /// from whichever earlier call produced it.
    applied_count: u32,
    /// Ops the cut left out, and why. A property of the stack, not of the
    /// call: a cache hit replays them from the sidecar (`cached_cut_skips`).
    /// `skipped_count` on the wire is their number, `skipped_op_ids` their
    /// ids, which is all a window that only marks rows needs.
    pub(crate) skipped_ops: Vec<SkippedOp>,
}

impl MaterializedMesh {
    fn untouched_source(obj: &ObjectRecord) -> Self {
        Self::cached(obj.source_path.clone(), Vec::new())
    }

    /// A mesh already on disk: no FFI ran for it on this call.
    fn cached(path: String, skipped_ops: Vec<SkippedOp>) -> Self {
        Self {
            path,
            applied_count: 0,
            skipped_ops,
        }
    }

    fn into_json(self) -> Value {
        let ids: Vec<&str> = self.skipped_ops.iter().map(|op| op.id.as_str()).collect();
        json!({
            "materialized_path": self.path,
            "applied_count": self.applied_count,
            "skipped_count": ids.len(),
            "skipped_op_ids": ids,
            "skipped_ops": self.skipped_ops,
        })
    }
}

/// Cut `operations` into `obj`'s source through `apply_operations`, unless
/// an identical stack already did (`ops_cache_path` is content-addressed),
/// and return the mesh slicing reads for it — the same file
/// `resolve_slice_input_path` resolves once the stack is `obj`'s.
///
/// Writes the ops cache only: the cut, and beside it the ops the cut left
/// out (`write_skipped_sidecar`). Recording the stack in the project is the
/// caller's decision: `object.set_operations` does, `object.
/// get_materialized_mesh` and a slice (`resolve_slice_input_path`) must not.
/// `context` names the caller in errors.
pub(crate) async fn materialize_operations(
    context: &str,
    obj: &ObjectRecord,
    operations: &[slicer_types::FeatureOp],
) -> Result<MaterializedMesh, RpcError> {
    // `all()` on an empty slice is vacuously true, so this also covers an
    // empty stack — no active op means slicing reads the untouched source,
    // same as an object that never had an op stack.
    if operations.iter().all(|o| o.suppressed) {
        return Ok(MaterializedMesh::untouched_source(obj));
    }
    let cache_root = slicer_state::object_cache_dir()
        .map_err(|e| RpcError::Project(format!("{context}: {e}")))?;
    let out = ops_cache_path(
        &cache_root,
        &obj.source_hash,
        obj.step_part_index,
        &ops_hash(operations),
    );
    let path = out.to_string_lossy().into_owned();
    if let Some(skipped_ops) = cached_cut_skips(&out) {
        return Ok(MaterializedMesh::cached(path, skipped_ops));
    }
    std::fs::create_dir_all(&cache_root)
        .map_err(|e| RpcError::Project(format!("{context}: cache dir: {e}")))?;
    let ops_json = serde_json::json!({ "ops": operations }).to_string();
    let source_path = obj.source_path.clone();
    let part = obj.step_part_index.map_or(-1, |i| i as i32);
    // The cut goes to a private file renamed into place, so the cache path
    // only ever names a whole mesh: a reader (a window loading the cut, a
    // slice) never sees half a file, a second daemon cutting the same stack
    // on the same cache swaps it atomically, and a daemon killed mid-write
    // leaves a stray `.tmp` rather than a truncated cache hit.
    let staging = staging_path(&out);
    let target = out.clone();
    // apply_operations crosses the C++ bridge (mcut boolean, possibly a
    // STEP load) and can take seconds. spawn_blocking keeps the dispatcher
    // responsive, same as inspect_model/split.
    let cut = tokio::task::spawn_blocking(move || {
        // See STEP_FFI_LOCK: apply_operations loads the source via
        // load_model_for_path, the same not-thread-safe OCCT STEP reader as
        // inspect_model/query_model_faces/split_model_to_files.
        let _step_lock = STEP_FFI_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Another call may have cut this very stack while this one waited.
        if cached_cut_skips(&target).is_some() {
            return Ok(None);
        }
        let outcome =
            slicer_ffi::apply_operations(&source_path, part, &ops_json, &staging.to_string_lossy());
        // The skips go down before the cut is renamed into place: whoever
        // finds the cut finds what it left out.
        let placed = if outcome.success {
            let skipped: Vec<SkippedOp> = outcome
                .skipped_ops
                .iter()
                .cloned()
                .map(SkippedOp::from)
                .collect();
            write_skipped_sidecar(&target, &skipped)
                .and_then(|()| std::fs::rename(&staging, &target))
        } else {
            Ok(())
        };
        if !outcome.success || placed.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        placed.map(|()| Some(outcome))
    })
    .await
    .map_err(|e| RpcError::Project(format!("apply task failed: {e}")))?
    .map_err(|e| RpcError::Project(format!("{context}: cache write: {e}")))?;
    let Some(outcome) = cut else {
        return Ok(MaterializedMesh::cached(
            path,
            cached_cut_skips(&out).unwrap_or_default(),
        ));
    };
    if !outcome.success {
        return Err(RpcError::Project(format!(
            "{context}: apply_operations failed: {}",
            outcome.error_message
        )));
    }
    Ok(MaterializedMesh {
        path,
        applied_count: outcome.applied_count,
        skipped_ops: outcome
            .skipped_ops
            .into_iter()
            .map(SkippedOp::from)
            .collect(),
    })
}

/// Record `operations` as the stack of `object_id`.
///
/// Refused for an object a part shows a body of, checked here under the state
/// lock as well as before the cut: the cut can take seconds, long enough for
/// a part to take the object as a body in the meantime.
fn record_operations(
    state: &StateLayer,
    object_id: ObjectId,
    operations: Vec<slicer_types::FeatureOp>,
) -> Result<(), RpcError> {
    state.transaction(|txn| {
        if let Some(object) = txn.project().objects.get(&object_id) {
            crate::part::reject_operations_on_body(object)?;
        }
        txn.apply(ProjectEvent::ObjectOperationsSet {
            object_id,
            operations,
        });
        Ok(())
    })
}

/// FFI `part_index` for an object's resolved slice input
/// (`resolve_slice_input_path`). A materialized ops-cache mesh is always a
/// plain single-object STL: `apply_operations` (the C++ bridge, run by
/// `materialize_operations`) already selected the one `ModelObject` named by
/// `step_part_index`, extracted its raw mesh, recentered it, cut it, and
/// wrote the result as ONE binary STL — there is no part list left to index
/// into. So whenever `resolved_input_path` diverges from `obj.source_path`
/// (i.e. we materialized), the correct index is -1 ("load this file's one
/// object directly"), same as any other single-mesh STL input.
///
/// This matters because `run_slice`'s C++ object-assembly loop treats
/// `part_index >= 0` as "index into `loaded.objects` from parsing
/// `input_path`" (`protype_slicer_bridge.cpp`, the `settings.objects` loop):
/// for a STEP source that's `obj.step_part_index`, a real index into the
/// multi-object `Model::read_from_step` result. Carrying that same index
/// through unchanged once `input_path` has been swapped to the ops-cache
/// STL would ask `Model::read_from_file` (which yields exactly one object
/// for an STL) for a part index that no longer exists — an out-of-range
/// error that fails the WHOLE slice, not just this object.
pub(crate) fn resolve_slice_part_index(obj: &ObjectRecord, resolved_input_path: &str) -> i32 {
    if resolved_input_path != obj.source_path {
        return -1;
    }
    obj.step_part_index.map(|i| i as i32).unwrap_or(-1)
}

/// Paint masks `objects_for_ffi` hands to `run_slice` for one object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlicePaintMasks {
    pub seam_facets_rle: String,
    pub support_facets_rle: String,
    pub fan_facets_rle: Option<String>,
    pub filament_facets_rle: Option<String>,
}

/// Seam, support and fan paint masks to forward for an object's resolved
/// slice input (`resolve_slice_input_path`).
///
/// All three masks are base64 RLE blobs that index triangles BY POSITION in
/// the object's *source* geometry, the mesh the UI paints on — set by
/// `object.set_seam_facets` / `object.set_support_facets` /
/// `object.set_fan_facets` (this file). That
/// is not quite the mesh libslic3r loads, which it repairs on import; the
/// bridge maps the difference by facet centroid (`apply_ui_facet_paint` in
/// `protype_slicer_bridge.cpp`). The indexing is only valid for the source
/// mesh at all: once `object.set_operations` materializes an active op stack,
/// the sliced mesh is the post-boolean-op cache STL, and `apply_operations`'s
/// mcut `A_NOT_B` changes both the triangle COUNT and ORDER — so a
/// source-indexed mask applied to it lands on entirely different facets (a
/// silent mis-paint, not a crash).
///
/// So the masks are forwarded ONLY while the resolved slice input is still
/// the untouched source (`resolved_input_path == obj.source_path`) — the
/// exact same discriminator `resolve_slice_part_index` uses, and for the same
/// "we materialized, the old per-triangle indexing no longer applies" reason.
/// The moment the input diverges (a materialized ops-cache mesh), both masks
/// are dropped to empty. Keying on the resolved path rather than on
/// `obj.operations` keeps the masks with the mesh that is actually sliced: a
/// stack whose every op is suppressed slices the source, paint included.
///
/// Deliberately NON-destructive: the stored `seam_facets_rle` /
/// `support_facets_rle` / `fan_facets_rle` are left untouched on the object, so suppressing or
/// deleting every op (which sends `resolve_slice_input_path` back to the
/// source — see that fn and the UI's `materializeAndReload`) restores the
/// paint on the next slice with no repainting. Remapping a mask THROUGH the
/// boolean op is out of scope for this foundation phase.
pub fn resolve_slice_paint_masks(obj: &ObjectRecord, resolved_input_path: &str) -> SlicePaintMasks {
    if resolved_input_path != obj.source_path {
        return SlicePaintMasks::default();
    }
    SlicePaintMasks {
        seam_facets_rle: obj.seam_facets_rle.clone().unwrap_or_default(),
        support_facets_rle: obj.support_facets_rle.clone().unwrap_or_default(),
        fan_facets_rle: obj.fan_facets_rle.clone(),
        filament_facets_rle: obj.filament_facets_rle.clone(),
    }
}

/// Everything `run_slice` is given for one object, read from `input_path`,
/// its slice input (`resolve_slice_input_path`): that mesh, its STEP part
/// index (`resolve_slice_part_index`), transform, paint masks
/// (`resolve_slice_paint_masks`), variable-layer-height profile, per-Z
/// overrides and per-object config overrides. `fake_slice` builds every plate
/// entry with it and `slice.estimate` the one object it costs, so an
/// estimate slices the part a print does.
pub(crate) fn object_slice_inputs(
    obj: &ObjectRecord,
    input_path: String,
) -> slicer_ffi::ObjectInputs {
    let part_index = resolve_slice_part_index(obj, &input_path);
    let masks = resolve_slice_paint_masks(obj, &input_path);
    slicer_ffi::ObjectInputs {
        input_path,
        part_index,
        object_transform: obj.transform,
        seam_facets_rle: masks.seam_facets_rle,
        support_facets_rle: masks.support_facets_rle,
        fan_facets_rle: masks.fan_facets_rle,
        filament_facets_rle: masks.filament_facets_rle,
        layer_height_profile: obj.layer_height_profile.clone(),
        layer_ranges: obj
            .layer_ranges
            .iter()
            .map(|r| slicer_ffi::LayerConfigRangeInputs {
                id: r.id.clone(),
                z_min: r.z_min,
                z_max: r.z_max,
                fan_value: r
                    .fan
                    .as_ref()
                    .map(slicer_types::LayerRangeFan::encoded_value)
                    .unwrap_or(0),
                overrides: r
                    .overrides
                    .iter()
                    .map(|(k, v)| (k.clone(), setting_value_to_libslic3r_text(v)))
                    .collect(),
            })
            .collect(),
        object_overrides: object_config_overrides_for(obj),
    }
}

/// Serialization for exactly four FFI call sites that cross into
/// OpenCascade's STEP reader: `slicer_ffi::inspect_model` (`object.
/// import_local`), `slicer_ffi::query_model_faces` (`ensure_step_edit_mesh`,
/// shared by `object.query_faces` / `object.get_edit_mesh` and the STEP
/// objects `mesh.*` / `verify.*` measure), `slicer_ffi::split_model_to_files`
/// (`object.split_to_parts`), and `slicer_ffi::apply_operations`
/// (`materialize_operations`, behind `object.set_operations`, `object.
/// get_materialized_mesh` and the recut of a lost cut for a slice or for
/// `mesh.*` / `verify.*` / `slice.estimate` — it loads the source through the
/// same `load_model_for_path` before cutting it).
///
/// A part's hole nodes cut through `apply_operations` too, from outside this
/// file: `part::kernel::LibKernel::hole` takes this same lock around the
/// call and the move of its result into place, which is why it is
/// `pub(crate)`.
///
/// OCCT's `Interface_Static` (used internally by the STEP reader that
/// backs all four) is not thread-safe: concurrent STEP parses inside
/// one process can terminate it with `Interface_InterfaceError` or
/// SIGSEGV (see the Task 1 FFI report for `query_model_faces`). When this
/// lock was introduced no production code path serialized STEP parsing at
/// all — only this crate's integration tests did, via a per-test-binary
/// `STEP_PARSE_LOCK` (see `tests/object_import_local.rs`). This mutex
/// closes that gap for these call sites: each one takes it immediately
/// before touching OCCT, so no two STEP parses can race each other *among
/// `import_local` / `split_to_parts` / `query_faces` + `get_edit_mesh` /
/// `set_operations` + `get_materialized_mesh` + a slice's recut*.
///
/// Formerly named `STEP_QUERY_LOCK` and held only around
/// `query_model_faces` inside `object.query_faces`. Renamed and widened
/// to close the cross-arm race that was tracked as a known gap in the
/// Task 2 report: concurrent `object.import_local` / `object.
/// split_to_parts` calls re-parsed STEP without taking any lock, so they
/// could still race a concurrent `object.query_faces`. All three call
/// sites now share this one lock.
///
/// Note this is coarser than strictly necessary — it also serializes
/// `inspect_model` calls for plain STL/3MF imports (which never touch
/// OCCT) behind any in-flight STEP parse. Accepted trade-off: STL/3MF
/// `inspect_model` calls are fast and the daemon is single-user, so the
/// extra contention is negligible next to the crash risk it closes.
///
/// NOT covered by *this* lock, and that is deliberate scope, not an
/// oversight: `slice.request` → `fake_slice` → `slicer_ffi::run_slice`
/// (this file, around the `run_slice` call inside `fake_slice`) also
/// parses STEP through OCCT — the C++ bridge's `run_slice` loads each
/// source via `load_model_for_path`, which calls
/// `Slic3r::Model::read_from_step` for STEP paths — but it does NOT take
/// `STEP_FFI_LOCK`. Slices are launched via a detached `tokio::spawn` and
/// can run for many seconds; wrapping all of `run_slice` in this lock
/// would stall a short inspector query (`import_local` / `query_faces` /
/// `get_edit_mesh` / `split_to_parts`) behind an in-flight multi-second
/// slice, which is an unacceptable UX regression. A slice takes this lock
/// only to recut an op stack whose cut the ops cache lost
/// (`resolve_slice_input_path`), and the recut releases it before
/// `run_slice` starts.
///
/// The slice-vs-inspect and slice-vs-slice STEP-parse races that gap used
/// to leave open are now closed one layer down, which is exactly where
/// this comment previously argued they should be: the C++ bridge holds
/// `occt_step_mutex()` (see `protype_slicer_bridge.cpp`) around the parse
/// *itself*, in both readers that exist — `load_model_for_path`
/// (`run_slice` / `inspect_model` / `split_model_to_files`) and
/// `collect_step_solids_in_upstream_order` (`query_model_faces`). That
/// serializes a slice's STEP read against an inspector's without holding
/// anything for the duration of the slice. So the whole-process "no two
/// STEP parses ever race" invariant does now hold — but it is enforced by
/// that C++ mutex, not by this one. Read `STEP_FFI_LOCK` as the outer
/// serialization of the inspector/edit call sites (it also covers what
/// happens around the parse: tessellation, the XCAF document lifecycle,
/// and `apply_operations`'s mcut boolean), not as the process-wide
/// invariant.
///
/// Lock ordering is one-way and therefore deadlock-free: a thread may
/// take `STEP_FFI_LOCK` and then `occt_step_mutex()` inside the FFI call,
/// never the reverse — nothing acquires `STEP_FFI_LOCK` while holding the
/// C++ mutex.
///
/// Acquired only inside `spawn_blocking` (never on the async task
/// itself) at each of the call sites above, so a contended lock parks a
/// blocking-pool thread instead of a tokio worker.
pub(crate) static STEP_FFI_LOCK: StdMutex<()> = StdMutex::new(());

fn read_value_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn read_value_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(v) => Some(*v),
        Value::Number(n) => n.as_i64().map(|v| v != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn read_string_value(values: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    match values.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(v) => Some(v.to_string()),
        _ => None,
    }
}

fn resolved_process_value<'a>(
    resolved_config: &'a Option<ResolvedSliceConfig>,
    key: &str,
) -> Option<&'a Value> {
    resolved_config.as_ref()?.process.get(key)
}

fn resolved_process_value_any<'a>(
    resolved_config: &'a Option<ResolvedSliceConfig>,
    keys: &[&str],
) -> Option<&'a Value> {
    let process = &resolved_config.as_ref()?.process;
    keys.iter().find_map(|key| process.get(*key))
}

fn resolved_process_f64(resolved_config: &Option<ResolvedSliceConfig>, key: &str) -> Option<f64> {
    read_value_f64(resolved_process_value(resolved_config, key)?)
}

fn resolved_process_f32(resolved_config: &Option<ResolvedSliceConfig>, key: &str) -> Option<f32> {
    resolved_process_f64(resolved_config, key).map(|v| v as f32)
}

fn resolved_process_f64_any(
    resolved_config: &Option<ResolvedSliceConfig>,
    keys: &[&str],
) -> Option<f64> {
    read_value_f64(resolved_process_value_any(resolved_config, keys)?)
}

fn resolved_process_f32_any(
    resolved_config: &Option<ResolvedSliceConfig>,
    keys: &[&str],
) -> Option<f32> {
    resolved_process_f64_any(resolved_config, keys).map(|v| v as f32)
}

fn resolved_process_bool_any(
    resolved_config: &Option<ResolvedSliceConfig>,
    keys: &[&str],
) -> Option<bool> {
    read_value_bool(resolved_process_value_any(resolved_config, keys)?)
}

fn process_support_enabled(
    resolved_config: &Option<ResolvedSliceConfig>,
    settings: &HashMap<(SettingScope, SettingKey), SettingValue>,
) -> bool {
    resolved_process_bool_any(resolved_config, &["enable_support", "support_enabled"])
        .or_else(|| {
            settings
                .get(&(SettingScope::Process, "enable_support".to_string()))
                .and_then(|v| v.as_bool())
        })
        .or_else(|| {
            settings
                .get(&(SettingScope::Process, "support_enabled".to_string()))
                .and_then(|v| v.as_bool())
        })
        .unwrap_or(false)
}

fn process_setting_string_any(
    resolved_config: &Option<ResolvedSliceConfig>,
    settings: &HashMap<(SettingScope, SettingKey), SettingValue>,
    keys: &[&str],
) -> String {
    resolved_process_string_any(resolved_config, keys)
        .or_else(|| {
            keys.iter().find_map(|key| {
                settings
                    .get(&(SettingScope::Process, (*key).to_string()))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
        })
        .unwrap_or_default()
}

fn process_infill_pattern_for_ffi(
    resolved_config: &Option<ResolvedSliceConfig>,
    settings: &HashMap<(SettingScope, SettingKey), SettingValue>,
) -> String {
    normalize_infill_pattern_alias(&process_setting_string_any(
        resolved_config,
        settings,
        &["sparse_infill_pattern", "infill_pattern"],
    ))
}

fn resolved_process_string_any(
    resolved_config: &Option<ResolvedSliceConfig>,
    keys: &[&str],
) -> Option<String> {
    let process = &resolved_config.as_ref()?.process;
    keys.iter().find_map(|key| read_string_value(process, key))
}

fn config_override_value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::Bool(v) => Some(if *v { "1" } else { "0" }.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let mut rendered = Vec::with_capacity(items.len());
            for item in items {
                rendered.push(config_override_value_to_string(item)?);
            }
            Some(rendered.join(","))
        }
        Value::Null | Value::Object(_) => None,
    }
}

fn printable_area_override_value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(points) => {
            let mut rendered = Vec::with_capacity(points.len());
            for point in points {
                match point {
                    Value::String(s) => rendered.push(s.clone()),
                    Value::Array(pair) if pair.len() == 2 => {
                        let x = config_override_value_to_string(&pair[0])?;
                        let y = config_override_value_to_string(&pair[1])?;
                        rendered.push(format!("{x}x{y}"));
                    }
                    _ => return None,
                }
            }
            Some(rendered.join(","))
        }
        _ => config_override_value_to_string(value),
    }
}

fn config_override_value_to_string_for_key(key: &str, value: &Value) -> Option<String> {
    match key {
        "printable_area" => printable_area_override_value_to_string(value),
        _ => config_override_value_to_string(value),
    }
}

fn normalize_infill_pattern_alias(value: &str) -> String {
    match value {
        "lines" => "line".to_string(),
        "stars" => "tri-hexagon".to_string(),
        "zig-zag" => "zigzag".to_string(),
        "cross-zag" => "crosszag".to_string(),
        "locked-zag" => "lockedzag".to_string(),
        "3d-honeycomb" => "3dhoneycomb".to_string(),
        _ => value.to_string(),
    }
}

fn normalize_config_override_value(key: &str, value: String) -> String {
    match key {
        "sparse_infill_pattern"
        | "internal_solid_infill_pattern"
        | "top_surface_pattern"
        | "bottom_surface_pattern" => normalize_infill_pattern_alias(&value),
        "support_type" => match value.as_str() {
            "normal" => "normal(auto)".to_string(),
            "normal_auto" => "normal(auto)".to_string(),
            "tree" => "tree(auto)".to_string(),
            "tree_auto" => "tree(auto)".to_string(),
            "normal_manual" => "normal(manual)".to_string(),
            "tree_manual" => "tree(manual)".to_string(),
            _ => value,
        },
        "seam_position" => match value.as_str() {
            "rear" => "back".to_string(),
            _ => value,
        },
        "ironing_type" => match value.as_str() {
            "none" => "no ironing".to_string(),
            "all_solid" => "solid".to_string(),
            _ => value,
        },
        _ => value,
    }
}

/// Переименовать ключ профиля в имя, которое понимает libslic3r, либо выбросить
/// его (`None`) как чисто приложенческий.
///
/// Эта таблица — половина словаря, по которому Slicer гейтит свою схему: правки
/// здесь и бампы пина libslic3r нужно переснимать в снапшот
/// `apps/agent/resources/libslic3r_keys.json` командой
/// `python3 scripts/dump-libslic3r-keys.py --daemon-repo <этот репозиторий>`
/// (гейт — `apps/agent/tests/schema_keys_reach_libslic3r.rs`). Иначе ключ,
/// который здесь перестал переименовываться, не дойдёт до libslic3r —
/// `slice.request` лишь назовёт его в `SliceCompleted.warnings`
/// (`ignored_overrides`), — а схема будет считаться проверенной.
fn normalize_config_override_key_value(key: &str, value: String) -> Option<(String, String)> {
    let mapped_key = match key {
        // Profile v2 canonical keys that are app-only metadata or visual hints.
        "machine_model"
        | "machine_variant"
        | "bed_type"
        | "bed_visual_model_asset"
        | "bed_visual_model_offset_x"
        | "bed_visual_model_offset_y"
        | "bed_visual_model_offset_z"
        | "bed_visual_model_rotation_x"
        | "bed_visual_model_rotation_y"
        | "bed_visual_model_rotation_z"
        | "bed_visual_model_scale_x"
        | "bed_visual_model_scale_y"
        | "bed_visual_model_scale_z"
        | "bed_visual_overlay_asset"
        | "bed_visual_overlay_enabled"
        | "bed_visual_overlay_offset_x"
        | "bed_visual_overlay_offset_y"
        | "bed_visual_overlay_scale_x"
        | "bed_visual_overlay_scale_y"
        | "bed_visual_overlay_rotation_deg"
        | "kinematics"
        | "silent_mode_supported"
        | "has_chamber_heater"
        | "chamber_max_temperature"
        | "extruder_count"
        | "extruder_slots"
        // `arc_fitting` is the legacy app-only alias with no libslic3r key —
        // still dropped. `enable_arc_fitting` IS a real libslic3r key and is
        // now passed through (fallback `_ => key` below) so a user profile
        // override reaches the bridge. See Slicer plan T1-7.
        | "arc_fitting" => return None,
        // Profile v2 names with Orca/libSlic3r spellings that differ from
        // the UI contract. The machine limits (`machine_max_speed_*`,
        // `machine_max_acceleration_*`) already carry libslic3r's names and
        // fall through: `_extruding` is the print ceiling `GCodeWriter`
        // clamps accelerations to, not the E-axis limit `_e`.
        // The profile's one XY jerk names libslic3r's X axis here; the Y axis
        // is added by `COMPANION_KEYS` (one arm can name a single target).
        "machine_max_jerk_xy" => "machine_max_jerk_x",
        "machine_notes" => "printer_notes",
        "firmware_retraction" => "use_firmware_retraction",
        "outer_wall_first" => {
            return Some((
                "wall_sequence".to_string(),
                match value.as_str() {
                    "1" | "true" | "yes" | "on" => "outer wall/inner wall".to_string(),
                    _ => "inner wall/outer wall".to_string(),
                },
            ));
        }
        "infill_speed" => "sparse_infill_speed",
        "solid_infill_speed" => "internal_solid_infill_speed",
        "infill_acceleration" => "sparse_infill_acceleration",
        // Per-object override names (`SettingsSchema::baseline`, the UI's
        // override panel). Mapping the key first also lets the value pass
        // through the libslic3r enum aliases below (`lines` → `line`).
        "infill_pattern" => "sparse_infill_pattern",
        "infill_density" => "sparse_infill_density",
        "support_enabled" => "enable_support",
        "perimeter_count" => "wall_loops",
        "gcode_resolution" => "resolution",
        "force_cooling_for_overhang" => "enable_overhang_bridge_fan",
        "min_print_speed" => "slow_down_min_speed",
        "nozzle_temperature_normal" | "temperature" => "nozzle_temperature",
        "first_layer_temperature" => "nozzle_temperature_initial_layer",
        "bed_temperature_normal" => "bed_temperature",
        "bed_temperature_initial_layer" | "first_layer_bed_temperature" => {
            "bed_temperature_initial_layer"
        }
        "chamber_temperature_target" => "chamber_temperature",
        "filament_flow_ratio" | "extrusion_multiplier" => "filament_flow_ratio",
        "max_volumetric_speed" => "filament_max_volumetric_speed",
        "filament_min_layer_time" | "slowdown_below_layer_time" => "slow_down_layer_time",
        "min_fan_speed" => "fan_min_speed",
        "max_fan_speed" => "fan_max_speed",
        "fan_disable_first_n_layers" | "disable_fan_first_layers" => "close_fan_the_first_x_layers",
        "filament_color_hex" => "filament_colour",
        // These filament-profile aliases are overlaid onto the per-slot
        // extruder vectors by the agent. Passing the original alias as a
        // flat libslic3r override would be an unknown key.
        "filament_retraction_length"
        | "filament_retract_length"
        | "filament_retraction_speed"
        | "filament_retract_speed"
        | "pressure_advance_smooth_time" => return None,
        "support_filament_index" => "support_filament",
        // Support INTERFACE extruder. The schema key matches the OrcaSlicer
        // 2.3.1 libslic3r key 1:1 (`support_interface_filament`, ConfigOptionInt),
        // so the `_ => key` fallback below already forwards it. This explicit arm
        // documents the BVOH-on-interface routing next to the support base and
        // pins it against a future "drop null-libslic3r_key process keys" cleanup.
        // 0 = "same extruder as the support base"; a real slot routes the
        // interface independently (e.g. soluble BVOH on slot 2 over a model-
        // filament support body on slot 1).
        "support_interface_filament" => "support_interface_filament",
        _ => key,
    };
    Some((
        mapped_key.to_string(),
        normalize_config_override_value(mapped_key, value),
    ))
}

fn collect_config_overrides(config: &ResolvedSliceConfig) -> Vec<slicer_ffi::ConfigOverride> {
    collect_config_overrides_with(config, override_join_kind)
}

/// Flatten the resolved bags into libslic3r `set_deserialize_strict` text.
///
/// `join_of` says how a (mapped) key's values are serialised — see
/// [`OptionJoinKind`]. Machine/process keys are scalars unless libslic3r
/// declares them `coStrings`, in which case the single value still has to be
/// C-style escaped (quoted) or a `;` inside it would split it. Filament bags
/// are per-slot: vector keys join with `,`, string vectors with `;` after
/// escaping, and everything else is last-writer-wins like before.
///
/// Several spellings can name one libslic3r option. Within a bag the libslic3r
/// spelling beats its aliases ([`shadowed_by_libslic3r_spelling`]) whatever
/// the key order; between two aliases the later one still wins.
fn collect_config_overrides_with(
    config: &ResolvedSliceConfig,
    join_of: fn(&str) -> OptionJoinKind,
) -> Vec<slicer_ffi::ConfigOverride> {
    let mut overrides = Vec::with_capacity(config.machine.len() + config.process.len());
    for values in [&config.machine, &config.process] {
        for (key, value) in values {
            let Some((mapped_key, value)) = render_flat_override(key, value, join_of) else {
                continue;
            };
            let targets = std::iter::once(mapped_key).chain(companion_keys(key));
            for target in targets {
                if shadowed_by_libslic3r_spelling(values, key, &target) {
                    continue;
                }
                overrides.push(slicer_ffi::ConfigOverride {
                    key: target,
                    value: value.clone(),
                });
            }
        }
    }
    let mut multi_filament_vectors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut multi_filament_strings: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for values in &config.filaments {
        // Per-slot dedup before pushing into the vector: multiple input
        // aliases can map to the same libslic3r key within one slot
        // (e.g. `temperature` and `nozzle_temperature_normal` both →
        // `nozzle_temperature`). The scalar path uses last-writer-wins via
        // `upsert_config_override`; mirror that here so we don't emit
        // `"245,245"` for a single slot that happened to set both aliases.
        // An alias never beats the libslic3r spelling itself, though.
        let mut slot_vectors: BTreeMap<String, String> = BTreeMap::new();
        let mut slot_strings: BTreeMap<String, String> = BTreeMap::new();
        for (key, value) in values {
            let Some(raw) = config_override_value_to_string_for_key(key, value) else {
                continue;
            };
            let Some((mapped_key, normalized)) = normalize_config_override_key_value(key, raw)
            else {
                continue;
            };
            if shadowed_by_libslic3r_spelling(values, key, &mapped_key) {
                continue;
            }
            match join_of(&mapped_key) {
                OptionJoinKind::Strings => {
                    slot_strings.insert(mapped_key, normalized);
                }
                OptionJoinKind::Vector => {
                    slot_vectors.insert(mapped_key, normalized);
                }
                OptionJoinKind::Scalar | OptionJoinKind::Unknown => {
                    upsert_config_override(&mut overrides, mapped_key, normalized);
                }
            }
        }
        for (key, value) in slot_vectors {
            multi_filament_vectors.entry(key).or_default().push(value);
        }
        for (key, value) in slot_strings {
            multi_filament_strings.entry(key).or_default().push(value);
        }
    }
    for (key, values) in multi_filament_vectors {
        upsert_config_override(&mut overrides, key, values.join(","));
    }
    for (key, values) in multi_filament_strings {
        upsert_config_override(&mut overrides, key, escape_strings_cstyle(&values));
    }
    overrides
}

/// Machine/process keys that set more than one libslic3r option: (profile key,
/// option set in addition to the one `normalize_config_override_key_value`
/// maps it to), with the same value.
///
/// `machine_max_jerk_xy` is the profile's one XY jerk (Klipper's
/// `square_corner_velocity`); libslic3r keeps one per axis. Dropping it left
/// both at the bundled X1C preset's 9 mm/s under a QIDI `printer.cfg` of 8.
const COMPANION_KEYS: &[(&str, &str)] = &[("machine_max_jerk_xy", "machine_max_jerk_y")];

/// The options `key` sets beyond its mapped name — see [`COMPANION_KEYS`].
fn companion_keys(key: &str) -> impl Iterator<Item = String> + '_ {
    COMPANION_KEYS
        .iter()
        .filter(move |(profile_key, _)| *profile_key == key)
        .map(|(_, companion)| (*companion).to_string())
}

/// True when `key` is only an alias of `mapped_key` (`force_cooling_for_overhang`
/// of `enable_overhang_bridge_fan`) and the same bag gives the option a value
/// under the libslic3r spelling itself. That value is authoritative; otherwise
/// the winner would be whichever spelling happens to sort last in the bag. A
/// null under the libslic3r spelling is no value, so the alias still applies.
fn shadowed_by_libslic3r_spelling(
    bag: &BTreeMap<String, Value>,
    key: &str,
    mapped_key: &str,
) -> bool {
    key != mapped_key
        && bag.get(mapped_key).is_some_and(|value| {
            config_override_value_to_string_for_key(mapped_key, value).is_some()
        })
}

/// One machine/process override in libslic3r text form: key mapped, enum
/// aliases normalised, and — for a `coStrings` key — the value C-style
/// escaped as a one-element (or, for a JSON array, n-element) vector.
fn render_flat_override(
    key: &str,
    value: &Value,
    join_of: fn(&str) -> OptionJoinKind,
) -> Option<(String, String)> {
    let raw = config_override_value_to_string_for_key(key, value)?;
    let (mapped_key, normalized) = normalize_config_override_key_value(key, raw)?;
    if join_of(&mapped_key) == OptionJoinKind::Strings {
        let items = string_items(value)?;
        return Some((mapped_key, escape_strings_cstyle(&items)));
    }
    Some((mapped_key, normalized))
}

/// The elements a `coStrings` value carries: a JSON array is one element per
/// entry, anything else is a single element.
fn string_items(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Array(items) => items.iter().map(config_override_value_to_string).collect(),
        other => Some(vec![config_override_value_to_string(other)?]),
    }
}

/// Port of libslic3r `escape_strings_cstyle` (Config.cpp) — the wire format
/// `ConfigOptionStrings::deserialize` expects. Elements are `;`-separated; one
/// that contains whitespace, quotes, backslashes or line breaks is
/// double-quoted with `\\`, `\"`, `\r`, `\n` escapes, and a lone empty
/// string is quoted so it survives as one empty element. We additionally
/// quote on `;`: libslic3r's own escaper leaves `a;b` bare and its parser
/// then splits it in two.
fn escape_strings_cstyle(values: &[String]) -> String {
    let mut out = String::new();
    for (i, s) in values.iter().enumerate() {
        if i > 0 {
            out.push(';');
        }
        let must_quote = (values.len() == 1 && s.is_empty())
            || s.chars()
                .any(|c| matches!(c, ' ' | '\t' | '\\' | '"' | '\r' | '\n' | ';'));
        if !must_quote {
            out.push_str(s);
            continue;
        }
        out.push('"');
        for c in s.chars() {
            match c {
                '\\' | '"' => {
                    out.push('\\');
                    out.push(c);
                }
                '\r' => out.push_str("\\r"),
                '\n' => out.push_str("\\n"),
                _ => out.push(c),
            }
        }
        out.push('"');
    }
    out
}

/// How a mapped key's values are joined: libslic3r's own option table when it
/// is linked in, the hand-written table below otherwise (stub builds). An
/// alias libslic3r does not know under that name (`handle_legacy` renames)
/// falls back to the table as well.
fn override_join_kind(key: &str) -> OptionJoinKind {
    match slicer_ffi::config_option_join_kind(key) {
        OptionJoinKind::Unknown => override_join_fallback(key),
        known => known,
    }
}

/// Stub-build fallback for [`override_join_kind`], kept honest against the
/// real definition table by `tests/override_join_kind_e2e.rs`.
pub fn override_join_fallback(key: &str) -> OptionJoinKind {
    if is_costrings_override_key(key) {
        OptionJoinKind::Strings
    } else if is_multi_filament_vector_override_key(key) {
        OptionJoinKind::Vector
    } else {
        OptionJoinKind::Scalar
    }
}

/// Every key the fallback tables know, so a gate can check each one against
/// libslic3r.
pub fn override_join_fallback_keys() -> impl Iterator<Item = &'static str> {
    COSTRINGS_OVERRIDE_KEYS
        .iter()
        .chain(MULTI_FILAMENT_VECTOR_OVERRIDE_KEYS.iter())
        .copied()
}

// libslic3r treats these as per-extruder vectors (`coFloats`/`coInts`/`coBools`/
// `coPercents`/`coEnums` in PrintConfig.cpp). When multiple filaments are
// active we must emit `"<slot0>,<slot1>,..."` so each slot keeps its own value;
// otherwise `upsert_config_override` overwrites and the **last** slot's value
// becomes the single scalar libslic3r sees for every slot. That silently caps
// fast filaments to the slowest filament's `max_volumetric_speed` (or any
// other property) and is invisible until you read the g-code trailer.
//
// With libslic3r linked in this table is only consulted for keys its option
// table does not know; the stub build relies on it entirely.
const MULTI_FILAMENT_VECTOR_OVERRIDE_KEYS: &[&str] = &[
    // Cooling / fan (coFloats / coInts / coBools)
    "fan_min_speed",
    "fan_max_speed",
    "close_fan_the_first_x_layers",
    "slow_down_layer_time",
    "slow_down_min_speed",
    "slow_down_for_layer_cooling",
    "min_layer_time_strategy",
    "park_and_wait_retract_length",
    "park_and_wait_z_hop",
    "reduce_fan_stop_start_freq",
    "ironing_fan_speed",
    "support_material_interface_fan_speed",
    // Pressure advance (coBools / coFloats)
    "enable_pressure_advance",
    "pressure_advance",
    "adaptive_pressure_advance",
    "adaptive_pressure_advance_overhangs",
    "adaptive_pressure_advance_bridges",
    // Flow / material (coFloats / coPercents)
    "filament_max_volumetric_speed",
    "filament_diameter",
    "filament_density",
    "filament_flow_ratio",
    "filament_cost",
    "filament_shrink",
    "filament_shrinkage_compensation_z",
    // Temperatures / chamber (coInts / coBools). `bed_temperature` and
    // `bed_temperature_initial_layer` are deliberately absent: libslic3r's
    // `handle_legacy` drops both names (its ignore set), so a flat override
    // never reaches a vector — the typed IDEX path (`set_ints_dynamic` in the
    // bridge) carries bed temperatures per slot instead.
    "nozzle_temperature",
    "nozzle_temperature_initial_layer",
    "chamber_temperature",
    "activate_chamber_temp_control",
];

// `coStrings` keys: per-slot values are `;`-joined after C-style escaping,
// and even a single value must be quoted or a `;` inside it (any G-code
// comment) splits it — `filament_start_gcode = "; start ; keep"` used to
// reach libslic3r as `"; start "`.
const COSTRINGS_OVERRIDE_KEYS: &[&str] = &[
    "filament_type",
    "filament_colour",
    "filament_vendor",
    "filament_notes",
    "filament_start_gcode",
    "filament_end_gcode",
    "adaptive_pressure_advance_model",
    "post_process",
    "small_area_infill_flow_compensation_model",
];

fn is_multi_filament_vector_override_key(key: &str) -> bool {
    MULTI_FILAMENT_VECTOR_OVERRIDE_KEYS.contains(&key)
}

fn is_costrings_override_key(key: &str) -> bool {
    COSTRINGS_OVERRIDE_KEYS.contains(&key)
}

fn setting_value_to_json_value(value: &SettingValue) -> Value {
    match value {
        SettingValue::Number(v) => json!(v),
        SettingValue::Int(v) => json!(v),
        SettingValue::Bool(v) => json!(v),
        SettingValue::String(v) => json!(v),
    }
}

/// Encode a typed `SettingValue` to libslic3r's flat text format
/// (matches `DynamicPrintConfig::set_deserialize`). Bool → `1`/`0`,
/// numbers → debug, strings → verbatim. Used to feed per-Z range
/// overrides to the C++ bridge.
fn setting_value_to_libslic3r_text(v: &SettingValue) -> String {
    match v {
        SettingValue::Number(n) => format!("{n}"),
        SettingValue::Int(i) => format!("{i}"),
        SettingValue::Bool(b) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        SettingValue::String(s) => s.clone(),
    }
}

fn upsert_config_override(
    overrides: &mut Vec<slicer_ffi::ConfigOverride>,
    key: String,
    value: String,
) {
    if let Some(existing) = overrides.iter_mut().find(|o| o.key == key) {
        existing.value = value;
    } else {
        overrides.push(slicer_ffi::ConfigOverride { key, value });
    }
}

/// Translate `ResolvedRoleMapping` entries into the per-role extruder keys
/// that OrcaSlicer's `DynamicPrintConfig` actually consults — see
/// `upstream/libslic3r/src/libslic3r/PrintConfig.cpp` (defs around lines
/// 3312 / 4131 / 4745 / 5089 / 5124 for the `*_filament` family; legacy
/// `*_extruder` aliases get remapped internally at line ~6878).
///
/// libslic3r is 1-based for these keys; `extruder_slot` from
/// `ResolvedRoleMapping` is 0-based. Translation table (extruder schema's
/// `role` enum is `model | support | auto`; `auto` is filtered out UI-side
/// before sending — see packages/ui/src/connection/daemonIdexPayload.ts:152):
///
///   role=model   → wall_filament, sparse_infill_filament, solid_infill_filament
///   role=support → support_filament, support_interface_filament
///
/// Closes audit finding `multi-tool-idex-role-mapping-dropped` (critical).
/// Before this, role_mapping was validated upstream then silently dropped
/// at the FFI boundary, so multi-tool IDEX always printed everything on
/// libslic3r's default extruder (typically T0) regardless of UI binding.
fn role_mapping_to_config_overrides(
    role_mapping: &[ResolvedRoleMapping],
) -> Vec<slicer_ffi::ConfigOverride> {
    let mut out: Vec<slicer_ffi::ConfigOverride> = Vec::new();
    for mapping in role_mapping {
        // 1-based for libslic3r. ResolvedSliceConfig::validate has already
        // confirmed extruder_slot is in range against `self.extruders` —
        // see slicer-types::ResolvedSliceConfig::validate.
        let one_based = (mapping.extruder_slot as u32) + 1;
        let value = one_based.to_string();
        match mapping.role.as_str() {
            "model" => {
                upsert_config_override(&mut out, "wall_filament".into(), value.clone());
                upsert_config_override(&mut out, "sparse_infill_filament".into(), value.clone());
                upsert_config_override(&mut out, "solid_infill_filament".into(), value);
            }
            "support" => {
                upsert_config_override(&mut out, "support_filament".into(), value.clone());
                upsert_config_override(&mut out, "support_interface_filament".into(), value);
            }
            "auto" => {
                // libslic3r default — no override. UI normally filters
                // these out before the wire, but tolerate them here too.
            }
            other => {
                tracing::warn!(
                    role = %other,
                    "role_mapping: unknown role; no libslic3r override emitted"
                );
            }
        }
    }
    out
}

/// Serialize one object's per-object overrides (`ObjectRecord.overrides`)
/// into the bridge's `ConfigOverride` list. Pure function — returns a
/// fresh `Vec` scoped to this object rather than pushing into a shared
/// plate-wide accumulator, so each `ObjectInputs` entry only carries its
/// own object's overrides (Q-5: per-object scope, not folded into the
/// plate-wide `config_overrides`).
fn object_config_overrides_for(
    object: &slicer_state::ObjectRecord,
) -> Vec<slicer_ffi::ConfigOverride> {
    let mut overrides = Vec::new();
    for (key, value) in &object.overrides {
        if matches!(
            key.as_str(),
            "seam_facets_rle" | "seam_target_point" | "serial_marking"
        ) {
            continue;
        }
        let json_value = setting_value_to_json_value(value);
        if let Some((mapped_key, mapped_value)) =
            config_override_value_to_string_for_key(key, &json_value)
                .and_then(|value| normalize_config_override_key_value(key, value))
        {
            // `perimeter_count` next to `wall_loops` on one object: the name
            // libslic3r reads wins, whatever order the overrides map yields.
            if *key != mapped_key && object.overrides.contains_key(&mapped_key) {
                continue;
            }
            upsert_config_override(&mut overrides, mapped_key, mapped_value);
        }
    }
    overrides
}

fn idex_from_resolved_config(
    config: &ResolvedSliceConfig,
) -> Result<slicer_ffi::IdexInputs, String> {
    config.validate()?;

    let mut extruders: Vec<&ResolvedExtruderConfig> = config.extruders.iter().collect();
    extruders.sort_by_key(|extruder| extruder.slot_id);
    let extruder_count = u8::try_from(extruders.len())
        .map_err(|_| "resolved_config has too many extruders".to_string())?;

    // Single-head IDEX selector. Carried verbatim to the bridge; `validate()`
    // already rejected an out-of-range head against the slot set.
    let print_head = config.print_head;

    fn collect_optional_f32(
        extruders: &[&ResolvedExtruderConfig],
        get: fn(&ResolvedExtruderConfig) -> Option<f32>,
    ) -> Vec<f32> {
        if extruders.iter().all(|extruder| get(extruder).is_some()) {
            extruders
                .iter()
                .map(|extruder| get(extruder).expect("checked above"))
                .collect()
        } else {
            Vec::new()
        }
    }

    fn collect_optional_i32(
        extruders: &[&ResolvedExtruderConfig],
        get: fn(&ResolvedExtruderConfig) -> Option<i32>,
    ) -> Vec<i32> {
        if extruders.iter().all(|extruder| get(extruder).is_some()) {
            extruders
                .iter()
                .map(|extruder| get(extruder).expect("checked above"))
                .collect()
        } else {
            Vec::new()
        }
    }

    fn collect_optional_bool(
        extruders: &[&ResolvedExtruderConfig],
        get: fn(&ResolvedExtruderConfig) -> Option<bool>,
    ) -> Vec<bool> {
        if extruders.iter().all(|extruder| get(extruder).is_some()) {
            extruders
                .iter()
                .map(|extruder| get(extruder).expect("checked above"))
                .collect()
        } else {
            Vec::new()
        }
    }

    fn collect_optional_string(
        extruders: &[&ResolvedExtruderConfig],
        get: fn(&ResolvedExtruderConfig) -> Option<&String>,
    ) -> Vec<String> {
        if extruders.iter().all(|extruder| get(extruder).is_some()) {
            extruders
                .iter()
                .map(|extruder| get(extruder).expect("checked above").clone())
                .collect()
        } else {
            Vec::new()
        }
    }

    let idex = slicer_ffi::IdexInputs {
        extruder_count,
        // Wired from the resolved config below (see `print_head` binding).
        print_head,
        extruder_offset_xy: extruders
            .iter()
            .map(|extruder| extruder.nozzle_offset_xy)
            .collect(),
        nozzle_diameter: extruders
            .iter()
            .map(|extruder| extruder.nozzle_diameter)
            .collect(),
        retraction_length: collect_optional_f32(&extruders, |e| e.retraction_length),
        retraction_speed: collect_optional_f32(&extruders, |e| e.retraction_speed),
        retract_restart_extra: collect_optional_f32(&extruders, |e| e.retract_restart_extra),
        deretraction_speed: collect_optional_f32(&extruders, |e| e.deretraction_speed),
        retraction_minimum_travel: collect_optional_f32(&extruders, |e| {
            e.retraction_minimum_travel
        }),
        retract_when_changing_layer: collect_optional_bool(&extruders, |e| {
            e.retract_when_changing_layer
        }),
        wipe: collect_optional_bool(&extruders, |e| e.wipe),
        wipe_distance: collect_optional_f32(&extruders, |e| e.wipe_distance),
        retract_before_wipe_pct: collect_optional_f32(&extruders, |e| e.retract_before_wipe),
        retract_length_toolchange: collect_optional_f32(&extruders, |e| {
            e.retract_length_toolchange
        }),
        z_hop: collect_optional_f32(&extruders, |e| e.z_hop),
        z_hop_types: collect_optional_string(&extruders, |e| e.z_hop_types.as_ref()),
        travel_slope_deg: collect_optional_f32(&extruders, |e| e.travel_slope),
        retract_lift_above: collect_optional_f32(&extruders, |e| e.retract_lift_above),
        retract_lift_below: collect_optional_f32(&extruders, |e| e.retract_lift_below),
        retract_lift_enforce: collect_optional_string(&extruders, |e| {
            e.retract_lift_enforce.as_ref()
        }),
        min_layer_height: collect_optional_f32(&extruders, |e| e.min_layer_height),
        max_layer_height: collect_optional_f32(&extruders, |e| e.max_layer_height),
        filament_type: collect_optional_string(&extruders, |e| e.filament_type.as_ref()),
        filament_colour: collect_optional_string(&extruders, |e| e.filament_colour.as_ref()),
        nozzle_temperature_initial_layer: collect_optional_i32(&extruders, |e| {
            e.nozzle_temperature_initial_layer
        }),
        nozzle_temperature: collect_optional_i32(&extruders, |e| e.nozzle_temperature),
        bed_temperature_initial_layer_single: collect_optional_i32(&extruders, |e| {
            e.bed_temperature_initial_layer_single
        }),
        bed_temperature: collect_optional_i32(&extruders, |e| e.bed_temperature),
        change_filament_gcode: read_string_value(&config.machine, "change_filament_gcode")
            .unwrap_or_default(),
        machine_pause_gcode: read_string_value(&config.machine, "machine_pause_gcode")
            .unwrap_or_default(),
        time_lapse_gcode: read_string_value(&config.machine, "time_lapse_gcode")
            .unwrap_or_default(),
        change_extrusion_role_gcode: read_string_value(
            &config.machine,
            "change_extrusion_role_gcode",
        )
        .unwrap_or_default(),
        printing_by_object_gcode: read_string_value(&config.machine, "printing_by_object_gcode")
            .unwrap_or_default(),
    };
    idex.validate_slot_lengths()?;
    Ok(idex)
}

#[derive(Deserialize)]
struct PresetDeleteParams {
    preset_id: PresetId,
}

#[derive(Deserialize)]
struct ObjectSetOverrideParams {
    object_id: ObjectId,
    key: SettingKey,
    /// `null` clears the override, any value sets it.
    #[serde(default)]
    value: Option<SettingValue>,
}

#[derive(Deserialize)]
struct ObjectSetSeamFacetsParams {
    object_id: ObjectId,
    #[serde(default)]
    facets_rle: Option<String>,
    #[serde(default)]
    target_point: Option<String>,
}

#[derive(Deserialize)]
struct ObjectSetSupportFacetsParams {
    object_id: ObjectId,
    #[serde(default)]
    facets_rle: Option<String>,
}

#[derive(Deserialize)]
struct ObjectSetFanFacetsParams {
    object_id: ObjectId,
    /// `null` (or absent) clears the mask.
    #[serde(default)]
    facets_rle: Option<String>,
}
#[derive(Deserialize)]
struct ObjectSetFilamentFacetsParams {
    object_id: ObjectId,
    /// `null` (or absent) clears the mask.
    #[serde(default)]
    facets_rle: Option<String>,
}

#[derive(Deserialize)]
struct ObjectSetLayerHeightProfileParams {
    object_id: ObjectId,
    #[serde(default)]
    profile: Vec<f32>,
}

#[derive(Deserialize)]
struct ObjectSetLayerRangesParams {
    object_id: ObjectId,
    #[serde(default)]
    ranges: Vec<slicer_types::project::LayerConfigRange>,
}

#[derive(Deserialize)]
struct ObjectSetOperationsParams {
    object_id: ObjectId,
    /// Idempotent: the full desired op stack, every call. An empty list
    /// clears the stack back to "no ops" (slice reads the raw source).
    #[serde(default)]
    operations: Vec<slicer_types::FeatureOp>,
}

/// A pathologically long profile would mean one control point per
/// micron — reject anything past a generous ceiling.
const MAX_LAYER_HEIGHT_PROFILE_LEN: usize = 200_000;

const MAX_FACETS_RLE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TARGET_POINT_BYTES: usize = 256;

/// Size cap and base64 check shared by the facet-paint setters, so a
/// malformed blob is refused here instead of persisting to disk and surfacing
/// later as a cryptic FFI error.
fn validate_filament_facets_rle(rle: Option<&str>) -> Result<(), RpcError> {
    validate_facets_rle(rle)?;
    let Some(rle) = rle else {
        return Ok(());
    };
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD
        .decode(rle)
        .map_err(|_| RpcError::Project("facets_rle is not valid base64".into()))?;
    if !data.len().is_multiple_of(5) {
        return Err(RpcError::Project(
            "filament paint requires 5-byte RLE runs".into(),
        ));
    }
    let mut total: u64 = 0;
    for run in data.as_chunks::<5>().0 {
        let count = u32::from_le_bytes(run[1..5].try_into().unwrap());
        total += u64::from(count);
        if run[0] > 16 || count == 0 || total > 20_000_000 {
            return Err(RpcError::Project(
                "invalid filament paint value, run or facet count".into(),
            ));
        }
    }
    Ok(())
}

fn validate_facets_rle(facets_rle: Option<&str>) -> Result<(), RpcError> {
    let Some(rle) = facets_rle else {
        return Ok(());
    };
    if rle.len() > MAX_FACETS_RLE_BYTES {
        return Err(RpcError::Project(format!(
            "facets_rle exceeds {} bytes",
            MAX_FACETS_RLE_BYTES
        )));
    }
    use base64::Engine as _;
    if base64::engine::general_purpose::STANDARD
        .decode(rle.as_bytes())
        .is_err()
    {
        return Err(RpcError::Project(
            "facets_rle is not valid base64".to_string(),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct ObjectTransformParams {
    object_id: ObjectId,
    /// Row-major 4x4 matrix in object → world space.
    transform: [f32; 16],
}

#[derive(Deserialize)]
struct AdaptiveRuleSetParams {
    rule: AdaptiveRule,
}

#[derive(Deserialize)]
struct AdaptiveRuleIdParams {
    rule_id: String,
}

#[derive(Deserialize)]
struct ProjectSavePathParams {
    path: String,
    /// Take over a lock of another host. Same-host live-pid locks are
    /// never overridden via this flag; the user must close the other
    /// instance first.
    #[serde(default)]
    force: bool,
}

/// `force` on a lock of another host: take the lock over rather than read
/// past it — left in place, it would ask again next time and never be
/// released by this daemon.
/// «Open anyway» over another host's lock. The person asked to go on, so a
/// lock that cannot be written does not stop them; the reason goes to the log.
fn take_over_lock(path: &std::path::Path) -> Result<(), RpcError> {
    if let Err(e) = slicer_state::lockfile::take_over(path) {
        tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "project lock take-over failed; going on without it");
    }
    Ok(())
}

/// The project lock only warns about two windows editing one file. When it
/// cannot be read or written — a folder the OS will not let the daemon write a
/// sibling into (macOS ties folder access to the signature, and an ad-hoc
/// signed update loses it), read-only media, a share — opening and saving go
/// on without it, and the whole error chain (`{:#}`, with the OS reason) goes
/// to the log. The old `{e}` printed only the outermost context, «write
/// <path>», which hid why.
fn acquire_project_lock(path: &std::path::Path) -> slicer_state::lockfile::AcquireOutcome {
    match slicer_state::lockfile::try_acquire(path) {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "project lock unavailable; going on without it");
            slicer_state::lockfile::AcquireOutcome::Acquired
        }
    }
}

/// The name of the `#[cfg(test)]` arm of `Dispatcher::handle` that is not in
/// `METHODS`.
#[cfg(test)]
const TEST_UNLISTED_METHOD: &str = "test.unlisted_arm";

/// What a client gets for a method the daemon does not answer. The text and
/// the code (-32603) are what clients have always seen; keep them as they are.
fn method_not_found(method: &str) -> RpcError {
    RpcError::Internal(format!("method not found: {method}"))
}

impl Dispatcher {
    /// Where crashed sessions are looked for: next to this session's own
    /// autosave, or under the app data when it runs without one.
    fn autosave_root(&self) -> Result<std::path::PathBuf, RpcError> {
        match self.state.autosave() {
            Some(sink) => Ok(sink.root().to_path_buf()),
            None => slicer_state::autosave::default_root()
                .map_err(|e| RpcError::Project(format!("autosave directory unknown: {e}"))),
        }
    }

    pub async fn handle(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        // The public surface is `METHODS`, not whatever arms the `match`
        // below happens to have: a method that is not listed there is
        // refused here with the very error the `_` arm gives, so an arm
        // added without a line in `METHODS` (and in the daemon's
        // `PUBLIC_SURFACE.toml`) is unreachable rather than quietly public.
        if !METHODS.contains(&method) {
            return Err(method_not_found(method));
        }
        match method {
            // Lightweight capability introspection through the
            // dispatcher itself — no auth/session semantics, ignores
            // `params`. The real client-facing `hello` handshake (token
            // check, protocol version negotiation, `session_id`) lives in
            // `server.rs`'s `handle_socket`, entirely before any message
            // reaches this dispatcher; this arm exists so `capabilities()`
            // can be asserted directly (e.g. tests/object_get_edit_mesh.rs)
            // without spinning up a WS server. Both entry points read the
            // same `capabilities()` list and `build_info`, so they can't
            // drift apart.
            "hello" => Ok(json!({
                "capabilities": capabilities(),
                "daemon_commit": build_info::DAEMON_COMMIT,
                "libslic3r_commit": build_info::LIBSLIC3R_COMMIT,
                "daemon_dirty": build_info::DIRTY,
            })),
            "state.snapshot" => Ok(json!({
                "autosave": self.state.autosave_status(),
                "current_event_id": self.state.current_event_id(),
                "can_undo": self.state.can_undo(),
                "can_redo": self.state.can_redo(),
            })),
            "project.undo" => match self.state.undo() {
                Some(envelope) => Ok(json!({
                    "applied": true,
                    "event_id": envelope.event_id,
                    "can_undo": self.state.can_undo(),
                    "can_redo": self.state.can_redo(),
                })),
                None => Ok(json!({
                    "applied": false,
                    "can_undo": false,
                    "can_redo": self.state.can_redo(),
                })),
            },
            "project.redo" => match self.state.redo() {
                Some(envelope) => Ok(json!({
                    "applied": true,
                    "event_id": envelope.event_id,
                    "can_undo": self.state.can_undo(),
                    "can_redo": self.state.can_redo(),
                })),
                None => Ok(json!({
                    "applied": false,
                    "can_undo": self.state.can_undo(),
                    "can_redo": false,
                })),
            },
            "state.events_since" => {
                let p: EventsSinceParams = serde_json::from_value(params).unwrap_or_default();
                let events = self.state.events_since(slicer_types::EventId(p.since));
                Ok(json!({ "events": events }))
            }
            "events.subscribe" => Ok(json!({ "subscription_id": "sub-default" })),
            "events.unsubscribe" => Ok(json!({})),
            "project.new" => {
                let p: ProjectNewParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let meta = ProjectMeta {
                    title: p.title,
                    description: p.description,
                    created_at: now,
                    modified_at: now,
                    schema_version: 1,
                };
                // A new project has no file. Let go of the previous one:
                // nobody has it open any more, and a kept lock would turn
                // other instances away from it.
                if let Some(prev) = self.state.current_path() {
                    let _ = slicer_state::lockfile::release(&prev);
                }
                self.state.set_current_path(None);
                self.state
                    .apply(ProjectEvent::ProjectCreated { meta: meta.clone() });
                Ok(json!({ "meta": meta }))
            }
            "preset.activate" => {
                let p: PresetActivateParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let snap = self.state.snapshot();
                let preset = snap.preset_library.get(&p.preset_id).ok_or_else(|| {
                    RpcError::Project(format!("preset not found: {:?}", p.preset_id))
                })?;
                let scope = scope_for_kind(p.kind);
                if preset.scope != scope {
                    return Err(RpcError::Project(format!(
                        "preset {:?} has scope {:?}, cannot activate as {:?}",
                        p.preset_id, preset.scope, p.kind
                    )));
                }
                // Apply each value in the preset as a SettingChanged event.
                // Every value the preset changes, as one step of history.
                let preset_values = preset.values.clone();
                self.state.transaction(|txn| {
                    for (key, new_value) in preset_values {
                        let old_value = snap.settings.get(&(scope, key.clone())).cloned();
                        if old_value.as_ref() == Some(&new_value) {
                            continue;
                        }
                        txn.apply(ProjectEvent::SettingChanged {
                            scope,
                            key,
                            old_value,
                            new_value,
                        });
                    }
                    txn.apply(ProjectEvent::PresetActivated {
                        kind: p.kind,
                        preset_id: p.preset_id,
                    });
                });
                Ok(json!({}))
            }
            // Backend-2 geometry creation. Pure Rust, no FFI, so unlike
            // the object.* arms this needs no spawn_blocking for a C++
            // call — but a large tree is still CPU-bound for tens of
            // milliseconds, and blocking the dispatcher task would queue
            // every other RPC on this connection behind it.
            "object.serial_candidates" | "object.set_serial_marking" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Params {
                    object_id: ObjectId,
                    #[serde(default)]
                    template: Option<crate::serial::Template>,
                    #[serde(default)]
                    height_mm: Option<f64>,
                    #[serde(default)]
                    depth_mm: Option<f64>,
                    #[serde(default)]
                    min_wall_mm: Option<f64>,
                    #[serde(default)]
                    layout: slicer_geometry::serial::SerialLayout,
                }
                let p: Params =
                    serde_json::from_value(params).map_err(|e| RpcError::Project(e.to_string()))?;
                let snap = self.state.snapshot();
                let object = snap
                    .objects
                    .get(&p.object_id)
                    .ok_or_else(|| RpcError::Project("Object not found".into()))?;
                if method == "object.set_serial_marking" && p.template.is_none() {
                    self.state.apply(ProjectEvent::ObjectOverrideSet {
                        object_id: p.object_id,
                        key: crate::serial::TEMPLATE_KEY.into(),
                        new_value: None,
                    });
                    return Ok(json!({}));
                }
                crate::serial::eligible(object)?;
                let path = if is_step_extension(&object.source_path) {
                    self.ensure_step_edit_mesh(object)
                        .await?
                        .0
                        .to_string_lossy()
                        .into_owned()
                } else {
                    object.source_path.clone()
                };
                if method == "object.serial_candidates" {
                    return tokio::task::spawn_blocking(move || {
                        let (mesh, hash) = crate::serial::load(&path)?;
                        let candidates = slicer_geometry::serial::candidates(
                            &mesh,
                            p.height_mm.unwrap_or(3.5),
                            p.depth_mm.unwrap_or(0.4),
                            p.min_wall_mm.unwrap_or(1.0),
                            p.layout,
                        )
                        .map_err(RpcError::Project)?;
                        Ok(json!({"candidates":candidates,"mesh_path":path,"mesh_sha256":hash}))
                    })
                    .await
                    .map_err(|e| RpcError::Internal(e.to_string()))?;
                }
                let template = p.template.unwrap();
                template.validate()?;
                let test = template.clone();
                tokio::task::spawn_blocking(move || {
                    let (mesh, hash) = crate::serial::load(&path)?;
                    if hash != test.mesh_sha256 {
                        return Err(RpcError::Project(
                            "Model changed; find placements again".into(),
                        ));
                    }
                    slicer_geometry::serial::engrave(&mesh, &test.mark("000000".into()))
                        .map_err(RpcError::Project)?;
                    Ok(())
                })
                .await
                .map_err(|e| RpcError::Internal(e.to_string()))??;
                self.state.apply(ProjectEvent::ObjectOverrideSet {
                    object_id: p.object_id,
                    key: crate::serial::TEMPLATE_KEY.into(),
                    new_value: Some(SettingValue::String(
                        serde_json::to_string(&template).unwrap(),
                    )),
                });
                Ok(json!({"template":template}))
            }
            "geometry.engrave_serial" => {
                tokio::task::spawn_blocking(move || crate::serial::handle(params))
                    .await
                    .map_err(|e| RpcError::Project(format!("serial marking task failed: {e}")))?
            }
            "geometry.build" => {
                // From the tree alone: a `recipe` is the client's own, and an
                // `object_id` in it is not a reference to resolve.
                let tree = params.get("tree").unwrap_or(&Value::Null);
                let meshes = self.measured_meshes(method, tree).await?;
                let state = self.state.clone();
                tokio::task::spawn_blocking(move || {
                    crate::geometry::handle_build(&state, &meshes, params)
                })
                .await
                .map_err(|e| RpcError::Project(format!("geometry task failed: {e}")))?
            }
            // CAD parts. A program can run kernels for seconds (OCCT, mcut on
            // a large mesh), so it runs on the blocking pool like
            // geometry.build; reads answer from the snapshot directly.
            "part.create" | "part.create_from_object" | "part.set_document" | "part.remove" => {
                let state = self.state.clone();
                let method = method.to_string();
                tokio::task::spawn_blocking(move || match method.as_str() {
                    "part.create" => crate::part::handle_create(&state, params),
                    "part.create_from_object" => {
                        crate::part::handle_create_from_object(&state, params)
                    }
                    "part.set_document" => crate::part::handle_set_document(&state, params),
                    "part.remove" => crate::part::handle_remove(&state, params),
                    // The outer pattern names exactly the four methods above. One
                    // added there and not here must fail loudly, not run as the
                    // removal of a part.
                    other => Err(RpcError::Internal(format!(
                        "part method {other} is routed to the blocking pool but has no handler"
                    ))),
                })
                .await
                .map_err(|e| RpcError::Project(format!("part task failed: {e}")))?
            }
            "part.get" => crate::part::handle_get(&self.state, params),
            "parts.list" => crate::part::handle_list(&self.state),
            "object.add" => {
                let p: ObjectAddParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let object_id = ObjectId(format!("obj-{}", Ulid::new()));
                self.state.apply(ProjectEvent::ObjectAdded {
                    object_id: object_id.clone(),
                    name: p.name.clone(),
                    source_path: p.source_path.clone(),
                    triangle_count: p.triangle_count,
                    bounds: p.bounds.clone(),
                    source_hash: p.source_hash.clone(),
                    // `object.add` is the legacy direct-add path used by tests
                    // and the UI when the caller already has mesh metadata in
                    // hand. STEP fan-out happens in `object.import_local`.
                    step_part: None,
                    split_part: None,
                    transform: None,
                });
                Ok(json!({
                    "object_id": object_id,
                    "name": p.name,
                    "triangle_count": p.triangle_count,
                    "bounds": p.bounds,
                }))
            }
            "object.import_local" => {
                let p: ObjectImportLocalParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                // inspect_model crosses the C++ FFI and can take seconds on a
                // large STEP — same shape as run_slice (see line 2189). Without
                // spawn_blocking it parks this connection's dispatcher task,
                // queueing every other RPC behind it. Audit finding
                // `rpc-clients-no-timeouts-pending-leak`.
                let source_path = p.source_path.clone();
                let inspected = tokio::task::spawn_blocking(move || {
                    // See STEP_FFI_LOCK: OCCT's STEP reader is not
                    // thread-safe; serialize concurrent STEP parses
                    // across all FFI call sites. Poisoning is safe to
                    // swallow — the lock guards no mutable state, only
                    // OCCT re-entry.
                    let _step_lock = STEP_FFI_LOCK
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    slicer_ffi::inspect_model(&source_path)
                })
                .await
                .map_err(|e| RpcError::Project(format!("inspect task failed: {e}")))?;
                if !inspected.success {
                    return Err(RpcError::Project(format!(
                        "import failed: {}",
                        inspected.error_message
                    )));
                }
                if inspected.parts.is_empty() {
                    return Err(RpcError::Project(
                        "import failed: inspect_model returned no parts".to_string(),
                    ));
                }

                let source_bytes = std::fs::read(&p.source_path)
                    .map_err(|e| RpcError::Project(format!("import failed: {e}")))?;
                let source_hash = hex::encode(Sha256::digest(&source_bytes));
                let basename = std::path::Path::new(&p.source_path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("object")
                    .to_string();
                let is_step = is_step_extension(&p.source_path);

                // Fan out: one ObjectAdded per part. For STL/3MF and
                // single-part STEPs `parts.len() == 1`, so the wire
                // behaviour for existing callers is identical except
                // for the response shape (`object_id` → `object_ids`).
                // One import is one step of history, however many parts a
                // STEP fans out into.
                let (object_ids, objects_summary) = self.state.transaction(|txn| {
                    let mut object_ids: Vec<ObjectId> = Vec::with_capacity(inspected.parts.len());
                    let mut objects_summary: Vec<Value> = Vec::with_capacity(inspected.parts.len());
                    for part in inspected.parts {
                        // Use the libslic3r-resolved part name when present;
                        // libslic3r emits "" for STL-derived objects, so fall
                        // back to the source basename in that case.
                        let display_name = if !part.part_name.is_empty() {
                            part.part_name.clone()
                        } else {
                            basename.clone()
                        };
                        let bounds = Bounds {
                            min: part.bounds_min,
                            max: part.bounds_max,
                        };
                        let object_id = ObjectId(format!("obj-{}", Ulid::new()));
                        let step_part = if is_step {
                            Some(slicer_types::StepPartPayload {
                                part_name: part.part_name.clone(),
                                part_index: part.part_index,
                            })
                        } else {
                            None
                        };

                        txn.apply(ProjectEvent::ObjectAdded {
                            object_id: object_id.clone(),
                            name: display_name.clone(),
                            source_path: p.source_path.clone(),
                            triangle_count: part.triangle_count,
                            bounds: bounds.clone(),
                            source_hash: source_hash.clone(),
                            step_part: step_part.clone(),
                            split_part: None,
                            transform: None,
                        });

                        objects_summary.push(json!({
                            "object_id": object_id,
                            "name": display_name,
                            "triangle_count": part.triangle_count,
                            "bounds": bounds,
                            "step_part": step_part,
                        }));
                        object_ids.push(object_id);
                    }
                    (object_ids, objects_summary)
                });

                Ok(json!({
                    "object_ids": object_ids,
                    "source_path": p.source_path,
                    "source_hash": source_hash,
                    "objects": objects_summary,
                }))
            }
            "object.query_faces" => {
                let p: ObjectQueryFacesParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let snap = self.state.snapshot();
                let obj = snap.objects.get(&p.object_id).ok_or_else(|| {
                    RpcError::Project(format!("object_id not found: {:?}", p.object_id))
                })?;
                // Only STEP sources carry B-rep topology. An STL has its flat
                // faces grown back from the triangles (`"source": "mesh"`,
                // which the window's B-rep path ignores). Anything else has
                // nothing to say — a normal answer, not an error, so the UI
                // can call this unconditionally on selection. Read-only arm:
                // no events, no undo entry.
                if !is_step_extension(&obj.source_path) {
                    return Ok(mesh_face_report(obj).await);
                }
                let (_stl_path, map_path) = self.ensure_step_edit_mesh(obj).await?;
                let faces = read_facemap_faces(&map_path).map_err(RpcError::Project)?;
                Ok(json!({ "source": "brep", "faces": faces }))
            }
            "object.get_edit_mesh" => {
                let p: ObjectGetEditMeshParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let snap = self.state.snapshot();
                let obj = snap.objects.get(&p.object_id).ok_or_else(|| {
                    RpcError::Project(format!("object_id not found: {:?}", p.object_id))
                })?;
                // Non-STEP sources (STL/3MF, split-derived STL parts)
                // have no derived edit mesh to materialize — the UI
                // loads the source file directly. Read-only arm: no
                // events, no undo entry.
                if !is_step_extension(&obj.source_path) {
                    return Ok(json!({
                        "mesh_path": obj.source_path,
                        "face_map_path": serde_json::Value::Null,
                    }));
                }
                let (stl_path, map_path) = self.ensure_step_edit_mesh(obj).await?;
                Ok(json!({
                    "mesh_path": stl_path.to_string_lossy(),
                    "face_map_path": map_path.to_string_lossy(),
                }))
            }
            "object.split_to_parts" => {
                let p: ObjectSplitToPartsParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let snap = self.state.snapshot();
                let obj = snap.objects.get(&p.object_id).ok_or_else(|| {
                    RpcError::Project(format!("object_id not found: {:?}", p.object_id))
                })?;
                let source_path = obj.source_path.clone();
                let source_hash = obj.source_hash.clone();
                let source_transform = obj.transform;

                // Per-source-hash cache root; the C++ side calls
                // `std::filesystem::create_directories` itself but mirror
                // it here so the path exists even if the FFI is stubbed.
                let cache_root = slicer_state::object_cache_dir()
                    .map_err(|e| RpcError::Project(format!("split failed: {e}")))?;
                let output_dir = cache_root.join(&source_hash);
                std::fs::create_dir_all(&output_dir)
                    .map_err(|e| RpcError::Project(format!("split failed: {e}")))?;

                // Same reasoning as inspect_model above: split crosses the C++
                // bridge and may take seconds. spawn_blocking keeps the
                // dispatcher responsive.
                let split_source_path = source_path.clone();
                let split_output_dir = output_dir.to_string_lossy().into_owned();
                let outcome = tokio::task::spawn_blocking(move || {
                    // See STEP_FFI_LOCK: OCCT's STEP reader is not
                    // thread-safe; serialize concurrent STEP parses
                    // across all FFI call sites. Poisoning is safe to
                    // swallow — the lock guards no mutable state, only
                    // OCCT re-entry.
                    let _step_lock = STEP_FFI_LOCK
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    slicer_ffi::split_model_to_files(&split_source_path, &split_output_dir)
                })
                .await
                .map_err(|e| RpcError::Project(format!("split task failed: {e}")))?;
                if !outcome.success {
                    // Pass the daemon's `error_message` through verbatim
                    // so UI substring matching ("single shell") works.
                    return Err(RpcError::Project(format!(
                        "split failed: {}",
                        outcome.error_message
                    )));
                }
                let parts_count = outcome.parts.len() as u32;
                if parts_count <= 1 {
                    // Defense in depth: the FFI should have returned
                    // success=false with "single shell" already, but be
                    // safe in case a future bridge change drops that.
                    return Err(RpcError::Project("split failed: single shell".to_string()));
                }

                let basename = std::path::Path::new(&source_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("object")
                    .to_string();

                // First pass: collect every child's data. Anything fallible
                // happens here so that the apply loop below can't fail
                // partway through and leave the state half-split. The spec
                // requires the source -> parts replacement to be atomic
                // from the project's point of view; the transaction below
                // holds the project mutex for the whole replacement, and no
                // fallible step can wedge it partway.
                struct PreparedChild {
                    object_id: ObjectId,
                    event: ProjectEvent,
                }
                let mut prepared: Vec<PreparedChild> = Vec::with_capacity(outcome.parts.len());
                for part in &outcome.parts {
                    let new_oid = ObjectId(format!("obj-{}", Ulid::new()));
                    let part_bytes = std::fs::read(&part.out_path)
                        .map_err(|e| RpcError::Project(format!("split failed: {e}")))?;
                    let part_hash = hex::encode(Sha256::digest(&part_bytes));
                    let display_name = format!("{} - Part {}", basename, part.part_index + 1);
                    let bounds = Bounds {
                        min: part.bounds_min,
                        max: part.bounds_max,
                    };
                    prepared.push(PreparedChild {
                        object_id: new_oid.clone(),
                        event: ProjectEvent::ObjectAdded {
                            object_id: new_oid,
                            name: display_name,
                            source_path: part.out_path.clone(),
                            triangle_count: part.triangle_count,
                            bounds,
                            source_hash: part_hash,
                            step_part: None,
                            split_part: Some(slicer_types::SplitPartPayload {
                                source_object_id: p.object_id.clone(),
                                part_index: part.part_index,
                                part_count: parts_count,
                            }),
                            transform: Some(source_transform),
                        },
                    });
                }

                // Second pass: infallible apply. From here on, no early
                // returns or errors — the state transition either fully
                // happens or (if a panic landed earlier) didn't start.
                // One step of history: undo takes the children off and puts
                // the parent back in a single Cmd+Z.
                let new_object_ids: Vec<ObjectId> = self.state.transaction(|txn| {
                    let mut ids = Vec::with_capacity(prepared.len());
                    for child in prepared {
                        txn.apply(child.event);
                        ids.push(child.object_id);
                    }
                    txn.apply(ProjectEvent::ObjectRemoved {
                        object_id: p.object_id.clone(),
                    });
                    ids
                });

                Ok(json!({
                    "source_object_id": p.object_id,
                    "new_object_ids": new_object_ids,
                }))
            }
            "object.cut_with_plane" => {
                let p = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                crate::cut::cut_with_plane(&self.state, p).await
            }
            "object.remove" => {
                let p: ObjectRemoveParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let snap = self.state.snapshot();
                if !snap.objects.contains_key(&p.object_id) {
                    return Err(RpcError::Project(format!(
                        "object_id not found: {:?}",
                        p.object_id
                    )));
                }
                self.state.apply(ProjectEvent::ObjectRemoved {
                    object_id: p.object_id,
                });
                Ok(json!({}))
            }
            "object.transform" => {
                let p: ObjectTransformParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let snap = self.state.snapshot();
                let obj = snap.objects.get(&p.object_id).ok_or_else(|| {
                    RpcError::Project(format!("object_id not found: {:?}", p.object_id))
                })?;
                let old_transform = obj.transform;
                if old_transform == p.transform {
                    return Ok(json!({ "changed": false }));
                }
                self.state.apply(ProjectEvent::ObjectTransformed {
                    object_id: p.object_id,
                    old_transform,
                    new_transform: p.transform,
                });
                Ok(json!({ "changed": true }))
            }
            "object.set_override" => {
                let p: ObjectSetOverrideParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                // Stored under the key clients read and clear, so a legacy
                // spelling (`perimeter_count`) cannot outlive a cleared override.
                let key = canonical_override_key(&p.key).to_string();
                if !is_overridable(&key) {
                    return Err(RpcError::Project(format!(
                        "key '{key}' is not overridable per-object"
                    )));
                }
                let snap = self.state.snapshot();
                if !snap.objects.contains_key(&p.object_id) {
                    return Err(RpcError::Project(format!(
                        "object_id not found: {:?}",
                        p.object_id
                    )));
                }
                if let Some(ref v) = p.value {
                    if key == "extruder" {
                        // Per-object extruder is a libslic3r ModelObject config
                        // key (1-based), not a schema Process setting, so the
                        // schema-driven validate_setting has no def for it.
                        // Validate inline: a positive 1-based integer.
                        match v {
                            SettingValue::Int(n) if (1..=16).contains(n) => {}
                            _ => {
                                return Err(RpcError::Project(format!(
                                    "extruder must be a 1-based integer from 1 to 16, got {v:?}"
                                )));
                            }
                        }
                    } else if let Some(constraint) = object_override_constraint(&key) {
                        validate_constraint(&key, &constraint, v)?;
                    } else {
                        validate_setting(&SettingScope::Process, &key, v)?;
                    }
                }
                self.state.apply(ProjectEvent::ObjectOverrideSet {
                    object_id: p.object_id,
                    key,
                    new_value: p.value,
                });
                Ok(json!({}))
            }
            "object.set_seam_facets" => {
                let p: ObjectSetSeamFacetsParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                validate_facets_rle(p.facets_rle.as_deref())?;
                if let Some(ref tp) = p.target_point {
                    if tp.len() > MAX_TARGET_POINT_BYTES {
                        return Err(RpcError::Project(format!(
                            "target_point exceeds {} bytes",
                            MAX_TARGET_POINT_BYTES
                        )));
                    }
                }
                let snap = self.state.snapshot();
                if !snap.objects.contains_key(&p.object_id) {
                    return Err(RpcError::Project(format!(
                        "object_id not found: {:?}",
                        p.object_id
                    )));
                }
                self.state.apply(ProjectEvent::ObjectSeamFacetsSet {
                    object_id: p.object_id,
                    facets_rle: p.facets_rle,
                    target_point: p.target_point,
                });
                Ok(json!({}))
            }
            "object.set_support_facets" => {
                let p: ObjectSetSupportFacetsParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                // DIAG(support-paint): trace incoming RLE while we
                // verify the paint round-trip — remove once verified.
                eprintln!(
                    "[support-paint] daemon got set_support_facets object_id={:?} rle_len={}",
                    p.object_id,
                    p.facets_rle.as_ref().map(|r| r.len()).unwrap_or(0),
                );
                validate_facets_rle(p.facets_rle.as_deref())?;
                let snap = self.state.snapshot();
                if !snap.objects.contains_key(&p.object_id) {
                    return Err(RpcError::Project(format!(
                        "object_id not found: {:?}",
                        p.object_id
                    )));
                }
                self.state.apply(ProjectEvent::ObjectSupportFacetsSet {
                    object_id: p.object_id,
                    facets_rle: p.facets_rle,
                });
                Ok(json!({}))
            }
            "object.set_fan_facets" => {
                let p: ObjectSetFanFacetsParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                // Values above 147 are refused by the bridge, like the other masks'.
                validate_facets_rle(p.facets_rle.as_deref())?;
                let snap = self.state.snapshot();
                if !snap.objects.contains_key(&p.object_id) {
                    return Err(RpcError::Project(format!(
                        "object_id not found: {:?}",
                        p.object_id
                    )));
                }
                self.state.apply(ProjectEvent::ObjectFanFacetsSet {
                    object_id: p.object_id,
                    facets_rle: p.facets_rle,
                });
                Ok(json!({}))
            }
            "object.set_filament_facets" => {
                let p: ObjectSetFilamentFacetsParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                // Material 0 is unpainted; 1..16 are the virtual filament slots.
                validate_filament_facets_rle(p.facets_rle.as_deref())?;
                let snap = self.state.snapshot();
                if !snap.objects.contains_key(&p.object_id) {
                    return Err(RpcError::Project(format!(
                        "object_id not found: {:?}",
                        p.object_id
                    )));
                }
                self.state.apply(ProjectEvent::ObjectFilamentFacetsSet {
                    object_id: p.object_id,
                    facets_rle: p.facets_rle,
                });
                Ok(json!({}))
            }
            "object.set_layer_height_profile" => {
                let p: ObjectSetLayerHeightProfileParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                if !p.profile.len().is_multiple_of(2) {
                    return Err(RpcError::Project(
                        "layer_height_profile length must be even (z,height pairs)".to_string(),
                    ));
                }
                if p.profile.len() > MAX_LAYER_HEIGHT_PROFILE_LEN {
                    return Err(RpcError::Project(format!(
                        "layer_height_profile exceeds {} entries",
                        MAX_LAYER_HEIGHT_PROFILE_LEN
                    )));
                }
                for chunk in p.profile.as_chunks::<2>().0 {
                    if !chunk[0].is_finite() || !chunk[1].is_finite() || chunk[1] <= 0.0 {
                        return Err(RpcError::Project(
                            "layer_height_profile has a non-finite or non-positive value"
                                .to_string(),
                        ));
                    }
                }
                let snap = self.state.snapshot();
                if !snap.objects.contains_key(&p.object_id) {
                    return Err(RpcError::Project(format!(
                        "object_id not found: {:?}",
                        p.object_id
                    )));
                }
                self.state.apply(ProjectEvent::ObjectLayerHeightProfileSet {
                    object_id: p.object_id,
                    profile: p.profile,
                });
                Ok(json!({}))
            }
            "object.set_layer_ranges" => {
                let p: ObjectSetLayerRangesParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;

                // Validate: each range well-formed.
                for r in &p.ranges {
                    if !r.z_min.is_finite() || !r.z_max.is_finite() {
                        return Err(RpcError::Project(format!(
                            "range {}: z bounds must be finite",
                            r.id
                        )));
                    }
                    if r.z_min < 0.0 {
                        return Err(RpcError::Project(format!(
                            "range {}: z_min must be >= 0",
                            r.id
                        )));
                    }
                    if r.z_min >= r.z_max {
                        return Err(RpcError::Project(format!(
                            "range {}: require z_min < z_max",
                            r.id
                        )));
                    }
                    for (key, value) in &r.overrides {
                        if !slicer_types::settings::is_layer_range_overridable(key) {
                            return Err(RpcError::Project(format!(
                                "range {}: key '{}' not allowed per layer range",
                                r.id, key
                            )));
                        }
                        slicer_types::settings::validate_layer_range_value(key, value)
                            .map_err(|e| RpcError::Project(format!("range {}: {e}", r.id)))?;
                    }
                    if let Some(fan) = &r.fan {
                        fan.validate()
                            .map_err(|e| RpcError::Project(format!("range {}: {e}", r.id)))?;
                    }
                }

                // Validate: pairwise non-overlapping. Sort indices by z_min.
                let mut sorted: Vec<&slicer_types::project::LayerConfigRange> =
                    p.ranges.iter().collect();
                sorted.sort_by(|a, b| {
                    a.z_min
                        .partial_cmp(&b.z_min)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                for w in sorted.windows(2) {
                    if w[0].z_max > w[1].z_min {
                        return Err(RpcError::Project(format!(
                            "ranges {} and {} overlap",
                            w[0].id, w[1].id
                        )));
                    }
                }

                // Verify object exists.
                let snap = self.state.snapshot();
                if !snap.objects.contains_key(&p.object_id) {
                    return Err(RpcError::Project(format!(
                        "object_id not found: {:?}",
                        p.object_id
                    )));
                }

                self.state.apply(ProjectEvent::ObjectLayerRangesSet {
                    object_id: p.object_id,
                    ranges: p.ranges,
                });
                Ok(json!({}))
            }
            "object.set_operations" => {
                let p: ObjectSetOperationsParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let snap = self.state.snapshot();
                let obj = snap.objects.get(&p.object_id).ok_or_else(|| {
                    RpcError::Project(format!("object_id not found: {:?}", p.object_id))
                })?;
                // A body's holes are its part's, not the object's: refused
                // before anything is cut.
                crate::part::reject_operations_on_body(obj)?;
                let materialized =
                    materialize_operations("set_operations", obj, &p.operations).await?;

                // Persist the stack (undoable — see `classify` in slicer-state).
                // Recorded even when no op is active so an all-suppressed or
                // emptied stack is itself undoable.
                record_operations(&self.state, p.object_id, p.operations)?;

                Ok(materialized.into_json())
            }
            "object.get_materialized_mesh" => {
                let p: ObjectGetMaterializedMeshParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let snap = self.state.snapshot();
                let obj = snap.objects.get(&p.object_id).ok_or_else(|| {
                    RpcError::Project(format!("object_id not found: {:?}", p.object_id))
                })?;
                // The mesh of the stack the object already has — what a
                // client rebuilding its scene after `ProjectReplaced` shows.
                // Recut when the ops cache lost it, so the slice reads the cut
                // again too. Read-only arm: no events, no undo entry — a
                // freshly opened project must not look edited. Like
                // `object.set_operations`, a recut runs inside the call, and
                // the connection answers nothing else meanwhile
                // (`handle_socket`): a client asks for its parts' cuts one at a
                // time, not all at once.
                let materialized =
                    materialize_operations("get_materialized_mesh", obj, &obj.operations).await?;
                Ok(materialized.into_json())
            }
            "slice.request" => {
                let p: SliceRequestParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                gcode_header::producer_line(p.producer_line.as_deref())
                    .map_err(|e| RpcError::Slicing(format!("invalid producer_line: {e}")))?;
                let preview = crate::preview::preview_payload(
                    p.preview_png_base64.as_deref(),
                    p.preview_width,
                    p.preview_height,
                )
                .and_then(|payload| {
                    payload
                        .as_ref()
                        .map(crate::preview::decode_scene_preview)
                        .transpose()
                })
                .map_err(|e| RpcError::Slicing(format!("invalid preview: {e}")))?;
                let context = slicer_ffi::SliceContext {
                    producer_line: p.producer_line,
                    preview,
                    is_bbl_machine: p.is_bbl_machine,
                };
                let fan_lead = p.fan_lead.unwrap_or_default();
                fan_lead
                    .validate()
                    .map_err(|e| RpcError::Slicing(format!("invalid fan_lead: {e}")))?;
                if let Some(test) = &p.calibration_test {
                    test.validate()
                        .map_err(|e| RpcError::Slicing(format!("invalid calibration_test: {e}")))?;
                }
                let calibration_test = p.calibration_test;
                let resolved_idex = if let Some(ref config) = p.resolved_config {
                    Some(idex_from_resolved_config(config).map_err(|e| {
                        RpcError::Slicing(format!("invalid resolved_config payload: {e}"))
                    })?)
                } else {
                    None
                };
                if let Some(ref idex) = p.idex {
                    idex.validate_slot_lengths()
                        .map_err(|e| RpcError::Slicing(format!("invalid idex payload: {e}")))?;
                }
                let _plate_id = p.plate_id.unwrap_or(0);
                let snap = self.state.snapshot();
                if snap.objects.is_empty() {
                    return Err(RpcError::Slicing(
                        "no objects on plate to slice".to_string(),
                    ));
                }
                let mut serial_paths = BTreeMap::new();
                for object in snap.objects.values() {
                    if !object.overrides.contains_key(crate::serial::TEMPLATE_KEY) {
                        continue;
                    }
                    crate::serial::eligible(object)?;
                    let path = if is_step_extension(&object.source_path) {
                        self.ensure_step_edit_mesh(object)
                            .await?
                            .0
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        object.source_path.clone()
                    };
                    serial_paths.insert(object.id.0.clone(), path);
                }
                let (snap, run_id, serial_markings) = tokio::task::spawn_blocking(move || {
                    crate::serial::personalize(snap, p.traceability, serial_paths)
                })
                .await
                .map_err(|e| RpcError::Internal(e.to_string()))??;
                let job_id = format!("job-{}", Ulid::new());
                self.state.apply(ProjectEvent::SliceRequested {
                    job_id: job_id.clone(),
                });
                // Register the cancel handle BEFORE spawning the slicer
                // task — otherwise an immediate slice.cancel from the UI
                // could race past the insert and lose the cancellation.
                let cancel = CancelHandle::new();
                {
                    let mut tokens = self.cancel_tokens.lock().expect("cancel_tokens poisoned");
                    tokens.insert(job_id.clone(), cancel.clone());
                }
                let state = self.state.clone();
                let tokens_arc = self.cancel_tokens.clone();
                let last_slice_arc = self.last_slice.clone();
                let job = job_id.clone();
                let idex = p.idex.or(resolved_idex);
                let resolved_config = p.resolved_config;
                tokio::spawn(async move {
                    fake_slice(SliceTask {
                        state,
                        job_id: job.clone(),
                        cancel,
                        last_slice: last_slice_arc,
                        idex,
                        resolved_config,
                        fan_lead,
                        calibration_test,
                        context,
                        snap,
                        run_id,
                    })
                    .await;
                    // Slot is freed regardless of how the slice ended;
                    // future slice.cancel calls for this job_id correctly
                    // return "no such job".
                    if let Ok(mut tokens) = tokens_arc.lock() {
                        tokens.remove(&job);
                    }
                });
                Ok(
                    json!({ "job_id": job_id, "serial_markings":serial_markings, "engine_version":env!("CARGO_PKG_VERSION") }),
                )
            }
            "slice.cancel" => {
                #[derive(Deserialize)]
                struct SliceCancelParams {
                    job_id: String,
                }
                let p: SliceCancelParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Slicing(format!("invalid params: {e}")))?;
                let handle = {
                    let tokens = self.cancel_tokens.lock().expect("cancel_tokens poisoned");
                    tokens.get(&p.job_id).cloned()
                };
                match handle {
                    Some(h) => {
                        let was_first = h.cancel();
                        Ok(json!({
                            "job_id": p.job_id,
                            // True only on the *first* successful cancel —
                            // lets the UI distinguish "we just told it to
                            // stop" from "user double-clicked Cancel".
                            "requested": was_first,
                        }))
                    }
                    None => Err(RpcError::Slicing(format!(
                        "no in-flight slice with job_id={}",
                        p.job_id
                    ))),
                }
            }
            "settings.schema" => Ok(serde_json::to_value(SettingsSchema::baseline())
                .map_err(|e| RpcError::Internal(format!("schema serialization: {e}")))?),
            "setting.set" => {
                let p: SettingSetParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                validate_setting(&p.scope, &p.key, &p.value)?;
                let snap = self.state.snapshot();
                let old_value = snap.settings.get(&(p.scope, p.key.clone())).cloned();
                if old_value.as_ref() == Some(&p.value) {
                    return Ok(json!({ "changed": false }));
                }
                self.state.apply(ProjectEvent::SettingChanged {
                    scope: p.scope,
                    key: p.key,
                    old_value,
                    new_value: p.value,
                });
                Ok(json!({ "changed": true }))
            }
            "setting.get" => {
                let p: SettingGetParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let snap = self.state.snapshot();
                let value = snap.settings.get(&(p.scope, p.key.clone())).cloned();
                Ok(json!({
                    "scope": p.scope,
                    "key": p.key,
                    "value": value,
                }))
            }
            "setting.list" => {
                let snap = self.state.snapshot();
                let values: Vec<_> = snap
                    .settings
                    .iter()
                    .map(|((scope, key), value)| {
                        json!({ "scope": scope, "key": key, "value": value })
                    })
                    .collect();
                Ok(json!({ "values": values }))
            }
            "preset.list" => {
                let p: PresetListParams = serde_json::from_value(params).unwrap_or_default();
                let snap = self.state.snapshot();
                let mut presets: Vec<&Preset> = snap
                    .preset_library
                    .values()
                    .filter(|pr| p.scope.map(|s| s == pr.scope).unwrap_or(true))
                    .collect();
                // Stable order: vendor first, then user; within each, by name.
                presets.sort_by(|a, b| {
                    (a.source as u8, a.name.as_str()).cmp(&(b.source as u8, b.name.as_str()))
                });
                let active = json!({
                    "process": snap.presets.active_process,
                    "filament": snap.presets.active_filament,
                    "printer": snap.presets.active_printer,
                });
                Ok(json!({
                    "presets": presets,
                    "active": active,
                }))
            }
            "preset.save" => {
                let p: PresetSaveParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                if p.name.trim().is_empty() {
                    return Err(RpcError::Project("preset name is empty".into()));
                }
                let snap = self.state.snapshot();
                let values = match p.values {
                    Some(v) => {
                        for (key, value) in &v {
                            validate_setting(&p.scope, key, value)?;
                        }
                        v
                    }
                    None => snap
                        .settings
                        .iter()
                        .filter(|((sc, _), _)| *sc == p.scope)
                        .map(|((_, k), v)| (k.clone(), v.clone()))
                        .collect(),
                };
                let id = PresetId(format!("user-{}", Ulid::new()));
                let preset = Preset {
                    id: id.clone(),
                    name: p.name,
                    scope: p.scope,
                    source: PresetSource::User,
                    derived_from: p.derived_from,
                    values,
                };
                self.state.apply(ProjectEvent::PresetSaved {
                    preset: preset.clone(),
                });
                if p.activate {
                    self.state.apply(ProjectEvent::PresetActivated {
                        kind: kind_for_scope(p.scope),
                        preset_id: id.clone(),
                    });
                }
                Ok(json!({ "preset_id": id, "preset": preset }))
            }
            "preset.delete" => {
                let p: PresetDeleteParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let snap = self.state.snapshot();
                let preset = snap.preset_library.get(&p.preset_id).ok_or_else(|| {
                    RpcError::Project(format!("preset not found: {:?}", p.preset_id))
                })?;
                if preset.source == PresetSource::Vendor {
                    return Err(RpcError::Project("vendor presets cannot be deleted".into()));
                }
                // Don't leave dangling active reference: re-activate the
                // first remaining preset of the same scope (vendor preferred,
                // sorted by name) so the user keeps a valid configuration.
                let scope = preset.scope;
                self.state.apply(ProjectEvent::PresetDeleted {
                    preset_id: p.preset_id.clone(),
                });
                let active_ref = match scope {
                    SettingScope::Process => &snap.presets.active_process,
                    SettingScope::Filament => &snap.presets.active_filament,
                    SettingScope::Printer => &snap.presets.active_printer,
                };
                if active_ref == &p.preset_id {
                    let after = self.state.snapshot();
                    let mut candidates: Vec<&Preset> = after
                        .preset_library
                        .values()
                        .filter(|pr| pr.scope == scope)
                        .collect();
                    candidates.sort_by(|a, b| {
                        (a.source as u8, a.name.as_str()).cmp(&(b.source as u8, b.name.as_str()))
                    });
                    if let Some(next) = candidates.first() {
                        self.state.apply(ProjectEvent::PresetActivated {
                            kind: kind_for_scope(scope),
                            preset_id: next.id.clone(),
                        });
                    }
                }
                Ok(json!({}))
            }
            "project.save" => {
                let p: ProjectSavePathParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let path = std::path::PathBuf::from(&p.path);
                // Acquire lock on the destination unless we already own it
                // (saving over the currently-open project doesn't need a
                // re-acquire).
                if self.state.current_path().as_deref() != Some(path.as_path()) {
                    match acquire_project_lock(&path) {
                        slicer_state::lockfile::AcquireOutcome::Acquired => {}
                        slicer_state::lockfile::AcquireOutcome::HeldByLivePid { pid, doubt } => {
                            let lock = slicer_state::lockfile::lock_path_for(&path);
                            return Err(RpcError::held_by_running_pid(pid, doubt, &lock));
                        }
                        slicer_state::lockfile::AcquireOutcome::HeldByOtherMachine {
                            machine_id,
                            hostname,
                            started_at_unix_ms,
                        } => {
                            if !p.force {
                                return Err(RpcError::ProjectLocked {
                                    machine_id,
                                    hostname,
                                    started_at_unix_ms,
                                });
                            }
                            take_over_lock(&path)?;
                        }
                    }
                    // Release the previous lock if we were holding one.
                    if let Some(prev) = self.state.current_path() {
                        let _ = slicer_state::lockfile::release(&prev);
                    }
                    self.state.set_current_path(Some(path.clone()));
                }
                self.state
                    .save_to(&path)
                    .map_err(|e| RpcError::Project(format!("save failed: {e:#}")))?;
                Ok(json!({ "path": p.path }))
            }
            "project.open" => {
                let p: ProjectSavePathParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let path = std::path::PathBuf::from(&p.path);
                match acquire_project_lock(&path) {
                    slicer_state::lockfile::AcquireOutcome::Acquired => {}
                    slicer_state::lockfile::AcquireOutcome::HeldByLivePid { pid, doubt } => {
                        let lock = slicer_state::lockfile::lock_path_for(&path);
                        return Err(RpcError::held_by_running_pid(pid, doubt, &lock));
                    }
                    slicer_state::lockfile::AcquireOutcome::HeldByOtherMachine {
                        machine_id,
                        hostname,
                        started_at_unix_ms,
                    } => {
                        if !p.force {
                            return Err(RpcError::ProjectLocked {
                                machine_id,
                                hostname,
                                started_at_unix_ms,
                            });
                        }
                        take_over_lock(&path)?;
                    }
                }
                let loaded = slicer_state::persist::load(&path)
                    .map_err(|e| RpcError::Project(format!("open failed: {e:#}")))?;
                // Release the previous file's lock before swapping state.
                if let Some(prev) = self.state.current_path() {
                    let _ = slicer_state::lockfile::release(&prev);
                }
                self.state.replace_state(loaded, Some(path));
                Ok(json!({ "path": p.path }))
            }
            "project.recover_list" => {
                let current = self.state.autosave().map(|s| s.project_id().to_string());
                let root = self.autosave_root()?;
                let orphans = slicer_state::autosave::list_orphans(&root, current.as_deref())
                    .map_err(|e| RpcError::Project(format!("recover_list failed: {e}")))?;
                Ok(json!({ "orphans": orphans }))
            }
            "project.recover" => {
                #[derive(serde::Deserialize)]
                struct RecoverParams {
                    project_id: String,
                }
                let p: RecoverParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let root = self.autosave_root()?;
                let current = self.state.autosave().map(|s| s.project_id().to_string());
                let recovered =
                    slicer_state::autosave::recover(&root, &p.project_id, current.as_deref())
                        .map_err(|e| RpcError::Project(format!("recover failed: {e}")))?;
                let (_, durable) = self.state.replace_state_checkpointed(recovered, None);
                // Keep the previous copy until a synced replacement exists.
                if durable {
                    let _ =
                        slicer_state::autosave::discard(&root, &p.project_id, current.as_deref());
                }
                Ok(json!({ "project_id": p.project_id }))
            }
            "project.discard_autosave" => {
                // "Don't Save" on the live session: what it recorded must not
                // come back as a recovery offer on the next launch, however
                // this daemon goes down. `discarded: false` — no autosave was
                // attached, so there was nothing to drop.
                match self.state.discard_autosave() {
                    None => Ok(json!({ "discarded": false })),
                    Some(Ok(())) => Ok(json!({ "discarded": true })),
                    Some(Err(e)) => {
                        Err(RpcError::Project(format!("discard_autosave failed: {e:#}")))
                    }
                }
            }
            "project.discard_recovery" => {
                #[derive(serde::Deserialize)]
                struct DiscardParams {
                    project_id: String,
                }
                let p: DiscardParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let current = self.state.autosave().map(|s| s.project_id().to_string());
                slicer_state::autosave::discard(
                    &self.autosave_root()?,
                    &p.project_id,
                    current.as_deref(),
                )
                .map_err(|e| RpcError::Project(format!("discard failed: {e}")))?;
                Ok(json!({ "project_id": p.project_id }))
            }
            "adaptive.list" => {
                let snap = self.state.snapshot();
                Ok(json!({ "rules": snap.adaptive_rules }))
            }
            "adaptive.set" => {
                let p: AdaptiveRuleSetParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                if p.rule.id.trim().is_empty() {
                    return Err(RpcError::Project("rule id is empty".into()));
                }
                if p.rule.name.trim().is_empty() {
                    return Err(RpcError::Project("rule name is empty".into()));
                }
                self.state.apply(ProjectEvent::AdaptiveRuleSet {
                    rule: p.rule.clone(),
                });
                Ok(json!({ "rule_id": p.rule.id }))
            }
            "adaptive.remove" => {
                let p: AdaptiveRuleIdParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                self.state.apply(ProjectEvent::AdaptiveRuleRemoved {
                    rule_id: p.rule_id.clone(),
                });
                Ok(json!({ "rule_id": p.rule_id }))
            }
            "objects.list" => {
                let snap = self.state.snapshot();
                let objects: Vec<_> = snap
                    .objects
                    .values()
                    .map(|o| {
                        json!({
                            "id": o.id,
                            "name": o.name,
                            "recipe": o.recipe,
                            "source_path": o.source_path,
                            "source_hash": o.source_hash,
                            "triangle_count": o.triangle_count,
                            "bounds": o.bounds,
                            "transform": o.transform,
                            "overrides": o.overrides,
                            "seam_facets_rle": o.seam_facets_rle,
                            "seam_target_point": o.seam_target_point,
                            "support_facets_rle": o.support_facets_rle,
                            "fan_facets_rle": o.fan_facets_rle,
                            "filament_facets_rle": o.filament_facets_rle,
                            // Layer ranges as `LayerConfigRange` serializes
                            // them (`fan` omitted when unset): the UI rebuilds
                            // its range panel from this on rehydrate.
                            "layer_ranges": o.layer_ranges,
                            // The feature-op stack, suppressed ops included,
                            // shaped like `ObjectOperationsSet.operations`.
                            // The UI rebuilds its feature tree from this on
                            // rehydrate; without it the next hole it sends
                            // (the whole stack, see `object.set_operations`)
                            // would drop the ones already cut.
                            "operations": o.operations,
                            // STEP fan-out identity. The UI needs this on
                            // rehydrate (ProjectReplaced) to re-run STEP
                            // tessellation and restore the mesh preview —
                            // `object.import_local` only tessellates on the
                            // live add path, not when a session is reloaded.
                            "step_part_index": o.step_part_index,
                            // Which part body the object shows (`null` for a
                            // plain object): the UI's way back to the part on
                            // rehydrate.
                            "body": o.body,
                        })
                    })
                    .collect();
                Ok(json!({ "objects": objects }))
            }
            "project.export_gcode" => {
                #[derive(Deserialize)]
                struct ExportParams {
                    /// Absolute destination path the user picked in the
                    /// host's Save As dialog. The daemon does the copy
                    /// itself rather than asking the host process to —
                    /// keeps G-code lifecycle owned end-to-end inside
                    /// the daemon.
                    dest_path: String,
                    /// Optional PNG preview captured from the 3D viewport.
                    /// When present, it is embedded as a slicer-compatible
                    /// thumbnail comment block at the top of the exported
                    /// G-code.
                    #[serde(default)]
                    preview_png_base64: Option<String>,
                    #[serde(default)]
                    preview_width: Option<u32>,
                    #[serde(default)]
                    preview_height: Option<u32>,
                }
                let p: ExportParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
                let info = self
                    .last_slice
                    .lock()
                    .expect("last_slice poisoned")
                    .clone()
                    .ok_or_else(|| RpcError::Project("no completed slice to export".to_string()))?;
                let dest = std::path::PathBuf::from(&p.dest_path);
                // Refuse anything that isn't an absolute path. A
                // relative "./something.gcode" would resolve against
                // the daemon's cwd, which is not what the user picked
                // in the host's dialog — better to fail loudly.
                if !dest.is_absolute() {
                    return Err(RpcError::Project(format!(
                        "dest_path must be absolute: {}",
                        p.dest_path
                    )));
                }
                if let Some(parent) = dest.parent() {
                    if !parent.exists() {
                        return Err(RpcError::Project(format!(
                            "destination directory does not exist: {}",
                            parent.display()
                        )));
                    }
                }
                let payload = crate::preview::preview_payload(
                    p.preview_png_base64.as_deref(),
                    p.preview_width,
                    p.preview_height,
                )
                .map_err(RpcError::Project)?;
                let source = match &payload {
                    Some(_) => Some(std::fs::read(&info.gcode_path).map_err(|e| {
                        RpcError::Project(format!("read {} failed: {e}", info.gcode_path))
                    })?),
                    None => None,
                };
                let embedded = match (&payload, &source) {
                    (Some(payload), Some(source)) => crate::preview::embed_thumbnail_block(
                        source,
                        &crate::preview::thumbnail_block(payload),
                    ),
                    _ => None,
                };
                let bytes = if let Some(exported) = embedded {
                    std::fs::write(&dest, &exported).map_err(|e| {
                        RpcError::Project(format!("write {} failed: {e}", dest.display()))
                    })?;
                    exported.len() as u64
                } else {
                    std::fs::copy(&info.gcode_path, &dest).map_err(|e| {
                        RpcError::Project(format!(
                            "copy {} → {} failed: {e}",
                            info.gcode_path,
                            dest.display()
                        ))
                    })?
                };
                Ok(json!({
                    "dest_path": p.dest_path,
                    "bytes": bytes,
                    "suggested_filename": info.suggested_filename,
                }))
            }
            // `machine.*` is pure arithmetic over embedded data — no
            // filesystem, no FFI, no state — so unlike `geometry.build`
            // these need no spawn_blocking.
            // `mesh.*` reads a file and does real arithmetic over it, so
            // unlike `machine.*` these go to a blocking pool rather than
            // parking this connection's dispatcher task. Same reasoning
            // as `geometry.build` above.
            "mesh.measure"
            | "mesh.section"
            | "mesh.profile"
            | "mesh.free_circle"
            | "mesh.inscribed"
            | "mesh.wall_thickness"
            | "mesh.centre_bore" => {
                let meshes = self.measured_meshes(method, &params).await?;
                let method = method.to_string();
                tokio::task::spawn_blocking(move || match method.as_str() {
                    "mesh.measure" => crate::mesh::handle_measure(&meshes, params),
                    "mesh.section" => crate::mesh::handle_section(&meshes, params),
                    "mesh.profile" => crate::mesh::handle_profile(&meshes, params),
                    "mesh.free_circle" => crate::mesh::handle_free_circle(&meshes, params),
                    "mesh.inscribed" => crate::mesh::handle_inscribed(&meshes, params),
                    "mesh.centre_bore" => crate::mesh::handle_centre_bore(&meshes, params),
                    _ => crate::mesh::handle_wall_thickness(&meshes, params),
                })
                .await
                .map_err(|e| RpcError::Project(format!("mesh task failed: {e}")))?
            }
            // File I/O plus a full-file transform: same blocking-pool
            // reasoning as `mesh.*` above.
            "gcode.annotate_layers" | "gcode.select_tool" | "gcode.plan_resume" => {
                let method = method.to_string();
                tokio::task::spawn_blocking(move || match method.as_str() {
                    "gcode.annotate_layers" => crate::gcode::handle_annotate_layers(params),
                    "gcode.select_tool" => crate::gcode::handle_select_tool(params),
                    _ => crate::gcode::handle_plan_resume(params),
                })
                .await
                .map_err(|e| RpcError::Project(format!("gcode task failed: {e}")))?
            }
            "verify.bore"
            | "verify.fit"
            | "verify.interference"
            | "verify.overhangs"
            | "verify.selftest" => {
                let meshes = self.measured_meshes(method, &params).await?;
                let method = method.to_string();
                tokio::task::spawn_blocking(move || match method.as_str() {
                    "verify.bore" => crate::verify::handle_bore(&meshes, params),
                    "verify.fit" => crate::verify::handle_fit(&meshes, params),
                    "verify.interference" => crate::verify::handle_interference(&meshes, params),
                    "verify.overhangs" => crate::verify::handle_overhangs(&meshes, params),
                    _ => crate::verify::handle_selftest(params),
                })
                .await
                .map_err(|e| RpcError::Project(format!("verify task failed: {e}")))?
            }
            "slice.estimate" => {
                let objects = self.sliced_objects(&params).await?;
                tokio::task::spawn_blocking(move || {
                    crate::estimate::handle_estimate(&objects, params)
                })
                .await
                .map_err(|e| RpcError::Slicing(format!("estimate task failed: {e}")))?
            }
            "verify.status" => crate::verify::handle_status(),
            "machine.list" => crate::machine::handle_list(),
            "machine.passport" => crate::machine::handle_passport(params),
            "machine.check_start_block" => crate::machine::handle_check_start_block(params),
            "object.duplicate" => Err(RpcError::Project(format!(
                "{method} not yet implemented (Phase 1 stub)"
            ))),
            "slice.get_layer_stream" => Err(RpcError::Slicing(format!(
                "{method} not yet implemented (Phase 1 stub)"
            ))),
            // Test-only, and deliberately NOT in `METHODS`: the check at the
            // top of `handle` is the one thing that keeps this arm
            // unreachable, which is what keeps every arm without a line in
            // `METHODS` (and `PUBLIC_SURFACE.toml`) non-public. See
            // `an_arm_missing_from_methods_is_not_reachable`.
            #[cfg(test)]
            TEST_UNLISTED_METHOD => Ok(json!({ "reached": true })),
            _ => Err(method_not_found(method)),
        }
    }

    /// Every object a `mesh.*` / `verify.*` call names, resolved to the mesh
    /// it is measured on (`measured_mesh`) before the call goes to a
    /// blocking thread: finding that mesh may cut the object's feature ops or
    /// tessellate its STEP through the FFI, each under `STEP_FFI_LOCK` and
    /// each done before the measurement starts, so the lock is never held
    /// across one.
    ///
    /// Finds references at any depth, because they sit at three different
    /// depths already: flattened at the top level for `mesh.*` /
    /// `verify.bore` / `verify.overhangs`, nested under `hole` and `peg` for
    /// `verify.fit`, and inside a list for `verify.interference`. Only the
    /// key `object_id` is read, and in these arms an `object_id` is always a
    /// mesh reference.
    ///
    /// An id that names no object is left out: reporting an unknown object
    /// is the handler's job, and it words it better than this step could.
    /// `method` names the call in errors.
    async fn measured_meshes(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<ObjectMeshes, RpcError> {
        let mut ids = Vec::new();
        collect_object_ids(params, &mut ids);
        let snapshot = self.state.snapshot();
        let mut meshes = ObjectMeshes::new();
        for id in ids.into_iter().map(ObjectId) {
            let Some(object) = snapshot.objects.get(&id) else {
                continue;
            };
            let mesh = self.measured_mesh(method, object).await?;
            meshes.insert(id, mesh);
        }
        Ok(meshes)
    }

    /// The mesh `mesh.*` and `verify.*` measure for `obj`: the part as it
    /// prints, which is the cut a slice reads (`printed_mesh_path`) whenever
    /// a feature op is active. A stack that cannot be cut fails the call, as
    /// it fails a slice: the source would be measured without the holes and
    /// nothing would say so.
    ///
    /// A cut comes with the offset back to its source's frame, worked out
    /// here once per object from the uncut part (`uncut_mesh_path`) on a
    /// blocking thread, so a call naming the object twice reads it once.
    async fn measured_mesh(&self, method: &str, obj: &ObjectRecord) -> Result<MeshFile, RpcError> {
        let printed = printed_mesh_path(method, obj)
            .await
            .map_err(|e| RpcError::Project(cannot_cut(obj, e)))?;
        let uncut = self.uncut_mesh_path(obj).await?;
        if printed == obj.source_path {
            return Ok(MeshFile::at(uncut));
        }
        let offset = tokio::task::spawn_blocking(move || crate::mesh::recentering_offset(&uncut))
            .await
            .map_err(|e| RpcError::Project(format!("{method} task failed: {e}")))?
            .map_err(|e| {
                RpcError::Project(format!(
                    "object {}: its cut is placed where its uncut part is, and that cannot be \
                     read: {}",
                    obj.id.0,
                    inner_text(e)
                ))
            })?;
        Ok(MeshFile::cut(printed, offset))
    }

    /// The uncut part of `obj` as an STL: its source, or for a STEP source —
    /// imported from one, or built by `geometry.build` in OCCT — the
    /// tessellated STL the daemon already derives for `object.query_faces`
    /// and `object.get_edit_mesh`, cached by source hash: one tessellation
    /// shared by every reader, already paid for if the part has been opened
    /// in the editor, and made with the deflection libslic3r imports the STEP
    /// with — so its bounding box is the one `apply_operations` recentered
    /// the part on before cutting it.
    async fn uncut_mesh_path(&self, obj: &ObjectRecord) -> Result<String, RpcError> {
        if !is_step_extension(&obj.source_path) {
            return Ok(obj.source_path.clone());
        }
        let (stl_path, _facemap) = self.ensure_step_edit_mesh(obj).await?;
        Ok(stl_path.to_string_lossy().into_owned())
    }

    /// The object `slice.estimate` names, resolved to what a slice of it is
    /// given (`object_slice_inputs` over `printed_mesh_path`), so the
    /// estimate costs the part as it prints: with the holes its feature ops
    /// cut, at the size and in the orientation its transform gives it, with
    /// its paint, layer ranges and overrides. Only the top-level `object_id`
    /// is a reference here: `config_overrides` holds raw libslic3r keys.
    ///
    /// An id that names no object is left out, for the handler to report.
    async fn sliced_objects(&self, params: &Value) -> Result<SlicedObjects, RpcError> {
        let Some(id) = params.get("object_id").and_then(Value::as_str) else {
            return Ok(SlicedObjects::new());
        };
        let id = ObjectId(id.to_string());
        let snapshot = self.state.snapshot();
        let Some(object) = snapshot.objects.get(&id) else {
            return Ok(SlicedObjects::new());
        };
        let path = printed_mesh_path("slice.estimate", object)
            .await
            .map_err(|e| RpcError::Project(cannot_cut(object, e)))?;
        if path != object.source_path && object.filament_facets_rle.is_some() {
            return Err(RpcError::Project("material paint belongs to the original mesh; clear paint or suppress geometry operations before estimating".into()));
        }
        Ok(SlicedObjects::from([(
            id,
            object_slice_inputs(object, path),
        )]))
    }

    /// Materialize (or retrieve from cache) the tessellated STL + facemap
    /// JSON derived from one part of a STEP object. Shared by
    /// `object.query_faces` (which goes on to parse the facemap into its
    /// `faces` response), `object.get_edit_mesh` (which only needs the
    /// paths) and `uncut_mesh_path` (which needs the STL) — all three
    /// materialize the exact same cache entry, so a STEP object queried via
    /// one and then loaded via another never re-tessellates.
    ///
    /// Concurrent cold calls for one part — the window opening the editor
    /// while a `mesh.*` call resolves the same object — tessellate it once:
    /// the in-lock recheck makes the loser take the winner's entry instead
    /// of rewriting it (`tessellate_edit_mesh` says how an entry is written).
    ///
    /// Callers must confirm `is_step_extension(&obj.source_path)`
    /// themselves first — this fn always attempts a STEP parse on
    /// `obj.source_path` regardless of extension.
    async fn ensure_step_edit_mesh(
        &self,
        obj: &ObjectRecord,
    ) -> Result<(std::path::PathBuf, std::path::PathBuf), RpcError> {
        let cache_root = slicer_state::object_cache_dir()
            .map_err(|e| RpcError::Project(format!("edit mesh failed: {e}")))?;
        let (stl_path, map_path) =
            edit_mesh_cache_paths(&cache_root, &obj.source_hash, obj.step_part_index);
        if edit_mesh_cached(&stl_path, &map_path) {
            return Ok((stl_path, map_path));
        }

        std::fs::create_dir_all(&cache_root)
            .map_err(|e| RpcError::Project(format!("edit mesh failed: cache dir: {e}")))?;
        // Same reasoning as inspect_model above: the tessellation crosses
        // the C++ bridge and can take seconds on a large STEP.
        // spawn_blocking keeps the dispatcher responsive.
        let source_path = obj.source_path.clone();
        let part = obj.step_part_index.map_or(-1, |i| i as i32);
        let (stl_target, map_target) = (stl_path.clone(), map_path.clone());
        tokio::task::spawn_blocking(move || {
            // See STEP_FFI_LOCK: OCCT's STEP reader is not thread-safe;
            // serialize concurrent tessellations. Poisoning is safe to
            // swallow — the lock guards no mutable state, only OCCT
            // re-entry.
            let _step_lock = STEP_FFI_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Another call may have tessellated this part while this one
            // waited. The full check, not just `exists`, so a facemap that
            // does not parse is still tessellated again.
            if edit_mesh_cached(&stl_target, &map_target) {
                return Ok(());
            }
            tessellate_edit_mesh(&source_path, part, &stl_target, &map_target)
        })
        .await
        .map_err(|e| RpcError::Project(format!("edit mesh task failed: {e}")))?
        .map_err(|e| RpcError::Project(format!("edit mesh failed: {e}")))?;
        Ok((stl_path, map_path))
    }
}

/// Stub slicing pipeline: emits realistic-looking progress events over ~3 seconds
/// then writes a fake G-code file with rectangular perimeter contours at each
/// layer (derived from the union bounding box of all objects), and emits
/// SliceCompleted with that path. Replaced by real libslic3r call in T1A.4+T1A.8.
///
/// `cancel` is the handle the dispatcher registered for this job. It's
/// passed through to the FFI bridge so libslic3r can be aborted
/// cooperatively, and used here to distinguish a cancel-induced exit
/// (emit `SliceCanceled`) from a genuine slice failure (emit
/// `SliceFailed`).
///
/// `last_slice` is updated on success so `project.export_gcode` knows
/// what to copy. We deliberately don't clear it on failure/cancel —
/// the user's last-known-good slice stays exportable, matching the
/// behaviour of any other slicer.
struct SliceTask {
    state: StateLayer,
    job_id: String,
    cancel: CancelHandle,
    last_slice: Arc<StdMutex<Option<LastSliceInfo>>>,
    idex: Option<slicer_ffi::IdexInputs>,
    resolved_config: Option<ResolvedSliceConfig>,
    fan_lead: slicer_gcode::FanLeadParams,
    calibration_test: Option<slicer_ffi::CalibrationTest>,
    context: slicer_ffi::SliceContext,
    snap: slicer_state::project::ProjectState,
    run_id: Option<String>,
}

async fn fake_slice(task: SliceTask) {
    let SliceTask {
        state,
        job_id,
        cancel,
        last_slice,
        idex,
        resolved_config,
        fan_lead,
        calibration_test,
        context,
        snap,
        run_id,
    } = task;
    // Compute bounding box and derive layer count from active objects
    let (bbox_min, bbox_max) = compute_union_bounds(&snap);
    // The bridge makes this the plate-wide `layer_height`, which libslic3r
    // slices every object at that has none of its own — so it is the
    // project's. An object's own override reaches its ModelObject through
    // `object_config_overrides_for` instead.
    let project_layer_height = resolved_process_f32(&resolved_config, "layer_height")
        .or_else(|| {
            snap.settings
                .get(&(SettingScope::Process, "layer_height".to_string()))
                .and_then(|v| v.as_f64())
                .map(|v| v as f32)
        })
        .unwrap_or(0.2)
        .max(0.01);
    // Only sizes the progress estimate: the plate's height at its finest
    // pitch, as the fake pipeline always reported it. It predicts no real
    // layer count — objects of different pitches interleave by Z.
    let finest_layer_height = snap
        .objects
        .values()
        .filter_map(|obj| obj.overrides.get("layer_height").and_then(|v| v.as_f64()))
        .fold(project_layer_height, |finest, v| finest.min(v as f32))
        .max(0.01);

    // Pull profile-v2 resolved settings into FFI scalar parameters. Anything
    // missing falls back to legacy daemon snapshot keys, then to the bridge
    // sentinel value (0 / empty string = preserve preset default).
    let setting_f32 = |keys: &[&str], dflt: f32| {
        resolved_process_f32_any(&resolved_config, keys)
            .or_else(|| {
                keys.iter().find_map(|key| {
                    snap.settings
                        .get(&(SettingScope::Process, (*key).to_string()))
                        .and_then(|v| v.as_f64())
                        .map(|v| v as f32)
                })
            })
            .unwrap_or(dflt)
    };
    let setting_i32 = |keys: &[&str], dflt: i32| {
        // SettingValue exposes Int via as_f64() for convenience; cast back.
        resolved_process_f64_any(&resolved_config, keys)
            .or_else(|| {
                keys.iter().find_map(|key| {
                    snap.settings
                        .get(&(SettingScope::Process, (*key).to_string()))
                        .and_then(|v| v.as_f64())
                })
            })
            .map(|v| v.round() as i32)
            .unwrap_or(dflt)
    };
    let legacy_setting_f32 = |key: &str, dflt: f32| {
        snap.settings
            .get(&(SettingScope::Process, key.to_string()))
            .and_then(|v| v.as_f64())
            .map(|v| v as f32)
            .unwrap_or(dflt)
    };
    let wall_loops = setting_i32(&["wall_loops", "perimeter_count"], 0).max(0) as u32;
    let infill_density_pct = setting_f32(&["sparse_infill_density", "infill_density"], -1.0);
    let infill_pattern = process_infill_pattern_for_ffi(&resolved_config, &snap.settings);
    let support_enabled = process_support_enabled(&resolved_config, &snap.settings);
    let print_speed = if resolved_config.is_some() {
        0.0
    } else {
        legacy_setting_f32("print_speed", 0.0).max(0.0)
    };
    // Profile v2 sends top/bottom shell layers as config_overrides. Keep this
    // legacy scalar only for old daemon snapshots, otherwise it would overwrite
    // the resolved layer-count keys inside the bridge.
    let top_shell_thickness = if resolved_config.is_some() {
        0.0
    } else {
        legacy_setting_f32("top_shell_thickness", 0.0).max(0.0)
    };
    // Empty = bridge applies its own default (`arachne`).
    let wall_generator =
        process_setting_string_any(&resolved_config, &snap.settings, &["wall_generator"]);
    let height = (bbox_max[2] - bbox_min[2]).max(finest_layer_height);
    let total_layers = (height / finest_layer_height).ceil() as u32;
    let total_layers = total_layers.max(1);

    let stages = [
        ("preparing", 5.0, None),
        ("perimeters", 35.0, Some(total_layers / 4)),
        ("infill", 60.0, Some(total_layers / 2)),
        ("cooling", 85.0, Some(3 * total_layers / 4)),
        ("finalizing", 99.0, Some(total_layers)),
    ];
    for (stage, percent, layer) in stages {
        tokio::time::sleep(Duration::from_millis(500)).await;
        state.apply(ProjectEvent::SliceProgress {
            job_id: job_id.clone(),
            stage: stage.to_string(),
            percent,
            layer,
            total_layers: Some(total_layers),
        });
    }

    // Hand off to the C++ slicer bridge. The bridge currently writes the
    // same rectangle G-code the previous Rust fake_slice did — when
    // libslic3r is wired the bridge body changes but this call doesn't.
    let dir = std::env::temp_dir().join("protype-slicer");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{job_id}.gcode"));
    let path_str = path.to_string_lossy().to_string();
    let bbox_min_c = bbox_min;
    let bbox_max_c = bbox_max;
    let path_for_ffi = path_str.clone();
    // Every object's mesh, with any cut the ops cache lost made again. A
    // recut holds `STEP_FFI_LOCK`, so it finishes here, before `run_slice`
    // starts (see that lock). A stack that cannot be cut fails the slice —
    // unless the job was canceled meanwhile, which is then what it reports,
    // as a panicked slice does below.
    let input_paths = match resolve_slice_input_paths(&snap).await {
        Ok(paths) => paths,
        Err(error) => {
            state.apply(if cancel.is_canceled() {
                ProjectEvent::SliceCanceled { job_id }
            } else {
                ProjectEvent::SliceFailed { job_id, error }
            });
            return;
        }
    };
    // Build one `ObjectInputs` entry per object on the plate, in the
    // deterministic `BTreeMap` iteration order `snap.objects` already
    // provides, each from that object's own mesh path (source, or the
    // ops-cache STL its active op stack cuts — see
    // `resolve_slice_input_path`) — replacing the old "pick one object,
    // drop the rest" selection. See `object_slice_inputs` for what an entry
    // carries.
    let objects_for_ffi: Vec<slicer_ffi::ObjectInputs> = snap
        .objects
        .values()
        .zip(input_paths)
        .map(|(o, input_path)| object_slice_inputs(o, input_path))
        .collect();
    tracing::info!(
        object_count = objects_for_ffi.len(),
        layer_height = project_layer_height,
        finest_layer_height,
        wall_loops,
        infill_density_pct,
        infill_pattern,
        support_enabled,
        print_speed,
        top_shell_thickness,
        wall_generator,
        bbox_min = ?bbox_min,
        bbox_max = ?bbox_max,
        "fake_slice → ffi::run_slice"
    );
    let infill_pattern_for_ffi = infill_pattern.clone();
    let wall_generator_for_ffi = wall_generator.clone();
    let cancel_for_ffi = cancel.clone();
    let mut config_overrides = resolved_config
        .as_ref()
        .map(collect_config_overrides)
        .unwrap_or_default();
    // IDEX role binding: translate `[{role, extruder_slot}]` into the
    // per-role libslic3r keys (wall_filament, support_filament, ...).
    // Closes audit finding `multi-tool-idex-role-mapping-dropped`.
    if let Some(rc) = resolved_config.as_ref() {
        for ov in role_mapping_to_config_overrides(&rc.role_mapping) {
            upsert_config_override(&mut config_overrides, ov.key, ov.value);
        }
    }
    // Keys libslic3r would drop without a word, which `run_slice` refuses. A
    // GUI slice goes on without them and names them in its warnings, unless
    // dropping them loses nothing — see `ignored_overrides`.
    let (config_overrides, ignored_overrides) = crate::ignored_overrides::split(
        config_overrides,
        slicer_ffi::describe_ignored_config_override,
    );
    let ignored_overrides_warning = crate::ignored_overrides::warning(
        &ignored_overrides,
        resolved_config.as_ref(),
        idex.as_ref(),
    );
    // NOTE: the STEP parse `run_slice` performs internally (C++ bridge
    // `run_slice` → `load_model_for_path` → `Slic3r::Model::
    // read_from_step`) is deliberately NOT covered by `STEP_FFI_LOCK` —
    // holding a Rust lock for the whole multi-second slice would stall
    // the inspector arms. It is instead serialized against them one
    // layer down, by `occt_step_mutex()` in the C++ bridge, which wraps
    // the parse itself. See `STEP_FFI_LOCK`'s doc comment.
    let path_for_fan_lead = path_str.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = slicer_ffi::run_slice(slicer_ffi::SliceParams {
            output_path: &path_for_ffi,
            layer_height: project_layer_height,
            wall_loops,
            sparse_infill_density_pct: infill_density_pct,
            sparse_infill_pattern: infill_pattern_for_ffi,
            enable_support: support_enabled,
            print_speed_mm_s: print_speed,
            top_shell_thickness_mm: top_shell_thickness,
            wall_generator: wall_generator_for_ffi,
            config_overrides,
            bbox_min: bbox_min_c,
            bbox_max: bbox_max_c,
            // The window's plate: every transform is where the user put the
            // part, and a part left off the bed is the user's to fix.
            place_plate_on_bed: false,
            calibration_test,
            objects: objects_for_ffi,
            cancel: Some(cancel_for_ffi),
            idex,
            context,
        });
        // Still on the blocking thread and before `SliceCompleted`: the window
        // must never read a file the pass is about to rewrite.
        if result.success && !result.canceled {
            lead_painted_fan_zones(&path_for_fan_lead, fan_lead);
        }
        result
    })
    .await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    let event = match result {
        Ok(r) if r.canceled => ProjectEvent::SliceCanceled {
            job_id: job_id.clone(),
        },
        Ok(r) if r.success => {
            if let Some(ref run_id) = run_id {
                if let Err(error) = crate::serial::stamp_gcode(&path_str, run_id) {
                    state.apply(ProjectEvent::SliceFailed {
                        job_id: job_id.clone(),
                        error: format!("Could not attach print passport: {error}"),
                    });
                    return;
                }
            }

            // Defensive fallback: if the bridge couldn't render a
            // filename (stub mode with a sentinel input path, or
            // PlaceholderParser failure), fall back to the job id so
            // the UI never shows an empty Save As default.
            let suggested = if r.suggested_filename.is_empty() {
                format!("{job_id}.gcode")
            } else {
                r.suggested_filename.clone()
            };
            // Cache the slice for project.export_gcode. Take the lock
            // before we apply the event — the export RPC consults
            // last_slice, not the event log, so the lock has to be
            // populated before any sub-millisecond UI follow-up.
            *last_slice.lock().expect("last_slice poisoned") = Some(LastSliceInfo {
                job_id: job_id.clone(),
                gcode_path: path_str.clone(),
                suggested_filename: suggested.clone(),
            });
            let warnings: Vec<String> = r
                .warnings
                .into_iter()
                .chain(ignored_overrides_warning)
                .collect();
            if !warnings.is_empty() {
                tracing::warn!(job_id, warnings = ?warnings, "slice completed with warnings");
            }
            ProjectEvent::SliceCompleted {
                job_id: job_id.clone(),
                gcode_path: path_str,
                total_layers: r.total_layers,
                total_segments: r.total_segments,
                suggested_filename: suggested,
                warnings,
            }
        }
        Ok(r) => ProjectEvent::SliceFailed {
            job_id: job_id.clone(),
            error: format!("ffi run_slice failed: {}", r.error_message),
        },
        Err(e) => {
            // Tokio JoinError — this is rare (panic in spawn_blocking).
            // If the user happened to cancel while we panicked, prefer
            // SliceCanceled so the UI doesn't show a confusing error.
            if cancel.is_canceled() {
                ProjectEvent::SliceCanceled {
                    job_id: job_id.clone(),
                }
            } else {
                ProjectEvent::SliceFailed {
                    job_id: job_id.clone(),
                    error: format!("ffi task panicked: {e}"),
                }
            }
        }
    };
    state.apply(event);
}

/// The paint fan lead pass over a finished slice. It never fails the slice:
/// a file the pass cannot read keeps its zones where the engine put them and
/// says why in a `; fan_lead: skipped (…)` line.
fn lead_painted_fan_zones(
    gcode_path: &str,
    params: slicer_gcode::FanLeadParams,
) -> Option<crate::gcode::FanLeadReport> {
    use crate::gcode::FanLeadReport;
    match crate::gcode::apply_fan_lead_to_file(std::path::Path::new(gcode_path), params) {
        Ok(report) => {
            match &report {
                FanLeadReport::Unchanged => {}
                FanLeadReport::Applied { shifted, merged } => {
                    tracing::info!(gcode_path, shifted, merged, "fan_lead applied");
                }
                FanLeadReport::Skipped { reason } => {
                    tracing::warn!(gcode_path, %reason, "fan_lead skipped");
                }
            }
            Some(report)
        }
        Err(error) => {
            tracing::warn!(gcode_path, %error, "fan_lead could not rewrite the G-code");
            None
        }
    }
}

fn compute_union_bounds(snap: &slicer_state::project::ProjectState) -> ([f32; 3], [f32; 3]) {
    // Empty plate: fall back to a 20mm cube placeholder so fake_slice
    // still produces a meaningful G-code preview while the user is
    // setting up.
    if snap.objects.is_empty() {
        return ([-10.0, -10.0, 0.0], [10.0, 10.0, 20.0]);
    }
    // Real bounds: mirror the viewport's object-space convention before
    // applying the object transform. The UI recenters mesh geometry around
    // its local XY bbox center and lifts the local bottom to Z=0 before it
    // applies `object.transform`; slice preview must use the same space or
    // the generated G-code appears shifted after slicing.
    let mut min = [f32::INFINITY, f32::INFINITY, f32::INFINITY];
    let mut max = [f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
    for obj in snap.objects.values() {
        let (lo, hi) = transformed_visual_bounds(&obj.bounds, &obj.transform);
        for i in 0..3 {
            if lo[i] < min[i] {
                min[i] = lo[i];
            }
            if hi[i] > max[i] {
                max[i] = hi[i];
            }
        }
    }
    // Sanity guard: if every object had a degenerate bbox we'd end up
    // with infinities. Fall back to the empty-plate bbox.
    if !min[0].is_finite() || !max[0].is_finite() {
        return ([-10.0, -10.0, 0.0], [10.0, 10.0, 20.0]);
    }
    (min, max)
}

fn transformed_visual_bounds(bounds: &Bounds, transform: &[f32; 16]) -> ([f32; 3], [f32; 3]) {
    let center_x = (bounds.min[0] + bounds.max[0]) * 0.5;
    let center_y = (bounds.min[1] + bounds.max[1]) * 0.5;
    let bottom_z = bounds.min[2];
    let xs = [bounds.min[0] - center_x, bounds.max[0] - center_x];
    let ys = [bounds.min[1] - center_y, bounds.max[1] - center_y];
    let zs = [bounds.min[2] - bottom_z, bounds.max[2] - bottom_z];
    let mut min = [f32::INFINITY, f32::INFINITY, f32::INFINITY];
    let mut max = [f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];

    for x in xs {
        for y in ys {
            for z in zs {
                let p = transform_point(transform, [x, y, z]);
                for i in 0..3 {
                    if p[i] < min[i] {
                        min[i] = p[i];
                    }
                    if p[i] > max[i] {
                        max[i] = p[i];
                    }
                }
            }
        }
    }
    (min, max)
}

pub(crate) fn transform_point(m: &[f32; 16], p: [f32; 3]) -> [f32; 3] {
    [
        m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
        m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
        m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
    ]
}

fn scope_for_kind(kind: PresetKind) -> SettingScope {
    match kind {
        PresetKind::Process => SettingScope::Process,
        PresetKind::Filament => SettingScope::Filament,
        PresetKind::Printer => SettingScope::Printer,
    }
}

fn kind_for_scope(scope: SettingScope) -> PresetKind {
    match scope {
        SettingScope::Process => PresetKind::Process,
        SettingScope::Filament => PresetKind::Filament,
        SettingScope::Printer => PresetKind::Printer,
    }
}

fn validate_setting(
    scope: &SettingScope,
    key: &SettingKey,
    value: &SettingValue,
) -> Result<(), RpcError> {
    let schema = SettingsSchema::baseline();
    let def = schema
        .defs
        .iter()
        .find(|d| d.scope == *scope && d.key == *key)
        .ok_or_else(|| RpcError::Project(format!("unknown setting: {scope:?}/{key}")))?;
    validate_constraint(key, &def.constraint, value)
}

fn validate_constraint(
    key: &str,
    constraint: &SettingConstraint,
    value: &SettingValue,
) -> Result<(), RpcError> {
    match (constraint, value) {
        (SettingConstraint::Number { min, max, .. }, SettingValue::Number(v)) => {
            if v < min || v > max {
                return Err(RpcError::Project(format!(
                    "{key} out of range: {v} not in [{min}, {max}]"
                )));
            }
        }
        (SettingConstraint::Int { min, max }, SettingValue::Int(v)) => {
            if v < min || v > max {
                return Err(RpcError::Project(format!(
                    "{key} out of range: {v} not in [{min}, {max}]"
                )));
            }
        }
        (SettingConstraint::Bool, SettingValue::Bool(_)) => {}
        (SettingConstraint::Enum { options }, SettingValue::String(s)) => {
            if !options.iter().any(|o| &o.value == s) {
                return Err(RpcError::Project(format!(
                    "{key}: '{s}' is not a valid enum option"
                )));
            }
        }
        (constraint, value) => {
            return Err(RpcError::Project(format!(
                "{key}: value kind {value:?} does not match constraint {constraint:?}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use slicer_state::project::{ObjectRecord, ProjectState};

    #[test]
    fn slice_request_carries_the_fan_lead_times() {
        let p: SliceRequestParams = serde_json::from_value(json!({
            "fan_lead": { "spinup_s": 0.5, "spindown_s": 3.0 }
        }))
        .expect("parses");
        assert_eq!(
            p.fan_lead,
            Some(slicer_gcode::FanLeadParams {
                spinup_s: 0.5,
                spindown_s: 3.0,
            })
        );

        let older_window: SliceRequestParams =
            serde_json::from_value(json!({ "plate_id": 0 })).expect("parses");
        assert_eq!(older_window.fan_lead, None);
    }

    #[tokio::test]
    async fn slice_request_refuses_fan_lead_times_out_of_range() {
        let dispatcher = Dispatcher::new(StateLayer::new());
        let err = dispatcher
            .handle(
                "slice.request",
                json!({ "fan_lead": { "spinup_s": 11.0, "spindown_s": 2.0 } }),
            )
            .await
            .expect_err("11 s is outside 0..10");
        assert!(err.to_string().contains("invalid fan_lead"), "{err}");
    }

    #[tokio::test]
    async fn slice_request_refuses_an_invalid_calibration_test() {
        let dispatcher = Dispatcher::new(StateLayer::new());
        let err = dispatcher
            .handle(
                "slice.request",
                json!({ "calibration_test": { "mode": "pa_tower", "start": 0.0, "end": 0.08, "step": 0.0 } }),
            )
            .await
            .expect_err("a zero step must be refused");
        assert_eq!(err.code(), -35000);
        assert!(
            err.to_string().contains("invalid calibration_test"),
            "{err}"
        );
    }

    // A lock of another host: the refusal names its holder in `data` under a
    // code of its own, so the window can ask "open anyway?" instead of
    // showing the raw text — and `force` then takes the lock rather than
    // reading past it. The file comes from a second dispatcher's save: the
    // one that opens holds no project, as a fresh tab's daemon doesn't.
    #[tokio::test]
    async fn project_open_refuses_a_lock_of_another_host_with_its_holder_and_force_takes_it_over() {
        let dir =
            std::env::temp_dir().join(format!("dispatch-foreign-lock-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.3mf");
        let lock = dir.join("p.3mf.lock");
        Dispatcher::new(StateLayer::new())
            .handle("project.save", json!({ "path": path }))
            .await
            .expect("save a project to open");
        std::fs::write(
            &lock,
            br#"{"machine_id":"other-install","pid":1,"started_at_unix_ms":1700000000000,"host_id":"other-host","hostname":"office-pc"}"#,
        )
        .unwrap();

        let dispatcher = Dispatcher::new(StateLayer::new());
        let err = dispatcher
            .handle("project.open", json!({ "path": path }))
            .await
            .expect_err("a lock of another host must be refused");
        assert_eq!(err.code(), -33001);
        assert_eq!(
            err.data(),
            Some(json!({
                "machine_id": "other-install",
                "hostname": "office-pc",
                "started_at_unix_ms": 1_700_000_000_000_i64,
            }))
        );
        assert!(err.to_string().contains("office-pc"), "{err}");

        dispatcher
            .handle("project.open", json!({ "path": path, "force": true }))
            .await
            .expect("force opens past a lock of another host");
        let taken: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&lock).unwrap()).unwrap();
        assert_eq!(
            taken["pid"],
            std::process::id(),
            "force takes the lock over"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // The lock only warns about two windows editing one file. A lock that
    // cannot be read or written — a folder macOS will not let the daemon write
    // a sibling into after an update re-signed the app, read-only media, here
    // a directory squatting its name — must not keep the project from opening
    // or saving: 0.1.19 refused to open a project in ~/Documents over exactly
    // that, with the OS reason cut off the message.
    #[tokio::test]
    async fn project_opens_and_saves_when_its_lock_cannot_be_written() {
        let dir =
            std::env::temp_dir().join(format!("dispatch-unwritable-lock-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.3mf");
        let lock = dir.join("p.3mf.lock");
        Dispatcher::new(StateLayer::new())
            .handle("project.save", json!({ "path": path }))
            .await
            .expect("save a project to open");
        std::fs::remove_file(&lock).ok();
        std::fs::create_dir_all(&lock).unwrap();

        let dispatcher = Dispatcher::new(StateLayer::new());
        dispatcher
            .handle("project.open", json!({ "path": path }))
            .await
            .expect("a project opens even when its lock cannot be written");

        let other = dir.join("q.3mf");
        std::fs::create_dir_all(dir.join("q.3mf.lock")).unwrap();
        dispatcher
            .handle("project.save", json!({ "path": other }))
            .await
            .expect("a project saves even when its lock cannot be written");
        assert!(other.is_file(), "the project file itself was written");
        std::fs::remove_dir_all(&dir).ok();
    }

    // An open or save that fails must say why: the outermost context alone
    // («open <path>») hid whether the file was missing or the OS refused it.
    #[tokio::test]
    async fn project_open_failure_names_the_os_reason() {
        let missing =
            std::env::temp_dir().join(format!("dispatch-missing-{}.3mf", std::process::id()));
        std::fs::remove_file(&missing).ok();
        let err = Dispatcher::new(StateLayer::new())
            .handle("project.open", json!({ "path": missing }))
            .await
            .expect_err("a missing project cannot open");
        let text = err.to_string();
        assert!(text.contains("No such file or directory"), "{text}");
    }

    /// The direction of the public-surface gate that no listing can show:
    /// `handle` has an arm that `METHODS` does not name. The arm exists only
    /// under `cfg(test)`; the check at the top of `handle` must keep it from
    /// answering, as it keeps every real arm that someone adds without a
    /// line in `METHODS` and `PUBLIC_SURFACE.toml`.
    #[tokio::test]
    async fn an_arm_missing_from_methods_is_not_reachable() {
        assert!(
            !METHODS.contains(&TEST_UNLISTED_METHOD),
            "the test arm is only a test of the gate while it is not in METHODS"
        );
        let dispatcher = Dispatcher::new(StateLayer::new());

        let outcome = dispatcher.handle(TEST_UNLISTED_METHOD, json!({})).await;

        match outcome {
            Err(RpcError::Internal(message)) => {
                assert_eq!(message, format!("method not found: {TEST_UNLISTED_METHOD}"))
            }
            other => panic!(
                "an arm that METHODS does not list answered a client: {other:?}. \
                 `Dispatcher::handle` must refuse a method outside METHODS before its match."
            ),
        }
    }

    #[tokio::test]
    async fn hello_advertises_calibration_tests() {
        let dispatcher = Dispatcher::new(StateLayer::new());
        let hello = dispatcher.handle("hello", json!({})).await.unwrap();
        let caps = hello["capabilities"].as_array().unwrap();
        assert!(caps.iter().any(|c| c == "slice.calibration_test"));
        assert!(caps
            .iter()
            .any(|c| c == "slice.calibration_test.retraction_steps"));
    }

    #[test]
    fn fan_lead_pass_rewrites_a_finished_slice_with_a_painted_zone() {
        let path =
            std::env::temp_dir().join(format!("dispatch-fan-lead-{}.gcode", std::process::id()));
        let source = "M83\n;LAYER_CHANGE\n;Z:0.2\nG1 X0 Y0 F600\nG1 X10 E1\n\
                      ; PAINT_FAN S100\nM106 S255\nG1 X20 E1\n; PAINT_FAN END\nM106 S0\n";
        std::fs::write(&path, source).unwrap();

        let report = lead_painted_fan_zones(
            path.to_str().unwrap(),
            slicer_gcode::FanLeadParams::default(),
        );

        assert_eq!(
            report,
            Some(crate::gcode::FanLeadReport::Applied {
                shifted: 1,
                merged: 0
            })
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("G1 X0 Y0 F600\nM106 S255\nG1 X10 E1\n; PAINT_FAN S100\nG1 X20"),
            "{written}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fan_lead_pass_leaves_a_slice_without_markers_alone() {
        let path = std::env::temp_dir().join(format!(
            "dispatch-fan-lead-plain-{}.gcode",
            std::process::id()
        ));
        let source = ";LAYER_CHANGE\nG1 X0 Y0 F600\nG1 X10 E1\n";
        std::fs::write(&path, source).unwrap();

        let report = lead_painted_fan_zones(
            path.to_str().unwrap(),
            slicer_gcode::FanLeadParams::default(),
        );

        assert_eq!(report, Some(crate::gcode::FanLeadReport::Unchanged));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        let _ = std::fs::remove_file(&path);
    }

    fn identity() -> [f32; 16] {
        [
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0, //
        ]
    }

    fn translated(x: f32, y: f32, z: f32) -> [f32; 16] {
        let mut m = identity();
        m[12] = x;
        m[13] = y;
        m[14] = z;
        m
    }

    fn object(bounds: Bounds, transform: [f32; 16]) -> ObjectRecord {
        ObjectRecord {
            recipe: None,
            id: ObjectId("obj".into()),
            name: String::new(),
            source_path: String::new(),
            source_hash: "hash".into(),
            triangle_count: 12,
            bounds,
            transform,
            overrides: HashMap::new(),
            seam_facets_rle: None,
            seam_target_point: None,
            support_facets_rle: None,
            fan_facets_rle: None,
            filament_facets_rle: None,
            layer_height_profile: Vec::new(),
            layer_ranges: Vec::new(),
            step_part_index: None,
            operations: Vec::new(),
            body: None,
        }
    }

    #[test]
    fn slice_bounds_match_viewport_centered_mesh_space() {
        let bounds = Bounds {
            min: [0.0, 10.0, -2.0],
            max: [40.0, 70.0, 18.0],
        };

        let (min, max) = transformed_visual_bounds(&bounds, &translated(200.0, 150.0, 0.0));

        assert_eq!(min, [180.0, 120.0, 0.0]);
        assert_eq!(max, [220.0, 180.0, 20.0]);
    }

    #[test]
    fn union_bounds_use_visual_space_for_project_objects() {
        let mut state = ProjectState::default();
        state.objects.insert(
            ObjectId("obj".into()),
            object(
                Bounds {
                    min: [0.0, 0.0, 4.0],
                    max: [20.0, 10.0, 24.0],
                },
                translated(128.0, 150.0, 0.0),
            ),
        );

        let (min, max) = compute_union_bounds(&state);

        assert_eq!(min, [118.0, 145.0, 0.0]);
        assert_eq!(max, [138.0, 155.0, 20.0]);
    }

    #[test]
    fn visual_bounds_apply_full_column_major_transform() {
        let bounds = Bounds {
            min: [-10.0, -5.0, 0.0],
            max: [10.0, 5.0, 8.0],
        };
        let mut m = identity();
        m[0] = 2.0;
        m[5] = 3.0;
        m[10] = 1.0;
        m[12] = 100.0;
        m[13] = 200.0;
        m[14] = 0.0;

        let (min, max) = transformed_visual_bounds(&bounds, &m);

        assert_eq!(min, [80.0, 185.0, 0.0]);
        assert_eq!(max, [120.0, 215.0, 8.0]);
    }

    /// Unique-per-call nonce so tests that materialize a real file under
    /// the real `object_cache_dir()` (see `resolve_slice_input_path`'s
    /// tests below — there is no per-test tmpdir override for that path,
    /// same as `object_set_operations.rs`) never collide with a leftover
    /// from a previous run of this same test on this machine.
    fn nonce() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn hole_op(id: &str, suppressed: bool) -> slicer_types::FeatureOp {
        slicer_types::FeatureOp {
            id: id.to_string(),
            kind: "hole".to_string(),
            suppressed,
            origin: [0.0, 0.0, 0.0],
            axis: [0.0, 0.0, 1.0],
            radius_mm: 3.0,
            depth_mm: 5.0,
            through: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn resolve_slice_input_path_is_source_when_no_active_ops() {
        let mut obj = object(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            identity(),
        );
        obj.source_path = "/tmp/fixtures/cube.stl".to_string();
        assert!(obj.operations.is_empty());

        let resolved = resolve_slice_input_path(&obj).await;

        assert_eq!(resolved.expect("no op to cut"), obj.source_path);
    }

    #[tokio::test]
    async fn resolve_slice_input_path_is_source_when_all_ops_suppressed() {
        let mut obj = object(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            identity(),
        );
        obj.source_path = "/tmp/fixtures/cube.stl".to_string();
        // Non-empty but every op suppressed must behave exactly like the
        // empty case above — same gate `object.set_operations` uses.
        obj.operations = vec![hole_op("h1", true), hole_op("h2", true)];

        let resolved = resolve_slice_input_path(&obj).await;

        assert_eq!(resolved.expect("no op to cut"), obj.source_path);
    }

    /// Active op, no cut in the ops cache, and nothing to cut it from: the
    /// slice input is an error, not the source the stack was meant to cut.
    #[tokio::test]
    async fn resolve_slice_input_path_fails_when_a_lost_cut_cannot_be_recut() {
        let mut obj = object(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            identity(),
        );
        obj.source_path = format!("/tmp/fixtures/no-such-source-{}.stl", nonce());
        obj.source_hash = format!("resolve-slice-input-missing-{}", nonce());
        obj.operations = vec![hole_op("h1", false)];

        let resolved = resolve_slice_input_path(&obj).await;

        assert!(
            resolved.is_err(),
            "a stack that cannot be cut must not resolve to a mesh: {resolved:?}"
        );
    }

    #[tokio::test]
    async fn resolve_slice_input_path_is_ops_cache_when_active_op_and_cache_exists() {
        let mut obj = object(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            identity(),
        );
        obj.source_path = "/tmp/fixtures/cube.stl".to_string();
        obj.source_hash = format!("resolve-slice-input-hit-{}", nonce());
        obj.operations = vec![hole_op("h1", false)];

        // Pre-materialize the cache file at the exact path
        // `object.set_operations` would have written it to — computed via
        // the SAME `ops_cache_path`/`ops_hash` helpers `resolve_slice_
        // input_path` uses internally, so this test never re-derives the
        // hash logic itself.
        let root = slicer_state::object_cache_dir().expect("object_cache_dir");
        std::fs::create_dir_all(&root).expect("create cache dir");
        let cache_path = ops_cache_path(
            &root,
            &obj.source_hash,
            obj.step_part_index,
            &ops_hash(&obj.operations),
        );
        std::fs::write(&cache_path, b"stub stl bytes").expect("pre-materialize cache stub");
        write_skipped_sidecar(&cache_path, &[]).expect("pre-materialize its skips");

        let resolved = resolve_slice_input_path(&obj).await;
        let _ = std::fs::remove_file(&cache_path);
        let _ = std::fs::remove_file(ops_skipped_sidecar_path(&cache_path));

        let resolved = resolved.expect("a cache hit needs no cut");
        assert_eq!(resolved, cache_path.to_string_lossy());
        assert!(resolved.contains("ops-"));
        assert_ne!(resolved, obj.source_path);
    }

    #[tokio::test]
    async fn resolve_slice_input_path_is_ops_cache_with_mixed_suppressed_active_ops() {
        let mut obj = object(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            identity(),
        );
        obj.source_path = "/tmp/fixtures/cube.stl".to_string();
        obj.source_hash = format!("resolve-slice-input-mixed-{}", nonce());
        // Mixed list: one suppressed, one active.
        // With predicate `all(|o| o.suppressed)` this evaluates to false (not all are suppressed),
        // so the function checks the cache and returns the ops-cache path.
        // If mutated to `any(|o| o.suppressed)` this would evaluate to true (at least one is suppressed),
        // causing the function to return source_path instead — test fails.
        obj.operations = vec![hole_op("h1", true), hole_op("h2", false)];

        // Pre-materialize the cache file at the exact path.
        let root = slicer_state::object_cache_dir().expect("object_cache_dir");
        std::fs::create_dir_all(&root).expect("create cache dir");
        let cache_path = ops_cache_path(
            &root,
            &obj.source_hash,
            obj.step_part_index,
            &ops_hash(&obj.operations),
        );
        std::fs::write(&cache_path, b"stub stl bytes").expect("pre-materialize cache stub");
        write_skipped_sidecar(&cache_path, &[]).expect("pre-materialize its skips");

        let resolved = resolve_slice_input_path(&obj).await;
        let _ = std::fs::remove_file(&cache_path);
        let _ = std::fs::remove_file(ops_skipped_sidecar_path(&cache_path));

        // Must return the cache path, not the source path, because we have at least
        // one active (non-suppressed) operation.
        let resolved = resolved.expect("a cache hit needs no cut");
        assert_eq!(resolved, cache_path.to_string_lossy());
        assert!(resolved.contains("ops-"));
        assert_ne!(resolved, obj.source_path);
    }

    /// A window rebuilding its scene asks for the mesh the STORED stack cuts.
    /// On a cache hit that is the very file the slice reads, and nothing runs
    /// the FFI: nothing is newly applied, the ops the cut left out come from
    /// its sidecar, and the cached bytes stay as they were.
    #[tokio::test]
    async fn get_materialized_mesh_returns_the_cached_cut_the_slice_reads() {
        let state = StateLayer::new();
        let dispatcher = Dispatcher::new(state.clone());
        let object_id = ObjectId(format!("obj-materialized-{}", nonce()));
        let source_hash = format!("get-materialized-hit-{}", nonce());
        state.apply(ProjectEvent::ObjectAdded {
            object_id: object_id.clone(),
            name: "cube".to_string(),
            source_path: "/tmp/fixtures/cube.stl".to_string(),
            triangle_count: 12,
            bounds: Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            source_hash: source_hash.clone(),
            step_part: None,
            split_part: None,
            transform: None,
        });
        let stack = vec![hole_op("h1", true), hole_op("h2", false)];
        state.apply(ProjectEvent::ObjectOperationsSet {
            object_id: object_id.clone(),
            operations: stack.clone(),
        });
        let root = slicer_state::object_cache_dir().expect("object_cache_dir");
        std::fs::create_dir_all(&root).expect("create cache dir");
        let cache_path = ops_cache_path(&root, &source_hash, None, &ops_hash(&stack));
        std::fs::write(&cache_path, b"stub stl bytes").expect("pre-materialize cache stub");
        let skipped = SkippedOp {
            id: "h2".to_string(),
            code: "misses_part".to_string(),
            reason: "nothing there".to_string(),
        };
        write_skipped_sidecar(&cache_path, &[skipped]).expect("pre-materialize its skips");
        let mut events = state.subscribe();
        let history = (state.can_undo(), state.can_redo());

        let mesh = dispatcher
            .handle(
                "object.get_materialized_mesh",
                json!({ "object_id": object_id.0.clone() }),
            )
            .await;
        let snapshot = state.snapshot();
        let sliced = resolve_slice_input_path(&snapshot.objects[&object_id]).await;
        let cached_bytes = std::fs::read(&cache_path);
        let _ = std::fs::remove_file(&cache_path);
        let _ = std::fs::remove_file(ops_skipped_sidecar_path(&cache_path));

        // Read-only through the cache path too, not only for an inactive stack.
        assert!(
            events.try_recv().is_err(),
            "reading the cut must broadcast nothing"
        );
        assert_eq!((state.can_undo(), state.can_redo()), history);
        assert_eq!(state.snapshot().objects[&object_id].operations, stack);

        let mesh = mesh.expect("get_materialized_mesh should succeed");
        assert_eq!(
            mesh,
            json!({
                "materialized_path": cache_path.to_string_lossy(),
                "applied_count": 0,
                "skipped_count": 1,
                "skipped_op_ids": ["h2"],
                "skipped_ops": [{ "id": "h2", "code": "misses_part", "reason": "nothing there" }],
            })
        );
        assert_eq!(
            mesh["materialized_path"],
            json!(sliced.expect("a cache hit needs no cut"))
        );
        assert_eq!(cached_bytes.expect("cache file"), b"stub stl bytes");
    }

    /// `object.set_operations` looks at the object before it cuts and again
    /// where its event goes in: the cut takes seconds, and a part may take
    /// the object as one of its bodies meanwhile. This pins the second look —
    /// the stack is not recorded on the body, and nothing is broadcast.
    #[test]
    fn a_stack_is_not_recorded_on_an_object_a_part_took_during_the_cut() {
        let state = StateLayer::new();
        let object_id = ObjectId(format!("obj-taken-{}", nonce()));
        state.apply(ProjectEvent::ObjectAdded {
            object_id: object_id.clone(),
            name: "cube".to_string(),
            source_path: "/tmp/fixtures/cube.stl".to_string(),
            triangle_count: 12,
            bounds: Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            source_hash: format!("taken-{}", nonce()),
            step_part: None,
            split_part: None,
            transform: None,
        });
        let plain = state.snapshot().objects[&object_id].geometry();
        state.apply(ProjectEvent::ObjectGeometryUpdated {
            object_id: object_id.clone(),
            geometry: slicer_types::ObjectGeometry {
                body: Some(slicer_types::BodyRef {
                    part_id: slicer_types::PartId("part-1".to_string()),
                    body_key: "f1:main".to_string(),
                }),
                ..plain
            },
        });
        let mut events = state.subscribe();
        let history = (state.can_undo(), state.can_redo());

        let refused = record_operations(&state, object_id.clone(), vec![hole_op("h1", false)]);

        let err = refused.expect_err("a body's holes are its part's");
        assert!(err.to_string().contains("part-1"), "{err}");
        assert!(events.try_recv().is_err(), "nothing is broadcast");
        assert_eq!((state.can_undo(), state.can_redo()), history);
        assert!(state.snapshot().objects[&object_id].operations.is_empty());
    }

    /// STL bytes for a `geometry.build` tree, through the mesh kernel —
    /// which, unlike the cutter, is in every build.
    fn stl_of(tree: Value) -> Vec<u8> {
        let tree: slicer_geometry::CsgNode = serde_json::from_value(tree).expect("a valid tree");
        slicer_geometry::build_stl(&tree)
            .expect("the mesh kernel builds it")
            .stl
    }

    /// A 20 mm cube spanning 30..50 × 40..60 × 5..25: off the origin on every
    /// axis, so a cut left in the cutter's recentered frame cannot pass for it.
    fn off_origin_cube() -> Vec<u8> {
        stl_of(json!({"op":"translate","offset":[30.0, 40.0, 5.0],
            "child":{"op":"box","size":[20.0, 20.0, 20.0]}}))
    }

    /// The cut of a Ø6 through-hole in `off_origin_cube`, written the way
    /// `apply_operations` writes it: recentered on the cube's bounding box, so
    /// the cube spans -10..10 × -10..10 × 0..20 and the hole's axis stands at
    /// (3, -2) — at (43, 48) in the source.
    fn recentered_cut_of_off_origin_cube() -> Vec<u8> {
        stl_of(json!({"op":"difference","children":[
            {"op":"translate","offset":[-10.0, -10.0, 0.0],
             "child":{"op":"box","size":[20.0, 20.0, 20.0]}},
            {"op":"translate","offset":[3.0, -2.0, -1.0],
             "child":{"op":"cylinder","diameter":6.0,"height":22.0,"segments":64}},
        ]}))
    }

    /// The through-hole `recentered_cut_of_off_origin_cube` shows, as a stack
    /// entry: feature-op origins are in the recentered frame.
    fn off_origin_cube_hole() -> slicer_types::FeatureOp {
        slicer_types::FeatureOp {
            id: format!("h-off-origin-{}", nonce()),
            kind: "hole".to_string(),
            suppressed: false,
            origin: [3.0, -2.0, 20.0],
            axis: [0.0, 0.0, -1.0],
            radius_mm: 3.0,
            depth_mm: 20.0,
            through: true,
            ..Default::default()
        }
    }

    /// Add `off_origin_cube` as an object whose stack is `off_origin_cube_hole`,
    /// and — when `cached_cut` is set — put that stack's cut in the ops cache
    /// under the key a cut through the FFI would have written it to, so the
    /// call under test finds it without the cutter. Returns the files to
    /// remove afterwards.
    ///
    /// File names and the source hash derive from `object_id`, which each
    /// test makes its own: `nonce()` alone repeats between tests running in
    /// parallel, since the clock it reads ticks in microseconds on macOS.
    fn seed_off_origin_cube_with_hole(
        state: &StateLayer,
        object_id: &ObjectId,
        cached_cut: bool,
    ) -> Vec<std::path::PathBuf> {
        let source = std::env::temp_dir().join(format!("{}-source.stl", object_id.0));
        std::fs::write(&source, off_origin_cube()).expect("write the source");
        let source_hash = format!("{}-source", object_id.0);
        state.apply(ProjectEvent::ObjectAdded {
            object_id: object_id.clone(),
            name: "cube".to_string(),
            source_path: source.to_string_lossy().into_owned(),
            triangle_count: 12,
            bounds: Bounds {
                min: [30.0, 40.0, 5.0],
                max: [50.0, 60.0, 25.0],
            },
            source_hash: source_hash.clone(),
            step_part: None,
            split_part: None,
            transform: None,
        });
        let stack = vec![off_origin_cube_hole()];
        state.apply(ProjectEvent::ObjectOperationsSet {
            object_id: object_id.clone(),
            operations: stack.clone(),
        });
        if !cached_cut {
            return vec![source];
        }
        let root = slicer_state::object_cache_dir().expect("object_cache_dir");
        std::fs::create_dir_all(&root).expect("create cache dir");
        let cut = ops_cache_path(&root, &source_hash, None, &ops_hash(&stack));
        std::fs::write(&cut, recentered_cut_of_off_origin_cube()).expect("cache the cut");
        // A cut is a cache hit only with its sidecar (`cached_cut_skips`).
        write_skipped_sidecar(&cut, &[]).expect("cache the cut's skips");
        let sidecar = ops_skipped_sidecar_path(&cut);
        vec![source, cut, sidecar]
    }

    fn remove_files(files: &[std::path::PathBuf]) {
        for file in files {
            let _ = std::fs::remove_file(file);
        }
    }

    fn assert_point_near(actual: &Value, expected: [f64; 3], what: &str) {
        let actual: [f64; 3] = serde_json::from_value(actual.clone()).expect("a point");
        let off = (0..3)
            .map(|i| (actual[i] - expected[i]).abs())
            .fold(0.0, f64::max);
        assert!(off < 1e-4, "{what}: expected {expected:?}, got {actual:?}");
    }

    /// `mesh.measure` of an object measures the cut its feature-op stack makes
    /// — the mesh a slice reads — not the uncut source, and places it where
    /// the source puts the part, not where the cutter's recentering left it.
    #[tokio::test]
    async fn mesh_measure_of_an_object_measures_its_cut_in_the_source_frame() {
        let state = StateLayer::new();
        let dispatcher = Dispatcher::new(state.clone());
        let object_id = ObjectId(format!("obj-measure-cut-{}", nonce()));
        let files = seed_off_origin_cube_with_hole(&state, &object_id, true);

        let measured = dispatcher
            .handle("mesh.measure", json!({ "object_id": object_id.0 }))
            .await;
        remove_files(&files);

        let measured = measured.expect("mesh.measure should succeed");
        let cut_volume = slicer_geometry::mesh::TriangleMesh::from_stl_bytes(
            &recentered_cut_of_off_origin_cube(),
        )
        .expect("the cut parses")
        .volume_mm3();
        let volume = measured["volume_mm3"].as_f64().expect("a volume");
        assert!(
            (volume - cut_volume).abs() < 1e-6,
            "the cut holds {cut_volume:.2} mm³, measured {volume:.2} mm³ \
             (8000 is the uncut cube)"
        );
        assert_point_near(&measured["bounds_min"], [30.0, 40.0, 5.0], "bounds_min");
        assert_point_near(&measured["bounds_max"], [50.0, 60.0, 25.0], "bounds_max");
    }

    /// `verify.bore` of an object finds the hole its stack cut, at the axis
    /// that hole has in the source's frame.
    #[tokio::test]
    async fn verify_bore_of_an_object_finds_the_hole_its_stack_cut() {
        let state = StateLayer::new();
        let dispatcher = Dispatcher::new(state.clone());
        let object_id = ObjectId(format!("obj-bore-cut-{}", nonce()));
        let files = seed_off_origin_cube_with_hole(&state, &object_id, true);

        let verdict = dispatcher
            .handle(
                "verify.bore",
                json!({
                    "object_id": object_id.0,
                    "axis": [43.0, 48.0],
                    "z_from": 6.0,
                    "z_to": 24.0,
                    "need_diameter_mm": 5.9,
                }),
            )
            .await;
        remove_files(&files);

        let verdict = verdict.expect("verify.bore should succeed");
        assert_eq!(verdict["ok"], true, "{verdict}");
        let diameter = verdict["detail"]["min_diameter_mm"]
            .as_f64()
            .expect("a diameter");
        assert!(
            (diameter - 6.0).abs() < 0.05,
            "the hole is Ø6, measured Ø{diameter:.3}: {verdict}"
        );
    }

    /// A cut is placed by the uncut part it was cut from. With that part gone
    /// the measurement cannot tell where the cut sits, and says so, rather
    /// than fail on reading a file the caller never named.
    #[tokio::test]
    async fn mesh_measure_of_a_cut_whose_uncut_part_is_gone_says_why_it_needs_it() {
        let state = StateLayer::new();
        let dispatcher = Dispatcher::new(state.clone());
        // The label keeps "uncut" out of the id: the source's file name is
        // derived from it and would satisfy the check below by itself.
        let object_id = ObjectId(format!("obj-measure-source-gone-{}", nonce()));
        let files = seed_off_origin_cube_with_hole(&state, &object_id, true);
        std::fs::remove_file(&files[0]).expect("drop the source");

        let measured = dispatcher
            .handle("mesh.measure", json!({ "object_id": object_id.0 }))
            .await;
        remove_files(&files);

        let text = measured
            .expect_err("a cut cannot be placed without its uncut part")
            .to_string();
        assert!(
            text.contains(&format!("object {}", object_id.0)) && text.contains("uncut"),
            "the error must say the object's uncut part is needed to place its cut: {text}"
        );
    }

    /// A refusal over a stack that cannot be cut names the object, its
    /// operations and the call that asked — one error, not a slice's error
    /// wrapped in another.
    #[cfg(not(feature = "with-libslic3r"))]
    fn assert_refusal_names_the_stack(text: &str, object_id: &ObjectId, method: &str) {
        assert!(
            text.contains(&object_id.0) && text.contains("operations"),
            "the refusal must name the object and its operations: {text}"
        );
        assert!(
            text.contains(&format!("{method}:")),
            "the refusal must name {method}: {text}"
        );
        assert_eq!(
            text.matches("project:").count(),
            1,
            "one error, not one wrapped in another: {text}"
        );
    }

    /// Without the cutter (`with-libslic3r` off) no stack can be cut. A
    /// measurement then refuses, naming the object and its operations, rather
    /// than measure the uncut source as if it were the part.
    #[cfg(not(feature = "with-libslic3r"))]
    #[tokio::test]
    async fn mesh_measure_refuses_an_object_whose_stack_cannot_be_cut() {
        let state = StateLayer::new();
        let dispatcher = Dispatcher::new(state.clone());
        let object_id = ObjectId(format!("obj-measure-uncuttable-{}", nonce()));
        let files = seed_off_origin_cube_with_hole(&state, &object_id, false);

        let measured = dispatcher
            .handle("mesh.measure", json!({ "object_id": object_id.0 }))
            .await;
        remove_files(&files);

        let err = measured.expect_err("an uncut part is not the part");
        assert_refusal_names_the_stack(&err.to_string(), &object_id, "mesh.measure");
    }

    /// Same for `slice.estimate`: the grams of the uncut source are not the
    /// grams of the part.
    #[cfg(not(feature = "with-libslic3r"))]
    #[tokio::test]
    async fn slice_estimate_refuses_an_object_whose_stack_cannot_be_cut() {
        let state = StateLayer::new();
        let dispatcher = Dispatcher::new(state.clone());
        let object_id = ObjectId(format!("obj-estimate-uncuttable-{}", nonce()));
        let files = seed_off_origin_cube_with_hole(&state, &object_id, false);

        let estimated = dispatcher
            .handle("slice.estimate", json!({ "object_id": object_id.0 }))
            .await;
        remove_files(&files);

        let err = estimated.expect_err("an uncut part is not the part");
        assert_refusal_names_the_stack(&err.to_string(), &object_id, "slice.estimate");
    }

    /// `slice.estimate` costs an object as a print slices it: what it hands
    /// the slicer for an `object_id` carries the object's transform, its own
    /// settings, paint, layer ranges and layer-height profile. It used to be
    /// the raw file with an identity transform and nothing else, so a part
    /// the user had doubled was costed at its imported size.
    #[tokio::test]
    async fn slice_estimate_is_given_the_object_as_a_print_slices_it() {
        let state = StateLayer::new();
        let dispatcher = Dispatcher::new(state.clone());
        let object_id = ObjectId(format!("obj-estimate-state-{}", nonce()));
        // Never read: resolving what to slice does not open the mesh.
        let source = format!("/tmp/{}-no-such-mesh.stl", object_id.0);
        state.apply(ProjectEvent::ObjectAdded {
            object_id: object_id.clone(),
            name: "cube".to_string(),
            source_path: source.clone(),
            triangle_count: 12,
            bounds: Bounds {
                min: [0.0, 0.0, 0.0],
                max: [20.0, 20.0, 20.0],
            },
            source_hash: object_id.0.clone(),
            step_part: None,
            split_part: None,
            transform: None,
        });
        let mut doubled = translated(128.0, 128.0, 0.0);
        for diagonal in [0, 5, 10] {
            doubled[diagonal] = 2.0;
        }
        state.apply(ProjectEvent::ObjectTransformed {
            object_id: object_id.clone(),
            new_transform: doubled,
            old_transform: identity(),
        });
        state.apply(ProjectEvent::ObjectOverrideSet {
            object_id: object_id.clone(),
            key: "layer_height".to_string(),
            new_value: Some(SettingValue::Number(0.1)),
        });
        state.apply(ProjectEvent::ObjectSeamFacetsSet {
            object_id: object_id.clone(),
            facets_rle: Some("seam-mask".to_string()),
            target_point: None,
        });
        state.apply(ProjectEvent::ObjectSupportFacetsSet {
            object_id: object_id.clone(),
            facets_rle: Some("support-mask".to_string()),
        });
        state.apply(ProjectEvent::ObjectFanFacetsSet {
            object_id: object_id.clone(),
            facets_rle: Some("fan-mask".to_string()),
        });
        let profile = vec![0.0, 0.2, 40.0, 0.1];
        state.apply(ProjectEvent::ObjectLayerHeightProfileSet {
            object_id: object_id.clone(),
            profile: profile.clone(),
        });
        state.apply(ProjectEvent::ObjectLayerRangesSet {
            object_id: object_id.clone(),
            ranges: vec![slicer_types::project::LayerConfigRange {
                id: "solid-base".to_string(),
                z_min: 0.0,
                z_max: 5.0,
                overrides: BTreeMap::from([(
                    "sparse_infill_density".to_string(),
                    SettingValue::Number(100.0),
                )]),
                fan: None,
            }],
        });

        let objects = dispatcher
            .sliced_objects(&json!({ "object_id": object_id.0 }))
            .await
            .expect("the object resolves");
        let inputs = objects
            .get(&object_id)
            .expect("the named object is resolved");

        assert_eq!(inputs.input_path, source);
        assert_eq!(inputs.part_index, -1);
        assert_eq!(inputs.object_transform, doubled);
        let layer_height = inputs
            .object_overrides
            .iter()
            .find(|o| o.key == "layer_height")
            .map(|o| o.value.parse::<f64>());
        assert!(
            matches!(layer_height, Some(Ok(h)) if (h - 0.1).abs() < 1e-9),
            "the object's own layer height: {:?}",
            inputs.object_overrides
        );
        assert_eq!(inputs.seam_facets_rle, "seam-mask");
        assert_eq!(inputs.support_facets_rle, "support-mask");
        assert_eq!(inputs.fan_facets_rle.as_deref(), Some("fan-mask"));
        assert_eq!(inputs.layer_height_profile, profile);
        let [range] = inputs.layer_ranges.as_slice() else {
            panic!("one layer range: {:?}", inputs.layer_ranges);
        };
        assert_eq!((range.z_min, range.z_max), (0.0, 5.0));
        assert_eq!(
            range.overrides,
            vec![("sparse_infill_density".to_string(), "100".to_string())]
        );
    }

    /// The second of two cold edit-mesh reads of one STEP part misses the
    /// cache, then queues behind the FFI lock while the first tessellates.
    /// Once through, it takes the mesh the first one wrote instead of
    /// tessellating again. The test plays the first read: it holds the lock
    /// and writes the entry. The source does not exist, so a read that ran
    /// the FFI anyway would fail.
    #[tokio::test]
    async fn edit_mesh_read_queued_behind_the_ffi_lock_reuses_the_mesh_written_meanwhile() {
        use std::future::Future;
        use std::task::{Context, Waker};

        let state = StateLayer::new();
        let dispatcher = Dispatcher::new(state.clone());
        let object_id = ObjectId(format!("obj-edit-mesh-queued-{}", nonce()));
        let source_hash = format!("edit-mesh-queued-{}", nonce());
        state.apply(ProjectEvent::ObjectAdded {
            object_id: object_id.clone(),
            name: "part".to_string(),
            source_path: format!("/tmp/fixtures/no-such-part-{}.stp", nonce()),
            triangle_count: 12,
            bounds: Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            source_hash: source_hash.clone(),
            step_part: None,
            split_part: None,
            transform: None,
        });
        let root = slicer_state::object_cache_dir().expect("object_cache_dir");
        std::fs::create_dir_all(&root).expect("create cache dir");
        let (stl_path, map_path) = edit_mesh_cache_paths(&root, &source_hash, None);

        let ffi_lock = STEP_FFI_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut read = Box::pin(dispatcher.handle(
            "object.get_edit_mesh",
            json!({ "object_id": object_id.0.clone() }),
        ));
        let first_poll = read.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(
            first_poll.is_pending(),
            "the read must miss the cache and wait for the FFI lock"
        );
        std::fs::write(&stl_path, [0u8; 84]).expect("write the mesh");
        std::fs::write(&map_path, r#"{"faces":[]}"#).expect("write the facemap");
        drop(ffi_lock);
        let mesh = read.await;
        let _ = std::fs::remove_file(&stl_path);
        let _ = std::fs::remove_file(&map_path);

        assert_eq!(
            mesh.expect("the read takes the mesh written while it waited"),
            json!({
                "mesh_path": stl_path.to_string_lossy(),
                "face_map_path": map_path.to_string_lossy(),
            })
        );
    }

    /// Regression: `object.import_local` gives every sibling object of a
    /// multi-solid STEP fan-out the SAME `source_hash` (one content hash for
    /// the whole imported file — see that handler), distinguished only by
    /// `step_part_index`. Before `ops_cache_path` took a `part_index`
    /// parameter, two such siblings with an identical op list (e.g. both
    /// suppressed, or two independently-authored ops that happen to hash the
    /// same) collapsed to the SAME cache path — the daemon's on-disk
    /// correctness depended entirely on the WS client (UI) minting distinct
    /// op ids, an invariant the public API cannot enforce. The same
    /// `source_hash` and the same `ops_hash` with a different `part_index`
    /// MUST still produce different paths.
    #[test]
    fn ops_cache_path_differs_by_step_part_index_for_same_source_and_ops_hash() {
        let root = std::path::PathBuf::from("/tmp/fixtures-cache-root");
        let source_hash = "shared-step-source-hash";
        let ops = "shared-ops-hash";

        let part0 = ops_cache_path(&root, source_hash, Some(0), ops);
        let part1 = ops_cache_path(&root, source_hash, Some(1), ops);
        let part_none = ops_cache_path(&root, source_hash, None, ops);

        assert_ne!(
            part0, part1,
            "distinct STEP parts of one import must not collide on cache path"
        );
        // `None` (non-STEP / legacy caller) normalizes to the same slot as
        // `Some(0)` — same convention `edit_mesh_cache_paths` uses.
        assert_eq!(
            part_none, part0,
            "part_index None must normalize to the same path as Some(0)"
        );
        assert_ne!(part_none, part1);
    }

    /// The cache filename carries a format-version token (`v1`) independent
    /// of its content-addressed inputs, so a future change to
    /// `apply_operations`'s output for an unchanged (source, part, ops)
    /// triple (e.g. a cutter-geometry fix like 776d00e) can invalidate old
    /// cache entries by bumping the token, instead of silently serving a
    /// stale STL that still matches the old hash-based key.
    /// Cuts are cached under the hash of the stack's encoding, and a plain
    /// hole's cut did not change when heads arrived. Encoding the new fields
    /// for it would re-cut every project for nothing.
    #[test]
    fn a_plain_holes_encoding_has_no_head_in_it() {
        let encoded = serde_json::to_value(hole_op("h", false)).unwrap();

        assert_eq!(
            encoded.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "axis",
                "depth_mm",
                "id",
                "kind",
                "origin",
                "radius_mm",
                "suppressed",
                "through"
            ]
        );
    }

    #[test]
    fn ops_cache_path_carries_a_format_version_token() {
        let root = std::path::PathBuf::from("/tmp/fixtures-cache-root");
        let path = ops_cache_path(&root, "hash", Some(0), "ops");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("ops-v2-"),
            "cache filename must carry a version token, got: {name}"
        );
    }

    #[test]
    fn resolve_slice_part_index_is_minus_one_for_non_step_object() {
        let obj = object(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            identity(),
        );
        assert_eq!(obj.step_part_index, None);

        assert_eq!(resolve_slice_part_index(&obj, &obj.source_path), -1);
    }

    #[test]
    fn resolve_slice_part_index_passes_through_step_part_when_input_is_source() {
        let mut obj = object(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            identity(),
        );
        obj.source_path = "/tmp/fixtures/assembly.step".to_string();
        obj.step_part_index = Some(2);

        // No materialization happened (resolved input == source): the
        // original STEP part index must still reach `run_slice` unchanged,
        // exactly like before `resolve_slice_input_path` existed.
        assert_eq!(resolve_slice_part_index(&obj, &obj.source_path), 2);
    }

    #[test]
    fn resolve_slice_part_index_resets_to_minus_one_when_input_is_materialized() {
        let mut obj = object(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            identity(),
        );
        obj.source_path = "/tmp/fixtures/assembly.step".to_string();
        // This object is part 2 of a multi-solid STEP fan-out (see
        // `object.import_local`) AND has an active op stack materialized
        // to its own single-object cache STL (see `resolve_slice_input_
        // path`). `apply_operations` already selected part 2, extracted
        // ITS mesh, and wrote ONE object into the cache file — so the
        // stale part index must NOT be forwarded, or `run_slice`'s
        // `Model::read_from_file(cache_stl)` (exactly one object) rejects
        // index 2 as out of range and fails the whole slice.
        obj.step_part_index = Some(2);
        let materialized_path = "/tmp/fixtures-cache-root/object_cache/ops-hash-hash.stl";

        assert_eq!(resolve_slice_part_index(&obj, materialized_path), -1);
    }

    #[test]
    fn resolve_slice_paint_masks_forwards_stored_masks_when_input_is_source() {
        let mut obj = object(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            identity(),
        );
        obj.source_path = "/tmp/fixtures/cube.stl".to_string();
        obj.seam_facets_rle = Some("AQID".to_string());
        obj.support_facets_rle = Some("BAUG".to_string());
        obj.fan_facets_rle = Some("kwEAAAA=".to_string());

        // No materialization (resolved input == source): the masks index this
        // exact mesh, so all three pass through verbatim.
        let masks = resolve_slice_paint_masks(&obj, &obj.source_path);
        assert_eq!(masks.seam_facets_rle, "AQID");
        assert_eq!(masks.support_facets_rle, "BAUG");
        assert_eq!(masks.fan_facets_rle.as_deref(), Some("kwEAAAA="));
    }

    #[test]
    fn resolve_slice_paint_masks_are_empty_for_unpainted_source_object() {
        let obj = object(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            identity(),
        );
        // No paint set (both masks `None`) and no ops: forwarding must yield
        // empty strings, not panic — same `unwrap_or_default` the old inline
        // `objects_for_ffi` used.
        assert_eq!(obj.seam_facets_rle, None);
        assert_eq!(
            resolve_slice_paint_masks(&obj, &obj.source_path),
            SlicePaintMasks::default()
        );
    }

    #[test]
    fn resolve_slice_paint_masks_drops_all_masks_when_input_is_materialized() {
        let mut obj = object(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            identity(),
        );
        obj.source_path = "/tmp/fixtures/cube.stl".to_string();
        obj.seam_facets_rle = Some("AQID".to_string());
        obj.support_facets_rle = Some("BAUG".to_string());
        obj.fan_facets_rle = Some("kwEAAAA=".to_string());
        // An active op stack materialized to its own single-object cache STL
        // (see `resolve_slice_input_path`). `apply_operations` rewrote the
        // triangle set, so the source-indexed masks no longer correspond to
        // the mesh being sliced and MUST be dropped — otherwise the paint
        // silently lands on the wrong facets of the cut mesh.
        obj.operations = vec![hole_op("h1", false)];
        let materialized = "/tmp/fixtures-cache-root/object_cache/ops-hash-hash.stl";
        assert_ne!(materialized, obj.source_path);

        assert_eq!(
            resolve_slice_paint_masks(&obj, materialized),
            SlicePaintMasks::default()
        );
    }

    fn resolved_config_with_process(process: BTreeMap<String, Value>) -> ResolvedSliceConfig {
        resolved_config_with_process_and_filaments(process, Vec::new())
    }

    fn resolved_config_with_process_and_filaments(
        process: BTreeMap<String, Value>,
        filaments: Vec<BTreeMap<String, Value>>,
    ) -> ResolvedSliceConfig {
        ResolvedSliceConfig {
            schema_version: 1,
            profile_snapshot_hash: "hash".to_string(),
            extruders: vec![ResolvedExtruderConfig {
                slot_id: 0,
                nozzle_diameter: 0.4,
                nozzle_offset_xy: [0.0, 0.0],
                retraction_length: None,
                retraction_speed: None,
                retract_restart_extra: None,
                deretraction_speed: None,
                retraction_minimum_travel: None,
                retract_when_changing_layer: None,
                wipe: None,
                wipe_distance: None,
                retract_before_wipe: None,
                retract_length_toolchange: None,
                z_hop: None,
                z_hop_types: None,
                travel_slope: None,
                retract_lift_above: None,
                retract_lift_below: None,
                retract_lift_enforce: None,
                min_layer_height: None,
                max_layer_height: None,
                filament_type: None,
                filament_colour: None,
                nozzle_temperature_initial_layer: None,
                nozzle_temperature: None,
                bed_temperature_initial_layer_single: None,
                bed_temperature: None,
            }],
            machine: BTreeMap::new(),
            process,
            filaments,
            calibration: Default::default(),
            role_mapping: Vec::new(),
            print_head: None,
            unsupported_settings: Vec::new(),
        }
    }

    #[test]
    fn print_head_propagates_to_idex_inputs() {
        // Two-extruder IDEX config with single-head selection → the bridge
        // input must carry print_head so it can route the plate to that tool.
        let mut cfg = resolved_config_with_process_and_filaments(
            BTreeMap::new(),
            vec![BTreeMap::new(), BTreeMap::new()],
        );
        let mut slot1 = cfg.extruders[0].clone();
        slot1.slot_id = 1;
        slot1.nozzle_offset_xy = [18.0, 0.0];
        cfg.extruders.push(slot1);
        cfg.print_head = Some(1);

        let idex = idex_from_resolved_config(&cfg).expect("valid idex config");
        assert_eq!(idex.extruder_count, 2);
        assert_eq!(idex.print_head, Some(1));
    }

    #[test]
    fn print_head_out_of_range_is_rejected() {
        let mut cfg = resolved_config_with_process_and_filaments(
            BTreeMap::new(),
            vec![BTreeMap::new(), BTreeMap::new()],
        );
        let mut slot1 = cfg.extruders[0].clone();
        slot1.slot_id = 1;
        cfg.extruders.push(slot1);
        cfg.print_head = Some(5);

        let err = idex_from_resolved_config(&cfg).expect_err("out-of-range head");
        assert!(
            err.contains("print_head"),
            "expected print_head validation error, got: {err}"
        );
    }

    #[test]
    fn support_flag_uses_profile_v2_enable_support_key() {
        let mut process = BTreeMap::new();
        process.insert("enable_support".to_string(), json!(true));
        let config = resolved_config_with_process(process);

        assert!(process_support_enabled(&Some(config), &HashMap::new()));
    }

    #[test]
    fn process_value_helpers_prefer_profile_v2_keys() {
        let mut process = BTreeMap::new();
        process.insert("perimeter_count".to_string(), json!(2));
        process.insert("wall_loops".to_string(), json!(5));
        process.insert("infill_density".to_string(), json!(12.0));
        process.insert("sparse_infill_density".to_string(), json!(33.0));
        process.insert("infill_pattern".to_string(), json!("grid"));
        process.insert("sparse_infill_pattern".to_string(), json!("stars"));
        let config = Some(resolved_config_with_process(process));

        assert_eq!(
            resolved_process_f64_any(&config, &["wall_loops", "perimeter_count"])
                .map(|v| v.round() as i32),
            Some(5)
        );
        assert_eq!(
            resolved_process_f32_any(&config, &["sparse_infill_density", "infill_density"]),
            Some(33.0)
        );
        assert_eq!(
            resolved_process_string_any(&config, &["sparse_infill_pattern", "infill_pattern"]),
            Some("stars".to_string())
        );
    }

    #[test]
    fn infill_pattern_scalar_uses_orca_aliases_for_ffi() {
        let mut process = BTreeMap::new();
        process.insert("sparse_infill_pattern".to_string(), json!("stars"));
        let config = Some(resolved_config_with_process(process));

        assert_eq!(
            process_infill_pattern_for_ffi(&config, &HashMap::new()),
            "tri-hexagon"
        );
    }

    #[test]
    fn support_type_short_aliases_use_auto_modes() {
        let cases = [
            ("normal", "normal(auto)"),
            ("tree", "tree(auto)"),
            ("normal_auto", "normal(auto)"),
            ("tree_auto", "tree(auto)"),
            ("normal_manual", "normal(manual)"),
            ("tree_manual", "tree(manual)"),
        ];

        for (input, expected) in cases {
            assert_eq!(
                normalize_config_override_value("support_type", input.to_string()),
                expected
            );
        }
    }

    #[test]
    fn printable_area_array_uses_libslic3r_point_syntax() {
        let value = json!([[0.0, 0.0], [400.0, 0.0], [400.0, 365.0], [0.0, 365.0]]);

        assert_eq!(
            config_override_value_to_string_for_key("printable_area", &value),
            Some("0.0x0.0,400.0x0.0,400.0x365.0,0.0x365.0".to_string())
        );
    }

    #[test]
    fn resolved_config_overrides_use_profile_v2_orca_keys() {
        let mut process = BTreeMap::new();
        process.insert("wall_loops".to_string(), json!(7));
        process.insert("sparse_infill_density".to_string(), json!(33.0));
        process.insert("sparse_infill_pattern".to_string(), json!("stars"));
        process.insert("infill_speed".to_string(), json!(123.0));
        process.insert("solid_infill_speed".to_string(), json!(45.0));
        process.insert("gcode_resolution".to_string(), json!(0.01));
        process.insert("force_cooling_for_overhang".to_string(), json!(true));
        process.insert("outer_wall_first".to_string(), json!(true));
        process.insert("support_on_build_plate_only".to_string(), json!(true));
        let mut config = resolved_config_with_process(process);
        config
            .machine
            .insert("machine_model".to_string(), json!("CD400"));
        config.machine.insert(
            "machine_end_gcode".to_string(),
            json!("M104 S0\nG91\nG1 Z10 F600\nG90\n"),
        );
        config.machine.insert(
            "printable_area".to_string(),
            json!([[0, 0], [400, 0], [400, 365], [0, 365]]),
        );

        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(map.get("wall_loops").map(String::as_str), Some("7"));
        assert_eq!(
            map.get("sparse_infill_density").map(String::as_str),
            Some("33.0")
        );
        assert_eq!(
            map.get("sparse_infill_pattern").map(String::as_str),
            Some("tri-hexagon")
        );
        assert_eq!(
            map.get("sparse_infill_speed").map(String::as_str),
            Some("123.0")
        );
        assert_eq!(
            map.get("internal_solid_infill_speed").map(String::as_str),
            Some("45.0")
        );
        assert_eq!(map.get("resolution").map(String::as_str), Some("0.01"));
        assert_eq!(
            map.get("enable_overhang_bridge_fan").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            map.get("wall_sequence").map(String::as_str),
            Some("outer wall/inner wall")
        );
        assert_eq!(
            map.get("support_on_build_plate_only").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            map.get("printable_area").map(String::as_str),
            Some("0x0,400x0,400x365,0x365")
        );
        assert_eq!(
            map.get("machine_end_gcode").map(String::as_str),
            Some("M104 S0\nG91\nG1 Z10 F600\nG90\n")
        );
        assert!(!map.contains_key("machine_model"));
    }

    /// Every value handed to libslic3r under `key`, in application order.
    /// Duplicates stay visible: the bridge applies them all and the last sticks.
    fn override_values(config: &ResolvedSliceConfig, key: &str) -> Vec<String> {
        collect_config_overrides(config)
            .into_iter()
            .filter(|o| o.key == key)
            .map(|o| o.value)
            .collect()
    }

    #[test]
    fn canonical_process_key_beats_its_alias_whatever_the_key_order() {
        for (enabled, legacy, expected) in [(true, false, "1"), (false, true, "0")] {
            let mut process = BTreeMap::new();
            process.insert("enable_overhang_bridge_fan".to_string(), json!(enabled));
            process.insert("force_cooling_for_overhang".to_string(), json!(legacy));
            let config = resolved_config_with_process(process);

            assert_eq!(
                override_values(&config, "enable_overhang_bridge_fan"),
                vec![expected.to_string()],
                "enable_overhang_bridge_fan={enabled}, force_cooling_for_overhang={legacy}"
            );
        }
    }

    #[test]
    fn a_process_alias_still_maps_when_the_canonical_key_is_absent() {
        let mut process = BTreeMap::new();
        process.insert("force_cooling_for_overhang".to_string(), json!(false));
        let config = resolved_config_with_process(process);

        assert_eq!(
            override_values(&config, "enable_overhang_bridge_fan"),
            vec!["0".to_string()]
        );
    }

    #[test]
    fn a_null_canonical_value_does_not_shadow_its_alias() {
        let mut process = BTreeMap::new();
        process.insert("enable_overhang_bridge_fan".to_string(), Value::Null);
        process.insert("force_cooling_for_overhang".to_string(), json!(false));
        let config = resolved_config_with_process(process);

        assert_eq!(
            override_values(&config, "enable_overhang_bridge_fan"),
            vec!["0".to_string()]
        );
    }

    #[test]
    fn canonical_filament_key_beats_its_alias_within_a_slot() {
        // `max_fan_speed` sorts after `fan_max_speed`, so plain per-slot
        // last-writer-wins let the alias through.
        let mut first = BTreeMap::new();
        first.insert("fan_max_speed".to_string(), json!(80));
        first.insert("max_fan_speed".to_string(), json!(20));
        let mut second = BTreeMap::new();
        second.insert("max_fan_speed".to_string(), json!(35));
        let config =
            resolved_config_with_process_and_filaments(BTreeMap::new(), vec![first, second]);

        assert_eq!(
            override_values(&config, "fan_max_speed"),
            vec!["80,35".to_string()]
        );
    }

    fn object_with_overrides<const N: usize>(overrides: [(&str, SettingValue); N]) -> ObjectRecord {
        ObjectRecord {
            overrides: overrides
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
            ..object(
                Bounds {
                    min: [0.0; 3],
                    max: [10.0; 3],
                },
                identity(),
            )
        }
    }

    /// The bridge-bound overrides of one object, in application order.
    fn bridge_overrides(record: &ObjectRecord) -> Vec<(String, String)> {
        object_config_overrides_for(record)
            .into_iter()
            .map(|o| (o.key, o.value))
            .collect()
    }

    /// UI and the daemon schema name these settings differently from libslic3r,
    /// whose `handle_legacy` silently drops a name it does not know: the
    /// override was accepted and the object sliced without it.
    #[test]
    fn object_overrides_reach_libslic3r_under_its_option_names() {
        let record = object_with_overrides([
            ("infill_pattern", SettingValue::String("lines".into())),
            ("infill_density", SettingValue::Number(35.0)),
            ("support_enabled", SettingValue::Bool(true)),
            ("perimeter_count", SettingValue::Int(4)),
            ("layer_height", SettingValue::Number(0.12)),
            ("seam_position", SettingValue::String("back".into())),
        ]);

        let map: HashMap<_, _> = bridge_overrides(&record).into_iter().collect();

        assert_eq!(
            map,
            HashMap::from([
                ("sparse_infill_pattern".to_string(), "line".to_string()),
                ("sparse_infill_density".to_string(), "35.0".to_string()),
                ("enable_support".to_string(), "1".to_string()),
                ("wall_loops".to_string(), "4".to_string()),
                ("layer_height".to_string(), "0.12".to_string()),
                ("seam_position".to_string(), "back".to_string()),
            ])
        );
    }

    /// `object.set_override` and `replace_state` store only `wall_loops`, but a
    /// record built any other way can still carry both spellings. The canonical
    /// key wins rather than whichever the overrides map yields last. Every
    /// record gets a fresh hasher, so 32 of them cover both iteration orders.
    #[test]
    fn object_wall_loops_beats_its_perimeter_count_alias() {
        for _ in 0..32 {
            let record = object_with_overrides([
                ("perimeter_count", SettingValue::Int(3)),
                ("wall_loops", SettingValue::Int(6)),
            ]);

            assert_eq!(
                bridge_overrides(&record),
                vec![("wall_loops".to_string(), "6".to_string())]
            );
        }
    }

    /// Every per-object key must reach libslic3r under a name its option table
    /// knows: `handle_legacy` drops any other name without an error, and a key
    /// the daemon itself drops never gets that far.
    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn every_object_overridable_key_maps_to_a_libslic3r_option() {
        let unknown: Vec<String> = slicer_types::settings::OVERRIDABLE_KEYS
            .iter()
            // Paint data, decoded by the bridge rather than passed as options.
            .filter(|key| !matches!(**key, "seam_facets_rle" | "seam_target_point"))
            .filter_map(
                |key| match normalize_config_override_key_value(key, String::new()) {
                    None => Some(format!("{key} -> dropped by the daemon")),
                    Some((mapped, _)) => (slicer_ffi::config_option_join_kind(&mapped)
                        == OptionJoinKind::Unknown)
                        .then(|| format!("{key} -> {mapped}")),
                },
            )
            .collect();

        assert!(
            unknown.is_empty(),
            "per-object keys libslic3r does not know: {unknown:?}"
        );
    }

    #[test]
    fn process_infill_and_support_aliases_map_to_libslic3r_names() {
        let config = resolved_config_with_process(BTreeMap::from([
            ("infill_pattern".to_string(), json!("lines")),
            ("infill_density".to_string(), json!(30)),
            ("support_enabled".to_string(), json!(true)),
        ]));

        assert_eq!(
            override_values(&config, "sparse_infill_pattern"),
            vec!["line".to_string()]
        );
        assert_eq!(
            override_values(&config, "sparse_infill_density"),
            vec!["30".to_string()]
        );
        assert_eq!(
            override_values(&config, "enable_support"),
            vec!["1".to_string()]
        );
        for alias in ["infill_pattern", "infill_density", "support_enabled"] {
            assert!(
                override_values(&config, alias).is_empty(),
                "{alias} reached libslic3r under its own name"
            );
        }
    }

    /// The profile's flat overrides must agree with the typed FFI scalars,
    /// which read the libslic3r spelling first (`process_infill_pattern_for_ffi`,
    /// `process_support_enabled`) — including `support_enabled`, which sorts
    /// after `enable_support` and would otherwise be applied last.
    #[test]
    fn libslic3r_infill_and_support_names_beat_their_aliases_in_the_process_bag() {
        let config = resolved_config_with_process(BTreeMap::from([
            ("infill_pattern".to_string(), json!("grid")),
            ("sparse_infill_pattern".to_string(), json!("honeycomb")),
            ("infill_density".to_string(), json!(12)),
            ("sparse_infill_density".to_string(), json!(33)),
            ("enable_support".to_string(), json!(true)),
            ("support_enabled".to_string(), json!(false)),
        ]));

        assert_eq!(
            override_values(&config, "sparse_infill_pattern"),
            vec!["honeycomb".to_string()]
        );
        assert_eq!(
            override_values(&config, "sparse_infill_density"),
            vec!["33".to_string()]
        );
        assert_eq!(
            override_values(&config, "enable_support"),
            vec!["1".to_string()]
        );
    }

    #[test]
    fn resolved_config_overrides_include_acceleration_overrides() {
        let mut process = BTreeMap::new();
        process.insert("default_acceleration".to_string(), json!(5000.0));
        process.insert("outer_wall_acceleration".to_string(), json!(4500.0));
        process.insert("inner_wall_acceleration".to_string(), json!(4200.0));
        process.insert("top_surface_acceleration".to_string(), json!(2000.0));
        process.insert("bridge_acceleration".to_string(), json!(1500.0));
        process.insert("sparse_infill_acceleration".to_string(), json!(4800.0));
        process.insert(
            "internal_solid_infill_acceleration".to_string(),
            json!(3600.0),
        );
        process.insert("travel_acceleration".to_string(), json!(5000.0));
        process.insert("initial_layer_acceleration".to_string(), json!(1200.0));

        let config = resolved_config_with_process(process);
        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(
            map.get("default_acceleration").map(String::as_str),
            Some("5000.0")
        );
        assert_eq!(
            map.get("outer_wall_acceleration").map(String::as_str),
            Some("4500.0")
        );
        assert_eq!(
            map.get("inner_wall_acceleration").map(String::as_str),
            Some("4200.0")
        );
        assert_eq!(
            map.get("top_surface_acceleration").map(String::as_str),
            Some("2000.0")
        );
        assert_eq!(
            map.get("bridge_acceleration").map(String::as_str),
            Some("1500.0")
        );
        assert_eq!(
            map.get("sparse_infill_acceleration").map(String::as_str),
            Some("4800.0")
        );
        assert_eq!(
            map.get("internal_solid_infill_acceleration")
                .map(String::as_str),
            Some("3600.0")
        );
        assert_eq!(
            map.get("travel_acceleration").map(String::as_str),
            Some("5000.0")
        );
        assert_eq!(
            map.get("initial_layer_acceleration").map(String::as_str),
            Some("1200.0")
        );
    }

    /// The profile's machine limits already carry libslic3r's names, so none of
    /// them may be renamed. `machine_max_acceleration_extruding` — the ceiling
    /// `GCodeWriter` clamps print and (on Klipper) travel accelerations to —
    /// used to be sent to `machine_max_acceleration_e`, the E-axis limit.
    #[test]
    fn machine_limit_keys_keep_their_libslic3r_names() {
        for key in [
            "machine_max_speed_x",
            "machine_max_speed_y",
            "machine_max_speed_z",
            "machine_max_speed_e",
            "machine_max_acceleration_x",
            "machine_max_acceleration_y",
            "machine_max_acceleration_e",
            "machine_max_acceleration_extruding",
            "machine_max_acceleration_z",
            "machine_max_acceleration_travel",
            "machine_max_jerk_x",
            "machine_max_jerk_y",
        ] {
            assert_eq!(
                normalize_config_override_key_value(key, "3500".to_string()),
                Some((key.to_string(), "3500".to_string())),
                "{key} must reach libslic3r under its own name"
            );
        }
    }

    /// The profile keeps one XY jerk (Klipper's `square_corner_velocity`);
    /// libslic3r has one option per axis. Dropping it left both axes at the
    /// bundled X1C preset's 9 mm/s under a QIDI `printer.cfg` limit of 8.
    #[test]
    fn xy_jerk_reaches_both_libslic3r_axes() {
        let mut config = resolved_config_with_process(BTreeMap::new());
        config
            .machine
            .insert("machine_max_jerk_xy".to_string(), json!(8.0));

        assert_eq!(
            override_values(&config, "machine_max_jerk_x"),
            vec!["8.0".to_string()]
        );
        assert_eq!(
            override_values(&config, "machine_max_jerk_y"),
            vec!["8.0".to_string()]
        );
        assert!(override_values(&config, "machine_max_jerk_xy").is_empty());
    }

    /// A per-axis jerk under libslic3r's own name is authoritative for its
    /// axis; the XY jerk still fills the other one.
    #[test]
    fn per_axis_jerk_beats_the_xy_jerk_on_its_own_axis() {
        for (axis, other) in [
            ("machine_max_jerk_x", "machine_max_jerk_y"),
            ("machine_max_jerk_y", "machine_max_jerk_x"),
        ] {
            let mut config = resolved_config_with_process(BTreeMap::new());
            config
                .machine
                .insert("machine_max_jerk_xy".to_string(), json!(8.0));
            config.machine.insert(axis.to_string(), json!(5.0));

            assert_eq!(
                override_values(&config, axis),
                vec!["5.0".to_string()],
                "{axis} set explicitly"
            );
            assert_eq!(
                override_values(&config, other),
                vec!["8.0".to_string()],
                "{other} from the XY jerk"
            );
        }
    }

    #[test]
    fn extruding_acceleration_ceiling_leaves_the_e_axis_limit_alone() {
        let mut config = resolved_config_with_process(BTreeMap::new());
        config.machine.insert(
            "machine_max_acceleration_extruding".to_string(),
            json!(3500.0),
        );

        assert_eq!(
            override_values(&config, "machine_max_acceleration_extruding"),
            vec!["3500.0".to_string()]
        );
        assert!(
            override_values(&config, "machine_max_acceleration_e").is_empty(),
            "the print ceiling was written to the E-axis limit"
        );
    }

    #[test]
    fn resolved_config_overrides_include_filament_cooling_keys() {
        let mut filament = BTreeMap::new();
        filament.insert("fan_min_speed".to_string(), json!(35));
        filament.insert("fan_max_speed".to_string(), json!(80));
        filament.insert("fan_disable_first_n_layers".to_string(), json!(3));

        let config = resolved_config_with_process_and_filaments(BTreeMap::new(), vec![filament]);
        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(map.get("fan_min_speed").map(String::as_str), Some("35"));
        assert_eq!(map.get("fan_max_speed").map(String::as_str), Some("80"));
        assert_eq!(
            map.get("close_fan_the_first_x_layers").map(String::as_str),
            Some("3")
        );
    }

    #[test]
    fn resolved_config_overrides_keep_multifilament_cooling_slot_order() {
        let mut petg = BTreeMap::new();
        petg.insert("fan_min_speed".to_string(), json!(30));
        petg.insert("fan_max_speed".to_string(), json!(100));
        petg.insert("fan_disable_first_n_layers".to_string(), json!(3));
        petg.insert("filament_min_layer_time".to_string(), json!(5));

        let mut pa_cf = BTreeMap::new();
        pa_cf.insert("fan_min_speed".to_string(), json!(0));
        pa_cf.insert("fan_max_speed".to_string(), json!(20));
        pa_cf.insert("fan_disable_first_n_layers".to_string(), json!(1));
        pa_cf.insert("filament_min_layer_time".to_string(), json!(15));

        let config = resolved_config_with_process_and_filaments(BTreeMap::new(), vec![petg, pa_cf]);
        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(map.get("fan_min_speed").map(String::as_str), Some("30,0"));
        assert_eq!(map.get("fan_max_speed").map(String::as_str), Some("100,20"));
        assert_eq!(
            map.get("close_fan_the_first_x_layers").map(String::as_str),
            Some("3,1")
        );
        assert_eq!(
            map.get("slow_down_layer_time").map(String::as_str),
            Some("5,15")
        );
    }

    #[test]
    fn resolved_config_overrides_keep_multifilament_pressure_advance_slot_order() {
        let mut petg = BTreeMap::new();
        petg.insert("enable_pressure_advance".to_string(), json!(true));
        petg.insert("pressure_advance".to_string(), json!(0.039));

        let mut pa_cf = BTreeMap::new();
        pa_cf.insert("enable_pressure_advance".to_string(), json!(true));
        pa_cf.insert("pressure_advance".to_string(), json!(0.04));

        let config = resolved_config_with_process_and_filaments(BTreeMap::new(), vec![petg, pa_cf]);
        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(
            map.get("enable_pressure_advance").map(String::as_str),
            Some("1,1")
        );
        assert_eq!(
            map.get("pressure_advance").map(String::as_str),
            Some("0.039,0.04")
        );
    }

    #[test]
    fn resolved_config_overrides_normalize_filament_aliases_without_unknown_keys() {
        let mut filament = BTreeMap::new();
        filament.insert("nozzle_temperature_normal".to_string(), json!(245));
        filament.insert("temperature".to_string(), json!(245));
        filament.insert("bed_temperature_normal".to_string(), json!(80));
        filament.insert("chamber_temperature_target".to_string(), json!(55));
        filament.insert("max_volumetric_speed".to_string(), json!(12.5));
        filament.insert("filament_min_layer_time".to_string(), json!(6.0));
        filament.insert("min_print_speed".to_string(), json!(8.0));
        filament.insert("filament_color_hex".to_string(), json!("#abcdef"));
        filament.insert("filament_retraction_length".to_string(), json!(1.2));
        filament.insert("filament_retraction_speed".to_string(), json!(45.0));
        filament.insert("pressure_advance_smooth_time".to_string(), json!(0.04));

        let config = resolved_config_with_process_and_filaments(BTreeMap::new(), vec![filament]);
        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(
            map.get("nozzle_temperature").map(String::as_str),
            Some("245")
        );
        assert_eq!(map.get("bed_temperature").map(String::as_str), Some("80"));
        assert_eq!(
            map.get("chamber_temperature").map(String::as_str),
            Some("55")
        );
        assert_eq!(
            map.get("filament_max_volumetric_speed").map(String::as_str),
            Some("12.5")
        );
        assert_eq!(
            map.get("slow_down_layer_time").map(String::as_str),
            Some("6.0")
        );
        assert_eq!(
            map.get("slow_down_min_speed").map(String::as_str),
            Some("8.0")
        );
        assert_eq!(
            map.get("filament_colour").map(String::as_str),
            Some("#abcdef")
        );
        assert!(!map.contains_key("nozzle_temperature_normal"));
        assert!(!map.contains_key("temperature"));
        assert!(!map.contains_key("filament_retraction_length"));
        assert!(!map.contains_key("filament_retraction_speed"));
        assert!(!map.contains_key("pressure_advance_smooth_time"));
    }

    #[test]
    fn support_interface_filament_routes_independently_of_support_base() {
        // BVOH-on-interface (soluble support for PA6/PA6-T on the CD400 IDEX):
        // the support BODY prints on the model filament (slot 1, cheaper/
        // stronger) while the support INTERFACE prints on soluble BVOH
        // (slot 2). The two schema keys must reach DISTINCT libslic3r keys:
        //   support_filament_index      -> support_filament            (base)
        //   support_interface_filament  -> support_interface_filament  (interface)
        // libslic3r uses 1-based extruder ids; 0 = "same as support base".
        let mut process = BTreeMap::new();
        process.insert("support_filament_index".to_string(), json!(1));
        process.insert("support_interface_filament".to_string(), json!(2));

        let config = resolved_config_with_process(process);
        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(map.get("support_filament").map(String::as_str), Some("1"));
        assert_eq!(
            map.get("support_interface_filament").map(String::as_str),
            Some("2"),
        );
        // The `_index` alias must not leak through unmapped alongside the real key.
        assert!(!map.contains_key("support_filament_index"));
    }

    #[test]
    fn resolved_extruder_extended_settings_reach_idex_inputs() {
        let mut config = resolved_config_with_process(BTreeMap::new());
        let extruder = &mut config.extruders[0];
        extruder.deretraction_speed = Some(35.0);
        extruder.retract_restart_extra = Some(0.08);
        extruder.retraction_minimum_travel = Some(1.5);
        extruder.retract_when_changing_layer = Some(true);
        extruder.wipe = Some(true);
        extruder.wipe_distance = Some(2.5);
        extruder.retract_before_wipe = Some(70.0);
        extruder.retract_length_toolchange = Some(12.0);
        extruder.z_hop = Some(0.4);
        extruder.z_hop_types = Some("normal".to_string());
        extruder.travel_slope = Some(12.5);
        extruder.retract_lift_above = Some(0.2);
        extruder.retract_lift_below = Some(10.0);
        extruder.min_layer_height = Some(0.08);
        extruder.max_layer_height = Some(0.3);

        let idex = idex_from_resolved_config(&config).expect("valid resolved config");

        assert_eq!(idex.deretraction_speed, vec![35.0]);
        assert_eq!(idex.retract_restart_extra, vec![0.08]);
        assert_eq!(idex.retraction_minimum_travel, vec![1.5]);
        assert_eq!(idex.retract_when_changing_layer, vec![true]);
        assert_eq!(idex.wipe, vec![true]);
        assert_eq!(idex.wipe_distance, vec![2.5]);
        assert_eq!(idex.retract_before_wipe_pct, vec![70.0]);
        assert_eq!(idex.retract_length_toolchange, vec![12.0]);
        assert_eq!(idex.z_hop, vec![0.4]);
        assert_eq!(idex.z_hop_types, vec!["normal".to_string()]);
        assert_eq!(idex.travel_slope_deg, vec![12.5]);
        assert_eq!(idex.retract_lift_above, vec![0.2]);
        assert_eq!(idex.retract_lift_below, vec![10.0]);
        assert_eq!(idex.min_layer_height, vec![0.08]);
        assert_eq!(idex.max_layer_height, vec![0.3]);
    }

    #[test]
    fn resolved_config_overrides_keep_multifilament_material_per_slot() {
        // Regression: with two filament slots, per-extruder vector keys must
        // be joined as "<slot0>,<slot1>" rather than overwritten by the last
        // slot. Pre-fix, PA-CF's max_volumetric_speed=10 silently overwrote
        // PETG's 30, capping outer-wall feedrate to ~123 mm/s regardless of
        // process target speed.
        let mut petg = BTreeMap::new();
        petg.insert("filament_type".to_string(), json!("PETG"));
        petg.insert("max_volumetric_speed".to_string(), json!(30.0));
        petg.insert("filament_diameter".to_string(), json!(1.74));
        petg.insert("filament_density".to_string(), json!(1.27));
        petg.insert("filament_flow_ratio".to_string(), json!(0.97));
        petg.insert("filament_cost".to_string(), json!(60.0));
        petg.insert("filament_shrink".to_string(), json!(98.5));
        petg.insert("filament_color_hex".to_string(), json!("#018001"));
        petg.insert("nozzle_temperature_normal".to_string(), json!(220));
        petg.insert("nozzle_temperature_initial_layer".to_string(), json!(235));
        petg.insert("bed_temperature_normal".to_string(), json!(120));
        petg.insert("bed_temperature_initial_layer".to_string(), json!(125));
        petg.insert("chamber_temperature_target".to_string(), json!(60));
        petg.insert("min_print_speed".to_string(), json!(20.0));

        let mut pa_cf = BTreeMap::new();
        pa_cf.insert("filament_type".to_string(), json!("PA-CF"));
        pa_cf.insert("max_volumetric_speed".to_string(), json!(10.0));
        pa_cf.insert("filament_diameter".to_string(), json!(1.75));
        pa_cf.insert("filament_density".to_string(), json!(1.18));
        pa_cf.insert("filament_flow_ratio".to_string(), json!(1.0));
        pa_cf.insert("filament_cost".to_string(), json!(50.0));
        pa_cf.insert("filament_shrink".to_string(), json!(100.0));
        pa_cf.insert("filament_color_hex".to_string(), json!("#1c1c1c"));
        pa_cf.insert("nozzle_temperature_normal".to_string(), json!(280));
        pa_cf.insert("nozzle_temperature_initial_layer".to_string(), json!(280));
        pa_cf.insert("bed_temperature_normal".to_string(), json!(100));
        pa_cf.insert("bed_temperature_initial_layer".to_string(), json!(100));
        pa_cf.insert("chamber_temperature_target".to_string(), json!(60));
        pa_cf.insert("min_print_speed".to_string(), json!(15.0));

        let config = resolved_config_with_process_and_filaments(BTreeMap::new(), vec![petg, pa_cf]);
        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        // Dump everything once so a future formatting change tells us exactly
        // which key drifted, rather than failing on the first assertion.
        let dump = |k: &str| map.get(k).map(String::as_str).unwrap_or("<missing>");
        eprintln!(
            "filament_max_volumetric_speed = {}",
            dump("filament_max_volumetric_speed")
        );
        eprintln!("filament_type                 = {}", dump("filament_type"));
        eprintln!(
            "filament_diameter             = {}",
            dump("filament_diameter")
        );
        eprintln!(
            "filament_density              = {}",
            dump("filament_density")
        );
        eprintln!(
            "filament_flow_ratio           = {}",
            dump("filament_flow_ratio")
        );
        eprintln!("filament_cost                 = {}", dump("filament_cost"));
        eprintln!(
            "filament_shrink               = {}",
            dump("filament_shrink")
        );
        eprintln!(
            "filament_colour               = {}",
            dump("filament_colour")
        );
        eprintln!(
            "nozzle_temperature            = {}",
            dump("nozzle_temperature")
        );
        eprintln!(
            "nozzle_temperature_initial_layer = {}",
            dump("nozzle_temperature_initial_layer")
        );
        eprintln!(
            "bed_temperature               = {}",
            dump("bed_temperature")
        );
        eprintln!(
            "bed_temperature_initial_layer = {}",
            dump("bed_temperature_initial_layer")
        );
        eprintln!(
            "chamber_temperature           = {}",
            dump("chamber_temperature")
        );
        eprintln!(
            "slow_down_min_speed           = {}",
            dump("slow_down_min_speed")
        );

        // The bug under fix: PETG (30) must not be overwritten by PA-CF (10).
        let vol = map.get("filament_max_volumetric_speed").map(String::as_str);
        assert!(
            vol == Some("30,10") || vol == Some("30.0,10.0"),
            "PETG slot 0 must not be overwritten by PA-CF slot 1; got {vol:?}"
        );

        // Both slots must keep their identity for every per-extruder key.
        // `coStrings` keys (`filament_type`, `filament_colour`) use libslic3r's
        // `;` separator, every other vector type `,`.
        let two_slots = |k: &str| {
            let sep = if k == "filament_type" || k == "filament_colour" {
                ';'
            } else {
                ','
            };
            map.get(k)
                .map(|v| v.split(sep).count() == 2)
                .unwrap_or(false)
        };
        for k in [
            "filament_type",
            "filament_diameter",
            "filament_density",
            "filament_flow_ratio",
            "filament_cost",
            "filament_shrink",
            "filament_colour",
            "nozzle_temperature",
            "nozzle_temperature_initial_layer",
            "chamber_temperature",
            "slow_down_min_speed",
        ] {
            assert!(two_slots(k), "expected `{k}` to keep two slot values");
        }
        // `bed_temperature*` flat overrides are dropped by libslic3r's
        // `handle_legacy` ignore set; the typed IDEX path carries them.
        // Integer-typed keys retain their integer formatting (no trailing
        // `.0`) so this snapshot doubles as a formatting guard.
        assert_eq!(
            map.get("filament_type").map(String::as_str),
            Some("PETG;PA-CF")
        );
        assert_eq!(
            map.get("filament_colour").map(String::as_str),
            Some("#018001;#1c1c1c")
        );
        assert_eq!(
            map.get("nozzle_temperature").map(String::as_str),
            Some("220,280")
        );
        // Not a per-slot vector on the flat path: libslic3r's `handle_legacy`
        // ignores `bed_temperature` outright, so the mapper keeps the plain
        // last-writer scalar and the typed IDEX path carries both slots.
        assert_eq!(map.get("bed_temperature").map(String::as_str), Some("100"));
        assert_eq!(
            map.get("chamber_temperature").map(String::as_str),
            Some("60,60")
        );
    }

    #[test]
    fn resolved_config_overrides_include_ironing_keys() {
        let mut process = BTreeMap::new();
        process.insert("ironing_type".to_string(), json!("top"));
        process.insert("ironing_flow".to_string(), json!(15.0));
        process.insert("ironing_speed".to_string(), json!(25.0));
        process.insert("ironing_spacing".to_string(), json!(0.1));
        process.insert("ironing_inset".to_string(), json!(0.25));
        let config = resolved_config_with_process(process);

        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(map.get("ironing_type").map(String::as_str), Some("top"));
        assert_eq!(map.get("ironing_flow").map(String::as_str), Some("15.0"));
        assert_eq!(map.get("ironing_speed").map(String::as_str), Some("25.0"));
        assert_eq!(map.get("ironing_spacing").map(String::as_str), Some("0.1"));
        assert_eq!(map.get("ironing_inset").map(String::as_str), Some("0.25"));
    }

    #[test]
    fn resolved_config_overrides_map_ironing_type_enum_values() {
        for (profile_value, libslic3r_value) in [
            ("none", "no ironing"),
            ("top", "top"),
            ("topmost", "topmost"),
            ("all_solid", "solid"),
        ] {
            let mut process = BTreeMap::new();
            process.insert("ironing_type".to_string(), json!(profile_value));
            let config = resolved_config_with_process(process);
            let overrides = collect_config_overrides(&config);
            let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();
            assert_eq!(
                map.get("ironing_type").map(String::as_str),
                Some(libslic3r_value),
                "profile {profile_value} → libslic3r {libslic3r_value}"
            );
        }
    }

    // -------------------------------------------------------------------
    // role_mapping → libslic3r config overrides
    //
    // Audit finding: `multi-tool-idex-role-mapping-dropped` (critical).
    // Pins the 0-based-slot → 1-based-libslic3r translation, the per-role
    // key set sourced from upstream OrcaSlicer's PrintConfig.cpp, and the
    // tolerance for `auto` / unknown roles.
    // -------------------------------------------------------------------

    fn role(role: &str, slot: u8) -> ResolvedRoleMapping {
        ResolvedRoleMapping {
            role: role.to_string(),
            extruder_slot: slot,
        }
    }

    #[test]
    fn role_mapping_support_slot_1_emits_support_filament_2() {
        let overrides = role_mapping_to_config_overrides(&[role("support", 1)]);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();
        assert_eq!(map.get("support_filament").map(String::as_str), Some("2"));
        assert_eq!(
            map.get("support_interface_filament").map(String::as_str),
            Some("2")
        );
        // Model side untouched — libslic3r default (extruder 1) handles walls/infill.
        assert!(!map.contains_key("wall_filament"));
        assert!(!map.contains_key("sparse_infill_filament"));
    }

    #[test]
    fn role_mapping_model_slot_0_emits_walls_and_infill_1() {
        let overrides = role_mapping_to_config_overrides(&[role("model", 0)]);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();
        assert_eq!(map.get("wall_filament").map(String::as_str), Some("1"));
        assert_eq!(
            map.get("sparse_infill_filament").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            map.get("solid_infill_filament").map(String::as_str),
            Some("1")
        );
        assert!(!map.contains_key("support_filament"));
    }

    #[test]
    fn role_mapping_dual_idex_model_0_support_1_emits_both_sets() {
        let overrides = role_mapping_to_config_overrides(&[role("model", 0), role("support", 1)]);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();
        // Walls/infill on slot 0 = libslic3r filament 1.
        assert_eq!(map.get("wall_filament").map(String::as_str), Some("1"));
        assert_eq!(
            map.get("sparse_infill_filament").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            map.get("solid_infill_filament").map(String::as_str),
            Some("1")
        );
        // Support on slot 1 = libslic3r filament 2.
        assert_eq!(map.get("support_filament").map(String::as_str), Some("2"));
        assert_eq!(
            map.get("support_interface_filament").map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn role_mapping_auto_emits_nothing() {
        // `auto` is libslic3r's default; UI normally filters before sending
        // but defensive tolerance avoids surprise if a vendor preset slips
        // an `auto` through.
        let overrides = role_mapping_to_config_overrides(&[role("auto", 0)]);
        assert!(overrides.is_empty(), "auto must not emit overrides");
    }

    #[test]
    fn role_mapping_unknown_role_emits_nothing_and_warns() {
        // Unknown role enums (e.g. a future role-set addition not yet
        // wired here) skip emission rather than poisoning the config —
        // a noisy warning in the daemon log is the canary.
        let overrides = role_mapping_to_config_overrides(&[role("future-role", 1)]);
        assert!(overrides.is_empty());
    }

    #[test]
    fn role_mapping_empty_input_emits_empty() {
        let overrides = role_mapping_to_config_overrides(&[]);
        assert!(overrides.is_empty());
    }

    #[test]
    fn role_mapping_last_writer_wins_on_duplicate_role() {
        // Defense in depth: if a (mis-built) ResolvedSliceConfig somehow
        // has two `support` entries pointing at different slots, the
        // second one wins via upsert_config_override. This matches the
        // existing behavior for plate-wide config_overrides.
        let overrides = role_mapping_to_config_overrides(&[role("support", 0), role("support", 1)]);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();
        assert_eq!(map.get("support_filament").map(String::as_str), Some("2"));
        assert_eq!(
            map.get("support_interface_filament").map(String::as_str),
            Some("2")
        );
    }

    // -------------------------------------------------------------------
    // libslic3r `coStrings` serialisation and per-slot joins
    //
    // `ConfigOptionStrings::deserialize` runs `unescape_strings_cstyle`,
    // which splits an *unquoted* value at every `;` — a G-code comment
    // used to truncate the macro. The mapper emits the C-style quoted form
    // for every `coStrings` key and joins per-slot strings with `;`
    // (libslic3r's separator for string vectors); every other vector type
    // keeps `,`.
    // -------------------------------------------------------------------

    #[test]
    fn escape_strings_cstyle_leaves_simple_words_alone() {
        assert_eq!(escape_strings_cstyle(&["PETG".to_string()]), "PETG");
        assert_eq!(
            escape_strings_cstyle(&["PETG".to_string(), "PA-CF".to_string()]),
            "PETG;PA-CF"
        );
    }

    #[test]
    fn escape_strings_cstyle_quotes_gcode_with_comments_newlines_and_escapes() {
        let gcode = "M117 hi ; comment\nG1 X1 \"q\" \\".to_string();
        assert_eq!(
            escape_strings_cstyle(&[gcode]),
            "\"M117 hi ; comment\\nG1 X1 \\\"q\\\" \\\\\""
        );
    }

    #[test]
    fn escape_strings_cstyle_quotes_a_bare_semicolon_word() {
        // libslic3r's own escaper leaves `a;b` unquoted and its parser would
        // then split it in two; quoting on `;` keeps the value verbatim.
        assert_eq!(escape_strings_cstyle(&["a;b".to_string()]), "\"a;b\"");
    }

    #[test]
    fn escape_strings_cstyle_quotes_only_a_lone_empty_string() {
        assert_eq!(escape_strings_cstyle(&[String::new()]), "\"\"");
        assert_eq!(escape_strings_cstyle(&[String::new(), String::new()]), ";");
    }

    #[test]
    fn filament_gcode_override_is_quoted_so_semicolons_survive() {
        let mut petg = BTreeMap::new();
        petg.insert(
            "filament_start_gcode".to_string(),
            json!("; start PETG ; keep\nM117 PETG"),
        );
        petg.insert("filament_end_gcode".to_string(), json!("; end ; tail"));
        let config = resolved_config_with_process_and_filaments(BTreeMap::new(), vec![petg]);

        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(
            map.get("filament_start_gcode").map(String::as_str),
            Some("\"; start PETG ; keep\\nM117 PETG\"")
        );
        assert_eq!(
            map.get("filament_end_gcode").map(String::as_str),
            Some("\"; end ; tail\"")
        );
    }

    #[test]
    fn multi_filament_strings_join_per_slot_with_semicolons() {
        let mut petg = BTreeMap::new();
        petg.insert("filament_type".to_string(), json!("PETG"));
        petg.insert("filament_start_gcode".to_string(), json!("; PETG ; a"));
        petg.insert("filament_notes".to_string(), json!("dry 80C; 4h"));
        let mut pa = BTreeMap::new();
        pa.insert("filament_type".to_string(), json!("PA-CF"));
        pa.insert("filament_start_gcode".to_string(), json!("; PA ; b"));
        pa.insert("filament_notes".to_string(), json!("plain"));
        let mut config =
            resolved_config_with_process_and_filaments(BTreeMap::new(), vec![petg, pa]);
        let mut slot1 = config.extruders[0].clone();
        slot1.slot_id = 1;
        config.extruders.push(slot1);

        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(
            map.get("filament_type").map(String::as_str),
            Some("PETG;PA-CF"),
            "coStrings slots are `;`-separated, not `,`"
        );
        assert_eq!(
            map.get("filament_start_gcode").map(String::as_str),
            Some("\"; PETG ; a\";\"; PA ; b\"")
        );
        assert_eq!(
            map.get("filament_notes").map(String::as_str),
            Some("\"dry 80C; 4h\";plain")
        );
    }

    #[test]
    fn process_costrings_key_is_escaped_as_a_single_element_vector() {
        let mut process = BTreeMap::new();
        process.insert(
            "post_process".to_string(),
            json!("/usr/local/bin/a.sh\n/opt/b.py"),
        );
        process.insert(
            "small_area_infill_flow_compensation_model".to_string(),
            json!("0,0\n0.2,0.4"),
        );
        let config = resolved_config_with_process(process);

        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(
            map.get("post_process").map(String::as_str),
            Some("\"/usr/local/bin/a.sh\\n/opt/b.py\"")
        );
        assert_eq!(
            map.get("small_area_infill_flow_compensation_model")
                .map(String::as_str),
            Some("\"0,0\\n0.2,0.4\"")
        );
    }

    /// An array is one element per entry. The agent sends the small-area flow
    /// model this way since 2026-09-25: `SmallAreaInfillFlowCompensator` reads
    /// one "length,factor" point per element, so the single string above
    /// reaches it as one point, (0, 0), and the whole model is rejected.
    #[test]
    fn process_costrings_array_is_escaped_element_by_element() {
        let mut process = BTreeMap::new();
        process.insert(
            "small_area_infill_flow_compensation_model".to_string(),
            json!(["0,0", "0.2,0.4444", "10,1"]),
        );
        let config = resolved_config_with_process(process);

        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(
            map.get("small_area_infill_flow_compensation_model")
                .map(String::as_str),
            Some("0,0;0.2,0.4444;10,1")
        );
    }

    #[test]
    fn scalar_string_keys_are_not_escaped() {
        // `machine_start_gcode` is a `coString` (scalar): libslic3r stores it
        // verbatim, so quoting it would inject literal quotes into the print.
        let mut config = resolved_config_with_process(BTreeMap::new());
        config.machine.insert(
            "machine_start_gcode".to_string(),
            json!("G28 ; home\nM104 S200"),
        );

        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(
            map.get("machine_start_gcode").map(String::as_str),
            Some("G28 ; home\nM104 S200")
        );
    }

    fn two_slot_config(
        slot0: BTreeMap<String, Value>,
        slot1: BTreeMap<String, Value>,
    ) -> ResolvedSliceConfig {
        let mut config =
            resolved_config_with_process_and_filaments(BTreeMap::new(), vec![slot0, slot1]);
        let mut second = config.extruders[0].clone();
        second.slot_id = 1;
        config.extruders.push(second);
        config
    }

    #[test]
    fn wave1_filament_keys_join_per_slot_instead_of_last_slot_wins() {
        let mut slot0 = BTreeMap::new();
        let mut slot1 = BTreeMap::new();
        for (key, a, b) in [
            ("ironing_fan_speed", json!(20), json!(70)),
            ("reduce_fan_stop_start_freq", json!(true), json!(false)),
            ("adaptive_pressure_advance", json!(true), json!(false)),
            (
                "adaptive_pressure_advance_overhangs",
                json!(true),
                json!(false),
            ),
            (
                "adaptive_pressure_advance_bridges",
                json!(0.02),
                json!(0.03),
            ),
            ("activate_chamber_temp_control", json!(true), json!(false)),
            ("support_material_interface_fan_speed", json!(-1), json!(75)),
            (
                "adaptive_pressure_advance_model",
                json!("0,0,0\n0,0,0"),
                json!("1,1,1"),
            ),
        ] {
            slot0.insert(key.to_string(), a);
            slot1.insert(key.to_string(), b);
        }
        let config = two_slot_config(slot0, slot1);

        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        for (key, expected) in [
            ("ironing_fan_speed", "20,70"),
            ("reduce_fan_stop_start_freq", "1,0"),
            ("adaptive_pressure_advance", "1,0"),
            ("adaptive_pressure_advance_overhangs", "1,0"),
            ("adaptive_pressure_advance_bridges", "0.02,0.03"),
            ("activate_chamber_temp_control", "1,0"),
            ("support_material_interface_fan_speed", "-1,75"),
            ("adaptive_pressure_advance_model", "\"0,0,0\\n0,0,0\";1,1,1"),
        ] {
            assert_eq!(map.get(key).map(String::as_str), Some(expected), "{key}");
        }
    }

    #[test]
    fn audit_found_filament_vector_keys_join_per_slot() {
        // Keys the 2026-09-15 audit found missing from the per-slot table;
        // every one is a `co*s` vector in PrintConfig.cpp.
        let mut slot0 = BTreeMap::new();
        let mut slot1 = BTreeMap::new();
        for (key, a, b) in [
            ("filament_vendor", json!("Protype Lab"), json!("eSUN")),
            ("slow_down_for_layer_cooling", json!(true), json!(false)),
            ("filament_shrinkage_compensation_z", json!(100), json!(99.5)),
            (
                "min_layer_time_strategy",
                json!("slowdown"),
                json!("park_and_wait"),
            ),
            ("park_and_wait_retract_length", json!(2), json!(3)),
            ("park_and_wait_z_hop", json!(0.5), json!(1)),
        ] {
            slot0.insert(key.to_string(), a);
            slot1.insert(key.to_string(), b);
        }
        let config = two_slot_config(slot0, slot1);

        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        for (key, expected) in [
            ("filament_vendor", "\"Protype Lab\";eSUN"),
            ("slow_down_for_layer_cooling", "1,0"),
            ("filament_shrinkage_compensation_z", "100,99.5"),
            ("min_layer_time_strategy", "slowdown,park_and_wait"),
            ("park_and_wait_retract_length", "2,3"),
            ("park_and_wait_z_hop", "0.5,1"),
        ] {
            assert_eq!(map.get(key).map(String::as_str), Some(expected), "{key}");
        }
    }

    #[test]
    fn resolved_extruder_retract_lift_enforce_reaches_idex_inputs() {
        let mut config = resolved_config_with_process(BTreeMap::new());
        config.extruders[0].retract_lift_enforce = Some("Top Only".to_string());

        let idex = idex_from_resolved_config(&config).expect("valid resolved config");

        assert_eq!(idex.retract_lift_enforce, vec!["Top Only".to_string()]);
    }

    #[test]
    fn bed_mesh_points_render_as_comma_pairs_for_libslic3r() {
        // `ConfigOptionPoint::deserialize` accepts "x,y"; the agent sends the
        // three coPoint keys as [x, y] pairs and the array join yields it.
        let mut config = resolved_config_with_process(BTreeMap::new());
        config
            .machine
            .insert("bed_mesh_min".to_string(), json!([12.5, 13.5]));
        config
            .machine
            .insert("bed_mesh_max".to_string(), json!([250.0, 251.0]));
        config
            .machine
            .insert("bed_mesh_probe_distance".to_string(), json!([25, 47.5]));
        config
            .machine
            .insert("adaptive_bed_mesh_margin".to_string(), json!(1000));

        let overrides = collect_config_overrides(&config);
        let map: HashMap<_, _> = overrides.into_iter().map(|o| (o.key, o.value)).collect();

        assert_eq!(
            map.get("bed_mesh_min").map(String::as_str),
            Some("12.5,13.5")
        );
        assert_eq!(
            map.get("bed_mesh_max").map(String::as_str),
            Some("250.0,251.0")
        );
        assert_eq!(
            map.get("bed_mesh_probe_distance").map(String::as_str),
            Some("25,47.5")
        );
        assert_eq!(
            map.get("adaptive_bed_mesh_margin").map(String::as_str),
            Some("1000")
        );
    }
}
