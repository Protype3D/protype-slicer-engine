use crate::auth::AuthToken;
use crate::build_info;
use crate::dispatch::Dispatcher;
use crate::error::RpcError;
use crate::methods::{HelloParams, HelloResult, EVENT_NOTIFICATION, GOODBYE, PROTOCOL_VERSION};
use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use slicer_state::StateLayer;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, error, info, warn};
use ulid::Ulid;

#[derive(Clone)]
pub struct ServerState {
    pub token: AuthToken,
    pub state: StateLayer,
}

pub struct ServerHandle {
    pub port: u16,
    pub token: AuthToken,
    pub shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

pub async fn start_server(state: StateLayer, requested_port: u16) -> Result<ServerHandle> {
    let token = AuthToken::generate();
    let server_state = ServerState {
        token: token.clone(),
        state,
    };
    let app = Router::new()
        .route("/rpc", get(ws_handler))
        .with_state(server_state);

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), requested_port);
    let listener = TcpListener::bind(addr).await?;
    let bound_addr = listener.local_addr()?;
    let port = bound_addr.port();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    info!(port, "slicer-daemon WS server listening");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });

    Ok(ServerHandle {
        port,
        token,
        shutdown_tx,
    })
}

async fn ws_handler(State(state): State<ServerState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: ServerState) {
    let session_id = Ulid::new().to_string();
    info!(%session_id, "ws client connected");

    let hello = match tokio::time::timeout(std::time::Duration::from_secs(5), socket.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => t,
        _ => {
            warn!(%session_id, "no hello within timeout, closing");
            return;
        }
    };

    let parsed: Value = match serde_json::from_str(&hello) {
        Ok(v) => v,
        Err(e) => {
            error!(?e, "hello parse error");
            return;
        }
    };

    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let params: HelloParams =
        match serde_json::from_value(parsed.get("params").cloned().unwrap_or(Value::Null)) {
            Ok(p) => p,
            Err(e) => {
                let err = RpcError::Internal(format!("invalid hello params: {e}"));
                let _ = socket.send(Message::Text(error_response(&id, &err))).await;
                return;
            }
        };

    if !state.token.matches(&params.token) {
        let err = RpcError::InvalidToken;
        let _ = socket.send(Message::Text(error_response(&id, &err))).await;
        warn!(%session_id, "invalid token, closing");
        return;
    }

    if !params.protocol_version.starts_with("1.") {
        let err = RpcError::VersionMismatch {
            client: params.protocol_version.clone(),
            server: PROTOCOL_VERSION.to_string(),
        };
        let _ = socket.send(Message::Text(error_response(&id, &err))).await;
        return;
    }

    // One cut: every event numbered above `current_event_id` reaches this
    // socket, none at or below it does. Taken before the reply, so what the
    // daemon logs while the client reads it waits in `event_rx`.
    let (mut event_rx, current_event_id) = state.state.subscribe_from_now();
    let result = HelloResult {
        session_id: session_id.clone(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_VERSION.to_string(),
        current_event_id,
        capabilities: crate::methods::capabilities(),
        daemon_commit: build_info::DAEMON_COMMIT.to_string(),
        libslic3r_commit: build_info::LIBSLIC3R_COMMIT.to_string(),
        daemon_dirty: build_info::DIRTY,
    };
    let response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    if socket
        .send(Message::Text(response.to_string()))
        .await
        .is_err()
    {
        return;
    }
    info!(%session_id, "hello complete");

    let dispatcher = Dispatcher::new(state.state.clone());

    loop {
        tokio::select! {
            msg = socket.recv() => {
                let Some(Ok(msg)) = msg else { break; };
                match msg {
                    Message::Text(t) => {
                        let req: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                        let req_id = req.get("id").cloned().unwrap_or(Value::Null);
                        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
                        let params = req.get("params").cloned().unwrap_or(Value::Null);
                        if method == GOODBYE {
                            let resp = json!({"jsonrpc":"2.0","id":req_id,"result":{}});
                            let _ = socket.send(Message::Text(resp.to_string())).await;
                            info!(%session_id, "goodbye received");
                            break;
                        }
                        let resp = match dispatcher.handle(&method, params).await {
                            Ok(result) => json!({"jsonrpc":"2.0","id":req_id,"result":result}),
                            Err(e) => error_value(&req_id, &e),
                        };
                        if socket.send(Message::Text(resp.to_string())).await.is_err() { break; }
                    }
                    Message::Binary(_) => {
                        debug!(%session_id, "recv binary (ignored in Phase 1 stub)");
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            evt = event_rx.recv() => {
                match evt {
                    Ok(envelope) => {
                        let notif = json!({
                            "jsonrpc":"2.0",
                            "method": EVENT_NOTIFICATION,
                            "params": envelope,
                        });
                        if socket.send(Message::Text(notif.to_string())).await.is_err() { break; }
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!(%session_id, "event subscriber lagged by {n}");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
}

fn error_response(id: &Value, err: &RpcError) -> String {
    error_value(id, err).to_string()
}

/// The JSON-RPC error reply to `id`, with `data` only for the errors that
/// have any (`RpcError::data`).
fn error_value(id: &Value, err: &RpcError) -> Value {
    let mut error = json!({ "code": err.code(), "message": err.to_string() });
    if let Some(data) = err.data() {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The wire contract the window branches on: a lock of another host
    // answers under its own code with the holder in `data`; every other
    // error keeps the bare `{code, message}` it always had.
    #[test]
    fn error_reply_carries_data_only_when_the_error_has_any() {
        let locked = RpcError::ProjectLocked {
            machine_id: "other-install".into(),
            hostname: Some("office-pc".into()),
            started_at_unix_ms: 42,
        };
        let reply = error_value(&json!(7), &locked);
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["error"]["code"], -33001);
        assert_eq!(reply["error"]["data"]["hostname"], "office-pc");
        assert_eq!(reply["error"]["data"]["started_at_unix_ms"], 42);

        let plain = error_value(&json!(8), &RpcError::Project("open failed".into()));
        assert_eq!(plain["error"]["code"], -33000);
        assert!(plain["error"].get("data").is_none(), "{plain}");
    }
}
