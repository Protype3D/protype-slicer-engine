//! Everywhere the nozzle is driven down to touch the bed.
//!
//! The first version of this crate modelled one probe point, because
//! that is what the incident report named: `G28` homes Z at the centre of
//! the bed. Reading the machine's console turned up three more, and one
//! of them is worse than the original:
//!
//! * `G28` — one point, the bed centre.
//! * `Z_TILT_ADJUST` — two fixed points at the left and right edges.
//! * `probe samples=1` — wherever the toolhead happens to be standing.
//! * `G29` — this is the dangerous one. With `bedmesh_before_print = 1`
//!   it runs KAMP's `BED_MESH_CALIBRATE`, which fits its mesh **to the
//!   bounding box of the objects being printed** and probes a grid across
//!   it. On a resume, the objects being printed are exactly the part
//!   already standing there, so it probes a grid straight onto it.
//!
//! A single-point model says a resume is safe as long as the part misses
//! the bed centre. It is not: `G29` will walk a grid over the part's own
//! footprint. Modelling probes as *regions* is what makes that visible.

use crate::passport::Point2;
use serde::{Deserialize, Serialize};

/// Where an operation's touchdowns land.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeRegion {
    /// A fixed set of touchdown points.
    Points { at: Vec<Point2> },
    /// A fixed rectangular grid.
    Grid {
        min: Point2,
        max: Point2,
        count_x: u32,
        count_y: u32,
    },
    /// A grid fitted to the area being printed: the objects' bounding box
    /// grown by `margin_mm`, never smaller than `min_size_mm`, clamped
    /// into `clamp_min`..`clamp_max`. With no objects declared it falls
    /// back to the whole clamp rectangle.
    AdaptiveToPrintArea {
        clamp_min: Point2,
        clamp_max: Point2,
        margin_mm: f64,
        min_size_mm: [f64; 2],
    },
    /// Probes wherever the toolhead is standing, which this crate cannot
    /// know from the command alone.
    AtCurrentPosition,
}

/// One command that touches the bed, and the evidence for what it does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeOperation {
    /// The command as it appears in G-code.
    pub command: String,
    pub what: String,
    /// The observation this is derived from. Same rule as everywhere
    /// else in the passport: not a manual, not an expectation.
    pub derivation: String,
    pub region: ProbeRegion,
}

/// What a probe operation actually covers, once the print area is known.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeCoverage {
    /// Exact touchdown points.
    Points(Vec<Point2>),
    /// Any point in this rectangle may be touched down on. Conservative
    /// by construction: used where the exact grid depends on runtime
    /// values this crate does not have.
    Region(Point2, Point2),
    /// Cannot be determined from the command.
    Unknown,
}

/// Resolve an operation's coverage.
///
/// `print_area` is the bounding box of what the job prints — for a
/// resume, that is the part already on the bed. `None` means it was not
/// declared, which for an adaptive mesh is the *worse* case, not a safer
/// one: KAMP falls back to meshing the entire clamp rectangle.
pub fn coverage(region: &ProbeRegion, print_area: Option<(Point2, Point2)>) -> ProbeCoverage {
    match region {
        ProbeRegion::Points { at } => ProbeCoverage::Points(at.clone()),
        ProbeRegion::Grid {
            min,
            max,
            count_x,
            count_y,
        } => ProbeCoverage::Points(grid_points(*min, *max, *count_x, *count_y)),
        ProbeRegion::AdaptiveToPrintArea {
            clamp_min,
            clamp_max,
            margin_mm,
            min_size_mm,
        } => {
            let (mut lo, mut hi) = match print_area {
                Some(area) => area,
                // No objects declared: KAMP says so on the console and
                // meshes the whole bed.
                None => return ProbeCoverage::Region(*clamp_min, *clamp_max),
            };
            lo = Point2 {
                x: lo.x - margin_mm,
                y: lo.y - margin_mm,
            };
            hi = Point2 {
                x: hi.x + margin_mm,
                y: hi.y + margin_mm,
            };
            // Grown to the minimum region about its own centre, then
            // clamped — the order the macro uses.
            let grow = |low: &mut f64, high: &mut f64, minimum: f64| {
                let shortfall = minimum - (*high - *low);
                if shortfall > 0.0 {
                    *low -= shortfall / 2.0;
                    *high += shortfall / 2.0;
                }
            };
            grow(&mut lo.x, &mut hi.x, min_size_mm[0]);
            grow(&mut lo.y, &mut hi.y, min_size_mm[1]);
            lo.x = lo.x.max(clamp_min.x);
            lo.y = lo.y.max(clamp_min.y);
            hi.x = hi.x.min(clamp_max.x);
            hi.y = hi.y.min(clamp_max.y);
            ProbeCoverage::Region(lo, hi)
        }
        ProbeRegion::AtCurrentPosition => ProbeCoverage::Unknown,
    }
}

fn grid_points(min: Point2, max: Point2, count_x: u32, count_y: u32) -> Vec<Point2> {
    let step = |lo: f64, hi: f64, n: u32| -> Vec<f64> {
        if n <= 1 {
            return vec![lo];
        }
        (0..n)
            .map(|i| lo + (hi - lo) * i as f64 / (n - 1) as f64)
            .collect()
    };
    let xs = step(min.x, max.x, count_x);
    let ys = step(min.y, max.y, count_y);
    ys.iter()
        .flat_map(|y| xs.iter().map(move |x| Point2 { x: *x, y: *y }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adaptive() -> ProbeRegion {
        ProbeRegion::AdaptiveToPrintArea {
            clamp_min: Point2 { x: 15.0, y: 15.0 },
            clamp_max: Point2 { x: 375.0, y: 375.0 },
            margin_mm: 5.0,
            min_size_mm: [90.0, 90.0],
        }
    }

    #[test]
    fn a_fixed_grid_expands_to_its_points() {
        let region = ProbeRegion::Grid {
            min: Point2 { x: 0.0, y: 0.0 },
            max: Point2 { x: 10.0, y: 10.0 },
            count_x: 3,
            count_y: 3,
        };
        let ProbeCoverage::Points(points) = coverage(&region, None) else {
            panic!("expected points");
        };
        assert_eq!(points.len(), 9);
        assert!(points.contains(&Point2 { x: 5.0, y: 5.0 }));
        assert!(points.contains(&Point2 { x: 10.0, y: 10.0 }));
    }

    /// The finding this module exists for: the adaptive mesh follows the
    /// print area, so on a resume it probes the standing part.
    #[test]
    fn the_adaptive_mesh_follows_the_print_area() {
        let part = (Point2 { x: 150.0, y: 150.0 }, Point2 { x: 240.0, y: 240.0 });
        let ProbeCoverage::Region(lo, hi) = coverage(&adaptive(), Some(part)) else {
            panic!("expected a region");
        };
        // 90 mm wide plus 5 mm of margin each side, already over the
        // 90 mm minimum, so the margin is the only growth.
        assert_eq!(lo, Point2 { x: 145.0, y: 145.0 });
        assert_eq!(hi, Point2 { x: 245.0, y: 245.0 });
    }

    #[test]
    fn a_small_part_still_gets_the_minimum_region() {
        let small = (Point2 { x: 190.0, y: 190.0 }, Point2 { x: 200.0, y: 200.0 });
        let ProbeCoverage::Region(lo, hi) = coverage(&adaptive(), Some(small)) else {
            panic!("expected a region");
        };
        assert!(
            (hi.x - lo.x - 90.0).abs() < 1e-9,
            "a 10 mm part still meshes 90 mm: {} .. {}",
            lo.x,
            hi.x
        );
    }

    #[test]
    fn the_region_is_clamped_to_what_the_machine_will_probe() {
        let edge = (Point2 { x: 0.0, y: 0.0 }, Point2 { x: 400.0, y: 400.0 });
        let ProbeCoverage::Region(lo, hi) = coverage(&adaptive(), Some(edge)) else {
            panic!("expected a region");
        };
        assert_eq!(lo, Point2 { x: 15.0, y: 15.0 });
        assert_eq!(hi, Point2 { x: 375.0, y: 375.0 });
    }

    /// Not declaring the print area is the *worse* case, not a safer one.
    #[test]
    fn an_undeclared_print_area_means_the_whole_bed_is_meshed() {
        let ProbeCoverage::Region(lo, hi) = coverage(&adaptive(), None) else {
            panic!("expected a region");
        };
        assert_eq!(lo, Point2 { x: 15.0, y: 15.0 });
        assert_eq!(hi, Point2 { x: 375.0, y: 375.0 });
    }

    #[test]
    fn probing_at_the_current_position_is_not_guessed_at() {
        assert_eq!(
            coverage(&ProbeRegion::AtCurrentPosition, None),
            ProbeCoverage::Unknown
        );
    }
}
