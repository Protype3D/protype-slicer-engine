//! What one G-code line means to the fan lead pass.
//!
//! Only what moves time or the part fan is read. The part fan is `M106`
//! without `P` or with `P1` and any `M107` — exactly what `FanMover` accepts;
//! `M106 P2`/`P3` drive the auxiliary and exhaust fans and are left alone.
//! Everything after `;` is a comment and cannot spell a command.

use slicer_machine::start_block::axis_word;

use super::PAINT_FAN_MARKER;
use crate::layers::LAYER_CHANGE_MARKER;

/// Full speed of `M106 S`, and what a bare `M106` means to the firmware.
pub(super) const FULL_FAN: u16 = 255;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Marker {
    /// `; PAINT_FAN S<pct>`. The speed itself is read from the command that
    /// follows, in the units the firmware gets.
    Entry,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Command {
    /// `G0`–`G3`. Arcs are timed by their chord: shorter than the arc, so the
    /// lead only grows.
    Move {
        x: Option<f64>,
        y: Option<f64>,
        z: Option<f64>,
        feed_mm_min: Option<f64>,
    },
    Dwell {
        seconds: f64,
    },
    Absolute,
    Relative,
    SetPosition {
        x: Option<f64>,
        y: Option<f64>,
        z: Option<f64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Line {
    Marker(Marker),
    /// Starts with `; PAINT_FAN` but says neither `S<0..100>` nor `END`.
    BadMarker,
    /// Part fan speed, 0..=255.
    PartFan(u16),
    ToolChange,
    LayerChange,
    Motion(Command),
    Other,
}

/// `Err` carries the word that could not be read.
pub(super) fn classify(text: &str) -> Result<Line, String> {
    let trimmed = text.trim();
    if trimmed == LAYER_CHANGE_MARKER {
        return Ok(Line::LayerChange);
    }
    if let Some(rest) = trimmed.strip_prefix(PAINT_FAN_MARKER) {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return Ok(marker(rest.trim()).map_or(Line::BadMarker, Line::Marker));
        }
    }
    let code = trimmed.split(';').next().unwrap_or("").trim();
    let mut tokens = code.split_whitespace();
    let Some(head) = tokens.next() else {
        return Ok(Line::Other);
    };
    // Matched in place: this runs over every line of files that reach ten
    // million of them, and an uppercased copy of the head would allocate on
    // each one. ASCII case folding is what a G-code head needs anyway.
    if head.eq_ignore_ascii_case("M106") {
        return part_fan(tokens);
    }
    if head.eq_ignore_ascii_case("M107") {
        return Ok(Line::PartFan(0));
    }
    if is_tool_change(head) {
        return Ok(Line::ToolChange);
    }
    match strip_letter(head, 'G').map(str::parse::<u32>) {
        Some(Ok(number)) => motion(number, tokens),
        _ => Ok(Line::Other),
    }
}

fn marker(payload: &str) -> Option<Marker> {
    if payload == "END" {
        return Some(Marker::Exit);
    }
    let pct: u8 = payload.strip_prefix('S')?.parse().ok()?;
    (pct <= 100).then_some(Marker::Entry)
}

fn is_tool_change(head: &str) -> bool {
    strip_letter(head, 'T')
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// What follows a leading `upper` letter, in whichever case the file writes
/// it (`G1` and `g1` alike).
fn strip_letter(head: &str, upper: char) -> Option<&str> {
    head.strip_prefix(upper)
        .or_else(|| head.strip_prefix(upper.to_ascii_lowercase()))
}

fn part_fan<'a>(tokens: impl Iterator<Item = &'a str>) -> Result<Line, String> {
    let mut speed = f64::from(FULL_FAN);
    for token in tokens {
        match axis_word(token) {
            Some(('P', Some(index))) if index != 1.0 => return Ok(Line::Other),
            Some(('S', Some(value))) => speed = value,
            Some(('P' | 'S', None)) => return Err(token.to_string()),
            _ => {}
        }
    }
    Ok(Line::PartFan(
        speed.round().clamp(0.0, f64::from(FULL_FAN)) as u16
    ))
}

/// Only the G-numbers the pass actually models are worth reading words
/// for — everything else (`G28` homing, `G10` tool offsets, `G29`
/// bed-leveling, …) is `Other` without looking at its words at all.
/// Otherwise a bare axis word in a command we do not track (`G28 X`, valid
/// homing syntax that can show up in a user's start/end G-code) would read
/// as an unreadable word and bail out the whole pass — the "unreadable
/// word" rule is scoped to the commands below, not to G-code in general.
fn motion<'a>(number: u32, tokens: impl Iterator<Item = &'a str>) -> Result<Line, String> {
    let command = match number {
        0..=3 => {
            let words = read_words(tokens)?;
            Command::Move {
                x: words.x,
                y: words.y,
                z: words.z,
                feed_mm_min: words.f,
            }
        }
        // `S` is seconds and wins over `P`, milliseconds — as in Klipper.
        4 => {
            let words = read_words(tokens)?;
            Command::Dwell {
                seconds: words
                    .s
                    .or(words.p.map(|ms| ms / 1000.0))
                    .unwrap_or(0.0)
                    .max(0.0),
            }
        }
        90 => {
            read_words(tokens)?;
            Command::Absolute
        }
        91 => {
            read_words(tokens)?;
            Command::Relative
        }
        92 => {
            let words = read_position_words(tokens)?;
            Command::SetPosition {
                x: words.x,
                y: words.y,
                z: words.z,
            }
        }
        _ => return Ok(Line::Other),
    };
    Ok(Line::Motion(command))
}

#[derive(Default)]
struct Words {
    x: Option<f64>,
    y: Option<f64>,
    z: Option<f64>,
    f: Option<f64>,
    p: Option<f64>,
    s: Option<f64>,
}

fn read_words<'a>(tokens: impl Iterator<Item = &'a str>) -> Result<Words, String> {
    let mut words = Words::default();
    for token in tokens {
        let Some((axis, value)) = axis_word(token) else {
            continue;
        };
        let slot = match axis {
            'X' => &mut words.x,
            'Y' => &mut words.y,
            'Z' => &mut words.z,
            'F' => &mut words.f,
            'P' => &mut words.p,
            'S' => &mut words.s,
            _ => continue,
        };
        *slot = Some(value.ok_or_else(|| token.to_string())?);
    }
    Ok(words)
}

/// Like `read_words`, but for `G92`: it does not move the head, so a bare
/// axis word (`X` with no number) cannot leave a physical position
/// unknown the way it would in a move — it is ignored, the same as a
/// letter `G92` never tracks (`E`). A word whose number fails to parse
/// for another reason (`X1O`) is still an error.
fn read_position_words<'a>(tokens: impl Iterator<Item = &'a str>) -> Result<Words, String> {
    let mut words = Words::default();
    for token in tokens {
        let Some((axis, value)) = axis_word(token) else {
            continue;
        };
        let slot = match axis {
            'X' => &mut words.x,
            'Y' => &mut words.y,
            'Z' => &mut words.z,
            _ => continue,
        };
        match value {
            Some(value) => *slot = Some(value),
            None if is_bare_word(token) => {}
            None => return Err(token.to_string()),
        }
    }
    Ok(words)
}

/// True for a token that is only an axis letter, like `X` in `G92 X` — as
/// opposed to one whose number failed to parse, like `X1O`. `axis_word`
/// reports both as a valueless word, so the two have to be told apart by
/// the token's own length.
fn is_bare_word(token: &str) -> bool {
    token.chars().count() == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_marker_forms() {
        assert_eq!(
            classify("; PAINT_FAN S80\n"),
            Ok(Line::Marker(Marker::Entry))
        );
        assert_eq!(
            classify("; PAINT_FAN END\r\n"),
            Ok(Line::Marker(Marker::Exit))
        );
    }

    #[test]
    fn a_marker_with_an_impossible_speed_is_bad_not_ignored() {
        assert_eq!(classify("; PAINT_FAN S150"), Ok(Line::BadMarker));
        assert_eq!(classify("; PAINT_FAN"), Ok(Line::BadMarker));
        assert_eq!(classify("; PAINT_FAN S"), Ok(Line::BadMarker));
    }

    #[test]
    fn a_longer_comment_word_is_not_a_marker() {
        assert_eq!(classify("; PAINT_FANCY S80"), Ok(Line::Other));
    }

    #[test]
    fn reads_the_part_fan_the_way_fan_mover_does() {
        assert_eq!(classify("M106 S204"), Ok(Line::PartFan(204)));
        assert_eq!(classify("M106 S0 ; disable fan"), Ok(Line::PartFan(0)));
        assert_eq!(classify("M106 P1 S127"), Ok(Line::PartFan(127)));
        assert_eq!(classify("M106"), Ok(Line::PartFan(255)));
        assert_eq!(classify("M107"), Ok(Line::PartFan(0)));
        assert_eq!(classify("M106 P2 S255"), Ok(Line::Other));
        assert_eq!(classify("M106 P3 S0"), Ok(Line::Other));
    }

    #[test]
    fn a_lower_case_head_reads_the_same_as_an_upper_case_one() {
        // Classifying without copying the head has to keep folding its case.
        assert_eq!(classify("m106 s204"), Ok(Line::PartFan(204)));
        assert_eq!(classify("m107"), Ok(Line::PartFan(0)));
        assert_eq!(classify("t1"), Ok(Line::ToolChange));
        assert_eq!(
            classify("g4 p1500"),
            Ok(Line::Motion(Command::Dwell { seconds: 1.5 }))
        );
    }

    #[test]
    fn reads_tool_changes_with_and_without_a_comment() {
        assert_eq!(classify("T1"), Ok(Line::ToolChange));
        assert_eq!(classify("T0 ; change extruder"), Ok(Line::ToolChange));
        assert_eq!(classify("TIMELAPSE_TAKE_FRAME"), Ok(Line::Other));
    }

    #[test]
    fn reads_the_layer_change_marker() {
        assert_eq!(classify(";LAYER_CHANGE\n"), Ok(Line::LayerChange));
    }

    #[test]
    fn reads_moves_dwells_and_positioning() {
        assert_eq!(
            classify("G1 X10 Y-2.5 E.4 F600"),
            Ok(Line::Motion(Command::Move {
                x: Some(10.0),
                y: Some(-2.5),
                z: None,
                feed_mm_min: Some(600.0),
            }))
        );
        assert_eq!(
            classify("G4 P1500"),
            Ok(Line::Motion(Command::Dwell { seconds: 1.5 }))
        );
        assert_eq!(
            classify("G4 S2 P100"),
            Ok(Line::Motion(Command::Dwell { seconds: 2.0 }))
        );
        assert_eq!(classify("G91"), Ok(Line::Motion(Command::Relative)));
        assert_eq!(classify("G90"), Ok(Line::Motion(Command::Absolute)));
        assert_eq!(
            classify("G92 E0"),
            Ok(Line::Motion(Command::SetPosition {
                x: None,
                y: None,
                z: None,
            }))
        );
    }

    #[test]
    fn a_word_that_is_not_a_number_is_reported() {
        assert_eq!(classify("G1 X1O Y2"), Err("X1O".to_string()));
        assert_eq!(classify("M106 S{fan}"), Err("S{fan}".to_string()));
    }

    #[test]
    fn words_inside_a_comment_do_not_count() {
        assert_eq!(classify("; M106 S255 G1 X10"), Ok(Line::Other));
    }

    #[test]
    fn a_g_command_we_do_not_model_is_other_even_with_words_that_look_unreadable() {
        // Homing and other G-codes the pass does not track can carry bare
        // axis words (`G28 X`) or letters we never parse (`G10 L20 P1`) —
        // none of that should make an unrelated, untracked command look
        // like an unreadable line and bail out the whole pass.
        assert_eq!(classify("G28 X"), Ok(Line::Other));
        assert_eq!(classify("G28 X Y Z"), Ok(Line::Other));
        assert_eq!(classify("G29"), Ok(Line::Other));
        assert_eq!(classify("G10 L20 P1 X0"), Ok(Line::Other));
    }

    #[test]
    fn a_bare_axis_in_a_move_is_still_an_error() {
        assert_eq!(classify("G1 X"), Err("X".to_string()));
        assert_eq!(
            classify("G1 X10 Y20 F600"),
            Ok(Line::Motion(Command::Move {
                x: Some(10.0),
                y: Some(20.0),
                z: None,
                feed_mm_min: Some(600.0),
            }))
        );
    }

    #[test]
    fn a_bare_axis_in_g92_is_ignored_but_a_malformed_one_still_errors() {
        // G92 does not move the head, so a bare axis word cannot leave a
        // physical position unknown the way it would in a move — it is
        // ignored, same as a letter G92 never tracks (`E`). A word whose
        // number fails to parse for another reason is still an error.
        assert_eq!(
            classify("G92 X"),
            Ok(Line::Motion(Command::SetPosition {
                x: None,
                y: None,
                z: None,
            }))
        );
        assert_eq!(classify("G92 X1O"), Err("X1O".to_string()));
    }
}
