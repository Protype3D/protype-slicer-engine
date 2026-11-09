//! Per-session autosave (spec §6.7). Each daemon process owns a directory
//! `<app_data>/autosave/<project_id>/` containing:
//!   - meta.json   — session metadata (project_id, daemon_pid, started_at)
//!   - base.json   — the project the session last put in place (opened,
//!     recovered or saved) and the id of the last event it holds; absent
//!     until the first of those
//!   - events.log  — NDJSON of every event envelope applied since that base
//!
//! On daemon startup we list other autosave dirs whose `daemon_pid` is no
//! longer alive on this machine — those are "orphan" sessions left behind
//! by a crash and become candidates for recovery.
//!
//! Recovery starts from base.json, or from an empty project without one,
//! and replays the events.log envelopes numbered past it.

use crate::paths::app_data_dir;
use crate::persist::ProjectFile;
use crate::project::ProjectState;
use crate::reducer;
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use slicer_types::{EventEnvelope, EventId, ProjectEvent};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use ulid::Ulid;

/// Retention window since the last journal/base write, not session creation.
/// Empty sessions with a base already held by a project file are pruned
/// regardless of age.
const ORPHAN_MAX_AGE_MS: i64 = 14 * 24 * 60 * 60 * 1000;

const BASE_FILE: &str = "base.json";
const LOG_FILE: &str = "events.log";

/// The project a session put in place, which recovery starts from.
#[derive(Serialize, Deserialize)]
struct Base {
    /// The last event the base holds. Event ids keep growing for the life of
    /// the daemon, across project swaps, so a log line numbered at or below
    /// it is one the base already holds: a crash between writing the base and
    /// cutting the log leaves such lines behind.
    through: EventId,
    /// A project file holds this very state, so a session with no events past
    /// it has nothing a file does not.
    on_disk: bool,
    project: ProjectFile,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AutosaveMeta {
    pub project_id: String,
    pub daemon_pid: u32,
    pub started_at_unix_ms: i64,
    pub schema_version: u32,
    /// When the daemon process started, as the OS keeps it (see
    /// `process_owner`): tells a crashed daemon from a later process that
    /// got its pid. Absent in manifests written before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start: Option<u64>,
}

/// Exposed in state.snapshot; a failed writer retries on the next mutation.
#[derive(Debug, Clone, Serialize)]
pub struct AutosaveStatus {
    pub working: bool,
    pub reason: Option<String>,
    /// Written lossily: serde refuses a path that is not UTF-8, and
    /// state.snapshot builds its reply with `json!`, which would panic.
    #[serde(serialize_with = "serialize_lossy_path")]
    pub path: Option<PathBuf>,
}

fn serialize_lossy_path<S: serde::Serializer>(
    path: &Option<PathBuf>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match path {
        Some(path) => serializer.serialize_some(&path.to_string_lossy()),
        None => serializer.serialize_none(),
    }
}

/// Whether an event goes to the journal. Slice job events change nothing in
/// the project (the reducer ignores them), so recovery has no use for them;
/// journaling them would make a session saved and then sliced look like
/// unsaved work, and cost an fsync per progress tick under the state lock.
fn is_journaled(event: &ProjectEvent) -> bool {
    !matches!(
        event,
        ProjectEvent::SliceRequested { .. }
            | ProjectEvent::SliceProgress { .. }
            | ProjectEvent::SliceCompleted { .. }
            | ProjectEvent::SliceFailed { .. }
            | ProjectEvent::SliceCanceled { .. }
    )
}

pub struct AutosaveSink {
    project_id: String,
    root: PathBuf,
    dir: PathBuf,
    error: Mutex<Option<String>>,
    /// Set by a discard: the base and log no longer hold what the window
    /// shows, so the next recorded event writes a full snapshot instead of a
    /// delta onto what the discard left.
    rebase_pending: AtomicBool,
    started_at_unix_ms: i64,
}

impl AutosaveSink {
    /// Create a new autosave session under `<app_data>/autosave/<ulid>/`.
    /// Writes meta.json immediately so recovery can find this session.
    pub fn new() -> Result<Self> {
        Self::new_in(&default_root()?)
    }

    /// Create a new autosave session under `<root>/<ulid>/`.
    pub fn new_in(root: &Path) -> Result<Self> {
        let sink = Self::start_in(root);
        if let Some(error) = sink.status().reason {
            bail!("{error}");
        }
        Ok(sink)
    }

    /// Attach even when initialization fails, so later edits retry a full
    /// snapshot and clients can inspect the failure rather than lose it.
    pub fn start_in(root: &Path) -> Self {
        let project_id = Ulid::new().to_string();
        let sink = Self {
            dir: root.join(&project_id),
            project_id,
            root: root.to_path_buf(),
            error: Mutex::new(None),
            rebase_pending: AtomicBool::new(false),
            started_at_unix_ms: chrono::Utc::now().timestamp_millis(),
        };
        let result = sink.initialize();
        sink.set_result(&result);
        sink
    }

    fn initialize(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create {}", self.dir.display()))?;
        let meta = AutosaveMeta {
            project_id: self.project_id.clone(),
            daemon_pid: std::process::id(),
            started_at_unix_ms: self.started_at_unix_ms,
            schema_version: 1,
            process_start: crate::process_owner::this_process_start(),
        };
        write_atomic(&self.dir, "meta.json", &serde_json::to_vec(&meta)?)?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(LOG_FILE))
            .with_context(|| format!("create {}", self.dir.join(LOG_FILE).display()))?
            .sync_all()?;
        sync_directory(&self.dir)?;
        sync_directory(&self.root)
    }

    fn open_log(&self) -> Result<File> {
        OpenOptions::new()
            .append(true)
            .open(self.dir.join(LOG_FILE))
            .with_context(|| format!("open {}", self.dir.join(LOG_FILE).display()))
    }

    /// Cut the journal through a handle opened for writing: on Windows an
    /// append-only handle lacks FILE_WRITE_DATA, which truncation needs.
    fn truncate_log(&self) -> Result<()> {
        let path = self.dir.join(LOG_FILE);
        let log = OpenOptions::new()
            .write(true)
            .open(&path)
            .with_context(|| format!("open {} for truncation", path.display()))?;
        log.set_len(0)
            .with_context(|| format!("truncate {}", path.display()))?;
        log.sync_all()
            .with_context(|| format!("sync {}", path.display()))
    }

    pub fn status(&self) -> AutosaveStatus {
        let reason = self.error.lock().clone();
        AutosaveStatus {
            working: reason.is_none(),
            reason,
            path: Some(self.dir.clone()),
        }
    }

    fn set_result(&self, result: &Result<()>) {
        *self.error.lock() = result.as_ref().err().map(|error| format!("{error:#}"));
        if let Err(error) = result {
            tracing::warn!(?error, path = %self.dir.display(), "autosave write failed");
        }
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// The directory this session's siblings — the orphans it can recover —
    /// live in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Called under StateLayer's lock after reduction. Once any write failed,
    /// only a full snapshot can make the session healthy again.
    pub(crate) fn record(&self, envelope: &EventEnvelope, project: &ProjectState) {
        if !is_journaled(&envelope.event) {
            return;
        }
        if self.error.lock().is_some() || self.rebase_pending.swap(false, Ordering::SeqCst) {
            let _ = self.checkpoint(project, envelope.event_id, false);
            return;
        }
        let result = (|| {
            let mut bytes = serde_json::to_vec(envelope)?;
            bytes.push(b'\n');
            let mut log = self.open_log()?;
            log.write_all(&bytes)?;
            log.sync_data()?;
            Ok(())
        })();
        self.set_result(&result);
    }

    /// Install and sync the new base before truncating the previous journal.
    /// StateLayer serializes this with every event and project replacement.
    pub(crate) fn checkpoint(
        &self,
        project: &ProjectState,
        through: EventId,
        on_disk: bool,
    ) -> Result<()> {
        let result = (|| {
            self.initialize()?;
            let base = Base {
                through,
                on_disk,
                project: ProjectFile::from_state(project),
            };
            write_base(&self.dir, &base)?;
            self.truncate_log()
        })();
        self.set_result(&result);
        result
    }

    /// "Don't Save" from the window: drop what this live session holds
    /// beyond a project file, so the next launch offers nothing for it,
    /// however this daemon goes down. The journal is cut. A base no file
    /// holds (a recovered or never-saved project) is the refused work itself
    /// and goes too; a base a file holds stays, since the file has it.
    ///
    /// The session stays live. The first edit after this writes a full
    /// snapshot of the project in memory, so a crash after a cancelled quit
    /// restores what the window shows, refused edits included: losing work
    /// still on screen is worse than offering it again. A failure is
    /// returned, not kept in the status: the window must not report the
    /// refusal as done while the work is still on disk. Crate-only: callers
    /// go through [`crate::StateLayer::discard_autosave`], under the lock
    /// every journal write takes.
    pub(crate) fn discard_unsaved(&self) -> Result<()> {
        let log = self.dir.join(LOG_FILE);
        if log
            .try_exists()
            .with_context(|| format!("stat {}", log.display()))?
        {
            self.truncate_log()?;
        }
        if base_is_off_disk(&self.dir) {
            let base = self.dir.join(BASE_FILE);
            match std::fs::remove_file(&base) {
                Ok(()) => sync_directory(&self.dir)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("remove {}", base.display())),
            }
        }
        self.rebase_pending.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Explicit discard only (also used by test teardown). Never call on a
    /// process signal: it says nothing about whether the window saved edits.
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
pub fn list_orphans(root: &Path, current: Option<&str>) -> Result<Vec<OrphanSession>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(i64::MAX);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let meta_path = path.join("meta.json");
        let log_path = path.join(LOG_FILE);
        let Ok(meta_bytes) = std::fs::read(&meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_slice::<AutosaveMeta>(&meta_bytes) else {
            continue;
        };
        if Some(meta.project_id.as_str()) == current {
            continue;
        }
        if pid_is_alive(meta.daemon_pid, meta.process_start) {
            // Another live daemon owns this session.
            continue;
        }
        // No journal means no edits past the base. A journal that cannot be
        // read counts as one edit: better listed than deleted.
        let event_count = match log_path.try_exists() {
            Ok(false) => 0,
            _ => count_lines(&log_path).unwrap_or(1),
        };
        // Prune noise so the recovery list stays bounded: a session with no
        // events past a base some file holds carries no recoverable work,
        // and a snapshot past the retention window is never going to be
        // recovered. Best-effort delete — a failure just leaves the dir to be
        // retried next scan.
        let holds_nothing = event_count == 0 && !base_is_off_disk(&path);
        let last_write_ms = [log_path.as_path(), path.join(BASE_FILE).as_path()]
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
            .filter_map(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .max()
            .unwrap_or(now_ms);
        let age_ms = now_ms.saturating_sub(last_write_ms);
        if holds_nothing || age_ms > ORPHAN_MAX_AGE_MS {
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

/// Recover an orphan session: its base, or an empty project without one,
/// with the events.log envelopes past the base replayed onto it. Returns the
/// rebuilt state; the caller must durably checkpoint it before discarding
/// this source. The current live session is refused.
pub fn recover(root: &Path, project_id: &str, current: Option<&str>) -> Result<ProjectState> {
    let dir = orphan_dir(root, project_id, current)?;
    let log_path = dir.join(LOG_FILE);
    let base = read_base(&dir)?;
    let log = match File::open(&log_path) {
        Ok(f) => Some(f),
        // The base alone is a whole project; only a session with neither has
        // nothing to recover.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && base.is_some() => None,
        Err(e) => return Err(e).with_context(|| format!("open {}", log_path.display())),
    };
    let (mut state, through) = match base {
        Some(base) => (base.project.into_state()?, base.through),
        None => (ProjectState::default(), EventId(0)),
    };
    let Some(log) = log else {
        return Ok(state);
    };
    let mut reader = BufReader::new(log);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        // A killed writer may leave an incomplete final record. Only the
        // trailing non-terminated line is ignored; complete corrupt records
        // still fail instead of silently losing changes in the middle.
        if line.last() != Some(&b'\n') {
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let envelope: EventEnvelope = serde_json::from_slice(&line)
            .with_context(|| format!("parse autosave line in {}", log_path.display()))?;
        if envelope.event_id <= through {
            continue;
        }
        reducer::reduce(&mut state, &envelope.event);
    }
    Ok(state)
}

/// Delete an orphan directory. Used after a recovery is accepted (so we
/// don't keep showing it) or rejected (user chose to discard it).
pub fn discard(root: &Path, project_id: &str, current: Option<&str>) -> Result<()> {
    let dir = orphan_dir(root, project_id, current)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// Sync the temporary file, replace the destination, then sync its directory.
/// No previous base or journal is removed before this completes.
fn write_base(dir: &Path, base: &Base) -> Result<()> {
    write_atomic(dir, BASE_FILE, &serde_json::to_vec(base)?)
}

fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    let mut file = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    sync_directory(dir)
}

pub(crate) fn sync_directory(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)?
        .sync_all()
        .with_context(|| format!("sync {}", dir.display()))?;
    // Windows cannot open directories with File::open. File contents are
    // flushed above; directory syncing is a Unix durability guarantee.
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// The session's base, `None` while it has none.
fn read_base(dir: &Path) -> Result<Option<Base>> {
    let path = dir.join(BASE_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Whether the session's base is a project no file holds: one it recovered,
/// or one a save never reached. A base that cannot be read counts as one —
/// better to list a session the user then discards than to delete it.
fn base_is_off_disk(dir: &Path) -> bool {
    #[derive(Deserialize)]
    struct OnDisk {
        on_disk: bool,
    }
    match std::fs::read(dir.join(BASE_FILE)) {
        Ok(bytes) => serde_json::from_slice::<OnDisk>(&bytes).map_or(true, |base| !base.on_disk),
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
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

/// Whether the daemon `pid`, started at `started`, may still own its
/// session. Only a daemon proven gone gives it up: one that can't be
/// verified counts as running, since a live session must never be offered
/// for recovery, let alone pruned.
fn pid_is_alive(pid: u32, started: Option<u64>) -> bool {
    use crate::process_owner::{probe, Owner};
    !matches!(probe(pid, started), Owner::Gone)
}

/// `<app_data>/autosave`, where the daemon keeps its sessions.
pub fn default_root() -> Result<PathBuf> {
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
