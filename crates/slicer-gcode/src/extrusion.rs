//! Which mode the E axis is in, and where it stands, at a given line.
//!
//! A resumed file is spliced onto a generated block, and the block has to
//! hand the machine an extruder in the state the rest of the file expects.
//! Two spellings exist and they are not compatible:
//!
//! * `M83` — relative. Every `E` in the body is an increment, so the block
//!   only has to zero the axis.
//! * `M82` — absolute. Every `E` in the body is a position on a running
//!   total that started at the top of the file. Zeroing the axis here
//!   makes the first move's `E1234.5` a request for 1234.5 mm of filament
//!   instead of a tenth of a millimetre.
//!
//! So the mode is read out of the file rather than assumed, and when the
//! file says neither, this module says so and the caller refuses — the
//! same stance `ZReference` takes on Z.

use serde::{Deserialize, Serialize};
use slicer_machine::start_block::axis_word;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtrusionMode {
    /// `M82`: every `E` is a position on a running total.
    Absolute,
    /// `M83`: every `E` is an increment.
    Relative,
}

/// The state of the E axis as of some line in the file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtrusionState {
    pub mode: ExtrusionMode,
    /// Where the axis stands. Only meaningful in `Absolute` mode — in
    /// relative mode the body's numbers do not refer to it.
    pub e_position: f64,
}

/// Walk `lines[..cut]` and report the E state the resumed body inherits.
///
/// `None` means the file never declared a mode. That is not a defect to
/// paper over with a default: firmware defaults differ, and picking one
/// decides between "the first move extrudes 0.4 mm" and "the first move
/// extrudes half a metre".
pub fn state_before(lines: &[&str], cut: usize) -> Option<ExtrusionState> {
    let mut mode: Option<ExtrusionMode> = None;
    // Tracked from the top of the file so that an absolute resume can
    // restore the running total. `G92 E<v>` resets it, exactly as it does
    // on the machine.
    let mut e_position = 0.0f64;

    for line in lines.iter().take(cut) {
        let code = strip_comment(line);
        let mut tokens = code.split_whitespace();
        let Some(word) = tokens.next() else {
            continue;
        };
        match word.to_ascii_uppercase().as_str() {
            "M82" => mode = Some(ExtrusionMode::Absolute),
            "M83" => mode = Some(ExtrusionMode::Relative),
            "G92" => {
                if let Some(v) = e_word(tokens) {
                    e_position = v;
                }
            }
            "G0" | "G1" => {
                if let Some(v) = e_word(tokens) {
                    // Before any declaration the running total is a guess
                    // anyway; the caller refuses that file rather than
                    // using it.
                    match mode {
                        Some(ExtrusionMode::Relative) => e_position += v,
                        _ => e_position = v,
                    }
                }
            }
            _ => {}
        }
    }

    mode.map(|mode| ExtrusionState { mode, e_position })
}

fn e_word<'a>(tokens: impl Iterator<Item = &'a str>) -> Option<f64> {
    tokens
        .filter_map(axis_word)
        .find(|(axis, _)| *axis == 'E')
        .and_then(|(_, value)| value)
}

/// Everything before the first `;`. A comment can hold anything at all,
/// including words that look like axis words and bytes that are not ASCII.
fn strip_comment(line: &str) -> &str {
    line.split(';').next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(gcode: &str) -> Option<ExtrusionState> {
        let lines: Vec<&str> = gcode.lines().collect();
        state_before(&lines, lines.len())
    }

    #[test]
    fn reads_relative_mode_from_m83() {
        let s = state("M83\nG1 X1 Y1 E0.5\n").expect("M83 declares the mode");
        assert_eq!(s.mode, ExtrusionMode::Relative);
    }

    #[test]
    fn reads_absolute_mode_from_m82() {
        let s = state("M82\nG1 X1 Y1 E0.5\n").expect("M82 declares the mode");
        assert_eq!(s.mode, ExtrusionMode::Absolute);
    }

    #[test]
    fn the_last_declaration_before_the_cut_wins() {
        let s = state("M82\nG1 E5\nM83\nG1 E0.4\n").expect("declared twice");
        assert_eq!(s.mode, ExtrusionMode::Relative);
    }

    #[test]
    fn returns_none_when_the_file_declares_no_mode() {
        assert!(state("G1 X1 Y1 E0.5\nG1 X2 Y2 E1.0\n").is_none());
    }

    #[test]
    fn tracks_the_absolute_e_position_through_g92_resets() {
        let s = state("M82\nG1 E120\nG92 E0\nG1 E4.25\n").expect("declared");
        assert_eq!(s.mode, ExtrusionMode::Absolute);
        assert!((s.e_position - 4.25).abs() < 1e-9, "got {}", s.e_position);
    }

    #[test]
    fn accumulates_relative_extrusion() {
        let s = state("M83\nG1 E1\nG1 E0.5\nG1 E-0.2\n").expect("declared");
        assert!((s.e_position - 1.3).abs() < 1e-9, "got {}", s.e_position);
    }

    #[test]
    fn a_mode_word_inside_a_comment_does_not_count() {
        let s =
            state("M83\n; M82 is what the vendor profile used to emit\nG1 E1\n").expect("declared");
        assert_eq!(s.mode, ExtrusionMode::Relative);
    }

    #[test]
    fn a_non_ascii_comment_is_read_without_panicking() {
        let s = state("M83\nG1 X10 Y10 F600 ; подъём Z\n").expect("declared");
        assert_eq!(s.mode, ExtrusionMode::Relative);
    }
}
