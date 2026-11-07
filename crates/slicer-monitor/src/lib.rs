//! Watching a print, so that when it dies there is something to read.
//!
//! §7 of the print-pipeline spec. Written against a specific evening: a
//! print died at minute 51 and looked healthy for another half hour,
//! and its cause of death — `QDE_004_006 Extruder loading failure` — was
//! recovered by eye from a ring buffer minutes before it would have been
//! overwritten. `print_stats.message` was empty throughout.
//!
//! The decision logic ([`watch`]) does no I/O at all, so every case it
//! has to get right — including a machine stuck for twenty minutes — is
//! exercised by synthetic polls rather than by having a printer and the
//! patience. The HTTP transport is behind the `moonraker` feature and is
//! a thin adapter over it.
//!
//! ```
//! use slicer_monitor::{Snapshot, Watch, WatchConfig};
//!
//! let mut watch = Watch::new(WatchConfig::default());
//! let printing = Snapshot { state: "printing".into(), layer: Some(1), ..Default::default() };
//! watch.observe(0.0, &printing, &[]);
//!
//! let done = Snapshot { state: "complete".into(), ..printing.clone() };
//! watch.observe(1.0, &done, &[]);
//! assert!(watch.is_finished());
//! ```

pub mod console;
pub mod journal;
#[cfg(feature = "moonraker")]
pub mod moonraker;
pub mod snapshot;
pub mod watch;

pub use console::{find_vendor_code, ConsoleLine, VendorCode};
pub use journal::Journal;
pub use snapshot::Snapshot;
pub use watch::{Event, Outcome, Watch, WatchConfig};
