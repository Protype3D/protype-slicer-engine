//! Talking to Moonraker.
//!
//! Deliberately thin. Everything that can be decided wrongly lives in
//! [`crate::watch`], which has no I/O and is tested without a printer;
//! this module fetches bytes and hands them over.
//!
//! Two ports, because the machine serves from two: the API is on 7125,
//! and the webcam is behind the front-end web server on 80. Asking 7125
//! for `/webcam/?action=snapshot` returns a JSON 404, which looks like a
//! camera failure and is not.

use std::time::Duration;

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;

use crate::console::{parse_store, ConsoleLine};
use crate::snapshot::{Snapshot, QUERY};

pub const DEFAULT_API_PORT: u16 = 7125;

/// How long a single request may take before the machine counts as
/// unreachable.
///
/// A refused connection fails on its own; a wedged host that completes
/// the handshake and then answers nothing does not, and hyper applies no
/// deadline of its own. Ten seconds is far longer than any of these
/// endpoints needs — a snapshot off a healthy machine returns in
/// milliseconds — and short enough that a caller notices.
pub const DEFAULT_TIMEOUT_S: f64 = 10.0;

/// The floor under a poll deadline, whatever the interval asks for.
///
/// A poll should not outlive the interval that schedules it, but a fast
/// `--interval-s` must not start calling a healthy-if-slow machine dead.
pub const MIN_TIMEOUT_S: f64 = 5.0;

/// The per-request deadline for a watch polling every `interval_s`.
///
/// Non-finite and non-positive intervals fall back to the floor rather
/// than panicking in `Duration::from_secs_f64`.
pub fn poll_timeout(interval_s: f64) -> Duration {
    let seconds = if interval_s.is_finite() {
        interval_s.max(MIN_TIMEOUT_S)
    } else {
        MIN_TIMEOUT_S
    };
    Duration::from_secs_f64(seconds)
}

#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    #[error("cannot reach the machine at {url}: {source}")]
    Unreachable {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("{url} answered {status}")]
    Status { url: String, status: u16 },
    #[error("{url} did not answer with JSON: {source}")]
    Malformed {
        url: String,
        #[source]
        source: serde_json::Error,
    },
}

/// A camera frame, with the one fact that decides whether it is worth
/// looking at.
#[derive(Debug, Clone)]
pub struct CameraFrame {
    pub jpeg: Vec<u8>,
    /// Whether the chamber light was on.
    ///
    /// The camera returns a perfectly valid 200 and a well-formed JPEG
    /// with the light off — it is simply black. Measured: 50 kB of
    /// 1920x1080 essentially-black frame while `output_pin caselight`
    /// read 0. Anything that treats "a frame arrived" as "the print can
    /// be seen" is wrong in exactly that case.
    pub caselight_on: Option<bool>,
}

impl CameraFrame {
    /// Whether the frame can be expected to show anything.
    pub fn is_usable(&self) -> bool {
        self.caselight_on == Some(true)
    }

    pub fn caveat(&self) -> Option<&'static str> {
        match self.caselight_on {
            Some(true) => None,
            Some(false) => Some(
                "the chamber light was off, so this frame is black — it is not evidence \
                 about the print",
            ),
            None => Some("the chamber light state is unknown, so this frame may be dark"),
        }
    }
}

pub struct Moonraker {
    api: String,
    web: String,
    timeout: Duration,
    client: Client<HttpConnector, Empty<Bytes>>,
}

impl Moonraker {
    /// `host` without a scheme, e.g. `192.168.0.218`.
    pub fn new(host: &str) -> Self {
        let bare = host.split(':').next().unwrap_or(host);
        let api = if host.contains(':') {
            format!("http://{host}")
        } else {
            format!("http://{host}:{DEFAULT_API_PORT}")
        };
        Self {
            api,
            web: format!("http://{bare}"),
            timeout: Duration::from_secs_f64(DEFAULT_TIMEOUT_S),
            client: Client::builder(TokioExecutor::new()).build_http(),
        }
    }

    /// Override the per-request deadline. See [`poll_timeout`] for the
    /// value a polling loop should use.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn api_base(&self) -> &str {
        &self.api
    }

    /// Every request goes through here, so every request has a deadline —
    /// including the connect, which is where a blackholed host stalls.
    async fn bytes(&self, url: &str) -> Result<Vec<u8>, MonitorError> {
        match tokio::time::timeout(self.timeout, self.fetch(url)).await {
            Ok(result) => result,
            Err(_) => Err(MonitorError::Unreachable {
                url: url.to_string(),
                source: format!(
                    "the connection was accepted but nothing answered within {:.1}s",
                    self.timeout.as_secs_f64()
                )
                .into(),
            }),
        }
    }

    async fn fetch(&self, url: &str) -> Result<Vec<u8>, MonitorError> {
        let uri: hyper::Uri =
            url.parse().map_err(
                |e: hyper::http::uri::InvalidUri| MonitorError::Unreachable {
                    url: url.to_string(),
                    source: Box::new(e),
                },
            )?;
        let response = self
            .client
            .get(uri)
            .await
            .map_err(|e| MonitorError::Unreachable {
                url: url.to_string(),
                source: Box::new(e),
            })?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| MonitorError::Unreachable {
                url: url.to_string(),
                source: Box::new(e),
            })?
            .to_bytes();
        if !status.is_success() {
            return Err(MonitorError::Status {
                url: url.to_string(),
                status: status.as_u16(),
            });
        }
        Ok(body.to_vec())
    }

    async fn json(&self, path: &str) -> Result<serde_json::Value, MonitorError> {
        let url = format!("{}{path}", self.api);
        let body = self.bytes(&url).await?;
        serde_json::from_slice(&body).map_err(|source| MonitorError::Malformed { url, source })
    }

    pub async fn snapshot(&self) -> Result<Snapshot, MonitorError> {
        let payload = self
            .json(&format!("/printer/objects/query?{QUERY}"))
            .await?;
        let status = payload
            .get("result")
            .and_then(|r| r.get("status"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        Ok(Snapshot::from_status(&status))
    }

    /// New console lines, and the newest timestamp seen.
    ///
    /// `count` is asked for generously: this is a ring buffer, and the
    /// whole reason to read it is that its contents do not survive.
    pub async fn console_since(
        &self,
        seen_after: f64,
        count: u32,
    ) -> Result<(Vec<ConsoleLine>, f64), MonitorError> {
        let payload = self
            .json(&format!("/server/gcode_store?count={count}"))
            .await?;
        Ok(parse_store(&payload, seen_after))
    }

    /// A frame, plus whether the light was on to take it by.
    pub async fn camera_frame(&self) -> Result<CameraFrame, MonitorError> {
        let caselight_on = self.caselight_on().await.ok();
        let jpeg = self
            .bytes(&format!("{}/webcam/?action=snapshot", self.web))
            .await?;
        Ok(CameraFrame { jpeg, caselight_on })
    }

    async fn caselight_on(&self) -> Result<bool, MonitorError> {
        let payload = self
            .json("/printer/objects/query?output_pin%20caselight")
            .await?;
        Ok(payload
            .get("result")
            .and_then(|r| r.get("status"))
            .and_then(|s| s.get("output_pin caselight"))
            .and_then(|p| p.get("value"))
            .and_then(|v| v.as_f64())
            .is_some_and(|v| v > 0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_gets_the_api_port_and_the_web_root() {
        let m = Moonraker::new("192.168.0.218");
        assert_eq!(m.api, "http://192.168.0.218:7125");
        // The camera is not on the API port; asking 7125 for it returns a
        // JSON 404 that reads like a camera fault.
        assert_eq!(m.web, "http://192.168.0.218");
    }

    #[test]
    fn an_explicit_port_is_respected_and_the_web_root_still_drops_it() {
        let m = Moonraker::new("printer.local:7125");
        assert_eq!(m.api, "http://printer.local:7125");
        assert_eq!(m.web, "http://printer.local");
    }

    #[test]
    fn a_frame_taken_in_the_dark_says_so_rather_than_passing_as_evidence() {
        let dark = CameraFrame {
            jpeg: vec![0; 50_000],
            caselight_on: Some(false),
        };
        assert!(!dark.is_usable());
        assert!(dark.caveat().unwrap().contains("not evidence"));

        let lit = CameraFrame {
            jpeg: vec![0; 50_000],
            caselight_on: Some(true),
        };
        assert!(lit.is_usable());
        assert!(lit.caveat().is_none());
    }

    #[test]
    fn a_poll_deadline_follows_the_interval_but_never_drops_below_the_floor() {
        assert_eq!(poll_timeout(20.0), Duration::from_secs(20));
        assert_eq!(poll_timeout(1.0), Duration::from_secs_f64(MIN_TIMEOUT_S));
    }

    /// `Duration::from_secs_f64` panics on these, and `--interval-s` is a
    /// bare f64 off the command line, so the fallback is not academic.
    #[test]
    fn a_nonsense_interval_yields_the_floor_rather_than_panicking() {
        for absurd in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                poll_timeout(absurd),
                Duration::from_secs_f64(MIN_TIMEOUT_S),
                "interval {absurd} should fall back to the floor"
            );
        }
    }

    #[test]
    fn an_unknown_light_state_is_not_treated_as_lit() {
        let unknown = CameraFrame {
            jpeg: Vec::new(),
            caselight_on: None,
        };
        assert!(!unknown.is_usable());
        assert!(unknown.caveat().is_some());
    }
}
