//! The flat OrcaSlicer preset every libslic3r slice starts from.
//!
//! It is compiled into the binary instead of being read from the build
//! checkout: a path baked in at build time names a directory that exists
//! only on the machine that built the daemon, so an installed or published
//! daemon could not slice anywhere else. libslic3r's
//! `ConfigBase::load_from_json` reads only files, so each slice writes the
//! embedded text to the temp dir and hands the bridge that path.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bambu Lab X1 Carbon + PLA + 0.20 mm, flattened offline by
/// `scripts/flatten_orca_preset.py` (provenance in `profiles/LICENSE.md`).
pub(crate) const DEFAULT_PRESET_JSON: &str =
    include_str!("../../../profiles/protype-default-x1c-pla-0.20.json");

/// Tells stagings of one process apart when the clock does not.
static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);

/// The embedded preset written to disk for one slice; removed on drop.
#[derive(Debug)]
pub(crate) struct StagedPreset {
    path: PathBuf,
}

impl StagedPreset {
    /// Writes the embedded preset into a new file in `dir`. The name carries
    /// the pid, the time and a per-process counter, so neither the host's and
    /// the agent's daemons nor a file left by a crashed one with a reused pid
    /// collide.
    pub(crate) fn stage_in(dir: &Path) -> io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = NEXT_STAGING.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            "protype-slicer-preset-{}-{nanos}-{seq}.json",
            std::process::id()
        );
        Self::stage_at(dir.join(name))
    }

    /// Creates `path` exclusively: an existing file or a symlink planted in a
    /// shared temp dir is refused rather than written through.
    pub(crate) fn stage_at(path: PathBuf) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| staging_error(&path, e))?;
        // Owned from here on, so a failed write still removes the file.
        let staged = Self { path };
        file.write_all(DEFAULT_PRESET_JSON.as_bytes())
            .map_err(|e| staging_error(&staged.path, e))?;
        Ok(staged)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

fn staging_error(path: &Path, e: io::Error) -> io::Error {
    io::Error::new(
        e.kind(),
        format!("cannot stage embedded preset at {}: {e}", path.display()),
    )
}

impl Drop for StagedPreset {
    fn drop(&mut self) {
        // A file left behind (say, by a crash mid-slice) is harmless: every
        // slice creates a fresh name, and the OS clears the temp dir.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "default-preset-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn embedded_preset_is_a_complete_flat_profile() {
        let value: serde_json::Value =
            serde_json::from_str(DEFAULT_PRESET_JSON).expect("embedded preset is JSON");
        let keys = value.as_object().expect("top-level JSON object");
        for key in ["printable_area", "layer_height", "filament_type"] {
            assert!(keys.contains_key(key), "embedded preset lacks {key}");
        }
    }

    #[test]
    fn staged_file_is_the_profile_byte_for_byte() {
        let dir = scratch_dir("bytes");
        let staged = StagedPreset::stage_in(&dir).expect("stage preset");
        let profile = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../profiles/protype-default-x1c-pla-0.20.json"
        ))
        .expect("read profile from the repo");
        assert_eq!(std::fs::read(staged.path()).expect("read staged"), profile);
        drop(staged);
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");
    }

    #[test]
    fn staged_file_is_removed_on_drop() {
        let dir = scratch_dir("drop");
        let staged = StagedPreset::stage_in(&dir).expect("stage preset");
        let path = staged.path().to_path_buf();
        assert!(path.exists());
        drop(staged);
        assert!(!path.exists(), "{} survived drop", path.display());
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");
    }

    #[test]
    fn each_staging_creates_its_own_file() {
        let dir = scratch_dir("distinct");
        let first = StagedPreset::stage_in(&dir).expect("stage first");
        let second = StagedPreset::stage_in(&dir).expect("stage second");
        assert_ne!(first.path(), second.path());
        assert!(first.path().exists() && second.path().exists());
        drop((first, second));
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");
    }

    #[cfg(unix)]
    #[test]
    fn staging_refuses_a_planted_symlink() {
        let dir = scratch_dir("symlink");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "untouched").expect("write victim");
        let link = dir.join("planted.json");
        std::os::unix::fs::symlink(&victim, &link).expect("plant symlink");

        let err = StagedPreset::stage_at(link.clone()).expect_err("existing name");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        assert!(
            link.exists(),
            "a refused staging must not remove what it found"
        );
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");
    }

    #[test]
    fn staging_failure_names_the_path() {
        let dir = scratch_dir("missing");
        let missing = dir.join("does-not-exist");
        let err = StagedPreset::stage_in(&missing).expect_err("no such dir");
        let text = err.to_string();
        assert!(text.contains("cannot stage embedded preset at"), "{text}");
        assert!(text.contains("does-not-exist"), "{text}");
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");
    }
}
