use thiserror::Error;

/// Everything that can go wrong between a passport on disk and a
/// passport this crate is willing to answer questions from.
///
/// A passport is safety data — it is the thing that decides whether the
/// nozzle is about to be driven into a printed part — so every variant
/// names the offending value. "invalid passport" with no coordinates in
/// it would be worse than useless here.
#[derive(Debug, Error)]
pub enum PassportError {
    #[error("passport JSON is malformed: {0}")]
    Malformed(#[from] serde_json::Error),

    #[error("cannot read a passport from {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },

    #[error(
        "passport declares schema_version {found}, this build understands {supported} — \
         refusing to guess at the difference"
    )]
    SchemaVersion { found: u32, supported: u32 },

    #[error("machine_id must be a non-empty lowercase slug ([a-z0-9-]), got {0:?}")]
    MachineId(String),

    #[error("{field} must be a finite number greater than 0, got {value}")]
    NotPositive { field: String, value: f64 },

    #[error(
        "{what} sits at ({x}, {y}), outside the {bed_x} x {bed_y} mm bed — a passport that \
         points off the bed cannot be used to reason about collisions"
    )]
    OffBed {
        what: String,
        x: f64,
        y: f64,
        bed_x: f64,
        bed_y: f64,
    },

    #[error("tool {tool:?} maps to an empty slot variable")]
    EmptySlotVar { tool: String },

    #[error("mandatory line {line:?} is declared without the observable effect that proves it")]
    UnprovenMandatoryLine { line: String },

    #[error("no passport is bundled for machine {0:?}")]
    UnknownMachine(String),
}
