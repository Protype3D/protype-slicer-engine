//! Refuse a passport that cannot be reasoned from.
//!
//! A passport is load-bearing for collision arithmetic, so the failure
//! mode we care about is not "malformed JSON" — serde catches that — but
//! a well-formed passport that quietly says something impossible: a
//! probe point off the bed, a zero-height build volume, a mandatory line
//! asserted without the observation behind it. Each of those would turn
//! into a wrong answer somewhere downstream instead of an error here.

use crate::error::PassportError;
use crate::passport::{MachinePassport, Point2, SCHEMA_VERSION};

pub fn validate(p: &MachinePassport) -> Result<(), PassportError> {
    if p.schema_version != SCHEMA_VERSION {
        return Err(PassportError::SchemaVersion {
            found: p.schema_version,
            supported: SCHEMA_VERSION,
        });
    }

    if p.machine_id.is_empty()
        || !p
            .machine_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(PassportError::MachineId(p.machine_id.clone()));
    }

    for (field, value) in [("bed.x", p.bed.x), ("bed.y", p.bed.y), ("bed.z", p.bed.z)] {
        if !value.is_finite() || value <= 0.0 {
            return Err(PassportError::NotPositive {
                field: field.to_string(),
                value,
            });
        }
    }

    require_on_bed(p, "z_probe.point", p.z_probe.point)?;
    require_on_bed(
        p,
        "z_probe.leaves_toolhead_at",
        p.z_probe.leaves_toolhead_at.xy(),
    )?;

    if !p.z_probe.leaves_toolhead_at.z.is_finite() {
        return Err(PassportError::NotPositive {
            field: "z_probe.leaves_toolhead_at.z".to_string(),
            value: p.z_probe.leaves_toolhead_at.z,
        });
    }

    if p.z_probe.derivation.trim().is_empty() {
        return Err(PassportError::UnprovenMandatoryLine {
            line: "z_probe.derivation".to_string(),
        });
    }

    for (command, line) in &p.start_block.mandatory_lines {
        // §8: "отсутствие в справке — не доказательство". A mandatory
        // line with no observable behind it is an opinion, and opinions
        // do not get to keep lines in a start block.
        if line.proof.trim().is_empty() {
            return Err(PassportError::UnprovenMandatoryLine {
                line: command.clone(),
            });
        }
    }

    for (tool, mapping) in &p.tools {
        if mapping.slot_var.trim().is_empty() {
            return Err(PassportError::EmptySlotVar { tool: tool.clone() });
        }
    }

    Ok(())
}

fn require_on_bed(p: &MachinePassport, what: &str, point: Point2) -> Result<(), PassportError> {
    if p.contains_xy(point) {
        return Ok(());
    }
    Err(PassportError::OffBed {
        what: what.to_string(),
        x: point.x,
        y: point.y,
        bed_x: p.bed.x,
        bed_y: p.bed.y,
    })
}
