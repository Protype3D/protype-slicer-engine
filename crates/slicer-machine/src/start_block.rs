//! Derive what the vendor start block *does* from the G-code it *is*.
//!
//! The passport could have been written by reading the start block and
//! typing out what it looked like it did. That is exactly the class of
//! mistake this whole crate exists to stop: the block is 136 lines long,
//! two of the moves that matter are 40 lines apart, and the reason a
//! nozzle went through a finished part is that a human read it and
//! missed one.
//!
//! So the traverse list is machine-derived and a test locks it against
//! the shipped profile. A firmware or profile update that re-choreographs
//! the block turns that test red instead of turning a print into scrap.
//!
//! # What this scanner refuses to do
//!
//! It never guesses. A coordinate that is a slicer placeholder
//! (`{max_layer_z + 3}`), or a `G92` that re-labels the Z origin by a
//! placeholder amount, leaves the position *unknown* — and every move
//! made while it is unknown comes back as [`Unmodelled`] rather than as
//! a traverse with an invented height. Callers are expected to treat an
//! unmodelled in-bed move as unsafe, which is the honest reading.

use crate::passport::{Point2, Point3, Traverse};
use serde::{Deserialize, Serialize};

/// A move the scanner could not resolve to a height, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unmodelled {
    pub source_line: usize,
    pub raw: String,
    pub why: String,
    /// The swept XY segment, when XY is known but Z is not. `None` means
    /// even the position is unknown, which makes the whole bed suspect
    /// for this move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep: Option<(Point2, Point2)>,
    /// The height, when that much *is* known.
    ///
    /// Position and height fail independently, and keeping them apart
    /// matters: a move to an unknown place at a known height that clears
    /// everything on the bed is provably safe, and collapsing it into
    /// "unknown" would make every resume plan look dangerous — which
    /// trains a reader to skip the unknowns, exactly the habit this crate
    /// exists to prevent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub z: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scan {
    /// Moves with a fully known start, end and height, in execution order.
    pub traverses: Vec<Traverse>,
    /// Moves the scanner declined to resolve.
    pub unmodelled: Vec<Unmodelled>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Absolute,
    Relative,
}

/// Machine position in the *commanded* coordinate frame. `None` on an
/// axis means "this scanner does not know", never "zero".
#[derive(Debug, Clone, Copy, Default)]
struct Position {
    x: Option<f64>,
    y: Option<f64>,
    z: Option<f64>,
}

impl Position {
    fn from(p: Point3) -> Self {
        Self {
            x: Some(p.x),
            y: Some(p.y),
            z: Some(p.z),
        }
    }

    fn xy(&self) -> Option<Point2> {
        Some(Point2 {
            x: self.x?,
            y: self.y?,
        })
    }
}

/// Scan a block of G-code into the moves it makes.
///
/// `home_leaves_at` is where the machine's homing routine parks the
/// toolhead — a passport fact, because it is not derivable from the
/// block itself. `G28` sets the position there without emitting a
/// traverse: the motion homing performs internally is the firmware's,
/// not this block's, and it is covered by the passport's `z_probe`
/// rather than pretended into a straight line here.
pub fn scan(gcode: &str, home_leaves_at: Point3) -> Scan {
    let mut out = Scan::default();
    let mut pos = Position::default();
    let mut mode = Mode::Absolute;

    for (index, raw_line) in gcode.lines().enumerate() {
        let line_no = index + 1;
        let line = strip_comment(raw_line);
        if line.is_empty() || line.starts_with('{') {
            // `{if ...}` / `{else}` / `{endif}` are the slicer's template
            // control flow, not machine instructions.
            continue;
        }
        let tokens = tokenize(&line);
        let Some(word) = tokens.first() else { continue };
        match word.to_ascii_uppercase().as_str() {
            "G0" | "G1" => step_linear(&tokens, line_no, raw_line, mode, &mut pos, &mut out),
            "G2" | "G3" => step_arc(&tokens, line_no, raw_line, &mut pos, &mut out),
            "G90" => mode = Mode::Absolute,
            "G91" => mode = Mode::Relative,
            // `G92` (and QIDI's `G92_`) re-label the current position
            // without moving. Only the labelling changes, so an
            // unresolvable value poisons the axis rather than moving it.
            w if w == "G92" || w == "G92_" => step_set_position(&tokens, &mut pos),
            "G28" => {
                // Which axes are homed changes what is known afterwards,
                // and that distinction is the whole basis of resuming a
                // print: `G28` probes Z at the bed centre, `G28 X Y`
                // never touches Z.
                let homes_z = homed_axes(&tokens).contains(&'Z');
                if homes_z {
                    // Verified: the QIDI homing override ends at the bed
                    // centre and lifts 10 mm, so all three axes are known.
                    pos = Position::from(home_leaves_at);
                } else {
                    // Where XY homing parks the toolhead is *not* a
                    // verified fact, so it becomes unknown rather than
                    // being guessed. Z is untouched and stays known.
                    pos.x = None;
                    pos.y = None;
                }
                out.unmodelled.push(Unmodelled {
                    source_line: line_no,
                    raw: raw_line.trim().to_string(),
                    why: if homes_z {
                        "homing motion is the firmware's and is not modelled here; this form \
                         probes Z — unsafe over any part, see z_probe.triggers"
                            .into()
                    } else {
                        "XY homing motion is the firmware's and is not modelled here; this form \
                         does not touch Z, so a lift before it stands"
                            .into()
                    },
                    sweep: None,
                    // Deliberately not `pos.z` when Z is homed. A probing
                    // home *descends to the bed* and only then lifts to
                    // the parked height, so reporting where it ended up
                    // would let the height check clear a move that spent
                    // its middle at Z0. Height is unknown for that form,
                    // and genuinely retained for the XY-only one.
                    z: if homes_z { None } else { pos.z },
                });
            }
            _ => {}
        }
    }
    out
}

/// Axes a `G28` names. A bare `G28` homes all three, which is the form
/// that probes Z.
fn homed_axes(tokens: &[String]) -> Vec<char> {
    let named: Vec<char> = tokens[1..]
        .iter()
        .filter_map(|t| t.chars().next())
        .map(|c| c.to_ascii_uppercase())
        .filter(|c| matches!(c, 'X' | 'Y' | 'Z'))
        .collect();
    if named.is_empty() {
        vec!['X', 'Y', 'Z']
    } else {
        named
    }
}

fn step_linear(
    tokens: &[String],
    line_no: usize,
    raw: &str,
    mode: Mode,
    pos: &mut Position,
    out: &mut Scan,
) {
    let before = *pos;
    let mut unresolved: Vec<&'static str> = Vec::new();

    for token in &tokens[1..] {
        let Some((axis, value)) = axis_word(token) else {
            continue;
        };
        let slot = match axis {
            'X' => &mut pos.x,
            'Y' => &mut pos.y,
            'Z' => &mut pos.z,
            // E and F do not move the toolhead through space.
            _ => continue,
        };
        match value {
            Some(v) if mode == Mode::Absolute => *slot = Some(v),
            Some(v) => *slot = slot.map(|current| current + v),
            None => {
                *slot = None;
                unresolved.push(match axis {
                    'X' => "X",
                    'Y' => "Y",
                    _ => "Z",
                });
            }
        }
    }

    emit(before, *pos, line_no, raw, &unresolved, out);
}

/// Arcs are only followed when they do not move in XY — QIDI's start
/// block uses them for a stationary nozzle-wipe curl. An arc that *does*
/// travel bulges outside its chord, so approximating one by a straight
/// line would under-report the swept area; those come back unmodelled.
fn step_arc(tokens: &[String], line_no: usize, raw: &str, pos: &mut Position, out: &mut Scan) {
    let travels = tokens[1..]
        .iter()
        .filter_map(|t| axis_word(t))
        .any(|(axis, _)| axis == 'X' || axis == 'Y');
    if !travels {
        return;
    }
    let sweep = pos.xy().map(|p| (p, p));
    pos.x = None;
    pos.y = None;
    out.unmodelled.push(Unmodelled {
        source_line: line_no,
        raw: raw.trim().to_string(),
        why: "arc travel is not modelled; its swept area is wider than the chord".into(),
        sweep,
        z: pos.z,
    });
}

fn step_set_position(tokens: &[String], pos: &mut Position) {
    for token in &tokens[1..] {
        let Some((axis, value)) = axis_word(token) else {
            continue;
        };
        match axis {
            'X' => pos.x = value,
            'Y' => pos.y = value,
            'Z' => pos.z = value,
            _ => {}
        }
    }
}

fn emit(
    before: Position,
    after: Position,
    line_no: usize,
    raw: &str,
    unresolved: &[&str],
    out: &mut Scan,
) {
    let raw = raw.trim().to_string();
    let (Some(from), Some(to)) = (before.xy(), after.xy()) else {
        // Position unknown in XY: we cannot say where on the bed this
        // happened. It is still reported — with whatever *is* known,
        // usually the height — because a move that moved something and
        // then vanished from the report is the worst outcome available.
        //
        // The one thing skipped is a move that changed no axis at all
        // while no axis was known: a bare feedrate change before any
        // position exists, which refers to nowhere.
        let moved = before.x != after.x || before.y != after.y || before.z != after.z;
        if moved || !unresolved.is_empty() {
            out.unmodelled.push(Unmodelled {
                source_line: line_no,
                raw,
                why: describe_unknown(&before, &after, unresolved),
                sweep: None,
                z: after.z,
            });
        }
        return;
    };

    // A move that extrudes or changes feedrate without going anywhere
    // adds nothing: the nozzle is already at a point some earlier move
    // recorded. Pure-Z moves are emphatically not in this class — a bare
    // `G1 Z0.2` inside a part's footprint is the whole problem — so the
    // comparison covers all three axes, not just XY.
    if from == to && before.z == after.z {
        return;
    }

    match after.z {
        Some(z) => out.traverses.push(Traverse {
            from,
            to,
            z,
            source_line: line_no,
            raw,
        }),
        None => out.unmodelled.push(Unmodelled {
            source_line: line_no,
            raw,
            why: "height is unknown here, so clearance over a part cannot be judged".into(),
            sweep: Some((from, to)),
            z: None,
        }),
    }
}

fn describe_unknown(before: &Position, after: &Position, unresolved: &[&str]) -> String {
    if !unresolved.is_empty() {
        return format!(
            "{} given as a slicer placeholder, so the position is unknown",
            unresolved.join("/")
        );
    }
    let missing_before = before.x.is_none() || before.y.is_none();
    if missing_before && (after.x.is_some() || after.y.is_some()) {
        return "the position before this move was never established".into();
    }
    "the position is unknown".into()
}

/// `X195` -> `('X', Some(195.0))`; `Z{a + b}` -> `('Z', None)`.
///
/// Public because every G-code reader in the workspace needs exactly this
/// split, and the naive `split_at(1)` spelling of it panics on any token
/// whose first character is not one byte — a non-ASCII word in a comment
/// is enough. One implementation, one set of edge cases.
pub fn axis_word(token: &str) -> Option<(char, Option<f64>)> {
    let mut chars = token.chars();
    let axis = chars.next()?.to_ascii_uppercase();
    if !axis.is_ascii_alphabetic() {
        return None;
    }
    let rest = chars.as_str();
    if rest.is_empty() {
        return Some((axis, None));
    }
    if rest.contains('{') || rest.contains('[') {
        return Some((axis, None));
    }
    Some((axis, rest.parse::<f64>().ok()))
}

fn strip_comment(line: &str) -> String {
    line.split(';').next().unwrap_or("").trim().to_string()
}

/// Split on whitespace, but keep `{...}` and `[...]` groups whole —
/// slicer placeholders contain spaces (`{max_layer_z + 3.0}`) and a
/// naive split would tear them into nonsense tokens.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for ch in line.chars() {
        match ch {
            '{' | '[' => {
                depth += 1;
                current.push(ch);
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: Point3 = Point3 {
        x: 195.0,
        y: 195.0,
        z: 10.0,
    };

    #[test]
    fn absolute_moves_become_traverses_at_the_commanded_height() {
        let scan = scan("G28\nG1 Z5\nG1 X10 Y20", HOME);
        // The Z-only move is a traverse of zero XY length; the XY move
        // follows it at the same height.
        let last = scan.traverses.last().expect("a traverse");
        assert_eq!(last.from, Point2 { x: 195.0, y: 195.0 });
        assert_eq!(last.to, Point2 { x: 10.0, y: 20.0 });
        assert_eq!(last.z, 5.0);
    }

    #[test]
    fn relative_mode_offsets_from_the_current_point() {
        let scan = scan("G28\nG1 Z5\nG91\nG1 X10\nG90", HOME);
        let last = scan.traverses.last().expect("a traverse");
        assert_eq!(last.to, Point2 { x: 205.0, y: 195.0 });
        assert_eq!(last.z, 5.0);
    }

    #[test]
    fn a_placeholder_height_yields_no_traverse_and_is_reported() {
        let scan = scan("G28\nG1 Z{max_layer_z + 3.0} F1200\nG1 X10 Y20", HOME);
        assert!(
            scan.traverses.is_empty(),
            "a height that is a placeholder must not be invented, got {:?}",
            scan.traverses
        );
        assert!(scan.unmodelled.iter().any(|u| u.source_line == 3));
    }

    #[test]
    fn g92_relabels_without_moving_and_a_placeholder_poisons_the_axis() {
        // The QIDI block trims the Z origin by a placeholder amount; the
        // nozzle does not move, but the height stops being knowable.
        let scan = scan(
            "G28\nG1 X195 Y195 Z10\nG92_ Z{10 - (t - 130)/1400}\nG0 Y1",
            HOME,
        );
        let trailing = scan
            .unmodelled
            .iter()
            .find(|u| u.source_line == 4)
            .expect("the move after the re-label is unmodelled");
        assert!(trailing.sweep.is_some(), "its XY is still known");
    }

    #[test]
    fn g28_is_reported_as_unmodelled_rather_than_drawn_as_a_line() {
        let scan = scan("G1 Z5\nG28", HOME);
        let homing = scan
            .unmodelled
            .iter()
            .find(|u| u.raw == "G28")
            .expect("G28 is flagged");
        assert!(homing.sweep.is_none());
        assert!(homing.why.contains("z_probe.triggers"));
    }

    #[test]
    fn homing_xy_only_leaves_the_height_alone() {
        // The distinction resuming a print rests on: a lift before
        // `G28 X Y` still stands afterwards, so a traverse can be shown
        // to clear a part. A bare `G28` re-references Z and it cannot.
        let scan = scan("G28\nG1 Z40\nG28 X Y\nG1 X10 Y10", HOME);
        // The move after XY homing starts from a place the scanner does
        // not know, so it is not a traverse — but its *height* is still
        // known, and that is what lets a lifted resume be proved clear.
        let approach = scan
            .unmodelled
            .iter()
            .find(|u| u.raw == "G1 X10 Y10")
            .expect("the approach move is reported");
        assert_eq!(approach.z, Some(40.0), "Z survives an XY-only home");
        assert!(approach.sweep.is_none(), "its origin is genuinely unknown");
    }

    #[test]
    fn homing_xy_only_still_leaves_the_position_unknown_until_set() {
        let scan = scan("G28\nG1 Z40\nG28 X Y", HOME);
        let homing = scan
            .unmodelled
            .iter()
            .find(|u| u.raw == "G28 X Y")
            .expect("XY homing is flagged");
        assert!(
            homing.why.contains("does not touch Z"),
            "why: {}",
            homing.why
        );
    }

    /// A probing home dips to the bed before parking. If it reported the
    /// parked height, the hazard check would clear it over a short part
    /// that the probe descent would have gone straight through.
    #[test]
    fn a_probing_home_reports_no_height_at_all() {
        let scan = scan("G1 Z40\nG28", HOME);
        let homing = scan
            .unmodelled
            .iter()
            .find(|u| u.raw == "G28")
            .expect("G28 is flagged");
        assert_eq!(
            homing.z, None,
            "a home that descends to the bed must not advertise its parked height"
        );
    }

    #[test]
    fn a_bare_g28_re_references_every_axis() {
        let scan = scan("G1 Z40\nG28\nG1 X10 Y10", HOME);
        let last = scan.traverses.last().expect("a traverse");
        assert_eq!(last.z, HOME.z, "a full home replaces the height too");
    }

    #[test]
    fn template_control_lines_are_not_instructions() {
        let scan = scan(
            "G28\nG1 Z5\n{if chamber_temperature[0] == 0}\nM106 P3 S1\n{endif}",
            HOME,
        );
        assert_eq!(scan.traverses.len(), 1);
    }

    #[test]
    fn tokenizer_keeps_placeholders_with_spaces_intact() {
        assert_eq!(
            tokenize("G1 X{max_layer_z + 3.0} F1200"),
            vec!["G1", "X{max_layer_z + 3.0}", "F1200"]
        );
    }

    #[test]
    fn a_stationary_arc_is_followed_but_a_travelling_one_is_not() {
        let stationary = scan("G28\nG1 Z5\nG2 I0.5 J0.5 F480", HOME);
        assert!(stationary
            .unmodelled
            .iter()
            .all(|u| u.raw != "G2 I0.5 J0.5 F480"));

        let travelling = scan("G28\nG1 Z5\nG2 X10 Y10 I0.5 J0.5", HOME);
        assert!(travelling
            .unmodelled
            .iter()
            .any(|u| u.why.contains("chord")));
    }

    #[test]
    fn comments_do_not_become_axis_words() {
        let scan = scan("G28\nG1 Z5 ; Zippy comment\nG1 X1 Y1", HOME);
        assert_eq!(scan.traverses.last().unwrap().z, 5.0);
    }
}
