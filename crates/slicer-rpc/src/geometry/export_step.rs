//! Export exact solids without changing project state. All parts are validated
//! before the first cache lookup, build, or write.
use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};
use slicer_geometry::CsgNode;

use crate::error::RpcError;

#[derive(Deserialize)]
struct Export {
    path: String,
    parts: Vec<Part>,
}

#[derive(Deserialize)]
struct Part {
    name: String,
    tree: Option<CsgNode>,
    step_path: Option<String>,
    color: Option<[f64; 3]>,
    scale: Option<[f64; 3]>,
}

fn step_path(path: &str) -> bool {
    let p = Path::new(path);
    !path.contains('\0')
        && p.is_absolute()
        && p.extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("step") || s.eq_ignore_ascii_case("stp"))
}

fn error(message: impl ToString, data: Value) -> RpcError {
    RpcError::ExportStep {
        message: message.to_string(),
        data,
    }
}

fn parse(params: Value) -> Result<Export, RpcError> {
    let export: Export =
        serde_json::from_value(params).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
    if !step_path(&export.path) || !(1..=64).contains(&export.parts.len()) {
        return Err(RpcError::InvalidParams(
            "expected an absolute STEP output path and 1..=64 parts".into(),
        ));
    }
    for part in &export.parts {
        if !(1..=200).contains(&part.name.chars().count()) || part.name.contains('\0') {
            return Err(RpcError::InvalidParams(
                "part name must contain 1..=200 characters and no NUL".into(),
            ));
        }
        if part
            .color
            .is_some_and(|c| c.iter().any(|v| !(0.0..=1.0).contains(v)))
        {
            return Err(RpcError::InvalidParams(
                "color components must be in [0, 1]".into(),
            ));
        }
        if part
            .scale
            .is_some_and(|s| s.iter().any(|v| !(*v > 0.0 && *v <= 100.0)))
        {
            return Err(RpcError::InvalidParams(
                "scale components must be in (0, 100]".into(),
            ));
        }
        match (&part.tree, &part.step_path) {
            (Some(tree), None) => {
                tree.validate()
                    .map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                if let Some(op) = tree.mesh_only_op() {
                    return Err(error(
                        format!("{} requires the mesh kernel for {op}", part.name),
                        json!({"kind":"mesh_only","part":part.name,"op":op}),
                    ));
                }
            }
            (None, Some(path)) if step_path(path) => {}
            _ => {
                return Err(RpcError::InvalidParams(
                    "each part needs exactly one tree or absolute STEP input path".into(),
                ))
            }
        }
    }
    Ok(export)
}

pub(crate) fn handle(params: Value) -> Result<Value, RpcError> {
    let export = parse(params)?;
    #[cfg(feature = "with-libslic3r")]
    {
        write(export)
    }
    #[cfg(not(feature = "with-libslic3r"))]
    {
        Err(build_error(
            &export.parts[0].name,
            "this daemon was built without OCCT",
        ))
    }
}

fn build_error(name: &str, reason: impl ToString) -> RpcError {
    let reason = reason.to_string();
    error(
        format!("{name}: {reason}"),
        json!({"kind":"build_failed","part":name,"reason":reason}),
    )
}

// The reference is measured on the original in-memory solid. Reading the
// intermediate STEP for this number would hide serialization defects (#1773).
#[cfg(feature = "with-libslic3r")]
pub(super) fn cache_volume(path: &Path, volume: f64) {
    use std::io::Write;
    let store = || -> std::io::Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        write!(temp, "{volume}")?;
        temp.persist(path.with_extension("volume"))?;
        Ok(())
    };
    if let Err(e) = store() {
        tracing::warn!(?e, ?path, "could not cache B-rep volume");
    }
}

#[cfg(feature = "with-libslic3r")]
fn tree_source(tree: &CsgNode) -> Result<(String, f64), String> {
    let root = slicer_state::object_cache_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let key = super::tree_key(tree).map_err(|e| e.to_string())?;
    let path = super::cache_path(&root, &key.hash, "step");
    let cached = path.is_file();
    let source = path.to_str().ok_or("cache path is not UTF-8")?.to_owned();
    if cached {
        let volume = std::fs::read_to_string(path.with_extension("volume"))
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0);
        if let Some(volume) = volume {
            return Ok((source, volume));
        }
    }
    // Old caches have only the STEP. Keep its geometry and recover the
    // reference once using the same isolated builder as geometry.build.
    let temp = tempfile::Builder::new()
        .suffix(".step")
        .tempfile_in(&root)
        .map_err(|e| e.to_string())?
        .into_temp_path();
    let volume = crate::brep_worker::build_step(tree, &temp).map_err(|e| e.to_string())?;
    if !cached {
        temp.persist(&path).map_err(|e| e.to_string())?;
    }
    cache_volume(&path, volume);
    Ok((source, volume))
}

#[cfg(feature = "with-libslic3r")]
impl From<slicer_ffi::StepExportError> for RpcError {
    fn from(failure: slicer_ffi::StepExportError) -> Self {
        let mut data = json!({"kind":failure.kind});
        if failure.kind != "read_failed" {
            data["reason"] = json!(failure.reason);
        }
        if failure.has_volume {
            data["expected"] = json!(failure.expected);
            data["actual"] = json!(failure.actual);
        }
        let message = if failure.part.is_empty() {
            failure.reason
        } else {
            data["part"] = json!(failure.part);
            format!("{}: {}", failure.part, failure.reason)
        };
        error(message, data)
    }
}

#[cfg(feature = "with-libslic3r")]
fn write(export: Export) -> Result<Value, RpcError> {
    use slicer_ffi::StepExportPart;
    let write_error = |why: String| error(&why, json!({"kind":"write_failed","reason":why}));
    let destination = Path::new(&export.path);
    let mut staging = tempfile::Builder::new();
    staging.prefix(".step-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staging.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let staging = staging
        .tempdir_in(destination.parent().unwrap())
        .map_err(|e| write_error(e.to_string()))?;
    let partial = staging.path().join("verified.step");

    let mut inputs = Vec::with_capacity(export.parts.len());
    for part in export.parts {
        let (path, source_volume) = match part.tree {
            Some(tree) => tree_source(&tree).map_err(|e| build_error(&part.name, e))?,
            None => (part.step_path.unwrap(), 0.0),
        };
        inputs.push(StepExportPart {
            name: part.name,
            path,
            source_volume,
            color: part.color.unwrap_or_default(),
            has_color: part.color.is_some(),
            scale: part.scale.unwrap_or([1.0; 3]),
        });
    }
    let result = slicer_ffi::export_step(&inputs, partial.to_str().unwrap());
    if !result.error.kind.is_empty() {
        return Err(result.error.into());
    }
    #[cfg(unix)]
    match destination.metadata() {
        Ok(previous) => std::fs::set_permissions(&partial, previous.permissions())
            .map_err(|e| write_error(e.to_string()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(write_error(e.to_string())),
    }
    std::fs::rename(partial, destination).map_err(|e| write_error(e.to_string()))?;
    let parts: Vec<_> = result
        .stats
        .into_iter()
        .map(|stats| {
            json!({
                "name": stats.name,
                "volume_mm3": stats.volume_mm3,
                "faces": stats.faces,
                "solids": stats.solids
            })
        })
        .collect();
    Ok(json!({"path":export.path,"schema":"AP242DIS","parts":parts}))
}
