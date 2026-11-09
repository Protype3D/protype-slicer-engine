//! Lock files for projects (spec §6.7). When a project is opened the
//! daemon writes `<project>.3mf.lock` naming this host, this install, its
//! pid, when that process started and a timestamp. A second open attempt on
//! the same project asks `process_owner` whether the holder still runs on
//! this host: a lock whose holder is proven gone is taken over, one whose
//! holder can't be verified stays and the refusal says why. A lock of
//! another host is surfaced to the user but not broken on its own, since its
//! processes can't be probed from here.
//!
//! "This host" is decided by the OS host id (`host_identity`), not by the
//! per-install `machine_id`: that one is minted per HOME, so a dev build
//! and an agent sandbox on one Mac would otherwise take each other's stale
//! locks for another machine's. Locks from before the host id existed are
//! compared by `machine_id`, as they always were.

use crate::host_identity;
use crate::paths::app_data_dir;
use crate::process_owner::{self, Owner};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LockFile {
    /// Per-install id (a ULID under the app data dir). Kept for locks and
    /// daemons from before `host_id`.
    pub machine_id: String,
    pub pid: u32,
    pub started_at_unix_ms: i64,
    /// The OS host id of the writer; absent in locks written before it
    /// existed, and on hosts without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// The writer's host name, for the message shown on another machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// When the writing process started, as the OS keeps it (see
    /// `process_owner`): a later process that got the same pid has another.
    /// Absent in locks written before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start: Option<u64>,
}

#[derive(Debug)]
pub enum AcquireOutcome {
    /// Lock acquired (either fresh, or after taking over a stale one).
    Acquired,
    /// Same host, the pid is still running — refuse. `doubt` says why that
    /// process may not be the lock's writer when it can't be verified (a
    /// lock without the writer's start, a start the OS won't tell), for the
    /// person deciding what to do about the lock.
    HeldByLivePid { pid: u32, doubt: Option<String> },
    /// Different host — the caller decides whether to override. Carries
    /// what the lock says about its holder, for the question to the user.
    HeldByOtherMachine {
        machine_id: String,
        hostname: Option<String>,
        started_at_unix_ms: i64,
    },
}

/// Attempt to acquire `<path>.lock`. Stale locks (same host, holder proven
/// gone) are replaced.
pub fn try_acquire(project_path: &Path) -> Result<AcquireOutcome> {
    let lock_path = lock_path_for(project_path);
    let ours = our_lock()?;

    if let Some(existing) = read_lock(&lock_path) {
        if !is_same_host(&existing, &ours) {
            return Ok(AcquireOutcome::HeldByOtherMachine {
                machine_id: existing.machine_id,
                hostname: existing.hostname,
                started_at_unix_ms: existing.started_at_unix_ms,
            });
        }
        // Our own pid is ours, or a predecessor's that ended before we
        // got it: taken over either way.
        if existing.pid != ours.pid {
            let pid = existing.pid;
            match process_owner::probe(pid, existing.process_start) {
                Owner::Running => return Ok(AcquireOutcome::HeldByLivePid { pid, doubt: None }),
                Owner::Unverified(why) => {
                    return Ok(AcquireOutcome::HeldByLivePid {
                        pid,
                        doubt: Some(why),
                    })
                }
                Owner::Gone => tracing::info!(
                    lock = %lock_path.display(),
                    pid,
                    "taking over a project lock whose holder has exited"
                ),
            }
        }
    }

    write_lock(&lock_path, &ours)?;
    Ok(AcquireOutcome::Acquired)
}

/// Write our lock over whatever is there: "open anyway" on a lock of
/// another host. Reading past that lock instead would leave it in place —
/// to ask again next time, and never to be cleared by `release`.
pub fn take_over(project_path: &Path) -> Result<()> {
    write_lock(&lock_path_for(project_path), &our_lock()?)
}

/// Whether `existing` was written on this host. The OS host id decides when
/// both sides carry one; a lock from before it existed, or a host without
/// one, falls back to the per-install id.
fn is_same_host(existing: &LockFile, ours: &LockFile) -> bool {
    match (&existing.host_id, &ours.host_id) {
        (Some(theirs), Some(mine)) => theirs == mine,
        _ => existing.machine_id == ours.machine_id,
    }
}

fn our_lock() -> Result<LockFile> {
    Ok(LockFile {
        machine_id: machine_id()?,
        pid: std::process::id(),
        started_at_unix_ms: chrono::Utc::now().timestamp_millis(),
        host_id: host_identity::host_id().map(str::to_string),
        hostname: host_identity::hostname(),
        process_start: process_owner::this_process_start(),
    })
}

/// The lock at `lock_path`, or `None` when there is none or it is not one
/// (a lock nobody can read is nobody's — it gets overwritten).
fn read_lock(lock_path: &Path) -> Option<LockFile> {
    let bytes = std::fs::read(lock_path).ok()?;
    serde_json::from_slice::<LockFile>(&bytes).ok()
}

fn write_lock(lock_path: &Path, lock: &LockFile) -> Result<()> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(lock_path, serde_json::to_vec_pretty(lock)?)
        .with_context(|| format!("write {}", lock_path.display()))
}

/// Release the lock if (and only if) it's ours. Cross-machine or other-pid
/// locks are left alone.
pub fn release(project_path: &Path) -> Result<()> {
    let lock_path = lock_path_for(project_path);
    let our_machine = machine_id()?;
    if let Ok(bytes) = std::fs::read(&lock_path) {
        if let Ok(existing) = serde_json::from_slice::<LockFile>(&bytes) {
            if existing.machine_id == our_machine && existing.pid == std::process::id() {
                let _ = std::fs::remove_file(&lock_path);
            }
        }
    }
    Ok(())
}

/// Where the lock of `project_path` lives: next to it, `.lock` appended.
pub fn lock_path_for(project_path: &Path) -> PathBuf {
    let mut s = project_path.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// Stable per-machine id, persisted under app_data/machine_id (a ULID).
///
/// Cached process-wide via `OnceLock`: every caller in this process observes
/// the same value, even if concurrent threads race on first use. Without the
/// cache, two threads that both miss the file would each mint a fresh id —
/// then a single `try_acquire` that calls `machine_id()` twice could observe
/// two different ids and incorrectly report `HeldByOtherMachine` for its own
/// lock. The on-disk file is created via `hard_link` from a temp file, which
/// gives atomic test-and-set semantics so concurrent *processes* on a fresh
/// machine also converge on one persisted id instead of overwriting each
/// other.
static MACHINE_ID: OnceLock<String> = OnceLock::new();

fn machine_id() -> Result<String> {
    if let Some(id) = MACHINE_ID.get() {
        return Ok(id.clone());
    }
    let id = load_or_create_machine_id()?;
    Ok(MACHINE_ID.get_or_init(|| id).clone())
}

fn load_or_create_machine_id() -> Result<String> {
    let dir = app_data_dir()?;
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("machine_id");

    if let Some(id) = read_machine_id_file(&path) {
        return Ok(id);
    }

    let candidate = ulid::Ulid::new().to_string();
    let tmp = dir.join(format!(
        "machine_id.tmp.{}.{}",
        std::process::id(),
        ulid::Ulid::new()
    ));
    std::fs::write(&tmp, candidate.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    let result = match std::fs::hard_link(&tmp, &path) {
        Ok(()) => Ok(candidate),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_machine_id_file(&path)
            .ok_or_else(|| anyhow::anyhow!("machine_id present but unreadable after link race")),
        Err(e) => Err(anyhow::Error::new(e)).context("link machine_id"),
    };
    let _ = std::fs::remove_file(&tmp);
    result
}

fn read_machine_id_file(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_owner::test_support::{finished_pid, Sleeper};

    // `nanos()` alone is not unique across parallel tests (macOS clock
    // granularity + thread interleaving can collide), so two tests using
    // the same dir then race on the same lock file. Mix in a ULID for a
    // collision-free, per-call suffix.
    fn unique_test_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "protype-lock-test-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            ulid::Ulid::new()
        ))
    }

    #[test]
    fn acquire_and_release_round_trip() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let project = dir.join("p.3mf");
        std::fs::write(&project, b"placeholder").unwrap();
        let lock = dir.join("p.3mf.lock");

        match try_acquire(&project).unwrap() {
            AcquireOutcome::Acquired => {}
            other => panic!("expected Acquired, got {other:?}"),
        }
        assert!(lock.exists());

        // Re-acquiring with the same pid is fine (we take over our own lock).
        match try_acquire(&project).unwrap() {
            AcquireOutcome::Acquired => {}
            other => panic!("expected Acquired on self-reacquire, got {other:?}"),
        }

        release(&project).unwrap();
        assert!(!lock.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn live_pid_blocks_acquire() {
        let (dir, project, lock) = project_in_fresh_dir();
        // A lock owned by THIS machine but another, running process.
        let holder = Sleeper::spawn();
        let forged = LockFile {
            machine_id: machine_id().unwrap(),
            pid: holder.pid(),
            started_at_unix_ms: 0,
            host_id: None,
            hostname: None,
            process_start: process_owner::start_of(holder.pid()),
        };
        std::fs::write(&lock, serde_json::to_vec(&forged).unwrap()).unwrap();
        match try_acquire(&project).unwrap() {
            AcquireOutcome::HeldByLivePid { pid, doubt } => {
                assert_eq!(pid, holder.pid());
                assert_eq!(doubt, None, "the holder's start matches: no doubt");
            }
            other => panic!("expected HeldByLivePid, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A project file in its own fresh directory, with the path its lock
    /// would take.
    fn project_in_fresh_dir() -> (PathBuf, PathBuf, PathBuf) {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let project = dir.join("p.3mf");
        std::fs::write(&project, b"x").unwrap();
        let lock = dir.join("p.3mf.lock");
        (dir, project, lock)
    }

    fn read_lock(lock: &Path) -> LockFile {
        serde_json::from_slice(&std::fs::read(lock).unwrap()).unwrap()
    }

    // The per-install `machine_id` lives under the app data dir, so two
    // daemons on one Mac with different HOMEs (a dev build, an agent
    // sandbox, a CI runner) carry different ids. A lock left behind by a
    // killed daemon of the other install must still count as stale here:
    // the host is the same, and its pid can be probed.
    #[test]
    fn stale_lock_from_another_install_on_this_host_is_taken_over() {
        let (dir, project, lock) = project_in_fresh_dir();
        let forged = LockFile {
            machine_id: "other-install".to_string(),
            pid: finished_pid(),
            started_at_unix_ms: 0,
            host_id: crate::host_identity::host_id().map(str::to_string),
            hostname: None,
            process_start: None,
        };
        std::fs::write(&lock, serde_json::to_vec(&forged).unwrap()).unwrap();

        match try_acquire(&project).unwrap() {
            AcquireOutcome::Acquired => {}
            other => panic!("expected Acquired over a dead pid of this host, got {other:?}"),
        }
        let ours = read_lock(&lock);
        assert_eq!(ours.machine_id, machine_id().unwrap());
        assert_eq!(ours.pid, std::process::id());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn live_pid_from_another_install_on_this_host_blocks() {
        let (dir, project, lock) = project_in_fresh_dir();
        let holder = Sleeper::spawn();
        let forged = LockFile {
            machine_id: "other-install".to_string(),
            pid: holder.pid(),
            started_at_unix_ms: 0,
            host_id: crate::host_identity::host_id().map(str::to_string),
            hostname: None,
            process_start: process_owner::start_of(holder.pid()),
        };
        std::fs::write(&lock, serde_json::to_vec(&forged).unwrap()).unwrap();
        match try_acquire(&project).unwrap() {
            AcquireOutcome::HeldByLivePid { pid, doubt } => {
                assert_eq!(pid, holder.pid());
                assert_eq!(doubt, None);
            }
            other => panic!("expected HeldByLivePid, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_from_another_host_is_reported_with_its_name_and_time() {
        let (dir, project, lock) = project_in_fresh_dir();
        let forged = LockFile {
            machine_id: "other-install".to_string(),
            pid: std::process::id(),
            started_at_unix_ms: 1_700_000_000_000,
            host_id: Some("other-host".to_string()),
            hostname: Some("office-pc".to_string()),
            process_start: None,
        };
        std::fs::write(&lock, serde_json::to_vec(&forged).unwrap()).unwrap();
        match try_acquire(&project).unwrap() {
            AcquireOutcome::HeldByOtherMachine {
                machine_id,
                hostname,
                started_at_unix_ms,
            } => {
                assert_eq!(machine_id, "other-install");
                assert_eq!(hostname.as_deref(), Some("office-pc"));
                assert_eq!(started_at_unix_ms, 1_700_000_000_000);
            }
            other => panic!("expected HeldByOtherMachine, got {other:?}"),
        }
        // Not ours: left exactly as found.
        assert_eq!(read_lock(&lock).machine_id, "other-install");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Locks written before `host_id` existed say nothing about the host.
    // Without it, a different install is still taken for a different
    // machine — the old behaviour, which the user can override with force.
    #[test]
    fn legacy_lock_from_another_install_without_host_id_stays_foreign() {
        let (dir, project, lock) = project_in_fresh_dir();
        std::fs::write(
            &lock,
            br#"{"machine_id":"other-install","pid":1,"started_at_unix_ms":42}"#,
        )
        .unwrap();
        match try_acquire(&project).unwrap() {
            AcquireOutcome::HeldByOtherMachine {
                machine_id,
                hostname,
                started_at_unix_ms,
            } => {
                assert_eq!(machine_id, "other-install");
                assert_eq!(hostname, None);
                assert_eq!(started_at_unix_ms, 42);
            }
            other => panic!("expected HeldByOtherMachine, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // "Open anyway" means taking the lock, not just reading past it:
    // otherwise the foreign lock outlives this session and asks again next
    // time, and `release` would never clear it.
    #[test]
    fn take_over_replaces_a_foreign_lock_with_ours() {
        let (dir, project, lock) = project_in_fresh_dir();
        std::fs::write(
            &lock,
            br#"{"machine_id":"other-install","pid":1,"started_at_unix_ms":42,"host_id":"other-host"}"#,
        )
        .unwrap();

        take_over(&project).unwrap();

        let ours = read_lock(&lock);
        assert_eq!(ours.machine_id, machine_id().unwrap());
        assert_eq!(ours.pid, std::process::id());
        release(&project).unwrap();
        assert!(!lock.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn our_lock_names_this_host() {
        let (dir, project, lock) = project_in_fresh_dir();
        try_acquire(&project).unwrap();
        let ours = read_lock(&lock);
        assert_eq!(
            ours.host_id.as_deref(),
            crate::host_identity::host_id(),
            "the lock carries the host id the next daemon on this host will compare against"
        );
        assert_eq!(ours.hostname, crate::host_identity::hostname());
        release(&project).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // Windows hands a freed pid to the next process within moments: a lock
    // whose pid now runs a process started at another time is stale.
    #[test]
    fn lock_whose_pid_now_runs_a_later_process_is_taken_over() {
        let (dir, project, lock) = project_in_fresh_dir();
        let later = Sleeper::spawn();
        let started = process_owner::start_of(later.pid()).expect("the child's start");
        let forged = LockFile {
            machine_id: machine_id().unwrap(),
            pid: later.pid(),
            started_at_unix_ms: 0,
            host_id: crate::host_identity::host_id().map(str::to_string),
            hostname: None,
            process_start: Some(started - 1),
        };
        std::fs::write(&lock, serde_json::to_vec(&forged).unwrap()).unwrap();

        match try_acquire(&project).unwrap() {
            AcquireOutcome::Acquired => {}
            other => panic!("expected Acquired over a reused pid, got {other:?}"),
        }
        assert_eq!(read_lock(&lock).pid, std::process::id());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A lock as daemons wrote it before they recorded their start.
    fn legacy_lock_of(pid: u32) -> Vec<u8> {
        format!(
            r#"{{"machine_id":"{}","pid":{pid},"started_at_unix_ms":42}}"#,
            machine_id().unwrap()
        )
        .into_bytes()
    }

    #[test]
    fn legacy_lock_without_a_start_of_a_finished_pid_is_taken_over() {
        let (dir, project, lock) = project_in_fresh_dir();
        std::fs::write(&lock, legacy_lock_of(finished_pid())).unwrap();

        match try_acquire(&project).unwrap() {
            AcquireOutcome::Acquired => {}
            other => panic!("expected Acquired over a finished pid, got {other:?}"),
        }
        assert_eq!(read_lock(&lock).pid, std::process::id());
        std::fs::remove_dir_all(&dir).ok();
    }

    // Without a start, a running pid may be the writer or a stranger that
    // got its pid: the lock stays, and the refusal says why it is in doubt.
    #[test]
    fn legacy_lock_without_a_start_of_a_running_pid_stays_with_the_reason() {
        let (dir, project, lock) = project_in_fresh_dir();
        let holder = Sleeper::spawn();
        let legacy = legacy_lock_of(holder.pid());
        std::fs::write(&lock, &legacy).unwrap();

        match try_acquire(&project).unwrap() {
            AcquireOutcome::HeldByLivePid { pid, doubt } => {
                assert_eq!(pid, holder.pid());
                let why = doubt.expect("an unverified holder comes with the reason");
                assert!(why.contains("older version"), "{why}");
            }
            other => panic!("expected HeldByLivePid, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&lock).unwrap(),
            legacy,
            "left exactly as found"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn our_lock_records_when_this_process_started() {
        let (dir, project, lock) = project_in_fresh_dir();
        try_acquire(&project).unwrap();

        let ours = read_lock(&lock);

        assert!(ours.process_start.is_some());
        assert_eq!(ours.process_start, process_owner::this_process_start());
        release(&project).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
