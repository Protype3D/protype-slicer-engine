//! Cut a mesh with a plane and chain the result into contours.
//!
//! Everything else in `measure` is built on this: a bore diameter, a
//! silhouette reach and an inscribed circle are all questions about one
//! cross-section. Sectioning the triangles is also the only way to ask
//! them of the mesh rather than of the model that produced it — a
//! crossbar left poking into a bore by a boolean is invisible to bounds
//! and to volume, and unmissable in a section.

use crate::error::MeshError;
use crate::mesh::TriangleMesh;
use serde::{Deserialize, Serialize};

/// Endpoints closer than this are the same point when chaining segments
/// into contours. Loose enough to absorb the interpolation error of two
/// triangles sharing an edge, tight enough not to bridge a real gap.
const CHAIN_TOLERANCE_MM: f64 = 1e-6;

pub type Point2 = [f64; 2];

/// A closed or open loop of the section outline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contour {
    pub points: Vec<Point2>,
    /// False when chaining ran out of segments before returning to the
    /// start. On a watertight mesh every contour closes, so an open one
    /// is a finding about the mesh, not a quirk of this code.
    pub closed: bool,
}

impl Contour {
    /// Twice the signed area. Positive is counter-clockwise, which for a
    /// consistently wound solid means an outer boundary; negative means
    /// a hole.
    pub fn signed_area(&self) -> f64 {
        let n = self.points.len();
        if n < 3 {
            return 0.0;
        }
        let mut sum = 0.0;
        for index in 0..n {
            let a = self.points[index];
            let b = self.points[(index + 1) % n];
            sum += a[0] * b[1] - b[0] * a[1];
        }
        sum / 2.0
    }

    pub fn is_hole(&self) -> bool {
        self.signed_area() < 0.0
    }
}

/// A horizontal cut, which is the one printing cares about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub z: f64,
    pub contours: Vec<Contour>,
}

impl Section {
    pub fn is_empty(&self) -> bool {
        self.contours.iter().all(|c| c.points.len() < 3)
    }

    /// Every edge of every contour, as segments. The form the ray-based
    /// measurements want.
    pub fn segments(&self) -> Vec<(Point2, Point2)> {
        let mut out = Vec::new();
        for contour in &self.contours {
            let n = contour.points.len();
            if n < 2 {
                continue;
            }
            let last = if contour.closed { n } else { n - 1 };
            for index in 0..last {
                out.push((contour.points[index], contour.points[(index + 1) % n]));
            }
        }
        out
    }

    /// Is this point inside material?
    ///
    /// Convenience over [`contains_in`]; rebuilds the segment list on
    /// every call, so anything asking this thousands of times — the
    /// inscribed-circle search does — should hoist [`Section::segments`]
    /// out and call [`contains_in`] instead.
    pub fn contains(&self, p: Point2) -> bool {
        contains_in(&self.segments(), p)
    }
}

/// Even-odd crossing count against a prepared segment list.
///
/// Counting over every contour at once handles holes without needing to
/// know which contour is whose: a point in a hole crosses the outer
/// boundary and the hole boundary, an even number, and reads as
/// outside — correctly.
pub fn contains_in(segments: &[(Point2, Point2)], p: Point2) -> bool {
    let mut inside = false;
    for (a, b) in segments {
        let straddles = (a[1] > p[1]) != (b[1] > p[1]);
        if !straddles {
            continue;
        }
        let t = (p[1] - a[1]) / (b[1] - a[1]);
        if p[0] < a[0] + t * (b[0] - a[0]) {
            inside = !inside;
        }
    }
    inside
}

/// Cut the mesh at height `z`.
///
/// Vertices lying exactly on the plane are treated as below it, so every
/// crossed triangle yields exactly two intersection points and a
/// triangle merely touching the plane yields none. That is the standard
/// convention and it keeps a flat top or bottom face — where every
/// vertex is on the plane — from erupting into degenerate segments.
pub fn section_at_z(mesh: &TriangleMesh, z: f64) -> Result<Section, MeshError> {
    if !z.is_finite() {
        return Err(MeshError::NotFinite {
            field: "z",
            value: z,
        });
    }
    let (min, max) = mesh.bounds().ok_or(MeshError::Empty)?;
    if z < min[2] || z > max[2] {
        return Err(MeshError::NoSection {
            z,
            min_z: min[2],
            max_z: max[2],
        });
    }

    let mut segments: Vec<(Point2, Point2)> = Vec::new();
    for tri in &mesh.triangles {
        let distances = [tri[0][2] - z, tri[1][2] - z, tri[2][2] - z];
        let above = [distances[0] > 0.0, distances[1] > 0.0, distances[2] > 0.0];
        let count = above.iter().filter(|a| **a).count();
        if count == 0 || count == 3 {
            continue;
        }
        let mut crossings: Vec<Point2> = Vec::with_capacity(2);
        for index in 0..3 {
            let next = (index + 1) % 3;
            if above[index] == above[next] {
                continue;
            }
            let (di, dj) = (distances[index], distances[next]);
            let denominator = di - dj;
            // The pair straddles the plane, so the denominator cannot be
            // zero — but guard rather than trust, since a NaN here would
            // travel silently into every downstream measurement.
            let t = if denominator.abs() > f64::EPSILON {
                di / denominator
            } else {
                0.0
            };
            crossings.push([
                tri[index][0] + (tri[next][0] - tri[index][0]) * t,
                tri[index][1] + (tri[next][1] - tri[index][1]) * t,
            ]);
        }
        if crossings.len() == 2 && !same_point(crossings[0], crossings[1]) {
            segments.push((crossings[0], crossings[1]));
        }
    }

    Ok(Section {
        z,
        contours: chain(segments),
    })
}

/// Join loose segments end-to-end into contours.
fn chain(segments: Vec<(Point2, Point2)>) -> Vec<Contour> {
    let mut remaining: Vec<Option<(Point2, Point2)>> = segments.into_iter().map(Some).collect();
    let mut contours = Vec::new();

    for start in 0..remaining.len() {
        let Some((first, second)) = remaining[start].take() else {
            continue;
        };
        let mut points = vec![first, second];
        let mut closed = false;

        loop {
            let tail = *points.last().expect("seeded with two points");
            if same_point(tail, points[0]) && points.len() > 2 {
                points.pop();
                closed = true;
                break;
            }
            let mut extended = false;
            for slot in remaining.iter_mut() {
                let Some((a, b)) = *slot else { continue };
                if same_point(a, tail) {
                    points.push(b);
                } else if same_point(b, tail) {
                    points.push(a);
                } else {
                    continue;
                }
                *slot = None;
                extended = true;
                break;
            }
            if !extended {
                break;
            }
        }
        contours.push(Contour { points, closed });
    }
    contours
}

fn same_point(a: Point2, b: Point2) -> bool {
    (a[0] - b[0]).abs() <= CHAIN_TOLERANCE_MM && (a[1] - b[1]).abs() <= CHAIN_TOLERANCE_MM
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::CsgNode;

    fn mesh_of(tree: &CsgNode) -> TriangleMesh {
        let built = crate::build_stl(tree).expect("builds");
        TriangleMesh::from_stl_bytes(&built.stl).expect("reads")
    }

    fn tube(outer: f64, inner: f64, height: f64) -> CsgNode {
        CsgNode::Difference {
            children: vec![
                CsgNode::Cylinder {
                    diameter: outer,
                    diameter_top: None,
                    height,
                    segments: 128,
                },
                CsgNode::Cylinder {
                    diameter: inner,
                    diameter_top: None,
                    height: height + 2.0,
                    segments: 128,
                },
            ],
        }
    }

    #[test]
    fn a_cube_sections_into_one_closed_square() {
        let mesh = mesh_of(&CsgNode::Box {
            size: [10.0, 6.0, 4.0],
        });
        let section = section_at_z(&mesh, 2.0).expect("cuts");
        assert_eq!(section.contours.len(), 1);
        assert!(section.contours[0].closed);
        assert!((section.contours[0].signed_area().abs() - 60.0).abs() < 1e-6);
    }

    #[test]
    fn a_tube_sections_into_an_outer_ring_and_a_hole() {
        let mesh = mesh_of(&tube(20.0, 10.0, 8.0));
        let section = section_at_z(&mesh, 4.0).expect("cuts");
        assert_eq!(section.contours.len(), 2, "outer wall and bore");
        assert!(section.contours.iter().all(|c| c.closed));
        assert_eq!(
            section.contours.iter().filter(|c| c.is_hole()).count(),
            1,
            "exactly one contour is wound as a hole"
        );
    }

    #[test]
    fn a_point_in_the_bore_reads_as_outside_material() {
        let mesh = mesh_of(&tube(20.0, 10.0, 8.0));
        let section = section_at_z(&mesh, 4.0).unwrap();
        assert!(!section.contains([0.0, 0.0]), "the bore centre is void");
        assert!(section.contains([7.0, 0.0]), "7 mm out is in the wall");
        assert!(!section.contains([50.0, 0.0]), "well outside the part");
    }

    #[test]
    fn cutting_outside_the_z_range_says_so_instead_of_returning_nothing() {
        let mesh = mesh_of(&CsgNode::Box {
            size: [10.0, 10.0, 4.0],
        });
        let err = section_at_z(&mesh, 99.0).expect_err("nothing to cut");
        assert!(matches!(err, MeshError::NoSection { .. }));
        assert!(err.to_string().contains("99"));
    }

    #[test]
    fn a_torn_mesh_yields_an_open_contour_rather_than_a_silent_wrong_answer() {
        let mut mesh = mesh_of(&CsgNode::Box {
            size: [10.0, 10.0, 10.0],
        });
        // Drop the triangles forming one side wall, leaving a gap the
        // chainer cannot close.
        mesh.triangles.retain(|t| t.iter().any(|v| v[0] > 1e-9));
        let section = section_at_z(&mesh, 5.0).expect("still cuts");
        assert!(
            section.contours.iter().any(|c| !c.closed),
            "a hole in the surface must surface as an open contour"
        );
    }

    #[test]
    fn a_section_flush_with_the_top_face_is_not_a_spray_of_degenerate_segments() {
        let mesh = mesh_of(&CsgNode::Box {
            size: [10.0, 10.0, 4.0],
        });
        let section = section_at_z(&mesh, 4.0).expect("cuts at the very top");
        assert!(
            section.is_empty(),
            "the top face lies in the plane and contributes nothing, got {:?}",
            section.contours
        );
    }
}
