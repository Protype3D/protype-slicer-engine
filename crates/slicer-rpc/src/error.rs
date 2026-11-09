use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error, Serialize)]
#[serde(tag = "kind")]
pub enum RpcError {
    #[error("invalid token")]
    InvalidToken,
    #[error("protocol version mismatch: client {client}, server {server}")]
    VersionMismatch { client: String, server: String },
    #[error("internal error: {0}")]
    Internal(String),
    #[error("project: {0}")]
    Project(String),
    /// `project.open`/`project.save` found the file locked by another host.
    /// Its own code and `data` let a client ask "open anyway?" and name the
    /// holder, instead of pattern-matching the text of `Project`.
    #[error(
        "project is locked by another machine ({}); pass force=true to override",
        lock_holder(.hostname, .machine_id)
    )]
    ProjectLocked {
        machine_id: String,
        hostname: Option<String>,
        started_at_unix_ms: i64,
    },
    #[error("slicing: {0}")]
    Slicing(String),
}

impl RpcError {
    pub fn code(&self) -> i32 {
        match self {
            RpcError::InvalidToken => -32001,
            RpcError::VersionMismatch { .. } => -32002,
            RpcError::Internal(_) => -32603,
            RpcError::Project(_) => -33000,
            RpcError::ProjectLocked { .. } => -33001,
            RpcError::Slicing(_) => -35000,
        }
    }

    /// `project.open`/`project.save` found the file locked by `pid`, a
    /// running process of this host. Nothing overrides that lock; when the
    /// process can't be verified as its writer, the message says why and
    /// names the lock, for the person to remove if no other Protype Slicer
    /// has the file open.
    pub fn held_by_running_pid(pid: u32, doubt: Option<String>, lock: &Path) -> Self {
        let open_elsewhere =
            format!("project is open in another Protype Slicer instance (pid {pid})");
        RpcError::Project(match doubt {
            None => open_elsewhere,
            Some(why) => format!(
                "{open_elsewhere}, unverified: {why}. If no other Protype Slicer has it open, \
                 delete {}",
                lock.display()
            ),
        })
    }

    /// The JSON-RPC `error.data` of this error: what a client needs beyond
    /// the message, when there is anything.
    pub fn data(&self) -> Option<Value> {
        match self {
            RpcError::ProjectLocked {
                machine_id,
                hostname,
                started_at_unix_ms,
            } => Some(json!({
                "machine_id": machine_id,
                "hostname": hostname,
                "started_at_unix_ms": started_at_unix_ms,
            })),
            _ => None,
        }
    }
}

/// How the message names the lock's holder: its host name, or the start of
/// its install id for a lock written before host names were recorded.
fn lock_holder(hostname: &Option<String>, machine_id: &str) -> String {
    hostname
        .clone()
        .unwrap_or_else(|| machine_id.chars().take(10).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verified_holder_reads_as_before() {
        let err = RpcError::held_by_running_pid(1234, None, Path::new("/p/a.3mf.lock"));

        assert_eq!(err.code(), -33000);
        assert_eq!(
            err.to_string(),
            "project: project is open in another Protype Slicer instance (pid 1234)"
        );
    }

    // The window shows the message: an unverified holder must come with why
    // and with the lock to remove when no other Protype Slicer has the file.
    #[test]
    fn an_unverified_holder_names_the_doubt_and_the_lock() {
        let lock = Path::new("/p/a.3mf.lock");
        let doubt = "pid 1234 is running, but when it started could not be read".to_string();

        let err = RpcError::held_by_running_pid(1234, Some(doubt.clone()), lock);

        assert_eq!(err.code(), -33000);
        let text = err.to_string();
        assert!(text.contains("pid 1234"), "{text}");
        assert!(text.contains(&doubt), "{text}");
        assert!(text.contains(&lock.display().to_string()), "{text}");
    }
}
