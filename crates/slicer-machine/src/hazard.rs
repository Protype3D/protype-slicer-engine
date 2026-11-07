//! Does this motion hit something that is already standing on the bed?
//!
//! Written against one specific accident: a print was resumed from layer
//! 92, `G28` probed Z at the centre of the bed, the probe dropped into an
//! infill cell of the part standing there, and the nozzle went in after
//! it. Nothing in the model was wrong. The missing piece was arithmetic
//! nobody had done.
//!
//! Two rules, and the second matters as much as the first:
//!
//! 1. A move whose height is at or below the top of a part, anywhere
//!    inside that part's footprint, is a [`Severity::Collision`].
//! 2. A move whose height is *unknown* over a part is a
//!    [`Severity::Unknown`] — reported, never silently passed. A plan is
//!    only safe when it has no collisions **and** every unknown has been
//!    accounted for.
//!
//! Footprints are axis-aligned boxes. That over-approximates a round
//! part, which is the correct direction to be wrong in: this check may
//! cry collision where there is none, and never the reverse.

use crate::passport::{Point2, Traverse};
use crate::probe::{self, ProbeCoverage, ProbeOperation};
use crate::start_block::Unmodelled;
use serde::{Deserialize, Serialize};

/// Clearance below which a move counts as a collision even though the
/// arithmetic says it passes. Covers the part's own first-layer squish,
/// warp and the machine's Z repeatability — none of which the numbers
/// here know about.
pub const DEFAULT_MARGIN_MM: f64 = 1.0;

/// Something already standing on the bed, as a bounding box.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Part {
    pub name: String,
    pub min: Point2,
    pub max: Point2,
    /// Height of its highest point above the bed.
    pub top_z: f64,
}

impl Part {
    /// Build a footprint from a mesh bounding box in machine coordinates.
    pub fn from_bounds(name: impl Into<String>, min: [f64; 3], max: [f64; 3]) -> Self {
        Self {
            name: name.into(),
            min: Point2 {
                x: min[0],
                y: min[1],
            },
            max: Point2 {
                x: max[0],
                y: max[1],
            },
            top_z: max[2],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Cannot be ruled out from what is known.
    Unknown,
    /// The arithmetic says the nozzle goes through the part.
    Collision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Hazard {
    /// The machine probes Z where a part is standing.
    ZProbeIntoPart {
        part: String,
        probe: Point2,
        part_top_z: f64,
    },
    /// A move crosses the part's footprint at or below its top.
    TraverseThroughPart {
        part: String,
        traverse: Traverse,
        clearance_mm: f64,
    },
    /// A move crosses the footprint at a height that could not be
    /// resolved.
    UnknownHeightOverPart { part: String, at: Unmodelled },
    /// A move whose position could not be resolved at all, while parts
    /// are on the bed.
    UnknownPosition { at: Unmodelled },
    /// A probing operation whose touchdowns land on a part.
    ///
    /// Distinct from [`Hazard::ZProbeIntoPart`], which is the single
    /// `G28` point. This one covers the operations that probe a *region* —
    /// above all the adaptive bed mesh, which follows the print area and
    /// therefore walks a grid straight over the part being resumed.
    ProbeRegionOverPart {
        part: String,
        command: String,
        touchdowns: String,
    },
    /// A probing operation whose touchdowns cannot be located.
    ProbeRegionUnknown { command: String, why: String },
}

impl Hazard {
    pub fn severity(&self) -> Severity {
        match self {
            Hazard::ZProbeIntoPart { .. }
            | Hazard::TraverseThroughPart { .. }
            | Hazard::ProbeRegionOverPart { .. } => Severity::Collision,
            Hazard::UnknownHeightOverPart { .. }
            | Hazard::UnknownPosition { .. }
            | Hazard::ProbeRegionUnknown { .. } => Severity::Unknown,
        }
    }
}

impl std::fmt::Display for Hazard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Hazard::ZProbeIntoPart {
                part,
                probe,
                part_top_z,
            } => write!(
                f,
                "Z is probed at ({:.1}, {:.1}), inside {part}, which stands {part_top_z:.2} mm tall",
                probe.x, probe.y
            ),
            Hazard::TraverseThroughPart {
                part,
                traverse,
                clearance_mm,
            } => write!(
                f,
                "line {}: `{}` crosses {part} at Z{:.2} — clearance {:+.2} mm",
                traverse.source_line, traverse.raw, traverse.z, clearance_mm
            ),
            Hazard::UnknownHeightOverPart { part, at } => write!(
                f,
                "line {}: `{}` crosses {part} at an unknown height — {}",
                at.source_line, at.raw, at.why
            ),
            Hazard::UnknownPosition { at } => write!(
                f,
                "line {}: `{}` happens at an unknown position — {}",
                at.source_line, at.raw, at.why
            ),
            Hazard::ProbeRegionOverPart {
                part,
                command,
                touchdowns,
            } => write!(f, "`{command}` probes {touchdowns}, which is on {part}"),
            Hazard::ProbeRegionUnknown { command, why } => {
                write!(f, "`{command}` probes somewhere this cannot locate — {why}")
            }
        }
    }
}

/// Check every probing operation the passport knows against the parts.
///
/// `print_area` is the bounding box of what the job will print. For a
/// resume that is the part already standing on the bed, which is exactly
/// what makes an adaptive bed mesh dangerous: it fits itself to that box.
/// Passing `None` is not the safe option — an adaptive mesh with no
/// declared objects falls back to meshing the whole bed.
pub fn assess_probe_operations(
    operations: &[ProbeOperation],
    parts: &[Part],
    print_area: Option<(Point2, Point2)>,
) -> Vec<Hazard> {
    let mut out = Vec::new();
    if parts.is_empty() {
        return out;
    }
    for operation in operations {
        match probe::coverage(&operation.region, print_area) {
            ProbeCoverage::Points(points) => {
                for part in parts {
                    let hits: Vec<&Point2> =
                        points.iter().filter(|p| point_in_box(**p, part)).collect();
                    if hits.is_empty() {
                        continue;
                    }
                    out.push(Hazard::ProbeRegionOverPart {
                        part: part.name.clone(),
                        command: operation.command.clone(),
                        touchdowns: format!(
                            "{} point(s), the first at ({:.1}, {:.1})",
                            hits.len(),
                            hits[0].x,
                            hits[0].y
                        ),
                    });
                }
            }
            ProbeCoverage::Region(lo, hi) => {
                let region = Part {
                    name: String::new(),
                    min: lo,
                    max: hi,
                    top_z: 0.0,
                };
                for part in parts {
                    if !boxes_overlap(&region, part) {
                        continue;
                    }
                    out.push(Hazard::ProbeRegionOverPart {
                        part: part.name.clone(),
                        command: operation.command.clone(),
                        touchdowns: format!(
                            "a grid over X{:.1}..{:.1} Y{:.1}..{:.1}",
                            lo.x, hi.x, lo.y, hi.y
                        ),
                    });
                }
            }
            ProbeCoverage::Unknown => out.push(Hazard::ProbeRegionUnknown {
                command: operation.command.clone(),
                why: operation.what.clone(),
            }),
        }
    }
    out
}

fn boxes_overlap(a: &Part, b: &Part) -> bool {
    a.min.x <= b.max.x && a.max.x >= b.min.x && a.min.y <= b.max.y && a.max.y >= b.min.y
}

/// Check a sequence of resolved moves and unresolved ones against the
/// parts on the bed.
pub fn assess(
    traverses: &[Traverse],
    unmodelled: &[Unmodelled],
    parts: &[Part],
    margin_mm: f64,
) -> Vec<Hazard> {
    let mut out = Vec::new();
    if parts.is_empty() {
        return out;
    }

    for traverse in traverses {
        for part in parts {
            if !segment_hits_box(traverse.from, traverse.to, part) {
                continue;
            }
            let clearance = traverse.z - part.top_z;
            if clearance <= margin_mm {
                out.push(Hazard::TraverseThroughPart {
                    part: part.name.clone(),
                    traverse: traverse.clone(),
                    clearance_mm: clearance,
                });
            }
        }
    }

    for at in unmodelled {
        // Height alone can settle it. A move to an unknown place, at a
        // height that clears everything on the bed, cannot hit anything
        // wherever it goes — and this is the case a lift-then-home
        // resume sequence relies on. Reporting it as unknown anyway
        // would bury the unknowns that matter.
        if let Some(z) = at.z {
            if parts.iter().all(|part| z - part.top_z > margin_mm) {
                continue;
            }
        }
        match at.sweep {
            Some((from, to)) => {
                for part in parts {
                    if segment_hits_box(from, to, part) {
                        out.push(Hazard::UnknownHeightOverPart {
                            part: part.name.clone(),
                            at: at.clone(),
                        });
                    }
                }
            }
            None => out.push(Hazard::UnknownPosition { at: at.clone() }),
        }
    }

    out
}

/// Whether probing Z at `probe` drives the nozzle into anything.
pub fn assess_probe(probe: Point2, parts: &[Part]) -> Vec<Hazard> {
    parts
        .iter()
        .filter(|part| point_in_box(probe, part) && part.top_z > 0.0)
        .map(|part| Hazard::ZProbeIntoPart {
            part: part.name.clone(),
            probe,
            part_top_z: part.top_z,
        })
        .collect()
}

/// Whether the segment `a`→`b` shares any point with a footprint.
///
/// Public because deriving a passport needs the same question asked
/// about the bed itself: a start-block move that never enters the build
/// area cannot hit anything standing in it, and carrying those in the
/// passport would bury the handful that matter.
pub fn segment_intersects(a: Point2, b: Point2, footprint: &Part) -> bool {
    segment_hits_box(a, b, footprint)
}

fn point_in_box(p: Point2, part: &Part) -> bool {
    p.x >= part.min.x && p.x <= part.max.x && p.y >= part.min.y && p.y <= part.max.y
}

/// Liang–Barsky: does the segment `a`→`b` share any point with the
/// part's footprint? A zero-length segment degenerates correctly into a
/// point-in-box test, which is what we want for a pure-Z move.
fn segment_hits_box(a: Point2, b: Point2, part: &Part) -> bool {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let mut t0 = 0.0f64;
    let mut t1 = 1.0f64;

    let edges = [
        (-dx, a.x - part.min.x),
        (dx, part.max.x - a.x),
        (-dy, a.y - part.min.y),
        (dy, part.max.y - a.y),
    ];

    for (p, q) in edges {
        if p == 0.0 {
            // Parallel to this edge: inside only if it starts inside.
            if q < 0.0 {
                return false;
            }
            continue;
        }
        let r = q / p;
        if p < 0.0 {
            if r > t1 {
                return false;
            }
            t0 = t0.max(r);
        } else {
            if r < t0 {
                return false;
            }
            t1 = t1.min(r);
        }
    }
    t0 <= t1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part() -> Part {
        Part::from_bounds("cup holder", [100.0, 100.0, 0.0], [140.0, 140.0, 18.0])
    }

    fn traverse(from: (f64, f64), to: (f64, f64), z: f64) -> Traverse {
        Traverse {
            from: Point2 {
                x: from.0,
                y: from.1,
            },
            to: Point2 { x: to.0, y: to.1 },
            z,
            source_line: 1,
            raw: "G1".into(),
        }
    }

    #[test]
    fn a_move_below_the_part_top_across_its_footprint_is_a_collision() {
        let hazards = assess(
            &[traverse((0.0, 120.0), (390.0, 120.0), 10.0)],
            &[],
            &[part()],
            DEFAULT_MARGIN_MM,
        );
        assert_eq!(hazards.len(), 1);
        assert_eq!(hazards[0].severity(), Severity::Collision);
    }

    #[test]
    fn the_same_move_well_above_the_part_is_not() {
        let hazards = assess(
            &[traverse((0.0, 120.0), (390.0, 120.0), 30.0)],
            &[],
            &[part()],
            DEFAULT_MARGIN_MM,
        );
        assert!(hazards.is_empty(), "got {hazards:?}");
    }

    #[test]
    fn clearance_inside_the_margin_still_counts_as_a_collision() {
        // 18.5 mm over an 18 mm part is 0.5 mm of clearance: arithmetically
        // clear, physically a scrape.
        let hazards = assess(
            &[traverse((0.0, 120.0), (390.0, 120.0), 18.5)],
            &[],
            &[part()],
            DEFAULT_MARGIN_MM,
        );
        assert_eq!(hazards.len(), 1);
    }

    #[test]
    fn a_move_that_misses_the_footprint_is_ignored_however_low() {
        let hazards = assess(
            &[traverse((0.0, 50.0), (390.0, 50.0), 0.2)],
            &[],
            &[part()],
            DEFAULT_MARGIN_MM,
        );
        assert!(hazards.is_empty(), "got {hazards:?}");
    }

    #[test]
    fn a_pure_z_move_inside_the_footprint_is_caught() {
        // Zero XY length: the Liang-Barsky path has to degenerate to a
        // point-in-box test or this whole class goes unnoticed.
        let hazards = assess(
            &[traverse((120.0, 120.0), (120.0, 120.0), 5.0)],
            &[],
            &[part()],
            DEFAULT_MARGIN_MM,
        );
        assert_eq!(hazards.len(), 1);
    }

    #[test]
    fn probing_z_inside_a_part_is_the_accident_this_module_exists_for() {
        let hazards = assess_probe(Point2 { x: 120.0, y: 120.0 }, &[part()]);
        assert_eq!(hazards.len(), 1);
        assert_eq!(hazards[0].severity(), Severity::Collision);
        assert!(hazards[0].to_string().contains("cup holder"));
    }

    #[test]
    fn probing_z_on_bare_bed_is_fine() {
        assert!(assess_probe(Point2 { x: 10.0, y: 10.0 }, &[part()]).is_empty());
    }

    #[test]
    fn an_unresolved_height_over_a_part_is_reported_as_unknown_not_dropped() {
        let unmodelled = Unmodelled {
            source_line: 117,
            raw: "G0 Y1".into(),
            why: "height is unknown".into(),
            sweep: Some((Point2 { x: 120.0, y: 200.0 }, Point2 { x: 120.0, y: 1.0 })),
            z: None,
        };
        let hazards = assess(&[], &[unmodelled], &[part()], DEFAULT_MARGIN_MM);
        assert_eq!(hazards.len(), 1);
        assert_eq!(hazards[0].severity(), Severity::Unknown);
    }

    /// Position unknown, height known and clearing everything: provably
    /// safe wherever it went. This is what a lift-then-home resume relies
    /// on, and reporting it as unknown would bury the unknowns that
    /// actually need reading.
    #[test]
    fn an_unknown_position_high_above_everything_is_not_a_hazard() {
        let cleared = Unmodelled {
            source_line: 3,
            raw: "G28 X Y".into(),
            why: "XY homing motion is not modelled".into(),
            sweep: None,
            z: Some(40.0),
        };
        assert!(
            assess(&[], &[cleared], &[part()], DEFAULT_MARGIN_MM).is_empty(),
            "40 mm over an 18 mm part cannot hit it wherever it goes"
        );
    }

    /// …and the same move at a height that does *not* clear the part is
    /// still reported, so the rule above cannot be mistaken for "unknown
    /// positions are fine".
    #[test]
    fn an_unknown_position_at_a_height_that_does_not_clear_is_still_a_hazard() {
        let low = Unmodelled {
            source_line: 3,
            raw: "G28 X Y".into(),
            why: "XY homing motion is not modelled".into(),
            sweep: None,
            z: Some(12.0),
        };
        let hazards = assess(&[], &[low], &[part()], DEFAULT_MARGIN_MM);
        assert_eq!(hazards.len(), 1);
        assert_eq!(hazards[0].severity(), Severity::Unknown);
    }

    #[test]
    fn an_unknown_position_is_reported_whenever_the_bed_is_populated() {
        let unmodelled = Unmodelled {
            source_line: 16,
            raw: "G28".into(),
            why: "homing motion is not modelled".into(),
            sweep: None,
            z: None,
        };
        let hazards = assess(&[], &[unmodelled], &[part()], DEFAULT_MARGIN_MM);
        assert_eq!(hazards.len(), 1);
        assert_eq!(hazards[0].severity(), Severity::Unknown);
    }

    #[test]
    fn nothing_on_the_bed_means_nothing_to_hit() {
        let hazards = assess(
            &[traverse((0.0, 120.0), (390.0, 120.0), 0.1)],
            &[],
            &[],
            DEFAULT_MARGIN_MM,
        );
        assert!(hazards.is_empty());
    }
}
