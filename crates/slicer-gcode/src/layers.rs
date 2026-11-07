//! Emit the layer number the slicer does not.
//!
//! A file that sets `SET_PRINT_STATS_INFO TOTAL_LAYER=<n>` in the start
//! block and never mentions the *current* layer again leaves Klipper's
//! `print_stats.info.current_layer` at `0` for the whole print, and
//! external monitoring is blind: a job that died at minute 51 looked
//! healthy for another half hour.
//!
//! Confirmed on the live machine while writing this — mid-print, it
//! reported `total_layer: 200, current_layer: 0`.
//!
//! Our own slices write the line themselves now (the bridge adds it to
//! `layer_change_gcode`). This pass is for G-code from other slicers and
//! from builds before that; on a file that reports its layers it changes
//! nothing.

use crate::error::GcodeError;
use serde::{Deserialize, Serialize};

/// The marker OrcaSlicer writes at every layer transition.
pub const LAYER_CHANGE_MARKER: &str = ";LAYER_CHANGE";

/// What gets inserted. Klipper's own command, so no macro is needed on
/// the machine.
pub const CURRENT_LAYER_COMMAND: &str = "SET_PRINT_STATS_INFO CURRENT_LAYER=";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerAnnotation {
    pub gcode: String,
    /// Layer transitions found.
    pub layers: usize,
    pub inserted: usize,
    /// Transitions that already carried the command, left alone so the
    /// pass can be run twice without doubling up.
    pub already_present: usize,
    /// Places where our count disagreed with the slicer's own `M73 L`
    /// progress marker. Empty is the expected result; a non-empty list
    /// means one of the two is wrong and neither should be trusted until
    /// it is resolved.
    pub disagreements: Vec<String>,
}

/// Insert `SET_PRINT_STATS_INFO CURRENT_LAYER=n` at every layer change.
///
/// Numbering is 1-based and cross-checked against `M73 L<n>`, which
/// OrcaSlicer emits independently from its own layer counter. Agreeing
/// with a second signal in the same file is worth more than agreeing
/// with our own arithmetic.
pub fn annotate_layers(gcode: &str) -> Result<LayerAnnotation, GcodeError> {
    let lines: Vec<&str> = gcode.lines().collect();
    if !lines.iter().any(|l| l.trim() == LAYER_CHANGE_MARKER) {
        return Err(GcodeError::NoLayerMarkers);
    }

    let mut out = String::with_capacity(gcode.len() + gcode.len() / 20);
    let mut layer = 0usize;
    let mut inserted = 0usize;
    let mut already_present = 0usize;
    let mut disagreements = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        out.push_str(line);
        out.push('\n');

        if line.trim() != LAYER_CHANGE_MARKER {
            continue;
        }
        layer += 1;

        if layer_reports_itself(&lines, index + 1) {
            already_present += 1;
            continue;
        }
        out.push_str(CURRENT_LAYER_COMMAND);
        out.push_str(&layer.to_string());
        out.push('\n');
        inserted += 1;

        if let Some(theirs) = slicer_layer_number(&lines, index + 1) {
            if theirs != layer {
                disagreements.push(format!(
                    "line {}: counted layer {layer}, the slicer's own `M73 L{theirs}` says {theirs}",
                    index + 1
                ));
            }
        }
    }

    Ok(LayerAnnotation {
        gcode: out,
        layers: layer,
        inserted,
        already_present,
        disagreements,
    })
}

/// Whether the layer starting at `from` already carries the command. This
/// pass puts it right after the marker; a slice puts it in
/// `layer_change_gcode`, below the retract and the Z move.
fn layer_reports_itself(lines: &[&str], from: usize) -> bool {
    lines[from..]
        .iter()
        .take_while(|l| l.trim() != LAYER_CHANGE_MARKER)
        .any(|l| l.trim().starts_with(CURRENT_LAYER_COMMAND))
}

/// The `M73 L<n>` OrcaSlicer emits shortly after each layer change, if it
/// is within the next few lines. Bounded so a scan does not wander into
/// the following layer and compare against the wrong number.
fn slicer_layer_number(lines: &[&str], from: usize) -> Option<usize> {
    const LOOKAHEAD: usize = 25;
    lines
        .iter()
        .skip(from)
        .take(LOOKAHEAD)
        .take_while(|l| l.trim() != LAYER_CHANGE_MARKER)
        .find_map(|l| {
            l.trim()
                .strip_prefix("M73 L")
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse().ok())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like real OrcaSlicer output, including the `M73 L` the
    /// cross-check reads.
    fn sliced(layers: usize) -> String {
        let mut out = String::from("; generated\nM73 P0 R3\nG28\n");
        for n in 1..=layers {
            out.push_str(&format!(
                ";LAYER_CHANGE\n;Z:{:.1}\n;HEIGHT:0.2\nG1 E-.2 F1800\nM73 L{n}\nG1 X10 Y10 E.5\n",
                n as f64 * 0.2
            ));
        }
        out
    }

    #[test]
    fn every_layer_change_gets_a_numbered_command() {
        let result = annotate_layers(&sliced(3)).expect("annotates");
        assert_eq!(result.layers, 3);
        assert_eq!(result.inserted, 3);
        for n in 1..=3 {
            assert!(
                result.gcode.contains(&format!("CURRENT_LAYER={n}\n")),
                "layer {n} missing"
            );
        }
    }

    #[test]
    fn the_command_lands_immediately_after_its_marker() {
        let result = annotate_layers(&sliced(2)).unwrap();
        let lines: Vec<&str> = result.gcode.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.trim() == LAYER_CHANGE_MARKER)
            .expect("a marker");
        assert_eq!(lines[at + 1], "SET_PRINT_STATS_INFO CURRENT_LAYER=1");
    }

    #[test]
    fn our_numbering_agrees_with_the_slicers_own_progress_marker() {
        let result = annotate_layers(&sliced(5)).unwrap();
        assert!(
            result.disagreements.is_empty(),
            "disagreed with M73 L: {:?}",
            result.disagreements
        );
    }

    /// The cross-check has to be able to fire, or it is decoration.
    #[test]
    fn a_disagreement_with_the_slicers_marker_is_reported() {
        let tampered = sliced(3).replace("M73 L2", "M73 L7");
        let result = annotate_layers(&tampered).unwrap();
        assert_eq!(result.disagreements.len(), 1);
        assert!(result.disagreements[0].contains("M73 L7"));
    }

    #[test]
    fn running_the_pass_twice_does_not_double_the_commands() {
        let once = annotate_layers(&sliced(4)).unwrap();
        let twice = annotate_layers(&once.gcode).unwrap();
        assert_eq!(twice.inserted, 0);
        assert_eq!(twice.already_present, 4);
        assert_eq!(once.gcode, twice.gcode);
    }

    /// A slice that reports layers itself does so from `layer_change_gcode`,
    /// which libslic3r writes several lines below the marker.
    #[test]
    fn a_layer_that_reports_itself_further_down_is_left_alone() {
        let source = sliced(3).replace(
            "G1 E-.2 F1800\n",
            "G1 E-.2 F1800\nG92 E0\nSET_PRINT_STATS_INFO CURRENT_LAYER=1\n",
        );
        let result = annotate_layers(&source).unwrap();
        assert_eq!(result.inserted, 0);
        assert_eq!(result.already_present, 3);
        assert_eq!(result.gcode, source);
    }

    /// The search must stop at the next layer, or one reporting layer would
    /// hide every silent layer before it.
    #[test]
    fn a_layer_is_not_covered_by_the_report_of_the_next_one() {
        let source =
            sliced(2).replace(";Z:0.4\n", ";Z:0.4\nSET_PRINT_STATS_INFO CURRENT_LAYER=2\n");
        let result = annotate_layers(&source).unwrap();
        assert_eq!(result.inserted, 1);
        assert_eq!(result.already_present, 1);
    }

    #[test]
    fn nothing_else_in_the_file_is_touched() {
        let source = sliced(3);
        let result = annotate_layers(&source).unwrap();
        let stripped: String = result
            .gcode
            .lines()
            .filter(|l| !l.starts_with(CURRENT_LAYER_COMMAND))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(stripped, source);
    }

    #[test]
    fn a_file_with_no_layer_markers_is_refused() {
        let err = annotate_layers("G28\nG1 X10\n").expect_err("not sliced G-code");
        assert!(matches!(err, GcodeError::NoLayerMarkers));
    }
}
