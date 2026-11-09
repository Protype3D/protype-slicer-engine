//! Daemon app-data directory resolution. Single source of truth for the
//! platform-specific root under which lockfiles, autosaves, and derived
//! per-object caches live.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Root directory for daemon-managed application data on this host.
///
/// macOS: `~/Library/Application Support/Protype Slicer`
/// Linux: `$XDG_DATA_HOME/protype-slicer` (or `~/.local/share/protype-slicer`)
/// Windows: `%APPDATA%/Protype Slicer`
///
/// `PROTYPE_SLICER_DATA_DIR` overrides the root for isolated daemon runs.
///
/// Does not create the directory — callers that need it on disk must
/// `create_dir_all` themselves.
pub fn app_data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PROTYPE_SLICER_DATA_DIR") {
        anyhow::ensure!(!path.is_empty(), "PROTYPE_SLICER_DATA_DIR is empty");
        return Ok(PathBuf::from(path));
    }
    if cfg!(target_os = "macos") {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home).join("Library/Application Support/Protype Slicer"))
    } else if cfg!(target_os = "linux") {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            return Ok(PathBuf::from(xdg).join("protype-slicer"));
        }
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home).join(".local/share/protype-slicer"))
    } else {
        let appdata = std::env::var("APPDATA").context("APPDATA not set")?;
        Ok(PathBuf::from(appdata).join("Protype Slicer"))
    }
}

/// Cache root for derived per-object artifacts (e.g. split-to-parts part
/// STLs). Layout: `<cache_root>/<source_hash>/part_<i>.stl`.
///
/// Pure path query — does not create the directory. The first writer is
/// responsible for `create_dir_all`.
pub fn object_cache_dir() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("object_cache"))
}
