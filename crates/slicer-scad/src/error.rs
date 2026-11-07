use std::collections::BTreeMap;

use thiserror::Error;

/// Why a parametric build did not produce a mesh.
///
/// A failed assert is the interesting variant and the reason this is an
/// enum rather than an exit code. `assert(SOCK_OD + 2*RIM <= CUP_BASE)`
/// firing is not "the build failed" — it is "the socket is 0.33 mm too
/// wide for the base", and the caller can only act on the second if the
/// expression, the rendered message and the variable values survive to
/// it. Handing back a return code and a log to grep loses all three.
#[derive(Debug, Error)]
pub enum ScadError {
    #[error(
        "no `openscad` binary found (tried {tried}); set the OPENSCAD environment variable to \
         its path"
    )]
    NotFound { tried: String },

    #[error("cannot run openscad: {source}")]
    Spawn {
        #[source]
        source: std::io::Error,
    },

    #[error("openscad did not finish within {seconds}s and was killed")]
    Timeout { seconds: u64 },

    #[error(
        "assertion failed at {file}:{line}: {message}\n  expression: {expression}\n  values: \
         {}", format_values(.values)
    )]
    Assertion {
        /// The condition as OpenSCAD re-printed it, e.g.
        /// `((SOCK_OD + (2 * RIM)) <= CUP_BASE)`.
        expression: String,
        /// The `str(...)` message the assert carried.
        message: String,
        file: String,
        line: u32,
        /// Everything `echo()` had emitted before the assert fired.
        /// These are the variable values — OpenSCAD does not put them in
        /// the error itself, and reconstructing them by hand from the
        /// source is exactly the guesswork this crate exists to remove.
        values: BTreeMap<String, serde_json::Value>,
        /// `TRACE:` lines, which name the call chain for an assert
        /// inside a module.
        trace: Vec<String>,
    },

    #[error("{file}:{line}: {detail}")]
    Parse {
        file: String,
        line: u32,
        detail: String,
    },

    #[error(
        "the model produced no geometry — the top-level object is empty, which usually means \
         everything was subtracted away"
    )]
    EmptyResult,

    #[error(
        "the model produced a 2D shape, which has no mesh to write — wrap it in \
         linear_extrude() or rotate_extrude()"
    )]
    NotThreeDimensional,

    #[error("openscad exited with {}: {stderr_tail}", describe_exit(.exit_code))]
    Failed {
        exit_code: Option<i32>,
        stderr_tail: String,
    },

    #[error("openscad reported success but wrote no file to {path}")]
    NoOutput { path: String },

    #[error(
        "the build produced a degenerate mesh: {triangles} triangles enclosing {volume_mm3} mm3 — \
         writing this out would slice to no toolpaths at all"
    )]
    Degenerate { triangles: usize, volume_mm3: f64 },

    #[error("the mesh openscad wrote could not be read back: {0}")]
    Mesh(#[from] slicer_geometry::MeshError),

    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
}

fn format_values(values: &BTreeMap<String, serde_json::Value>) -> String {
    if values.is_empty() {
        return "(none echoed before the assert)".to_string();
    }
    values
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn describe_exit(code: &Option<i32>) -> String {
    match code {
        Some(c) => format!("code {c}"),
        None => "a signal".to_string(),
    }
}
