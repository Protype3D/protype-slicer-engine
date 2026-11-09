//! `slice.estimate` — what a part will cost, from a real slice.
//!
//! §4. Deliberately the same code path a real print takes: the numbers
//! are read out of G-code the slicer actually produced, never computed
//! from volume. On the part this came from, the arithmetic guess said
//! 141 g and the slicer said 58.5 g.
//!
//! An `object_id` is sliced with what a print gives the slicer for it
//! (`SlicedObjects`), but stood on the bed wherever its transform left it:
//! where a part stands does not change what it costs, and a part left off
//! the bed must still be costed.
//!
//! Built without `with-libslic3r` the bridge emits rectangles rather than
//! failing, so this arm refuses to report an estimate taken from output
//! that lacks the marks of real slicing. See
//! `slicer_pipeline::estimate::EstimateError::NotRealSlicing`.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};
use slicer_pipeline::{estimate, PlateObject};
use slicer_types::ObjectId;

use crate::error::RpcError;
use crate::mesh::{MeshRef, Referenced};

/// The project objects a call names, each resolved to what a slice of it
/// hands the slicer — the cut its feature ops make, its transform, paint,
/// layer ranges and overrides (`object_slice_inputs` in dispatch.rs) —
/// before the call goes to a blocking thread: finding the cut may make it
/// through the FFI.
pub type SlicedObjects = HashMap<ObjectId, slicer_ffi::ObjectInputs>;

#[derive(Debug, Deserialize)]
pub struct EstimateParams {
    #[serde(flatten)]
    pub mesh: MeshRef,
    /// Raw libslic3r keys, the same channel the headless CLI's
    /// `--overrides` uses: a profile goes here as libslic3r option keys
    /// only. A key libslic3r would ignore — misspelled, obsolete, a value in
    /// a legacy form, or a flattened Orca preset's metadata (`name`, `from`,
    /// `setting_id`, …) — refuses the estimate by name rather than pricing
    /// the part without it.
    #[serde(default)]
    pub config_overrides: std::collections::BTreeMap<String, String>,
}

pub fn handle_estimate(objects: &SlicedObjects, params: Value) -> Result<Value, RpcError> {
    let p: EstimateParams = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("invalid params: {e}")))?;
    let object = plate_object(objects, &p.mesh)?;
    let source = object.input_path().to_string();

    let output = std::env::temp_dir().join(format!(
        "slice-estimate-{}-{}.gcode",
        std::process::id(),
        ulid::Ulid::new()
    ));
    let result = estimate::estimate(slicer_pipeline::SliceRequest {
        objects: vec![object],
        output_path: output.to_string_lossy().into_owned(),
        printer_preset_path: String::new(),
        process_preset_path: String::new(),
        filament_preset_path: String::new(),
        idex: None,
        config_overrides: p
            .config_overrides
            .into_iter()
            .map(|(key, value)| slicer_ffi::ConfigOverride { key, value })
            .collect(),
        context: slicer_ffi::SliceContext::default(),
    });
    // The G-code was a means to the numbers; it is not the caller's.
    let _ = std::fs::remove_file(&output);

    let estimate = result.map_err(|e| RpcError::Slicing(e.to_string()))?;
    Ok(json!({
        "grams": estimate.grams,
        "seconds": estimate.seconds,
        "hours": estimate.hours(),
        "layers": estimate.layers,
        "filament_mm": estimate.filament_mm,
        "filament_cm3": estimate.filament_cm3,
        "printing_time": estimate.printing_time,
        // Stated, so nothing downstream has to wonder whether this came
        // from a real slice or from volume arithmetic.
        "source": "real_slice",
        "measured_from": source,
    }))
}

/// What the estimate slices for a mesh reference. A project object is
/// sliced as a print slices it (`SlicedObjects`); a file that is not one —
/// the reference part being costed usually is not — as its file has it.
fn plate_object(objects: &SlicedObjects, reference: &MeshRef) -> Result<PlateObject, RpcError> {
    let object = match crate::mesh::referenced(objects, reference)? {
        Referenced::Object(inputs) => PlateObject::described(inputs.clone()),
        Referenced::File(path) => PlateObject::new(path),
    };
    if object.input_path().is_empty() {
        return Err(RpcError::Project(
            "that object has no mesh on disk to slice".to_string(),
        ));
    }
    Ok(object)
}
