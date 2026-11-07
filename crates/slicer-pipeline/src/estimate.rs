//! What a part will actually cost, from a real slice.
//!
//! §4. The rule is that the numbers come from slicing, never from
//! `volume x density`: on the part this was written for, the arithmetic
//! guess said 141 g and the slicer said 58.5 g — a factor of 2.4, and a
//! factor of 2.4 is the difference between "print it" and "redesign it".
//!
//! # Why this refuses to answer from the stub
//!
//! Built without `with-libslic3r`, the FFI bridge emits deterministic
//! rectangles instead of slicing. It does not fail — it returns a
//! plausible G-code file. An estimate taken from that is not a bad
//! estimate, it is a number about a rectangle, and the incident list has
//! a line for it. So the totals are only accepted from output that
//! carries the marks of real slicing, and the check is on the *output*
//! rather than on a build flag, because the build flag is exactly the
//! thing that goes wrong.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub grams: f64,
    pub seconds: f64,
    pub layers: u32,
    pub filament_mm: f64,
    pub filament_cm3: f64,
    /// The slicer's own words for the time, kept because rounding a
    /// duration into seconds and back loses the form a human recognises.
    pub printing_time: String,
}

impl Estimate {
    pub fn hours(&self) -> f64 {
        self.seconds / 3600.0
    }
}

#[derive(Debug, Error)]
pub enum EstimateError {
    #[error(
        "this G-code was not produced by real slicing — it has no layer changes and no \
         totals, which is what the `with-libslic3r`-less stub bridge emits. It does not fail \
         loudly; it returns plausible rectangles. Rebuild with `--features with-libslic3r`"
    )]
    NotRealSlicing,
    #[error("the G-code carries no `{field}` total, so this number cannot be reported")]
    MissingTotal { field: &'static str },
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("slicing failed: {0}")]
    Slice(#[from] crate::SliceError),
}

/// Marks that only appear in output from the real slicer.
fn looks_sliced(gcode: &str) -> bool {
    gcode.contains(";LAYER_CHANGE") || gcode.contains("; total layer number:")
}

/// Read the slicer's own totals out of the G-code it wrote.
pub fn parse_totals(gcode: &str) -> Result<Estimate, EstimateError> {
    if !looks_sliced(gcode) {
        return Err(EstimateError::NotRealSlicing);
    }

    let grams = number_after(gcode, "; total filament used [g] = ")
        .or_else(|| number_after(gcode, "; filament used [g] = "))
        .ok_or(EstimateError::MissingTotal { field: "grams" })?;
    let printing_time = text_after(gcode, "; estimated printing time (normal mode) = ").ok_or(
        EstimateError::MissingTotal {
            field: "estimated printing time",
        },
    )?;
    let layers = number_after(gcode, "; total layers count = ")
        .or_else(|| number_after(gcode, "; total layer number: "))
        .ok_or(EstimateError::MissingTotal { field: "layers" })? as u32;

    Ok(Estimate {
        grams,
        seconds: parse_duration(&printing_time).ok_or(EstimateError::MissingTotal {
            field: "estimated printing time",
        })?,
        layers,
        filament_mm: number_after(gcode, "; filament used [mm] = ").unwrap_or(0.0),
        filament_cm3: number_after(gcode, "; filament used [cm3] = ").unwrap_or(0.0),
        printing_time,
    })
}

/// Slice for real, then read the totals back out.
pub fn estimate(request: crate::SliceRequest) -> Result<Estimate, EstimateError> {
    let outcome = crate::run_slice_blocking(request)?;
    let gcode =
        std::fs::read_to_string(&outcome.gcode_path).map_err(|source| EstimateError::Io {
            path: outcome.gcode_path.clone(),
            source,
        })?;
    parse_totals(&gcode)
}

fn line_after<'a>(gcode: &'a str, prefix: &str) -> Option<&'a str> {
    gcode
        .lines()
        .find(|l| l.starts_with(prefix))
        .map(|l| l[prefix.len()..].trim())
}

fn text_after(gcode: &str, prefix: &str) -> Option<String> {
    line_after(gcode, prefix).map(|s| s.to_string())
}

fn number_after(gcode: &str, prefix: &str) -> Option<f64> {
    line_after(gcode, prefix)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// `2h 11m 30s`, `3m 49s`, `1d 2h`. Returns `None` when nothing parses,
/// rather than zero — a print of no duration is not a plausible answer
/// and would sail through a cost gate.
fn parse_duration(text: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut matched = false;
    for token in text.split_whitespace() {
        let (value, unit) = token.split_at(token.len().saturating_sub(1));
        let Ok(value) = value.parse::<f64>() else {
            continue;
        };
        let multiplier = match unit {
            "d" => 86400.0,
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            _ => continue,
        };
        total += value * multiplier;
        matched = true;
    }
    matched.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim tail of G-code from a real libslic3r slice.
    const REAL: &str = "\
; total layer number: 50
;LAYER_CHANGE
G1 X1 Y1 E1
; filament used [mm] = 261.14
; filament used [cm3] = 0.63
; filament used [g] = 0.79
; total filament used [g] = 0.79
; total filament cost = 0.02
; total layers count = 50
; estimated printing time (normal mode) = 3m 49s
";

    /// What the stub bridge emits: no layer changes, no totals.
    const STUB: &str = "\
; generated by the protype slicer stub
G28
G1 X0 Y0 F3000
G1 X100 Y0 E5
G1 X100 Y100 E5
";

    #[test]
    fn a_real_slice_yields_its_totals() {
        let e = parse_totals(REAL).expect("parses");
        assert!((e.grams - 0.79).abs() < 1e-9);
        assert_eq!(e.layers, 50);
        assert!((e.seconds - 229.0).abs() < 1e-9, "3m 49s is 229 s");
        assert!((e.filament_mm - 261.14).abs() < 1e-9);
        assert!((e.filament_cm3 - 0.63).abs() < 1e-9);
        assert_eq!(e.printing_time, "3m 49s");
    }

    /// The incident this guard exists for: the stub does not fail, it
    /// returns plausible rectangles.
    #[test]
    fn stub_output_is_refused_rather_than_measured() {
        let err = parse_totals(STUB).expect_err("the stub must not yield an estimate");
        assert!(matches!(err, EstimateError::NotRealSlicing));
        assert!(err.to_string().contains("with-libslic3r"));
    }

    #[test]
    fn sliced_output_missing_its_totals_names_what_is_missing() {
        let truncated = ";LAYER_CHANGE\nG1 X1 Y1 E1\n";
        let err = parse_totals(truncated).expect_err("no totals");
        assert!(matches!(
            err,
            EstimateError::MissingTotal { field: "grams" }
        ));
    }

    #[test]
    fn durations_parse_in_every_form_the_slicer_emits() {
        assert_eq!(parse_duration("3m 49s"), Some(229.0));
        assert_eq!(parse_duration("2h 11m 30s"), Some(7890.0));
        assert_eq!(parse_duration("1d 2h"), Some(93600.0));
        assert_eq!(parse_duration("45s"), Some(45.0));
    }

    /// An unparseable duration must not become zero: a print of no
    /// duration sails through any cost gate ever written.
    #[test]
    fn an_unparseable_duration_is_none_not_zero() {
        assert_eq!(parse_duration("unknown"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn hours_are_derived_from_the_seconds() {
        let e = parse_totals(REAL).unwrap();
        assert!((e.hours() - 229.0 / 3600.0).abs() < 1e-12);
    }

    /// The 8h30 print, as the number a gate would have caught.
    #[test]
    fn a_long_print_reports_its_hours() {
        let long = REAL.replace(
            "; estimated printing time (normal mode) = 3m 49s",
            "; estimated printing time (normal mode) = 8h 30m 12s",
        );
        let e = parse_totals(&long).unwrap();
        assert!((e.hours() - 8.5033).abs() < 1e-3, "got {}", e.hours());
    }
}
