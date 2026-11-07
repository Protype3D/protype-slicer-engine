//! Change the part fan speed before a fan zone, not at it.
//!
//! The engine writes `; PAINT_FAN S<pct>` where the nozzle enters a fan zone —
//! painted on a part or set on a height range — and `; PAINT_FAN END` where it
//! leaves, each followed by the fan command when the speed changes there. A
//! fan needs time to reach a new speed — on Fab 2 the median painted window
//! was 0.11 s against a 0.3 s kick-start — so a command issued at the zone
//! edge lands after the zone.
//! This pass moves each entry command earlier by the time the fan needs:
//! `spinup_s × ΔS` when the speed rises, `spindown_s × ΔS` when it falls, ΔS
//! being the change as a fraction of full speed.
//!
//! Time is the sum of move lengths over feed rates plus dwells, the same
//! approximation `FanMover` uses; acceleration is ignored. The command goes
//! before the move during which the fan must start, so the lead is never
//! shorter than asked and no move is split.
//!
//! The lead never crosses a tool change — on an IDEX printer `M106` belongs
//! to the active tool, so a command carried over `T<n>` would blow the other
//! nozzle's fan. It never reaches before the first `;LAYER_CHANGE` either,
//! and never enters the previous zone: an entry whose lead would reach into
//! it stops at that zone's exit instead, taking the place of the exit's own
//! fan command, or standing right after `; PAINT_FAN END` when the exit has
//! none, so the fan does not dip between two close zones. Automatic fan commands inside the lead
//! window are dropped — the painted speed is exact. An exit command is never
//! moved, only ever given up to such an entry.
//!
//! Real files reach 280 MB (10 M lines), so the file is streamed twice: the
//! first pass keeps only the moves of the last `max(spinup_s, spindown_s)`
//! seconds — never more than `MAX_FAN_LEAD_S` — and a map of edits, the
//! second copies the lines through that map.

mod line;
mod planner;
mod timeline;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};

use crate::error::GcodeError;

/// The comment the engine writes at a painted zone's entry and exit.
pub const PAINT_FAN_MARKER: &str = "; PAINT_FAN";

/// Every diagnostic this pass writes into the G-code starts with this.
pub const DIAGNOSTIC_PREFIX: &str = "; fan_lead:";

/// Upper bound of either time; the printer settings share it.
pub const MAX_FAN_LEAD_S: f64 = 10.0;

/// Which line ending a G-code file uses: `\r\n` when any of its lines does,
/// `\n` otherwise (a file mixing the two is already mixed). A diagnostic is
/// written with it, so a CRLF file does not end in one bare `\n`. The same
/// rule both ways round: [`LineTerminator::of_file`] for a file in memory,
/// [`LineTerminator::saw_line`] for one being streamed line by line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineTerminator {
    crlf: bool,
}

impl LineTerminator {
    pub fn of_file(text: &str) -> Self {
        Self {
            crlf: text.contains("\r\n"),
        }
    }

    /// One line as read, its own terminator included.
    pub fn saw_line(&mut self, line: &[u8]) {
        self.crlf |= line.ends_with(b"\r\n");
    }

    pub fn as_str(self) -> &'static str {
        if self.crlf {
            "\r\n"
        } else {
            "\n"
        }
    }

    pub fn as_bytes(self) -> &'static [u8] {
        self.as_str().as_bytes()
    }
}

/// How long the part fan takes to change speed, per full swing.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FanLeadParams {
    /// Seconds from standstill to full speed. 0 disables the lead on rises.
    pub spinup_s: f64,
    /// Seconds from full speed to standstill. 0 disables the lead on falls.
    pub spindown_s: f64,
}

impl Default for FanLeadParams {
    fn default() -> Self {
        Self {
            spinup_s: 1.0,
            spindown_s: 2.0,
        }
    }
}

impl FanLeadParams {
    pub fn validate(&self) -> Result<(), GcodeError> {
        for (name, value) in [("spinup_s", self.spinup_s), ("spindown_s", self.spindown_s)] {
            if !(value.is_finite() && (0.0..=MAX_FAN_LEAD_S).contains(&value)) {
                return Err(GcodeError::InvalidFanLead {
                    name,
                    value,
                    max: MAX_FAN_LEAD_S,
                });
            }
        }
        Ok(())
    }
}

pub fn applied_diagnostic(shifted: usize, merged: usize) -> String {
    format!("{DIAGNOSTIC_PREFIX} applied shifted={shifted} merged={merged}")
}

pub fn skipped_diagnostic(reason: &str) -> String {
    format!("{DIAGNOSTIC_PREFIX} skipped ({reason})")
}

/// What the first pass decided, applied by [`FanLeadPlan::write`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FanLeadPlan {
    /// `; PAINT_FAN` lines seen. Zero means the file must not be touched.
    pub markers: usize,
    /// Entry commands moved earlier than their marker.
    pub shifted: usize,
    /// Of those, the ones that replaced the previous zone's exit command.
    pub merged: usize,
    edits: BTreeMap<usize, LineEdit>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct LineEdit {
    /// Raw line, terminator included, written before this line.
    insert_before: Option<Vec<u8>>,
    remove: bool,
}

/// First pass: read the whole G-code once and decide every edit.
///
/// A file without `; PAINT_FAN` lines always comes back as an empty plan,
/// whatever else is wrong with it — the pass has no business there.
pub fn plan<R: BufRead>(mut reader: R, params: FanLeadParams) -> Result<FanLeadPlan, GcodeError> {
    let mut planner = planner::Planner::new(params);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = reader
            .read_until(b'\n', &mut buffer)
            .map_err(|e| GcodeError::Unreadable {
                message: e.to_string(),
            })?;
        if read == 0 {
            break;
        }
        planner.feed(&buffer);
    }
    planner.finish()
}

impl FanLeadPlan {
    /// Second pass: copy `reader` to `writer` through the edits, then append
    /// the `applied` diagnostic, ended the way the file ends its own lines.
    pub fn write<R: BufRead, W: Write>(&self, mut reader: R, mut writer: W) -> std::io::Result<()> {
        let mut edits = self.edits.iter().peekable();
        let mut buffer = Vec::new();
        let mut index = 0usize;
        let mut ends_with_newline = true;
        let mut terminator = LineTerminator::default();
        loop {
            buffer.clear();
            if reader.read_until(b'\n', &mut buffer)? == 0 {
                break;
            }
            terminator.saw_line(&buffer);
            let mut remove = false;
            if let Some((_, edit)) = edits.next_if(|(line, _)| **line == index) {
                if let Some(text) = &edit.insert_before {
                    writer.write_all(text)?;
                    ends_with_newline = true;
                }
                remove = edit.remove;
            }
            if !remove {
                writer.write_all(&buffer)?;
                ends_with_newline = buffer.ends_with(b"\n");
            }
            index += 1;
        }
        if !ends_with_newline {
            writer.write_all(terminator.as_bytes())?;
        }
        writer.write_all(applied_diagnostic(self.shifted, self.merged).as_bytes())?;
        writer.write_all(terminator.as_bytes())?;
        writer.flush()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FanLeadOutcome {
    /// No `; PAINT_FAN` line: the input is the output, byte for byte.
    Unchanged,
    Applied {
        gcode: String,
        shifted: usize,
        merged: usize,
    },
    /// The file could not be read as G-code; it is returned unchanged but
    /// for the diagnostic line saying why.
    Skipped { gcode: String, reason: String },
}

/// Both passes over an in-memory file. The daemon streams a file on disk
/// instead (`slicer-rpc`); this is the same decision for callers that hold
/// the text already.
pub fn apply_fan_lead(gcode: &str, params: FanLeadParams) -> FanLeadOutcome {
    let plan = match plan(gcode.as_bytes(), params) {
        Ok(plan) => plan,
        Err(error) => return skipped(gcode, error.to_string()),
    };
    if plan.markers == 0 {
        return FanLeadOutcome::Unchanged;
    }
    let mut out = Vec::with_capacity(gcode.len() + 64);
    if let Err(error) = plan.write(gcode.as_bytes(), &mut out) {
        // Writing into a Vec does not fail; if it ever did, the file says why
        // rather than carrying half an edit.
        return skipped(gcode, error.to_string());
    }
    FanLeadOutcome::Applied {
        gcode: String::from_utf8_lossy(&out).into_owned(),
        shifted: plan.shifted,
        merged: plan.merged,
    }
}

fn skipped(gcode: &str, reason: String) -> FanLeadOutcome {
    let terminator = LineTerminator::of_file(gcode).as_str();
    let mut out = String::with_capacity(gcode.len() + reason.len() + 32);
    out.push_str(gcode);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(terminator);
    }
    out.push_str(&skipped_diagnostic(&reason));
    out.push_str(terminator);
    FanLeadOutcome::Skipped { gcode: out, reason }
}
