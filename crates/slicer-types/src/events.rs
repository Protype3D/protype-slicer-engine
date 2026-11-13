use crate::adaptive::AdaptiveRule;
use crate::part::{ObjectGeometry, PartId, PartSnapshot};
use crate::presets::Preset;
use crate::project::*;
use crate::settings::{SettingKey, SettingScope, SettingValue};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub struct EventId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum ProjectEvent {
    ProjectCreated {
        meta: ProjectMeta,
    },
    ProjectOpened {
        project_id: ProjectId,
        meta: ProjectMeta,
    },
    PresetActivated {
        kind: PresetKind,
        preset_id: PresetId,
    },
    ObjectAdded {
        object_id: ObjectId,
        name: String,
        /// Absolute filesystem path to the source mesh on the daemon-visible
        /// disk. Required for the FFI bridge to read the actual mesh — the
        /// hash + bounds metadata are not enough on their own.
        #[serde(default)]
        source_path: String,
        triangle_count: u64,
        bounds: Bounds,
        source_hash: String,
        /// Present when this object was synthesized from one part of a
        /// multi-part STEP/STP assembly. See `StepPartPayload`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step_part: Option<StepPartPayload>,
        /// Present when this object was synthesized by `object.split_to_parts`
        /// from a parent object's disconnected shells. See `SplitPartPayload`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        split_part: Option<SplitPartPayload>,
        /// Initial transform for this object. `None` keeps the historical
        /// behaviour of placing the object at the identity matrix. Set by
        /// `object.split_to_parts` so split children inherit the source's
        /// world position.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transform: Option<[f32; 16]>,
    },
    /// Opaque, bounded generator metadata used to reopen the parametric editor.
    ObjectRecipeSet {
        object_id: ObjectId,
        recipe: Option<serde_json::Value>,
    },
    ObjectRemoved {
        object_id: ObjectId,
    },
    SliceRequested {
        job_id: String,
    },
    SliceProgress {
        job_id: String,
        stage: String,
        percent: f32,
        layer: Option<u32>,
        total_layers: Option<u32>,
    },
    SliceCompleted {
        job_id: String,
        gcode_path: String,
        /// Layers the G-code prints: objects printed together share theirs,
        /// so it is the tallest stack on the plate, not a sum over objects.
        total_layers: u32,
        total_segments: u64,
        /// Filename libslic3r suggests for this slice's G-code, based
        /// on the active preset's `filename_format` template (default
        /// Orca: `{input_filename_base}_{filament_type[initial_tool]}_{print_time}.gcode`).
        /// Used by the UI as the default name in the Save As dialog.
        /// Non-empty when the libslic3r path runs; falls back to
        /// `<input_basename>.gcode` under the stub. Older daemon
        /// builds didn't emit this — `#[serde(default)]` keeps the
        /// event-log format backward-compatible.
        #[serde(default)]
        suggested_filename: String,
        /// Non-fatal warnings about this slice, in English; one entry per
        /// message, and a message may span several lines. The G-code is
        /// still produced; a warning says it may not be what the user asked
        /// for. Most are libslic3r's own, raised while validating — e.g.
        /// "Filament shrinkage will not be used because filament shrinkage
        /// for the used filaments differs significantly.", after which the
        /// part prints unscaled. The daemon adds one naming the profile
        /// settings libslic3r does not recognise and ignored ("libslic3r
        /// does not recognise these settings and ignored them: …"), and one
        /// naming the layer-range values it did not take — ranges a project
        /// brought in unchecked ("libslic3r ignored these layer-range
        /// settings: …"). Empty under the stub and from older daemon builds
        /// (`#[serde(default)]`).
        #[serde(default)]
        warnings: Vec<String>,
    },
    SliceFailed {
        job_id: String,
        error: String,
    },
    SliceCanceled {
        job_id: String,
    },
    SettingChanged {
        scope: SettingScope,
        key: SettingKey,
        old_value: Option<SettingValue>,
        new_value: SettingValue,
    },
    PresetSaved {
        preset: Preset,
    },
    PresetDeleted {
        preset_id: PresetId,
    },
    /// Set or clear a per-object override for one process-scope setting key.
    /// `new_value: None` clears the override and reverts to the project value.
    ObjectOverrideSet {
        object_id: ObjectId,
        key: SettingKey,
        new_value: Option<SettingValue>,
    },
    /// Set or clear the per-triangle seam-paint blob and/or the target point
    /// for an object. `facets_rle` is base64-encoded Orca-compatible RLE.
    /// `target_point` is JSON `{x,y,z,facet_idx}`. `None` clears the
    /// respective field; the painter writes both together at the end of
    /// each stroke or pick.
    ObjectSeamFacetsSet {
        object_id: ObjectId,
        facets_rle: Option<String>,
        target_point: Option<String>,
    },
    /// Set or clear the per-triangle support-paint blob for an object.
    /// `rle` is base64-encoded Orca-compatible RLE (same encoding as the
    /// seam variant — values 0/1/2 map to none/enforcer/blocker). `None`
    /// clears the field. The C++ bridge feeds this into
    /// `ModelVolume::supported_facets` so libslic3r treats it as paint-on
    /// support hints alongside the auto-generated supports.
    ObjectSupportFacetsSet {
        object_id: ObjectId,
        facets_rle: Option<String>,
    },
    /// Set or clear the per-triangle fan-paint blob for an object: base64 of
    /// `[value: u8][run: u32 LE]` runs, one value per facet — 0 unpainted,
    /// else `1 + (lines - 1) * 21 + speed_pct / 5` (see
    /// `MAX_FAN_FACET_VALUE`). `None` clears the field. The C++ bridge maps it
    /// onto the loaded facets for libslic3r's `PrintObject::set_fan_paint_facets`.
    ObjectFanFacetsSet {
        object_id: ObjectId,
        facets_rle: Option<String>,
    },
    ObjectFilamentFacetsSet {
        object_id: ObjectId,
        facets_rle: Option<String>,
    },
    /// Replace an object's variable-layer-height profile. `profile` is a
    /// flat `[z, height, ...]` list (mm). Empty clears it back to uniform.
    ObjectLayerHeightProfileSet {
        object_id: ObjectId,
        profile: Vec<f32>,
    },
    /// Replace the full per-Z setting-overrides list for an object.
    /// Reaches libslic3r as `ModelObject::layer_config_ranges`.
    ObjectLayerRangesSet {
        object_id: ObjectId,
        ranges: Vec<LayerConfigRange>,
    },
    /// Replace an object's parametric feature-operation stack (Phase 2 CAD
    /// edit mode — currently cylindrical holes only, more `kind`s later).
    /// Idempotent: the UI sends the full desired list every time. Emitted
    /// by `object.set_operations` in slicer-rpc AFTER that RPC has
    /// materialized the stack via `apply_operations` (mcut boolean) into
    /// a content-addressed cache STL — this event only persists the
    /// list, it never triggers FFI itself.
    ObjectOperationsSet {
        object_id: ObjectId,
        operations: Vec<FeatureOp>,
    },
    /// Apply a new 4x4 row-major transform matrix to an object. Old matrix
    /// is included so undo/redo can be implemented on the daemon later.
    ObjectTransformed {
        object_id: ObjectId,
        new_transform: [f32; 16],
        old_transform: [f32; 16],
    },
    /// Whole project state was replaced (project.open, autosave recovery).
    /// Subscribers should re-fetch derived state. Carries only meta so the
    /// payload stays small; the rest is queried via setting.list etc.
    ProjectReplaced {
        meta: Option<ProjectMeta>,
        /// File the new state was opened from, the one `project.save` writes
        /// back to. `None` for a recovered session, which has no file until it
        /// is saved. Serialized as `null` rather than omitted, so a client can
        /// tell it apart from a daemon that does not report the file.
        #[serde(default)]
        path: Option<String>,
    },
    /// Adaptive Print rule was added or updated (upsert by id).
    AdaptiveRuleSet {
        rule: AdaptiveRule,
    },
    /// Adaptive Print rule was removed by id.
    AdaptiveRuleRemoved {
        rule_id: String,
    },
    /// A part (CAD document) was created or changed: the full snapshot, so a
    /// subscriber never merges deltas and undo can put the previous one back
    /// whole.
    PartSet {
        part: PartSnapshot,
    },
    /// A part was removed. Its objects are removed by their own events.
    PartRemoved {
        part_id: PartId,
    },
    /// Replace an object's geometry in place — its part regenerated, or the
    /// object became a part's body. The object keeps its id, transform,
    /// settings and paint; `geometry.operations` replaces its op stack
    /// (empty for a body: its holes live in the part's document).
    ///
    /// `transform`, when present, is set on the object in the same event, so
    /// a mesh and the placement that goes with it never show apart (history
    /// replays both directions as one event). Absent: the transform stays.
    ObjectGeometryUpdated {
        object_id: ObjectId,
        geometry: ObjectGeometry,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transform: Option<[f32; 16]>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Bounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

/// One parametric operation in an object's feature stack (Phase 2 CAD edit
/// mode). `kind` is a plain string rather than a closed enum so unknown
/// future kinds round-trip losslessly on the wire; `apply_operations` (the
/// FFI boolean pass) currently only acts on `kind == "hole"` and silently
/// ignores anything else (forward-compat, not an error).
///
/// `origin`/`axis` are in the object's recentered local frame (XY-bbox
/// center -> 0, z-min -> 0) — the same frame Phase-1 B-rep measurement /
/// hole-detection tooling (`object.query_faces`) and `apply_operations`
/// both use, so op coordinates and the materialized mesh always agree
/// regardless of the object's `transform`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct FeatureOp {
    pub id: String,
    pub kind: String,
    pub suppressed: bool,
    pub origin: [f32; 3],
    pub axis: [f32; 3],
    pub radius_mm: f32,
    pub depth_mm: f32,
    pub through: bool,
    /// Fastener head recess on the hole's entry end, opening at `origin`,
    /// which has to lie on the entry surface. A head that cannot be cut skips
    /// the op; it never leaves a plain hole behind.
    #[serde(default, skip_serializing_if = "HeadKind::is_none")]
    pub head_kind: HeadKind,
    /// Width of the recess: the counterbore's bore, or the countersink's
    /// diameter at the surface.
    #[serde(default, skip_serializing_if = "is_unset")]
    pub head_dia_mm: f32,
    /// Depth of a counterbore. A countersink's follows from its diameter and
    /// angle.
    #[serde(default, skip_serializing_if = "is_unset")]
    pub head_depth_mm: f32,
    /// Included angle of a countersink, 90 for DIN 7991.
    #[serde(default, skip_serializing_if = "is_unset")]
    pub head_angle_deg: f32,
}

/// The recess cut for a fastener's head. Left out of a plain hole's encoding,
/// with the sizes that go with it: `ops_hash` keys the cuts of the ops cache
/// on that encoding, and a stack of plain holes keeps its key.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HeadKind {
    #[default]
    None,
    /// A wider cylindrical bore, for a socket-head cap screw (DIN 912).
    Counterbore,
    /// A cone, for a flat-head screw (DIN 7991).
    Countersink,
}

impl HeadKind {
    fn is_none(&self) -> bool {
        *self == Self::None
    }
}

fn is_unset(value: &f32) -> bool {
    *value == 0.0
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EventEnvelope {
    pub event_id: EventId,
    pub event: ProjectEvent,
    pub timestamp_ms: i64,
}

/// Metadata identifying which part of a multi-part STEP assembly an
/// `ObjectAdded` event represents. Carried in the `step_part` field of
/// the `ObjectAdded` variant; only set for STEP/STP imports.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct StepPartPayload {
    /// Name libslic3r resolved for this `ModelObject`. May be empty
    /// when libslic3r couldn't infer a name; the dispatcher falls back
    /// to a synthesized name in that case.
    pub part_name: String,
    /// Position in `Slic3r::Model::objects` for the originating
    /// `inspect_model` call. Stable within one import, not cross-call.
    pub part_index: u32,
}

/// Metadata identifying which part a split-derived `ObjectAdded` event
/// represents. Set by the `object.split_to_parts` RPC; `None` for STL/3MF
/// imports, STEP fan-out (uses `step_part` instead), and any other path.
/// Carries the (now-removed) parent's object id so the UI can copy
/// per-object overrides onto the child.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SplitPartPayload {
    pub source_object_id: ObjectId,
    pub part_index: u32,
    pub part_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_fan_facets_set_serializes_like_object_support_facets_set() {
        let fan = ProjectEvent::ObjectFanFacetsSet {
            object_id: ObjectId("o-1".into()),
            facets_rle: Some("kwEAAAA=".into()),
        };
        assert_eq!(
            serde_json::to_value(&fan).unwrap(),
            serde_json::json!({ "type": "ObjectFanFacetsSet", "object_id": "o-1", "facets_rle": "kwEAAAA=" })
        );
        let support = ProjectEvent::ObjectSupportFacetsSet {
            object_id: ObjectId("o-1".into()),
            facets_rle: Some("kwEAAAA=".into()),
        };
        let mut support_json = serde_json::to_value(&support).unwrap();
        support_json["type"] = serde_json::json!("ObjectFanFacetsSet");
        assert_eq!(serde_json::to_value(&fan).unwrap(), support_json);

        let cleared = ProjectEvent::ObjectFanFacetsSet {
            object_id: ObjectId("o-1".into()),
            facets_rle: None,
        };
        let back: ProjectEvent =
            serde_json::from_value(serde_json::to_value(&cleared).unwrap()).unwrap();
        assert!(matches!(
            back,
            ProjectEvent::ObjectFanFacetsSet {
                facets_rle: None,
                ..
            }
        ));
    }

    #[test]
    fn slice_completed_from_a_daemon_without_warnings_reads_as_no_warnings() {
        let logged = serde_json::json!({
            "type": "SliceCompleted",
            "job_id": "job-1",
            "gcode_path": "/tmp/job-1.gcode",
            "total_layers": 5,
            "total_segments": 50,
            "suggested_filename": "cube.gcode"
        });
        let event: ProjectEvent = serde_json::from_value(logged).unwrap();
        assert!(matches!(
            event,
            ProjectEvent::SliceCompleted { ref warnings, .. } if warnings.is_empty()
        ));
    }
}
