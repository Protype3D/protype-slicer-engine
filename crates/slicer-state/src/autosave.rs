//! Per-session autosave (spec §6.7). Each daemon process owns a directory
//! `<app_data>/autosave/<project_id>/` containing:
//!   - meta.json   — session metadata (project_id, daemon_pid, started_at)
//!   - events.log  — append-only NDJSON of every applied event envelope
//!
//! On daemon startup we list other autosave dirs whose `daemon_pid` is no
//! longer alive on this machine — those are "orphan" sessions left behind
//! by a crash and become candidates for recovery.
//!
//! Recovery replays events.log into a fresh ProjectState. We could
//! periodically snapshot the project to skip replay on large projects,
//! but for MVP scope full replay is fast enough.

use crate::paths::app_data_dir;
use crate::project::ProjectState;
use crate::reducer;
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use slicer_types::EventEnvelope;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use ulid::Ulid;

/// Retention window for orphan crash-recovery sessions. A snapshot older than
/// this is never realistically recovered — keeping it just grows the recovery
/// list unbounded (real-world instances hit 50+ stale dirs). Empty sessions
/// (no recorded events) are pruned regardless of age.
const ORPHAN_MAX_AGE_MS: i64 = 14 * 24 * 60 * 60 * 1000;

#[derive(Debug, Serialize, Deserialize)]
pub struct AutosaveMeta {
    pub project_id: String,
    pub daemon_pid: u32,
    pub started_at_unix_ms: i64,
    pub schema_version: u32,
}

pub struct AutosaveSink {
    project_id: String,
    dir: PathBuf,
    log: Mutex<File>,
}

impl AutosaveSink {
    /// Create a new autosave session under `<app_data>/autosave/<ulid>/`.
    /// Writes meta.json immediately so recovery can find this session.
    pub fn new() -> Result<Self> {
        let project_id = Ulid::new().to_string();
        let root = autosave_root()?;
        std::fs::create_dir_all(&root)?;
        let dir = root.join(&project_id);
        std::fs::create_dir_all(&dir)?;

        let meta = AutosaveMeta {
            project_id: project_id.clone(),
            daemon_pid: std::process::id(),
            started_at_unix_ms: chrono::Utc::now().timestamp_millis(),
            schema_version: 1,
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;

        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.log"))?;

        Ok(Self {
            project_id,
            dir,
            log: Mutex::new(log),
        })
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append one envelope as an NDJSON line. Errors are swallowed and
    /// logged: an autosave write failure must NEVER crash the running
    /// daemon — we'd rather lose a checkpoint than the whole session.
    pub fn record(&self, envelope: &EventEnvelope) {
        let mut log = self.log.lock();
        match serde_json::to_vec(envelope) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                if let Err(e) = log.write_all(&bytes) {
                    tracing::warn!(?e, "autosave write failed");
                }
            }
            Err(e) => {
                tracing::warn!(?e, "autosave envelope serialization failed");
            }
        }
    }

    /// Truncate the events.log and re-anchor metadata. Called after a
    /// project.save so the on-disk autosave only carries deltas since the
    /// last persisted state.
    pub fn checkpoint(&self) -> Result<()> {
        let mut log = self.log.lock();
        log.flush().ok();
        // Re-create the file in truncate mode.
        *log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(self.dir.join("events.log"))?;
        Ok(())
    }

    /// Best-effort cleanup on graceful shutdown. The orphan-detection path
    /// uses dead-pid checks so leaving the directory on a crash is fine —
    /// this is just for the happy path.
    pub fn cleanup_on_exit(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OrphanSession {
    pub project_id: String,
    pub dir: PathBuf,
    pub started_at_unix_ms: i64,
    pub event_count: usize,
    /// PID recorded at session start; reported so the UI can decide
    /// whether to surface "left over from PID N" to the user.
    pub stale_pid: u32,
}

/// Scan the autosave root for dirs whose meta.daemon_pid is no longer
/// alive. The current process's session (if any) is excluded by `current`.
pub fn list_orphans(current: Option<&str>) -> Result<Vec<OrphanSession>> {
    let root = autosave_root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(i64::MAX);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let meta_path = path.join("meta.json");
        let log_path = path.join("events.log");
        let Ok(meta_bytes) = std::fs::read(&meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_slice::<AutosaveMeta>(&meta_bytes) else {
            continue;
        };
        if Some(meta.project_id.as_str()) == current {
            continue;
        }
        if pid_is_alive(meta.daemon_pid) {
            // Another live daemon owns this session.
            continue;
        }
        let event_count = count_lines(&log_path).unwrap_or(0);
        // Prune noise so the recovery list stays bounded: an empty session
        // carries no recoverable work, and a snapshot past the retention
        // window is never going to be recovered. Best-effort delete — a
        // failure just leaves the dir to be retried next scan.
        let age_ms = now_ms.saturating_sub(meta.started_at_unix_ms);
        if event_count == 0 || age_ms > ORPHAN_MAX_AGE_MS {
            let _ = std::fs::remove_dir_all(&path);
            continue;
        }
        out.push(OrphanSession {
            project_id: meta.project_id,
            dir: path,
            started_at_unix_ms: meta.started_at_unix_ms,
            event_count,
            stale_pid: meta.daemon_pid,
        });
    }
    out.sort_by_key(|x| std::cmp::Reverse(x.started_at_unix_ms));
    Ok(out)
}

/// Recover an orphan session by replaying its events.log into a fresh
/// ProjectState. Returns the rebuilt state; the caller is responsible for
/// invoking StateLayer::replace_state with it. `current` is the live
/// session's id, which is refused like anything that is not a session id.
pub fn recover(project_id: &str, current: Option<&str>) -> Result<ProjectState> {
    let dir = orphan_dir(&autosave_root()?, project_id, current)?;
    let log_path = dir.join("events.log");
    let f = File::open(&log_path).with_context(|| format!("open {}", log_path.display()))?;
    let mut state = ProjectState::default();
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let envelope: EventEnvelope = serde_json::from_str(&line)
            .with_context(|| format!("parse autosave line in {}", log_path.display()))?;
        reducer::reduce(&mut state, &envelope.event);
    }
    Ok(state)
}

/// Delete an orphan directory. Used after a recovery is accepted (so we
/// don't keep showing it) or rejected (user chose to discard it). `current`
/// is refused as in [`recover`].
pub fn discard(project_id: &str, current: Option<&str>) -> Result<()> {
    let dir = orphan_dir(&autosave_root()?, project_id, current)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// The dir of the orphan session `project_id` names under `root`, which
/// recovery reads and then deletes. `project_id` comes from a client, so it
/// must be a session id exactly as [`AutosaveSink::new`] spells one — a
/// canonical ULID, which names a child of `root` and nothing else: not `/x`,
/// not `..`, not `""` or `<id>/..` (the root itself), not a lowercase
/// spelling a case-insensitive disk opens as the same dir. `current`, the
/// live session of this daemon, is refused too: its dir is where every
/// later event and checkpoint goes.
fn orphan_dir(root: &Path, project_id: &str, current: Option<&str>) -> Result<PathBuf> {
    let is_session_id = Ulid::from_string(project_id).is_ok_and(|id| id.to_string() == project_id);
    if !is_session_id {
        bail!("{project_id:?} is not an autosave session id");
    }
    if current == Some(project_id) {
        bail!("{project_id} is this daemon's own live autosave session, not an orphan");
    }
    Ok(root.join(project_id))
}

fn count_lines(path: &Path) -> Result<usize> {
    let f = File::open(path)?;
    Ok(BufReader::new(f).lines().count())
}

#[cfg(target_family = "unix")]
fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // kill(pid, 0) returns Ok if the process exists (or we lack
    // permission, which still counts as alive on this machine).
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(not(target_family = "unix"))]
fn pid_is_alive(_pid: u32) -> bool {
    // Conservative: treat as alive on platforms we can't probe. Better to
    // miss recovering than to clobber a live session.
    true
}

fn autosave_root() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("autosave"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> &'static Path {
        Path::new("/autosave-root")
    }

    #[test]
    fn orphan_dir_is_the_session_dir_under_the_root() {
        let id = Ulid::new().to_string();
        let live = Ulid::new().to_string();

        assert_eq!(orphan_dir(root(), &id, None).unwrap(), root().join(&id));
        assert_eq!(
            orphan_dir(root(), &id, Some(&live)).unwrap(),
            root().join(&id)
        );
    }

    #[test]
    fn orphan_dir_refuses_anything_but_a_session_id() {
        let id = Ulid::new().to_string();
        let not_ids = [
            String::new(),
            ".".into(),
            "..".into(),
            "/".into(),
            "/tmp".into(),
            format!("../{id}"),
            format!("{id}/.."),
            format!("{id}/events.log"),
            format!("{id}/"),
            format!(" {id}"),
            // The same dir on a case-insensitive disk, and so a way past the
            // live-session check.
            id.to_lowercase(),
            id[..id.len() - 1].to_string(),
        ];
        for not_id in not_ids {
            let err = orphan_dir(root(), &not_id, None)
                .expect_err(&format!("{not_id:?} must be refused"));
            assert!(
                err.to_string().contains("not an autosave session id"),
                "{not_id:?}: {err}"
            );
        }
    }

    #[test]
    fn orphan_dir_refuses_the_live_session() {
        let live = Ulid::new().to_string();

        let err =
            orphan_dir(root(), &live, Some(&live)).expect_err("the live session is no orphan");

        assert!(
            err.to_string().contains("own live autosave session"),
            "{err}"
        );
    }
}
