use thiserror::Error;

#[derive(Debug, Error)]
pub enum GcodeError {
    #[error(
        "no `;LAYER_CHANGE` markers found — this does not look like sliced G-code, and \
         post-processing it would produce something that only appears to work"
    )]
    NoLayerMarkers,

    #[error("asked to resume at layer {requested}, but the file has {found}")]
    LayerOutOfRange { requested: usize, found: usize },

    #[error("layer {layer} carries no `;Z:` marker, so its height is unknown")]
    NoLayerHeight { layer: usize },

    #[error("layer {layer} contains no XY move to approach")]
    NoApproachPoint { layer: usize },

    #[error(
        "nothing before layer {layer} declares `M82` or `M83`, so whether this file's `E` values \
         are positions or increments is unknown — resuming it would pick one, and picking wrong \
         turns the first move into a request for the whole running total"
    )]
    UnknownExtrusionMode { layer: usize },

    #[error("machine {machine} has no tool {tool:?}; it knows {known}")]
    UnknownTool {
        machine: String,
        tool: String,
        known: String,
    },

    #[error(
        "the passport lists no homing command that leaves Z alone, so there is no way to \
         re-establish position over a populated bed"
    )]
    NoSafeHoming,

    #[error(
        "resuming would drive the nozzle through something already on the bed:\n{}",
        .report
    )]
    WouldCollide { report: String },

    #[error(
        "a travel height of {safe_z_mm} mm does not clear {part}, which stands {top_z} mm tall \
         (margin {margin_mm} mm) — the approach would cross the part on the way in"
    )]
    TravelTooLow {
        safe_z_mm: f64,
        part: String,
        top_z: f64,
        margin_mm: f64,
    },

    #[error("line {line}: unreadable paint fan marker `{text}`")]
    PaintFanMarkerMalformed { line: usize, text: String },

    #[error("line {line}: paint fan zone {problem}")]
    PaintFanUnbalanced { line: usize, problem: &'static str },

    #[error("line {line}: cannot read `{word}` in a motion or fan command")]
    UnreadableWord { line: usize, word: String },

    #[error("cannot read the G-code: {message}")]
    Unreadable { message: String },

    #[error("fan lead {name} must be between 0 and {max} s, got {value}")]
    InvalidFanLead {
        name: &'static str,
        value: f64,
        max: f64,
    },
}
