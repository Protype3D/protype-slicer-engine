//! A triangle soup, loaded from STL, and the facts derivable from it
//! alone.
//!
//! The point of this module is the thing it *cannot* do: answer from
//! anything except the triangles. A bore diameter reported from the
//! parameter that generated it will happily report 68 mm for a bore a
//! boolean operation left a crossbar sticking into. Everything here
//! starts from vertices.

use std::collections::HashMap;

use crate::error::MeshError;

/// Distance below which two vertices are the same vertex.
///
/// Binary STL stores `f32`, whose spacing near 100 mm is about
/// 7.6e-6 mm, so the same logical corner written by two different
/// computations can differ in the last bit or two. Welding at 1e-4 mm
/// absorbs that while staying four orders of magnitude below anything a
/// 0.4 mm nozzle can express.
pub const WELD_TOLERANCE_MM: f64 = 1e-4;

pub type Triangle = [[f64; 3]; 3];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TriangleMesh {
    pub triangles: Vec<Triangle>,
}

/// What [`TriangleMesh::measure`] answers: the cheap facts, all of them
/// read off the triangles.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Measurement {
    pub triangle_count: usize,
    pub bounds_min: [f64; 3],
    pub bounds_max: [f64; 3],
    pub size: [f64; 3],
    /// Signed volume of the enclosed region, in mm³. Negative means the
    /// surface is wound inside-out.
    pub volume_mm3: f64,
    pub surface_area_mm2: f64,
    /// Directed edges that do not appear exactly once after welding at
    /// [`WELD_TOLERANCE_MM`]. Zero on a closed, consistently oriented
    /// surface.
    pub unpaired_edges: usize,
    pub is_manifold: bool,
    pub weld_tolerance_mm: f64,
}

impl TriangleMesh {
    pub fn new(triangles: Vec<Triangle>) -> Self {
        Self { triangles }
    }

    /// Load from a `.stl` file, binary or ASCII.
    pub fn from_stl_path(path: impl AsRef<std::path::Path>) -> Result<Self, MeshError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| MeshError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_stl_bytes(&bytes)
    }

    /// Load from STL bytes, choosing binary or ASCII by the only reliable
    /// tell: whether the declared triangle count accounts for the file
    /// length exactly. The `solid` prefix is not a tell — plenty of
    /// binary writers stamp it into the 80-byte header.
    pub fn from_stl_bytes(bytes: &[u8]) -> Result<Self, MeshError> {
        if bytes.len() >= 84 {
            let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
            if 84 + count * 50 == bytes.len() {
                return Ok(Self::new(parse_binary(bytes, count)));
            }
        }
        parse_ascii(bytes).map(Self::new)
    }

    pub fn is_empty(&self) -> bool {
        self.triangles.is_empty()
    }

    pub fn bounds(&self) -> Option<([f64; 3], [f64; 3])> {
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for tri in &self.triangles {
            for vertex in tri {
                for axis in 0..3 {
                    min[axis] = min[axis].min(vertex[axis]);
                    max[axis] = max[axis].max(vertex[axis]);
                }
            }
        }
        min[0].is_finite().then_some((min, max))
    }

    /// The same triangles, moved by `offset`.
    pub fn translated(&self, offset: [f64; 3]) -> Self {
        Self::new(
            self.triangles
                .iter()
                .map(|tri| tri.map(|v| [v[0] + offset[0], v[1] + offset[1], v[2] + offset[2]]))
                .collect(),
        )
    }

    /// Signed volume by the divergence theorem: the sum of the signed
    /// tetrahedra spanned by each triangle and the origin. Exact for a
    /// closed surface regardless of where the origin sits.
    pub fn volume_mm3(&self) -> f64 {
        self.triangles
            .iter()
            .map(|[a, b, c]| {
                let cross = [
                    b[1] * c[2] - b[2] * c[1],
                    b[2] * c[0] - b[0] * c[2],
                    b[0] * c[1] - b[1] * c[0],
                ];
                a[0] * cross[0] + a[1] * cross[1] + a[2] * cross[2]
            })
            .sum::<f64>()
            / 6.0
    }

    pub fn surface_area_mm2(&self) -> f64 {
        self.triangles
            .iter()
            .map(|[a, b, c]| {
                let u = sub(*b, *a);
                let v = sub(*c, *a);
                norm(cross(u, v)) / 2.0
            })
            .sum()
    }

    /// Count directed edges that are not properly paired, on a file
    /// whose vertices arrive as repeated coordinates rather than shared
    /// indices and so must be welded first.
    pub fn unpaired_edges(&self) -> usize {
        let welded = self.weld();
        count_unpaired((0..self.triangles.len()).map(|index| {
            [
                welded[index * 3],
                welded[index * 3 + 1],
                welded[index * 3 + 2],
            ]
        }))
    }

    /// The surface as shared vertices and the triangles between them, welded
    /// as [`Self::unpaired_edges`] welds it. A triangle the weld collapsed onto
    /// an edge encloses nothing, and is left out: kept, it would stand on an
    /// edge its neighbours already share.
    pub(crate) fn indexed(&self) -> (Vec<[f64; 3]>, Vec<[usize; 3]>) {
        let mut shared: HashMap<usize, usize> = HashMap::new();
        let mut vertices: Vec<[f64; 3]> = Vec::new();
        let triangles = self
            .weld()
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|[a, b, c]| a != b && b != c && c != a)
            .map(|tri| {
                tri.map(|welded| {
                    *shared.entry(welded).or_insert_with(|| {
                        vertices.push(self.triangles[welded / 3][welded % 3]);
                        vertices.len() - 1
                    })
                })
            })
            .collect();
        (vertices, triangles)
    }

    pub fn measure(&self) -> Result<Measurement, MeshError> {
        let (bounds_min, bounds_max) = self.bounds().ok_or(MeshError::Empty)?;
        let unpaired = self.unpaired_edges();
        Ok(Measurement {
            triangle_count: self.triangles.len(),
            bounds_min,
            bounds_max,
            size: [
                bounds_max[0] - bounds_min[0],
                bounds_max[1] - bounds_min[1],
                bounds_max[2] - bounds_min[2],
            ],
            volume_mm3: self.volume_mm3(),
            surface_area_mm2: self.surface_area_mm2(),
            unpaired_edges: unpaired,
            is_manifold: unpaired == 0,
            weld_tolerance_mm: WELD_TOLERANCE_MM,
        })
    }

    /// Map every vertex occurrence to a representative index, merging
    /// vertices within [`WELD_TOLERANCE_MM`].
    ///
    /// Bucketed on a grid of that size and searching the 27 neighbouring
    /// cells, rather than by quantising alone: two vertices a nanometre
    /// apart can straddle a cell boundary, and pure quantisation would
    /// then report a perfectly closed mesh as torn.
    fn weld(&self) -> Vec<usize> {
        let points: Vec<[f64; 3]> = self.triangles.iter().flatten().copied().collect();
        let mut buckets: HashMap<[i64; 3], Vec<usize>> = HashMap::new();
        let mut representative = vec![0usize; points.len()];

        for (index, point) in points.iter().enumerate() {
            let cell = grid_cell(*point);
            let mut found = None;
            'search: for dx in -1..=1 {
                for dy in -1..=1 {
                    for dz in -1..=1 {
                        let neighbour = [cell[0] + dx, cell[1] + dy, cell[2] + dz];
                        let Some(candidates) = buckets.get(&neighbour) else {
                            continue;
                        };
                        for &candidate in candidates {
                            if distance(points[candidate], *point) <= WELD_TOLERANCE_MM {
                                found = Some(candidate);
                                break 'search;
                            }
                        }
                    }
                }
            }
            match found {
                Some(existing) => representative[index] = existing,
                None => {
                    representative[index] = index;
                    buckets.entry(cell).or_default().push(index);
                }
            }
        }
        representative
    }
}

/// Count directed edges that are not part of exactly one opposed pair.
///
/// On a closed, consistently oriented surface every directed edge `(u,v)`
/// occurs exactly once and its reverse `(v,u)` occurs exactly once. Both
/// halves of that matter, and the second is easy to leave out:
///
/// * checking only for **duplicates** catches a doubled or inconsistently
///   wound face, but not a hole — deleting a triangle removes its own
///   directed edges without changing anyone else's count, so a mesh with
///   a face missing passes a duplicates-only test cleanly;
/// * checking only for **presence of the reverse** would miss the
///   duplicates.
///
/// So both are required here. Shared with [`crate::eval`], which asks the
/// same question of kernel output keyed on vertex bits instead of welded
/// indices.
pub(crate) fn count_unpaired<K>(triangles: impl Iterator<Item = [K; 3]>) -> usize
where
    K: Eq + std::hash::Hash + Copy,
{
    let mut seen: HashMap<(K, K), u32> = HashMap::new();
    for [a, b, c] in triangles {
        for (u, v) in [(a, b), (b, c), (c, a)] {
            *seen.entry((u, v)).or_insert(0) += 1;
        }
    }
    seen.iter()
        .filter(|((u, v), count)| **count != 1 || seen.get(&(*v, *u)).copied().unwrap_or(0) != 1)
        .count()
}

fn grid_cell(p: [f64; 3]) -> [i64; 3] {
    [
        (p[0] / WELD_TOLERANCE_MM).floor() as i64,
        (p[1] / WELD_TOLERANCE_MM).floor() as i64,
        (p[2] / WELD_TOLERANCE_MM).floor() as i64,
    ]
}

fn parse_binary(bytes: &[u8], count: usize) -> Vec<Triangle> {
    (0..count)
        .map(|index| {
            let at = 84 + index * 50;
            let coordinate = |slot: usize| -> f64 {
                // Skip the 12-byte facet normal: it is advisory, often
                // wrong, and every consumer worth trusting recomputes it
                // from the winding.
                let start = at + 12 + slot * 4;
                f32::from_le_bytes([
                    bytes[start],
                    bytes[start + 1],
                    bytes[start + 2],
                    bytes[start + 3],
                ]) as f64
            };
            [
                [coordinate(0), coordinate(1), coordinate(2)],
                [coordinate(3), coordinate(4), coordinate(5)],
                [coordinate(6), coordinate(7), coordinate(8)],
            ]
        })
        .collect()
}

fn parse_ascii(bytes: &[u8]) -> Result<Vec<Triangle>, MeshError> {
    let text = std::str::from_utf8(bytes).map_err(|_| MeshError::NotStl)?;
    let mut vertices: Vec<[f64; 3]> = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("vertex") {
            continue;
        }
        let mut coordinates = [0.0f64; 3];
        for slot in &mut coordinates {
            *slot = words
                .next()
                .and_then(|w| w.parse::<f64>().ok())
                .ok_or(MeshError::NotStl)?;
        }
        vertices.push(coordinates);
    }
    if vertices.is_empty() || !vertices.len().is_multiple_of(3) {
        return Err(MeshError::NotStl);
    }
    Ok(vertices.as_chunks::<3>().0.to_vec())
}

pub(crate) fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub(crate) fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub(crate) fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub(crate) fn norm(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    norm(sub(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit cube as raw triangles, wound outward. Written by hand
    /// rather than generated, so these tests do not depend on the kernel
    /// they are meant to be able to catch.
    fn unit_cube() -> TriangleMesh {
        let v = |x: f64, y: f64, z: f64| [x, y, z];
        let quad = |a, b, c, d| vec![[a, b, c], [a, c, d]];
        let mut triangles = Vec::new();
        // bottom (z=0), wound clockwise seen from outside (below)
        triangles.extend(quad(
            v(0., 0., 0.),
            v(0., 1., 0.),
            v(1., 1., 0.),
            v(1., 0., 0.),
        ));
        // top (z=1)
        triangles.extend(quad(
            v(0., 0., 1.),
            v(1., 0., 1.),
            v(1., 1., 1.),
            v(0., 1., 1.),
        ));
        // y=0
        triangles.extend(quad(
            v(0., 0., 0.),
            v(1., 0., 0.),
            v(1., 0., 1.),
            v(0., 0., 1.),
        ));
        // y=1
        triangles.extend(quad(
            v(0., 1., 0.),
            v(0., 1., 1.),
            v(1., 1., 1.),
            v(1., 1., 0.),
        ));
        // x=0
        triangles.extend(quad(
            v(0., 0., 0.),
            v(0., 0., 1.),
            v(0., 1., 1.),
            v(0., 1., 0.),
        ));
        // x=1
        triangles.extend(quad(
            v(1., 0., 0.),
            v(1., 1., 0.),
            v(1., 1., 1.),
            v(1., 0., 1.),
        ));
        TriangleMesh::new(triangles)
    }

    #[test]
    fn a_hand_written_cube_measures_as_a_cube() {
        let m = unit_cube().measure().expect("measurable");
        assert_eq!(m.triangle_count, 12);
        assert_eq!(m.bounds_min, [0.0, 0.0, 0.0]);
        assert_eq!(m.bounds_max, [1.0, 1.0, 1.0]);
        assert!((m.volume_mm3 - 1.0).abs() < 1e-12, "got {}", m.volume_mm3);
        assert!((m.surface_area_mm2 - 6.0).abs() < 1e-12);
        assert!(m.is_manifold, "{} unpaired edges", m.unpaired_edges);
    }

    #[test]
    fn translating_moves_every_vertex_and_leaves_the_original_alone() {
        let cube = unit_cube();

        let moved = cube.translated([10.0, -2.0, 0.5]);

        assert_eq!(moved.bounds(), Some(([10.0, -2.0, 0.5], [11.0, -1.0, 1.5])));
        assert_eq!(moved.triangles.len(), cube.triangles.len());
        assert_eq!(cube.bounds(), Some(([0.0; 3], [1.0; 3])));
    }

    #[test]
    fn removing_a_face_makes_the_mesh_non_manifold() {
        let mut mesh = unit_cube();
        mesh.triangles.truncate(10);
        let m = mesh.measure().unwrap();
        assert!(!m.is_manifold);
        assert!(m.unpaired_edges > 0);
    }

    #[test]
    fn welding_survives_vertices_that_straddle_a_grid_cell_boundary() {
        // Nudge every vertex by just under the weld tolerance, in a
        // direction that lands some of them in a neighbouring cell. Pure
        // quantisation reports this closed mesh as torn.
        let mut mesh = unit_cube();
        for (index, tri) in mesh.triangles.iter_mut().enumerate() {
            let nudge = if index % 2 == 0 { 1.0 } else { -1.0 } * WELD_TOLERANCE_MM * 0.4;
            for vertex in tri.iter_mut() {
                vertex[0] += nudge;
            }
        }
        assert_eq!(
            mesh.unpaired_edges(),
            0,
            "sub-tolerance jitter must not read as a hole"
        );
    }

    #[test]
    fn a_binary_stl_round_trips_through_the_writer() {
        let cube = unit_cube();
        let solid = crate::build_stl(&crate::CsgNode::Box {
            size: [2.0, 3.0, 4.0],
        })
        .expect("kernel builds a box");
        let reloaded = TriangleMesh::from_stl_bytes(&solid.stl).expect("reads its own output");
        let m = reloaded.measure().unwrap();
        assert_eq!(m.size, [2.0, 3.0, 4.0]);
        assert!((m.volume_mm3 - 24.0).abs() < 1e-6, "got {}", m.volume_mm3);
        assert!(m.is_manifold);
        let _ = cube;
    }

    #[test]
    fn an_ascii_stl_parses() {
        let text = "solid t\nfacet normal 0 0 1\nouter loop\n\
                    vertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\n\
                    endloop\nendfacet\nendsolid t\n";
        let mesh = TriangleMesh::from_stl_bytes(text.as_bytes()).expect("ascii parses");
        assert_eq!(mesh.triangles.len(), 1);
        assert_eq!(mesh.triangles[0][1], [1.0, 0.0, 0.0]);
    }

    #[test]
    fn a_binary_header_saying_solid_is_still_read_as_binary() {
        // The `solid` prefix is not a format tell. Build a binary file
        // whose header starts with it and check the length rule wins.
        let mut bytes = vec![0u8; 80];
        bytes[..5].copy_from_slice(b"solid");
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 12]); // normal
        for value in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&0u16.to_le_bytes());

        let mesh = TriangleMesh::from_stl_bytes(&bytes).expect("binary parses");
        assert_eq!(mesh.triangles.len(), 1);
    }

    #[test]
    fn garbage_is_an_error_rather_than_an_empty_mesh() {
        let err = TriangleMesh::from_stl_bytes(b"this is not a mesh").expect_err("must refuse");
        assert!(matches!(err, MeshError::NotStl));
    }

    #[test]
    fn an_empty_mesh_cannot_be_measured() {
        let err = TriangleMesh::default()
            .measure()
            .expect_err("nothing to measure");
        assert!(matches!(err, MeshError::Empty));
    }
}
