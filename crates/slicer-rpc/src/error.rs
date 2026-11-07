use serde::Serialize;
use serde_json::{json, Value};
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
