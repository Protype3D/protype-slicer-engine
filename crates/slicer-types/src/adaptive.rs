//! Adaptive Print rules (spec §6.9). MVP 1 ships the data model + manual
//! rules editor; the live rule engine lives in ProtypeAgent (Phase 5
//! Track A) and the AI Phase adds a `Defect` trigger that drops into the
//! same schema without breaking older project files.

use crate::settings::SettingValue;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Numeric comparison operator for `Threshold` triggers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdOp {
    GreaterThan,
    LessThan,
    Equal,
}

/// What fires the rule. `ManualEvent` is the operator pressing a button
/// ("I see stringing"), `Threshold` is a printer metric crossing a value,
/// and `Defect` is the AI Phase vision-supervisor — added now so older
/// project files keep round-tripping after AI Phase ships.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum AdaptiveTrigger {
    ManualEvent {
        /// Free-form label shown in the UI ("Stringing visible").
        event_label: String,
    },
    Threshold {
        /// Metric name (e.g. "nozzle_temp", "fan_speed", "elapsed_seconds").
        metric: String,
        op: ThresholdOp,
        value: f64,
    },
    Defect {
        /// Defect kind from the AI Phase taxonomy
        /// (warping, stringing, layer_cracking, ...). Empty in MVP 1.
        kind: String,
        /// Minimum classifier confidence to fire.
        min_confidence: f32,
    },
}

/// Parameter to adjust on `AdjustParameter` actions. Mirrors the
/// `ParameterKind` enum the agent's `PrinterAdapter` exposes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdjustableParameter {
    NozzleTemp,
    BedTemp,
    ChamberTemp,
    FlowRate,
    PrintSpeed,
    FanSpeed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum AdaptiveAction {
    /// Bump a printer parameter. `delta` is added to the current live
    /// value (e.g. +5 °C on nozzle); use `set` flavor for absolute
    /// targets later.
    AdjustParameter {
        parameter: AdjustableParameter,
        delta: f64,
    },
    /// Push a notification to the operator (Slack / desktop / webhook).
    /// MVP 1: desktop only via the host bridge.
    NotifyOperator { message: String },
    /// Pause the active print job.
    PausePrint,
    /// Abort the active print job.
    AbortPrint,
    /// Run a Klipper / Marlin macro by name with optional args.
    ExecuteMacro {
        name: String,
        #[serde(default)]
        args: std::collections::HashMap<String, SettingValue>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AdaptiveRule {
    /// Stable id (ULID) — referenced by audit log + telemetry.
    pub id: String,
    /// Human-readable name shown in the editor.
    pub name: String,
    /// Disabled rules are persisted but skipped by the agent's engine.
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub trigger: AdaptiveTrigger,
    pub actions: Vec<AdaptiveAction>,
    /// Higher fires first when multiple rules match the same event.
    #[serde(default)]
    pub priority: i32,
}

fn default_true() -> bool {
    true
}
