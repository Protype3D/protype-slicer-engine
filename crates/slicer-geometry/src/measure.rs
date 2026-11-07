//! Measurements that answer the questions a bounding box cannot.
//!
//! From the field notes these come from: a mug measures 125x93 overall
//! while the cylinder that actually has to seat in a socket is 52.5x53.0;
//! what a base plate covers is a 63.7 mm circle, not a 66x76 rectangle;
//! and a bore that reads Ø68 in the model had a crossbar poking into it
//! that neither bounds nor volume nor manifoldness could see.
//!
//! Each function here takes a mesh and returns a number about that mesh.

use crate::error::MeshError;
use crate::mesh::TriangleMesh;
use crate::section::{section_at_z, Point2, Section};
use serde::{Deserialize, Serialize};

/// Rays cast around a circle when probing a section. 360 puts one every
/// degree, which on a Ø50 bore samples the wall every 0.44 mm.
pub const DEFAULT_RAY_COUNT: usize = 360;

/// The largest circle centred on an axis that touches no material.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FreeCircle {
    pub z: f64,
    pub axis: Point2,
    pub diameter_mm: f64,
    /// Direction, in degrees CCW from +X, where the nearest material sits.
    pub tightest_at_deg: f64,
    pub ray_count: usize,
    /// Whether material was found in *every* direction.
    ///
    /// Without this the number above is worth nothing in two common
    /// cases, and neither announces itself: an axis in open air beside
    /// the part measures the distance to its outside, and an axis in a
    /// slot open at one end measures the width of the slot. Both come
    /// back as a large, confident, meaningless diameter.
    ///
    /// Learned by walking the pipeline end to end on a real part: a
    /// search for "the biggest clear circle" put the axis outside the
    /// part, and the bore check then passed — credibly, on a
    /// self-tested check — measuring nothing at all.
    pub enclosed: bool,
    /// How many rays left without meeting anything.
    pub escaped_rays: usize,
}

/// The largest circle that fits inside the material of a section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InscribedCircle {
    pub z: f64,
    pub centre: Point2,
    pub diameter_mm: f64,
    /// How close to optimal the search was driven, in mm.
    pub precision_mm: f64,
}

/// How far the silhouette reaches from an axis, at one height.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileSample {
    pub z: f64,
    /// Farthest point of the section from the axis. `None` when the
    /// plane cuts nothing at that height.
    pub max_reach_mm: Option<f64>,
    pub min_reach_mm: Option<f64>,
}

/// Largest circle around `axis` at height `z` that hits nothing — the
/// question "will this actually pass through?".
///
/// Cast rays outward and keep the nearest hit over all of them. Sampling
/// means the result is an upper bound on the true clearance: a spike
/// between two rays is missed. Raise `ray_count` where that matters; the
/// value used is reported back so a caller can say what it measured
/// with.
pub fn free_circle(
    mesh: &TriangleMesh,
    axis: Point2,
    z: f64,
    ray_count: usize,
) -> Result<FreeCircle, MeshError> {
    let section = section_at_z(mesh, z)?;
    free_circle_in(&section, axis, ray_count)
}

pub fn free_circle_in(
    section: &Section,
    axis: Point2,
    ray_count: usize,
) -> Result<FreeCircle, MeshError> {
    let rays = ray_count.max(3);
    // Answering with a number here would be worse than refusing: a circle
    // "around" an axis buried in material is not a thing, and the caller
    // has the axis wrong.
    if section.contains(axis) {
        return Err(MeshError::AxisInsideMaterial {
            x: axis[0],
            y: axis[1],
            z: section.z,
        });
    }

    let segments = section.segments();
    let mut nearest = f64::INFINITY;
    let mut nearest_at = 0.0;
    let mut escaped = 0usize;

    for index in 0..rays {
        let angle = std::f64::consts::TAU * index as f64 / rays as f64;
        let direction = [angle.cos(), angle.sin()];
        // Tracked per ray, not globally: a ray that meets nothing is the
        // signal that this is not a closed void, and folding it into one
        // running minimum throws that away.
        let mut along_this_ray = f64::INFINITY;
        for (a, b) in &segments {
            if let Some(distance) = ray_segment_distance(axis, direction, *a, *b) {
                if distance < along_this_ray {
                    along_this_ray = distance;
                }
            }
        }
        if !along_this_ray.is_finite() {
            escaped += 1;
            continue;
        }
        if along_this_ray < nearest {
            nearest = along_this_ray;
            nearest_at = angle.to_degrees();
        }
    }

    if !nearest.is_finite() {
        return Err(MeshError::NoSection {
            z: section.z,
            min_z: section.z,
            max_z: section.z,
        });
    }

    Ok(FreeCircle {
        z: section.z,
        axis,
        diameter_mm: nearest * 2.0,
        tightest_at_deg: nearest_at,
        ray_count: rays,
        enclosed: escaped == 0,
        escaped_rays: escaped,
    })
}

/// Find the axis of a bore, rather than being told it.
///
/// [`free_circle`] answers about the axis it is given, and loses exactly
/// twice whatever that axis is off by: measured on a real Ø53.72 socket,
/// an axis 0.41 mm off reported Ø52.88, and the part looked 0.83 mm out
/// of spec when nothing was wrong with it. That matters because the case
/// §1 exists for is measuring *someone else's* part, where the axis is
/// exactly what is not known.
///
/// Pattern search from a seed: try eight neighbours at the current step,
/// move to the best, halve the step when none improves. The clear
/// diameter around a point in a void rises smoothly towards the centre,
/// so this converges without needing derivatives.
///
/// Only enclosed candidates are accepted, so the search cannot wander out
/// of the bore and into open air — where the circle is larger and means
/// nothing.
pub fn centre_bore(
    mesh: &TriangleMesh,
    seed: Point2,
    z: f64,
    rays: usize,
    tolerance_mm: f64,
) -> Result<FreeCircle, MeshError> {
    if !(tolerance_mm.is_finite() && tolerance_mm > 0.0) {
        return Err(MeshError::NotFinite {
            field: "tolerance_mm",
            value: tolerance_mm,
        });
    }
    let section = section_at_z(mesh, z)?;
    let mut best = free_circle_in(&section, seed, rays)?;
    let mut step = (best.diameter_mm / 4.0).max(tolerance_mm * 8.0);

    while step > tolerance_mm {
        let mut improved = false;
        for (dx, dy) in [
            (1.0, 0.0),
            (-1.0, 0.0),
            (0.0, 1.0),
            (0.0, -1.0),
            (1.0, 1.0),
            (1.0, -1.0),
            (-1.0, 1.0),
            (-1.0, -1.0),
        ] {
            let candidate = [best.axis[0] + dx * step, best.axis[1] + dy * step];
            let Ok(measured) = free_circle_in(&section, candidate, rays) else {
                continue;
            };
            if measured.enclosed && measured.diameter_mm > best.diameter_mm {
                best = measured;
                improved = true;
            }
        }
        if !improved {
            step /= 2.0;
        }
    }
    Ok(best)
}

/// The narrowest free circle over a Z range — "does it pass through for
/// the whole depth", not "does it pass at one height".
///
/// This is the check that was written after a render showed a crossbar
/// inside a bore: measured at one height the bore was fine.
pub fn free_circle_over(
    mesh: &TriangleMesh,
    axis: Point2,
    z_from: f64,
    z_to: f64,
    step: f64,
    ray_count: usize,
) -> Result<Vec<FreeCircle>, MeshError> {
    if !(step.is_finite() && step > 0.0) {
        return Err(MeshError::BadStep(step));
    }
    let mut out = Vec::new();
    let steps = ((z_to - z_from) / step).floor() as i64;
    for index in 0..=steps.max(0) {
        let z = z_from + step * index as f64;
        match free_circle(mesh, axis, z, ray_count) {
            Ok(circle) => out.push(circle),
            // A height the plane misses is not a failure of the scan.
            Err(MeshError::NoSection { .. }) => continue,
            Err(other) => return Err(other),
        }
    }
    Ok(out)
}

/// Largest circle that fits *inside* the material at height `z`.
///
/// "Pole of inaccessibility" by quadtree subdivision: cells are split
/// most-promising-first, where a cell's promise is its centre's distance
/// to the boundary plus its own half-diagonal. That bound is what makes
/// the result provably within `precision_mm` of optimal rather than
/// merely the best of a grid.
pub fn inscribed_circle(
    mesh: &TriangleMesh,
    z: f64,
    precision_mm: f64,
) -> Result<InscribedCircle, MeshError> {
    let section = section_at_z(mesh, z)?;
    inscribed_circle_in(&section, precision_mm)
}

pub fn inscribed_circle_in(
    section: &Section,
    precision_mm: f64,
) -> Result<InscribedCircle, MeshError> {
    if !(precision_mm.is_finite() && precision_mm > 0.0) {
        return Err(MeshError::NotFinite {
            field: "precision_mm",
            value: precision_mm,
        });
    }
    let segments = section.segments();
    if segments.is_empty() {
        return Err(MeshError::NoSection {
            z: section.z,
            min_z: section.z,
            max_z: section.z,
        });
    }

    let (min, max) = section_bounds(&segments);
    let width = max[0] - min[0];
    let height = max[1] - min[1];
    let cell_size = width.min(height);
    if cell_size <= 0.0 {
        return Err(MeshError::NoSection {
            z: section.z,
            min_z: section.z,
            max_z: section.z,
        });
    }

    // A binary heap, not a linear scan for the maximum: the search
    // evaluates thousands of cells on a part with a small feature inside
    // a large outline (a Ø7 wall in a Ø68 ring), and popping the best of
    // a growing Vec turns that into quadratic time.
    let mut queue: std::collections::BinaryHeap<Cell> = std::collections::BinaryHeap::new();
    let mut x = min[0];
    while x < max[0] {
        let mut y = min[1];
        while y < max[1] {
            let half = cell_size / 2.0;
            queue.push(Cell::new([x + half, y + half], half, &segments));
            y += cell_size;
        }
        x += cell_size;
    }

    let mut best = Cell::new(
        [min[0] + width / 2.0, min[1] + height / 2.0],
        0.0,
        &segments,
    );

    while let Some(cell) = queue.pop() {
        if cell.distance > best.distance {
            best = cell.clone();
        }
        // Nothing inside this cell can beat the incumbent by more than
        // the precision we asked for, so stop splitting it.
        if cell.potential - best.distance <= precision_mm {
            continue;
        }
        let half = cell.half / 2.0;
        for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            queue.push(Cell::new(
                [cell.centre[0] + dx * half, cell.centre[1] + dy * half],
                half,
                &segments,
            ));
        }
    }

    Ok(InscribedCircle {
        z: section.z,
        centre: best.centre,
        diameter_mm: (best.distance.max(0.0)) * 2.0,
        precision_mm,
    })
}

/// How far the silhouette reaches from `axis`, sampled up the Z range.
///
/// This is the measurement that decided a bracket's 61.5 mm reach after
/// two layouts had already been drawn and thrown away: "how far does the
/// body stick out at this height" is not a question a bounding box can
/// be asked.
pub fn profile(
    mesh: &TriangleMesh,
    axis: Point2,
    z_from: f64,
    z_to: f64,
    step: f64,
) -> Result<Vec<ProfileSample>, MeshError> {
    if !(step.is_finite() && step > 0.0) {
        return Err(MeshError::BadStep(step));
    }
    let steps = ((z_to - z_from) / step).floor() as i64;
    let mut out = Vec::new();
    for index in 0..=steps.max(0) {
        let z = z_from + step * index as f64;
        let sample = match section_at_z(mesh, z) {
            Ok(section) => {
                let reaches: Vec<f64> = section
                    .contours
                    .iter()
                    .flat_map(|c| c.points.iter())
                    .map(|p| hypot(p[0] - axis[0], p[1] - axis[1]))
                    .collect();
                ProfileSample {
                    z,
                    max_reach_mm: reaches.iter().copied().reduce(f64::max),
                    min_reach_mm: reaches.iter().copied().reduce(f64::min),
                }
            }
            Err(MeshError::NoSection { .. }) => ProfileSample {
                z,
                max_reach_mm: None,
                min_reach_mm: None,
            },
            Err(other) => return Err(other),
        };
        out.push(sample);
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct Cell {
    centre: Point2,
    half: f64,
    /// Signed distance to the boundary, positive inside material.
    distance: f64,
    /// Best distance any point in this cell could have.
    potential: f64,
}

impl Cell {
    fn new(centre: Point2, half: f64, segments: &[(Point2, Point2)]) -> Self {
        let distance = signed_distance(centre, segments);
        Self {
            centre,
            half,
            distance,
            potential: distance + half * std::f64::consts::SQRT_2,
        }
    }
}

// Ordered by potential so `BinaryHeap` pops the most promising cell.
// `total_cmp` rather than `partial_cmp`: a NaN slipping in would make the
// heap's ordering inconsistent and the pops arbitrary, and silently wrong
// is the one outcome this crate is not allowed to have.
impl PartialEq for Cell {
    fn eq(&self, other: &Self) -> bool {
        self.potential.total_cmp(&other.potential).is_eq()
    }
}
impl Eq for Cell {}
impl PartialOrd for Cell {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Cell {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.potential.total_cmp(&other.potential)
    }
}

fn signed_distance(p: Point2, segments: &[(Point2, Point2)]) -> f64 {
    let nearest = segments
        .iter()
        .map(|(a, b)| point_segment_distance(p, *a, *b))
        .fold(f64::INFINITY, f64::min);
    if crate::section::contains_in(segments, p) {
        nearest
    } else {
        -nearest
    }
}

fn section_bounds(segments: &[(Point2, Point2)]) -> (Point2, Point2) {
    let mut min = [f64::INFINITY; 2];
    let mut max = [f64::NEG_INFINITY; 2];
    for (a, b) in segments {
        for p in [a, b] {
            for axis in 0..2 {
                min[axis] = min[axis].min(p[axis]);
                max[axis] = max[axis].max(p[axis]);
            }
        }
    }
    (min, max)
}

/// Distance from `origin` along `direction` to where it first crosses the
/// segment `a`→`b`, if it does.
pub(crate) fn ray_segment_distance(
    origin: Point2,
    direction: Point2,
    a: Point2,
    b: Point2,
) -> Option<f64> {
    let edge = [b[0] - a[0], b[1] - a[1]];
    let to_start = [a[0] - origin[0], a[1] - origin[1]];
    let denominator = direction[0] * edge[1] - direction[1] * edge[0];
    if denominator.abs() < 1e-12 {
        return None;
    }
    let along_ray = (to_start[0] * edge[1] - to_start[1] * edge[0]) / denominator;
    let along_edge = (to_start[0] * direction[1] - to_start[1] * direction[0]) / denominator;
    (along_ray > 1e-9 && (0.0..=1.0).contains(&along_edge)).then_some(along_ray)
}

fn point_segment_distance(p: Point2, a: Point2, b: Point2) -> f64 {
    let edge = [b[0] - a[0], b[1] - a[1]];
    let length_squared = edge[0] * edge[0] + edge[1] * edge[1];
    if length_squared <= 0.0 {
        return hypot(p[0] - a[0], p[1] - a[1]);
    }
    let t = (((p[0] - a[0]) * edge[0] + (p[1] - a[1]) * edge[1]) / length_squared).clamp(0.0, 1.0);
    hypot(p[0] - (a[0] + t * edge[0]), p[1] - (a[1] + t * edge[1]))
}

fn hypot(x: f64, y: f64) -> f64 {
    (x * x + y * y).sqrt()
}
