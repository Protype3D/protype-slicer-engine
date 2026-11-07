//! The console, and the vendor codes that only ever appear there.
//!
//! `print_stats.message` is empty for QIDI's own errors. The reason a
//! print stopped lives in Moonraker's `/server/gcode_store`, which is a
//! ring buffer that overwrites within minutes — so
//! `QDE_004_006 Extruder loading failure` was visible for a few minutes
//! and then gone, and was only recovered because someone happened to be
//! looking at the right moment.
//!
//! Two jobs here: strip the temperature stream so the ring buffer's real
//! content is not drowned, and name the codes.

use serde::{Deserialize, Serialize};

/// Prefix of QIDI's own error codes.
const VENDOR_PREFIX: &str = "QDE_";

/// What the codes mean.
///
/// The machine has no table for these — they come from a compiled module,
/// not from any config file — so this one is kept by hand and filled in
/// as codes are met. Deliberately not the only thing that fires: an
/// unknown `QDE_*` is still reported as a vendor error, with its raw code,
/// because a table that silently swallows what it does not recognise is
/// worse than no table.
const KNOWN: &[(&str, &str)] = &[(
    "QDE_004_006",
    "could not load filament into the extruder — an empty slot, filament that did not arrive, \
     or feeding started before the nozzle reached temperature",
)];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsoleLine {
    /// Moonraker's own timestamp, used to avoid re-reading lines.
    pub time: f64,
    pub kind: String,
    pub message: String,
}

impl ConsoleLine {
    /// Klipper prefixes real errors with `!!`.
    pub fn is_error(&self) -> bool {
        self.message.trim_start().starts_with("!!")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VendorCode {
    pub code: String,
    /// `None` for a code not in the table — which is a finding in itself,
    /// not a reason to stay quiet.
    pub meaning: Option<String>,
}

impl std::fmt::Display for VendorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.meaning {
            Some(meaning) => write!(f, "{} — {meaning}", self.code),
            None => write!(
                f,
                "{} — not in this build's table; record what it turned out to mean",
                self.code
            ),
        }
    }
}

/// Find a vendor code anywhere in a message.
pub fn find_vendor_code(message: &str) -> Option<VendorCode> {
    let at = message.find(VENDOR_PREFIX)?;
    let code: String = message[at..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let meaning = KNOWN
        .iter()
        .find(|(known, _)| *known == code)
        .map(|(_, meaning)| (*meaning).to_string());
    Some(VendorCode { code, meaning })
}

/// The periodic `B:70.0 /70.0 T0:246.8 /250.0` temperature report.
///
/// Dropped because it is the overwhelming majority of the ring buffer's
/// content, and keeping it means the buffer's few useful lines scroll out
/// of the window that much sooner.
pub fn is_temperature_report(message: &str) -> bool {
    let trimmed = message.trim_start();
    let Some(rest) = trimmed.strip_prefix("B:") else {
        return false;
    };
    // `B:<num> /<num>` then at least one `T<n>:<num> /<num>`.
    let mut chars = rest.chars().peekable();
    if !consume_reading(&mut chars) {
        return false;
    }
    while chars.peek().is_some_and(|c| c.is_whitespace()) {
        chars.next();
    }
    if chars.next() != Some('T') {
        return false;
    }
    while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
        chars.next();
    }
    if chars.next() != Some(':') {
        return false;
    }
    consume_reading(&mut chars)
}

/// `<number> /<number>`
fn consume_reading(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    if !consume_number(chars) {
        return false;
    }
    while chars.peek().is_some_and(|c| c.is_whitespace()) {
        chars.next();
    }
    if chars.next() != Some('/') {
        return false;
    }
    consume_number(chars)
}

fn consume_number(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    let mut seen = false;
    if chars.peek() == Some(&'-') {
        chars.next();
    }
    while chars
        .peek()
        .is_some_and(|c| c.is_ascii_digit() || *c == '.')
    {
        chars.next();
        seen = true;
    }
    seen
}

/// Keep only what is worth recording, newest-first bookkeeping left to
/// the caller.
pub fn worth_keeping(line: &ConsoleLine) -> bool {
    let message = line.message.trim();
    !message.is_empty() && !is_temperature_report(message)
}

/// Parse `/server/gcode_store`'s payload, dropping anything at or before
/// `seen_after` so a poll does not re-report what it already has.
pub fn parse_store(payload: &serde_json::Value, seen_after: f64) -> (Vec<ConsoleLine>, f64) {
    let mut newest = seen_after;
    let mut out = Vec::new();
    let entries = payload
        .get("result")
        .and_then(|r| r.get("gcode_store"))
        .and_then(|s| s.as_array());
    for entry in entries.into_iter().flatten() {
        let time = entry.get("time").and_then(|t| t.as_f64()).unwrap_or(0.0);
        if time <= seen_after {
            continue;
        }
        newest = newest.max(time);
        let line = ConsoleLine {
            time,
            kind: entry
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            message: entry
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .trim()
                .to_string(),
        };
        if worth_keeping(&line) {
            out.push(line);
        }
    }
    (out, newest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from the machine's `/server/gcode_store`.
    fn real_store() -> serde_json::Value {
        serde_json::json!({"result": {"gcode_store": [
            {"message": "B:70.0 /70.0 T0:246.8 /250.0", "time": 100.0, "type": "response"},
            {"message": "// 1B has been sync with Extruder.", "time": 101.0, "type": "response"},
            {"message": "// probe at 195.000,0.999 is z=0.109531", "time": 102.0, "type": "response"},
            {"message": "M220 S100", "time": 103.0, "type": "command"},
            {"message": "!! QDE_004_006 Extruder loading failure", "time": 104.0, "type": "response"}
        ]}})
    }

    #[test]
    fn the_temperature_stream_is_dropped() {
        let (lines, _) = parse_store(&real_store(), 0.0);
        assert!(
            !lines.iter().any(|l| l.message.starts_with("B:")),
            "temperature reports must not survive: {lines:?}"
        );
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn everything_that_is_not_temperature_survives() {
        let (lines, _) = parse_store(&real_store(), 0.0);
        assert!(lines.iter().any(|l| l.message.contains("probe at")));
        assert!(lines.iter().any(|l| l.message == "M220 S100"));
    }

    #[test]
    fn already_seen_lines_are_not_reported_twice() {
        let (first, newest) = parse_store(&real_store(), 0.0);
        assert!(!first.is_empty());
        let (again, _) = parse_store(&real_store(), newest);
        assert!(again.is_empty(), "got {again:?}");
    }

    #[test]
    fn the_known_vendor_code_is_named() {
        let (lines, _) = parse_store(&real_store(), 0.0);
        let failure = lines.iter().find(|l| l.is_error()).expect("an error line");
        let code = find_vendor_code(&failure.message).expect("a vendor code");
        assert_eq!(code.code, "QDE_004_006");
        assert!(code.to_string().contains("could not load filament"));
    }

    /// A table that swallows what it does not recognise would be worse
    /// than no table.
    #[test]
    fn an_unknown_vendor_code_is_still_reported_with_its_raw_code() {
        let code = find_vendor_code("!! QDE_999_001 Something new").expect("a vendor code");
        assert_eq!(code.code, "QDE_999_001");
        assert_eq!(code.meaning, None);
        assert!(code.to_string().contains("not in this build's table"));
    }

    #[test]
    fn an_ordinary_message_carries_no_vendor_code() {
        assert!(find_vendor_code("// Result is z=0.109531").is_none());
    }

    #[test]
    fn a_negative_bed_temperature_still_reads_as_the_temperature_report() {
        assert!(is_temperature_report("B:-1.0 /0.0 T0:25.0 /0.0"));
    }

    #[test]
    fn a_message_merely_starting_with_b_is_not_a_temperature_report() {
        assert!(!is_temperature_report("B: the bed is fine"));
        assert!(!is_temperature_report("BOX_PRINT_START EXTRUDER=0"));
    }

    #[test]
    fn errors_are_told_apart_from_responses() {
        let error = ConsoleLine {
            time: 1.0,
            kind: "response".into(),
            message: "!! QDE_004_006 Extruder loading failure".into(),
        };
        let chatter = ConsoleLine {
            time: 1.0,
            kind: "response".into(),
            message: "// Result is z=0.1".into(),
        };
        assert!(error.is_error());
        assert!(!chatter.is_error());
    }
}
