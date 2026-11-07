//! Machine passports: the behaviour of a physical printer, written down.
//!
//! A slicer knows about geometry and material. It does not know that this
//! particular machine probes Z at the centre of the bed, that its vendor
//! start block crosses that centre twice at 10 mm, that removing one
//! seemingly-inert line makes it feed filament from last week's slot, or
//! that its filament sensor reports `true` while switched off. Every one
//! of those cost a print.
//!
//! None of it is derivable from the model, and all of it is knowable in
//! advance. This crate is where it gets written down, in a form arithmetic
//! can be done on:
//!
//! * [`passport`] — the declarative facts, each carrying the observation
//!   that establishes it.
//! * [`start_block`] — derives what the vendor G-code actually does,
//!   rather than trusting a reading of it.
//! * [`hazard`] — the collision arithmetic those two exist to feed.
//!
//! ```
//! use slicer_machine::{bundled, hazard::{self, Part}};
//!
//! let passport = bundled("qidi-xmax4").expect("bundled passport");
//! let part = Part::from_bounds("holder", [100.0, 100.0, 0.0], [250.0, 250.0, 18.0]);
//!
//! // A part sitting over the centre of the bed makes homing unsafe.
//! let probing = hazard::assess_probe(passport.z_probe.point, &[part]);
//! assert!(!probing.is_empty());
//! ```

pub mod error;
pub mod hazard;
pub mod passport;
pub mod probe;
pub mod start_block;
pub mod validate;

pub use error::PassportError;
pub use passport::{MachinePassport, SCHEMA_VERSION};
pub use validate::validate;

/// Passports shipped inside the binary.
///
/// Embedded rather than read from disk so the daemon can answer a
/// collision question with no filesystem state, and so the tests that
/// lock these against the vendor profiles cannot be fooled by a stale
/// copy sitting in a working directory.
const BUNDLED: &[(&str, &str)] = &[(
    "qidi-xmax4",
    include_str!("../../../machines/qidi-xmax4.json"),
)];

/// Combined footprint of everything on the bed.
pub fn bounding_box(parts: &[hazard::Part]) -> Option<(passport::Point2, passport::Point2)> {
    let first = parts.first()?;
    let mut lo = first.min;
    let mut hi = first.max;
    for part in &parts[1..] {
        lo.x = lo.x.min(part.min.x);
        lo.y = lo.y.min(part.min.y);
        hi.x = hi.x.max(part.max.x);
        hi.y = hi.y.max(part.max.y);
    }
    Some((lo, hi))
}

/// Machine ids this build carries a passport for.
pub fn bundled_ids() -> Vec<&'static str> {
    BUNDLED.iter().map(|(id, _)| *id).collect()
}

/// Load a bundled passport by machine id, validating it on the way out.
pub fn bundled(machine_id: &str) -> Result<MachinePassport, PassportError> {
    let (_, raw) = BUNDLED
        .iter()
        .find(|(id, _)| *id == machine_id)
        .ok_or_else(|| PassportError::UnknownMachine(machine_id.to_string()))?;
    from_json(raw)
}

/// Parse and validate a passport from JSON text.
pub fn from_json(raw: &str) -> Result<MachinePassport, PassportError> {
    let passport: MachinePassport = serde_json::from_str(raw)?;
    validate(&passport)?;
    Ok(passport)
}

/// Read, parse and validate a passport from disk.
pub fn from_path(path: impl AsRef<std::path::Path>) -> Result<MachinePassport, PassportError> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path).map_err(|source| PassportError::Io {
        path: path.display().to_string(),
        source,
    })?;
    from_json(&raw)
}

impl MachinePassport {
    /// Every hazard the vendor start block poses to the given parts,
    /// including homing.
    ///
    /// This is the call that answers "is it safe to start a print with
    /// these already on the bed?". An empty result is the only safe
    /// answer; a result containing only [`hazard::Severity::Unknown`]
    /// entries means the question could not be settled, which is not the
    /// same thing as yes.
    pub fn start_block_hazards(
        &self,
        parts: &[hazard::Part],
        margin_mm: f64,
    ) -> Vec<hazard::Hazard> {
        // The print area defaults to the footprint of what is already on
        // the bed, which is the resume case: the job reprints the part
        // standing there, so an adaptive bed mesh fits itself to exactly
        // that box. Use `hazards_for` when the job prints something else.
        self.hazards_for(parts, bounding_box(parts), margin_mm)
    }

    /// Every hazard, with the print area stated rather than inferred.
    pub fn hazards_for(
        &self,
        parts: &[hazard::Part],
        print_area: Option<(passport::Point2, passport::Point2)>,
        margin_mm: f64,
    ) -> Vec<hazard::Hazard> {
        let mut out = hazard::assess_probe(self.z_probe.point, parts);
        out.extend(hazard::assess_probe_operations(
            &self.probe_operations,
            parts,
            print_area,
        ));
        out.extend(hazard::assess(
            &self.start_block.traverses,
            &self.start_block.unmodelled,
            parts,
            margin_mm,
        ));
        out
    }
}
