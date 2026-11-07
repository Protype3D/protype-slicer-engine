//! Post-processing as a pipeline step, not a hack over a finished file.
//!
//! §6 of the print-pipeline spec. Four passes, each answering a failure
//! that cost a print:
//!
//! * [`layers`] — the slicer emits `TOTAL_LAYER` and never the current
//!   one, so external monitoring is blind. A job that died at minute 51
//!   looked healthy for another half hour.
//! * [`slot`] — choose the filament slot by rewriting the two lines that
//!   decide it, and never by deleting one.
//! * [`resume`] — continue from layer K with a start block that does not
//!   drive through what is already on the bed, checked against the
//!   machine passport rather than asserted.
//! * [`fan_lead`] — a fan command issued at a painted zone's edge reaches
//!   speed after the zone is printed (0.11 s windows against a 0.3 s
//!   kick-start on Fab 2), so it is moved earlier by the fan's own spin-up
//!   or spin-down time.
//!
//! What ties them together is that none of them trusts the file: the
//! layer pass cross-checks its own numbering against the slicer's
//! independent `M73 L` counter, and the resume pass hands its output to
//! the same collision arithmetic the passport feeds and refuses a plan
//! that fails it.

pub mod error;
pub mod extrusion;
pub mod fan_lead;
pub mod layers;
pub mod resume;
pub mod slot;

pub use error::GcodeError;
pub use extrusion::{ExtrusionMode, ExtrusionState};
pub use fan_lead::{apply_fan_lead, FanLeadOutcome, FanLeadParams, FanLeadPlan};
pub use layers::{annotate_layers, LayerAnnotation};
pub use resume::{plan_resume, ResumePlan, ResumeRequest, ZReference};
pub use slot::{select_tool, SlotRewrite};
