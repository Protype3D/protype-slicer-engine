//! Build a mesh from a parametric source, atomically.
//!
//! # The failure this is shaped around
//!
//! A loop over six layout variants wrote each one to `/tmp/v.stl`,
//! checked `[ -s /tmp/v.stl ]`, and measured it. All six reported the
//! same volume. OpenSCAD does not overwrite its output when an assert
//! fires, so five of the runs measured the first run's file, and the
//! test that was supposed to catch that — "is the file non-empty" —
//! passed every time.
//!
//! Reproduced against OpenSCAD 2021.01 while writing this: after a
//! failing build over a successful one, the target file is still there
//! with an identical checksum.
//!
//! So this module makes the trap unrepresentable rather than documenting
//! it:
//!
//! * **Content-addressed by default.** The path is derived from the
//!   source, the parameters and the OpenSCAD version, so two variants
//!   cannot share a file to go stale in.
//! * **Written through a temporary.** OpenSCAD renders to a unique
//!   temporary path which is renamed onto the destination only after the
//!   mesh has been read back and found to enclose volume. A partial
//!   render never appears at the destination.
//! * **Destination cleared on failure.** When a caller names its own
//!   output path, a failed build removes it. After this call that path
//!   holds this build's output or nothing — never the previous
//!   iteration's.
//! * **Read back before returning.** Success means the triangles were
//!   loaded and measured, not that the exit code was zero.

pub mod diagnose;
pub mod echo;
pub mod error;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use slicer_geometry::mesh::{Measurement, TriangleMesh};

pub use echo::Echoes;
pub use error::ScadError;

/// Default ceiling on a render. CGAL can grind for a very long time on a
/// pathological tree, and a caller waiting forever is a worse outcome
/// than a stated timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub enum Source {
    /// A `.scad` file already on disk.
    Path(PathBuf),
    /// Source text, written to a temporary file for the run.
    Text(String),
}

#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub source: Source,
    /// `-D name=value` overrides. Values are OpenSCAD expressions, so a
    /// string parameter needs its own quotes.
    pub params: BTreeMap<String, String>,
    /// Where the mesh should end up. `None` puts it at a
    /// content-addressed path under `cache_dir`, which is the form that
    /// cannot go stale.
    pub output_path: Option<PathBuf>,
    pub cache_dir: PathBuf,
    pub timeout: Duration,
}

impl BuildRequest {
    pub fn new(source: Source, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            source,
            params: BTreeMap::new(),
            output_path: None,
            cache_dir: cache_dir.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.insert(name.into(), value.into());
        self
    }

    pub fn with_output(mut self, path: impl Into<PathBuf>) -> Self {
        self.output_path = Some(path.into());
        self
    }
}

/// A build that produced a mesh. There is no other way to get one.
#[derive(Debug, Clone)]
pub struct ScadBuild {
    pub mesh_path: PathBuf,
    /// SHA-256 over the source, the parameters and the OpenSCAD version.
    pub source_hash: String,
    /// Everything the model computed and echoed, as data.
    pub echoes: Echoes,
    /// Measured from the file that was just written — which is also the
    /// proof it was written and is readable.
    pub measurement: Measurement,
    pub openscad_version: String,
}

pub fn build(request: &BuildRequest) -> Result<ScadBuild, ScadError> {
    let binary = find_openscad()?;
    let version = version_of(&binary)?;

    let (source_path, _scratch) = materialize_source(&request.source, &request.cache_dir)?;
    let source_bytes = std::fs::read(&source_path).map_err(|source| ScadError::Io {
        action: "cannot read",
        path: source_path.display().to_string(),
        source,
    })?;
    let source_hash = hash_of(&source_bytes, &request.params, &version);

    std::fs::create_dir_all(&request.cache_dir).map_err(|source| ScadError::Io {
        action: "cannot create",
        path: request.cache_dir.display().to_string(),
        source,
    })?;

    let destination = request
        .output_path
        .clone()
        .unwrap_or_else(|| request.cache_dir.join(format!("scad-{source_hash}.stl")));

    let scratch = Scratch::beside(&destination, &source_hash)?;

    let output = run(&binary, &source_path, scratch.path(), request)?;
    let echoes = echo::parse(&output.stderr);

    if !output.success {
        // The destination is cleared, not left alone. Leaving the
        // previous iteration's file behind is the whole bug.
        clear(&destination)?;
        return Err(diagnose::classify(
            &output.stderr,
            output.exit_code,
            &echoes,
        ));
    }

    if !scratch.path().exists() {
        clear(&destination)?;
        return Err(ScadError::NoOutput {
            path: destination.display().to_string(),
        });
    }

    // Read it back before calling it a success. A zero exit code is a
    // claim; triangles that enclose volume are the evidence.
    let mesh = TriangleMesh::from_stl_path(scratch.path())?;
    let measurement = mesh.measure()?;
    if let Err(degenerate) = reject_degenerate(&measurement) {
        clear(&destination)?;
        return Err(degenerate);
    }

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ScadError::Io {
            action: "cannot create",
            path: parent.display().to_string(),
            source,
        })?;
    }
    // Rename rather than copy: within one filesystem it is atomic, so a
    // reader either sees the old file or the new one and never a half
    // written mesh.
    std::fs::rename(scratch.path(), &destination).map_err(|source| ScadError::Io {
        action: "cannot move the finished mesh into",
        path: destination.display().to_string(),
        source,
    })?;
    scratch.disarm();

    Ok(ScadBuild {
        mesh_path: destination,
        source_hash,
        echoes,
        measurement,
        openscad_version: version,
    })
}

/// A file is only a result if it encloses something.
///
/// Kept as its own function, and unit-tested, because it could not be
/// provoked end-to-end. OpenSCAD 2021.01 was probed with `cube([10,10,0])`,
/// `square(10)` and `linear_extrude(height=0)`: it refuses all three
/// itself, with exit 1 and no file. So this guards a state the current
/// backend does not reach — which is a reason to test it directly, not a
/// reason to ship it unexercised.
pub fn reject_degenerate(measurement: &Measurement) -> Result<(), ScadError> {
    if measurement.triangle_count == 0 || measurement.volume_mm3 <= 0.0 {
        return Err(ScadError::Degenerate {
            triangles: measurement.triangle_count,
            volume_mm3: measurement.volume_mm3,
        });
    }
    Ok(())
}

struct RunOutput {
    success: bool,
    exit_code: Option<i32>,
    stderr: String,
}

fn run(
    binary: &Path,
    source: &Path,
    output: &Path,
    request: &BuildRequest,
) -> Result<RunOutput, ScadError> {
    let mut command = Command::new(binary);
    for (name, value) in &request.params {
        command.arg("-D").arg(format!("{name}={value}"));
    }
    command
        .arg("-o")
        .arg(output)
        .arg(source)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|source| ScadError::Spawn { source })?;

    // `wait_with_output` would block past the deadline, so the pipe is
    // drained on a thread and the process polled. Draining matters: a
    // render chatty enough to fill the stderr pipe would otherwise
    // deadlock against a parent that is only polling.
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let reader = std::thread::spawn(move || {
        let mut buffer = String::new();
        use std::io::Read;
        let _ = stderr_pipe.read_to_string(&mut buffer);
        buffer
    });

    let deadline = Instant::now() + request.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(ScadError::Timeout {
                        seconds: request.timeout.as_secs(),
                    });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(source) => return Err(ScadError::Spawn { source }),
        }
    };

    Ok(RunOutput {
        success: status.success(),
        exit_code: status.code(),
        stderr: reader.join().unwrap_or_default(),
    })
}

/// A temporary render target that removes itself unless the build got as
/// far as renaming it into place.
struct Scratch {
    path: PathBuf,
    armed: bool,
}

impl Scratch {
    /// Placed next to the destination so the rename stays within one
    /// filesystem, and named uniquely so two concurrent builds of the
    /// same source cannot render into each other's file.
    fn beside(destination: &Path, hash: &str) -> Result<Self, ScadError> {
        let directory = destination.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(directory).map_err(|source| ScadError::Io {
            action: "cannot create",
            path: directory.display().to_string(),
            source,
        })?;
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = directory.join(format!(
            ".scad-{hash}-{}-{serial}.partial.stl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Ok(Self { path, armed: true })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Remove a destination so a failed build cannot leave the previous
/// iteration's mesh where a caller will read it.
fn clear(destination: &Path) -> Result<(), ScadError> {
    match std::fs::remove_file(destination) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ScadError::Io {
            action: "cannot clear the stale mesh at",
            path: destination.display().to_string(),
            source,
        }),
    }
}

/// Source text needs a file for OpenSCAD to read. The returned guard
/// deletes it when the build is done; a `Source::Path` has no guard
/// because the file is the caller's.
fn materialize_source(
    source: &Source,
    cache_dir: &Path,
) -> Result<(PathBuf, Option<TempFile>), ScadError> {
    match source {
        Source::Path(path) => Ok((path.clone(), None)),
        Source::Text(text) => {
            std::fs::create_dir_all(cache_dir).map_err(|source| ScadError::Io {
                action: "cannot create",
                path: cache_dir.display().to_string(),
                source,
            })?;
            let digest = hex::encode(Sha256::digest(text.as_bytes()));
            let path = cache_dir.join(format!(".inline-{}-{digest}.scad", std::process::id()));
            std::fs::write(&path, text).map_err(|source| ScadError::Io {
                action: "cannot write",
                path: path.display().to_string(),
                source,
            })?;
            Ok((path.clone(), Some(TempFile(path))))
        }
    }
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Includes the OpenSCAD version, so upgrading the tool cannot serve a
/// mesh built by the previous one out of the cache.
fn hash_of(source: &[u8], params: &BTreeMap<String, String>, version: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source);
    hasher.update([0u8]);
    for (name, value) in params {
        hasher.update(name.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
        hasher.update([0u8]);
    }
    hasher.update(version.as_bytes());
    hex::encode(hasher.finalize())
}

/// `$OPENSCAD` first, then the PATH, then the usual macOS bundle.
pub fn find_openscad() -> Result<PathBuf, ScadError> {
    if let Ok(explicit) = std::env::var("OPENSCAD") {
        let path = PathBuf::from(&explicit);
        if path.exists() {
            return Ok(path);
        }
        return Err(ScadError::NotFound { tried: explicit });
    }
    let candidates = [
        "openscad",
        "/opt/homebrew/bin/openscad",
        "/usr/local/bin/openscad",
        "/Applications/OpenSCAD.app/Contents/MacOS/openscad",
    ];
    for candidate in candidates {
        if Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return Ok(PathBuf::from(candidate));
        }
    }
    Err(ScadError::NotFound {
        tried: candidates.join(", "),
    })
}

/// OpenSCAD 2021.01 prints its version to **stderr**, not stdout.
fn version_of(binary: &Path) -> Result<String, ScadError> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|source| ScadError::Spawn { source })?;
    let text = if output.stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout)
    } else {
        String::from_utf8_lossy(&output.stderr)
    };
    Ok(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measurement(triangle_count: usize, volume_mm3: f64) -> Measurement {
        Measurement {
            triangle_count,
            bounds_min: [0.0; 3],
            bounds_max: [1.0, 1.0, 1.0],
            size: [1.0, 1.0, 1.0],
            volume_mm3,
            surface_area_mm2: 6.0,
            unpaired_edges: 0,
            is_manifold: true,
            weld_tolerance_mm: 1e-4,
        }
    }

    #[test]
    fn a_mesh_that_encloses_volume_is_accepted() {
        assert!(reject_degenerate(&measurement(12, 1000.0)).is_ok());
    }

    #[test]
    fn a_file_with_no_triangles_is_not_a_result() {
        let err = reject_degenerate(&measurement(0, 0.0)).expect_err("nothing there");
        assert!(matches!(err, ScadError::Degenerate { triangles: 0, .. }));
    }

    #[test]
    fn a_surface_enclosing_nothing_is_not_a_result() {
        // Triangles present, volume zero: the shape a degenerate outline
        // extrudes into. It would slice to no toolpaths at all.
        let err = reject_degenerate(&measurement(8, 0.0)).expect_err("encloses nothing");
        assert!(matches!(err, ScadError::Degenerate { .. }));
    }

    #[test]
    fn an_inside_out_surface_is_not_a_result_either() {
        let err = reject_degenerate(&measurement(12, -1000.0)).expect_err("wound inward");
        assert!(err.to_string().contains("-1000"));
    }

    #[test]
    fn the_hash_separates_inputs_that_would_otherwise_collide() {
        // Without the separator byte, ("ab", {}) and ("a", {"b": ""})
        // would feed the hasher the same stream.
        let mut params = BTreeMap::new();
        params.insert("W".to_string(), "1".to_string());
        let with_param = hash_of(b"cube(1);", &params, "2021.01");
        let without = hash_of(b"cube(1);", &BTreeMap::new(), "2021.01");
        assert_ne!(with_param, without);
    }

    #[test]
    fn upgrading_openscad_changes_the_address() {
        let a = hash_of(b"cube(1);", &BTreeMap::new(), "2021.01");
        let b = hash_of(b"cube(1);", &BTreeMap::new(), "2025.03");
        assert_ne!(a, b, "a cached mesh must not outlive the tool that made it");
    }
}
