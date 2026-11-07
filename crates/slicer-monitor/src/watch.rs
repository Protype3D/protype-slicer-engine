//! Deciding, from a sequence of polls, whether anything is wrong.
//!
//! This is the whole of §7 that can be got wrong quietly, so it is
//! deliberately free of I/O: it takes snapshots and console lines and a
//! clock reading, and returns events. Every case below is driven by
//! synthetic polls in the tests, including the ones that need a machine
//! to have been stuck for twenty minutes.
//!
//! # Why the stall detector is shaped the way it is
//!
//! The obvious detector watches `print_duration` and complains when it
//! stops moving. That one raised a false alarm thirteen minutes into a
//! healthy print: `print_duration` starts running during the purge, and
//! before the first layer there is still filament loading, `Z_TILT_ADJUST`
//! and a 9x9 bed mesh to get through.
//!
//! So idle time is measured from the **layer counter**, and only counted
//! once that counter has actually moved. Which leaves a hole the original
//! script had too: on G-code that was never post-processed the counter
//! never moves at all, so the detector is silently disabled for exactly
//! the print it was meant to watch. That case is reported as its own
//! outcome — the fix is different (post-process the file) and saying
//! "stalled" would send someone to look at the machine.

use serde::{Deserialize, Serialize};

use crate::console::{find_vendor_code, ConsoleLine, VendorCode};
use crate::snapshot::Snapshot;

/// Idle on a counter that has moved before now counts as stuck.
pub const DEFAULT_STALL_AFTER_S: f64 = 900.0;

/// How long a start block is allowed to run before never having reached
/// layer 1 becomes a finding. Generous against the ~13 minutes a Z-tilt
/// plus a full 9x9 mesh actually takes.
pub const DEFAULT_START_GRACE_S: f64 = 1500.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Failed {
        message: String,
    },
    Cancelled,
    Paused,
    /// The layer counter moved and then stopped.
    Stalled {
        idle_s: f64,
        at_layer: u32,
    },
    /// The start block ran, announced its total, and the current-layer
    /// counter never moved off zero. Almost always means the file was not
    /// post-processed, which makes it unwatchable rather than broken.
    LayerCounterNeverMoved {
        waited_s: f64,
    },
    /// A layer the caller asked to stop at.
    ReachedLayer {
        layer: u32,
    },
    Unreachable {
        detail: String,
    },
}

impl Outcome {
    /// Exit status when driven as a watchdog. Zero only for the two ways
    /// a watch ends without anything to report.
    pub fn exit_code(&self) -> i32 {
        match self {
            Outcome::Completed | Outcome::ReachedLayer { .. } => 0,
            Outcome::Failed { .. } => 10,
            Outcome::Cancelled => 11,
            Outcome::Paused => 12,
            Outcome::Unreachable { .. } => 14,
            Outcome::Stalled { .. } => 15,
            Outcome::LayerCounterNeverMoved { .. } => 16,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    StateChanged {
        from: Option<String>,
        to: String,
        message: String,
    },
    LayerChanged {
        layer: u32,
        total: Option<u32>,
        progress_pct: f64,
    },
    Console {
        line: ConsoleLine,
        #[serde(skip_serializing_if = "Option::is_none")]
        vendor_code: Option<VendorCode>,
    },
    /// Something worth saying that is not a state change — a telemetry
    /// field that cannot be believed, for instance.
    Note {
        text: String,
    },
    Finished {
        outcome: Outcome,
    },
}

#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub stall_after_s: f64,
    pub start_grace_s: f64,
    /// Stop once the print reaches this layer. `None` = watch to the end.
    pub until_layer: Option<u32>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            stall_after_s: DEFAULT_STALL_AFTER_S,
            start_grace_s: DEFAULT_START_GRACE_S,
            until_layer: None,
        }
    }
}

#[derive(Debug)]
pub struct Watch {
    config: WatchConfig,
    previous_state: Option<String>,
    previous_layer: Option<u32>,
    /// When the layer counter last changed, or when running began.
    last_progress_at: Option<f64>,
    /// Whether the counter has ever moved. Until it has, idle time means
    /// nothing.
    counter_has_moved: bool,
    warned_about_sensor: bool,
    finished: Option<Outcome>,
}

impl Watch {
    pub fn new(config: WatchConfig) -> Self {
        Self {
            config,
            previous_state: None,
            previous_layer: None,
            last_progress_at: None,
            counter_has_moved: false,
            warned_about_sensor: false,
            finished: None,
        }
    }

    pub fn outcome(&self) -> Option<&Outcome> {
        self.finished.as_ref()
    }

    pub fn is_finished(&self) -> bool {
        self.finished.is_some()
    }

    /// Fold one poll in. `at` is seconds on any steadily increasing
    /// clock; the tests supply their own so a twenty-minute stall costs
    /// nothing to exercise.
    pub fn observe(&mut self, at: f64, snapshot: &Snapshot, console: &[ConsoleLine]) -> Vec<Event> {
        let mut events = Vec::new();
        if self.finished.is_some() {
            return events;
        }

        for line in console {
            events.push(Event::Console {
                vendor_code: find_vendor_code(&line.message),
                line: line.clone(),
            });
        }

        if self.previous_state.as_deref() != Some(snapshot.state.as_str()) {
            events.push(Event::StateChanged {
                from: self.previous_state.clone(),
                to: snapshot.state.clone(),
                message: snapshot.message.clone(),
            });
            self.previous_state = Some(snapshot.state.clone());
            if snapshot.is_running() && self.last_progress_at.is_none() {
                self.last_progress_at = Some(at);
            }
        }

        // Said once, and only while it could mislead someone.
        if !self.warned_about_sensor
            && snapshot.filament_sensor_enabled == Some(false)
            && snapshot.filament_detected.is_some()
        {
            self.warned_about_sensor = true;
            events.push(Event::Note {
                text: "filament_detected is being reported by a disabled sensor and means \
                       nothing; the real checks are the camera and a person"
                    .to_string(),
            });
        }

        if let Some(layer) = snapshot.layer {
            if self.previous_layer != Some(layer) {
                if self.previous_layer.is_some() && layer > 0 {
                    self.counter_has_moved = true;
                }
                self.previous_layer = Some(layer);
                self.last_progress_at = Some(at);
                events.push(Event::LayerChanged {
                    layer,
                    total: snapshot.total_layer,
                    progress_pct: snapshot.progress_pct,
                });
            }
        }

        if let Some(outcome) = self.verdict(at, snapshot) {
            self.finished = Some(outcome.clone());
            events.push(Event::Finished { outcome });
        }
        events
    }

    /// Record that the machine could not be reached, and stop.
    pub fn unreachable(&mut self, detail: impl Into<String>) -> Vec<Event> {
        if self.finished.is_some() {
            return Vec::new();
        }
        let outcome = Outcome::Unreachable {
            detail: detail.into(),
        };
        self.finished = Some(outcome.clone());
        vec![Event::Finished { outcome }]
    }

    fn verdict(&self, at: f64, snapshot: &Snapshot) -> Option<Outcome> {
        match snapshot.state.as_str() {
            "complete" => return Some(Outcome::Completed),
            "error" => {
                return Some(Outcome::Failed {
                    // Usually empty for vendor errors. The console lines
                    // already recorded alongside are where the reason is.
                    message: snapshot.message.clone(),
                });
            }
            "cancelled" => return Some(Outcome::Cancelled),
            "paused" => return Some(Outcome::Paused),
            _ => {}
        }

        if let (Some(target), Some(layer)) = (self.config.until_layer, snapshot.layer) {
            if layer >= target {
                return Some(Outcome::ReachedLayer { layer });
            }
        }

        if !snapshot.is_running() {
            return None;
        }
        let since = at - self.last_progress_at?;

        if self.counter_has_moved {
            if since > self.config.stall_after_s {
                return Some(Outcome::Stalled {
                    idle_s: since,
                    at_layer: snapshot.layer.unwrap_or(0),
                });
            }
            return None;
        }

        // The counter has never moved. That is normal for as long as the
        // start block is running, and a finding once it cannot be.
        if snapshot.start_block_has_run() && since > self.config.start_grace_s {
            return Some(Outcome::LayerCounterNeverMoved { waited_s: since });
        }
        None
    }
}
