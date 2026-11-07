//! The flat faces of a mesh that came with no B-rep.
//!
//! A STEP part knows its faces. An STL has only triangles, and whoever was
//! handed one still has to find the face to drill into. So the faces are
//! grown back from the triangles: a seed, and every neighbour lying in the
//! seed's plane.
//!
//! Each neighbour is judged against the seed, never against the triangle that
//! reached it. Judged neighbour by neighbour, a finely tessellated cylinder is
//! one long flat face, each step being too small to object to.

use std::collections::{BTreeMap, HashMap};

use crate::mesh::{cross, dot, norm, sub, TriangleMesh, WELD_TOLERANCE_MM};

/// A triangle further from the seed's plane than this, at any corner, is not
/// on it. STL stores `f32`, whose spacing near 100 mm is about 7.6e-6 mm, so
/// this leaves room for a few bits of noise at any reasonable size.
const MAX_PLANE_DISTANCE_MM: f64 = 5e-4;

/// Cosine of the largest angle between a triangle's normal and the seed's: 0.1°,
/// less than one step around any cylinder a printer can tell from a polygon.
const MIN_NORMAL_COSINE: f64 = 0.999_998_5;

/// A triangle narrower than this has a normal that is mostly rounding noise,
/// so only its distance from the plane is asked.
const MIN_RELIABLE_ALTITUDE_MM: f64 = 0.01;

/// Sine of the turn below which a corner of an outline is not one: a point
/// that only splits a straight edge, as triangulation leaves behind.
const MIN_CORNER_SINE: f64 = 1e-5;

/// One flat face.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanarFace {
    /// Unit normal, pointing out of the part.
    pub normal: [f64; 3],
    /// The face's centroid, a point on its plane.
    pub origin: [f64; 3],
    /// The plane is `normal · p = plane_offset_mm`.
    pub plane_offset_mm: f64,
    pub area_mm2: f64,
    pub triangle_count: usize,
    /// The outer boundary, counter-clockwise seen from outside the part.
    pub outline: Vec<[f64; 3]>,
    /// One clockwise loop per hole in the face.
    pub holes: Vec<Vec<[f64; 3]>>,
}

/// What [`planar_faces`] found.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanarFaces {
    /// The largest faces, largest first, at most as many as were asked for.
    pub faces: Vec<PlanarFace>,
    /// How many faces the mesh has in all.
    pub total: usize,
}

struct Facet {
    corners: [u32; 3],
    normal: [f64; 3],
    area: f64,
    altitude: f64,
}

/// A grown face before anyone has asked for its outline.
struct Region {
    area: f64,
    members: Vec<u32>,
}

/// The `limit` largest flat faces of `mesh`. Coplanar faces that do not touch
/// stay separate, as they are in a B-rep. A curved surface comes back as its
/// individual facets: an STL has nothing else to say about it, and being
/// smallest they sort last.
///
/// Every face is grown, for the total, but only the ones returned are
/// outlined: a million-triangle scan is a million faces of one triangle each,
/// and an outline for each of those was most of the work.
pub fn planar_faces(mesh: &TriangleMesh, limit: usize) -> PlanarFaces {
    let (corner_ids, points) = weld_on_grid(mesh);
    let facets: Vec<Option<Facet>> = corner_ids
        .as_chunks::<3>()
        .0
        .iter()
        .map(|corners| facet(*corners, &points))
        .collect();
    let across = neighbours(&facets);
    let facets = facing_out(facets, &across, mesh);

    let mut by_area: Vec<(f64, u32)> = facets
        .iter()
        .enumerate()
        .filter_map(|(index, facet)| Some((facet.as_ref()?.area, index as u32)))
        .collect();
    by_area.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));

    let mut is_taken = vec![false; facets.len()];
    let mut regions: Vec<Region> = Vec::new();
    for (_, seed) in by_area {
        if !is_taken[seed as usize] {
            regions.push(grow(seed, &facets, &across, &points, &mut is_taken));
        }
    }
    regions.sort_by(|a, b| b.area.total_cmp(&a.area));

    let total = regions.len();
    let faces = regions
        .iter()
        .filter_map(|region| describe(region, &facets, &points))
        .take(limit)
        .collect();
    PlanarFaces { faces, total }
}

/// One id per corner, three to a triangle, and the point of each id.
///
/// Corners share an id when they round to one cell of [`WELD_TOLERANCE_MM`].
/// `TriangleMesh::measure` also searches the neighbouring cells, to be sure of
/// closing a mesh; that costs 27 lookups a corner, which is minutes on a large
/// scan. Here a seam that straddles a cell costs at worst one face split in
/// two, and the same corner written twice always lands in one cell.
fn weld_on_grid(mesh: &TriangleMesh) -> (Vec<u32>, Vec<[f64; 3]>) {
    let mut ids: HashMap<[i64; 3], u32> = HashMap::with_capacity(mesh.triangles.len() / 2);
    let mut points: Vec<[f64; 3]> = Vec::with_capacity(mesh.triangles.len() / 2);
    let corner_ids = mesh
        .triangles
        .iter()
        .flatten()
        .map(|point| {
            let cell = point.map(|value| (value / WELD_TOLERANCE_MM).round() as i64);
            *ids.entry(cell).or_insert_with(|| {
                points.push(*point);
                (points.len() - 1) as u32
            })
        })
        .collect();
    (corner_ids, points)
}

/// `corners` as a facet, or `None` for one that welded shut or has no area and
/// so no plane of its own.
fn facet(corners: [u32; 3], points: &[[f64; 3]]) -> Option<Facet> {
    let [a, b, c] = corners.map(|index| points[index as usize]);
    let sides = [sub(b, a), sub(c, b), sub(a, c)];
    let spanned = cross(sides[0], sub(c, a));
    let double_area = norm(spanned);
    let longest_side = sides.iter().map(|side| norm(*side)).fold(0.0, f64::max);
    if double_area <= 0.0 || longest_side <= 0.0 {
        return None;
    }
    Some(Facet {
        corners,
        normal: spanned.map(|value| value / double_area),
        area: double_area / 2.0,
        altitude: double_area / longest_side,
    })
}

fn edges(corners: [u32; 3]) -> [(u32, u32); 3] {
    let [a, b, c] = corners;
    [(a, b), (b, c), (c, a)]
}

/// Marks a side with no facet across it.
const NO_NEIGHBOUR: u32 = u32::MAX;

/// `facets`, turned the right way out. Some STLs are wound inside out, which
/// libslic3r turns right on import, and a normal into the part would aim a hole
/// out of it. Only a closed mesh can say: its signed volume is the same wherever
/// it sits, while that of an open sheet changes sign with its distance from the
/// origin. A sheet keeps the winding it came with.
fn facing_out(
    facets: Vec<Option<Facet>>,
    across: &[[u32; 3]],
    mesh: &TriangleMesh,
) -> Vec<Option<Facet>> {
    let is_closed = facets
        .iter()
        .zip(across)
        .all(|(facet, sides)| facet.is_none() || sides.iter().all(|&side| side != NO_NEIGHBOUR));
    if !is_closed || mesh.volume_mm3() >= 0.0 {
        return facets;
    }
    facets
        .into_iter()
        .map(|facet| {
            facet.map(|f| {
                let [a, b, c] = f.corners;
                Facet {
                    corners: [a, c, b],
                    normal: f.normal.map(|value| -value),
                    ..f
                }
            })
        })
        .collect()
}

/// The facet across each side of each facet, worked out once from one sorted
/// array of sides: a map of little lists is millions of allocations on a large
/// mesh. Each side has room for one neighbour, so an edge shared by more than
/// two facets, as a non-manifold one is, links them in a ring: which of them a
/// face can cross to depends on their order, and at worst it stops there and
/// the rest of the plane comes back as a face of its own.
fn neighbours(facets: &[Option<Facet>]) -> Vec<[u32; 3]> {
    let mut sides: Vec<((u32, u32), u32, u8)> = Vec::with_capacity(facets.len() * 3);
    for (index, facet) in facets.iter().enumerate() {
        let Some(facet) = facet else { continue };
        for (slot, (from, to)) in edges(facet.corners).into_iter().enumerate() {
            sides.push(((from.min(to), from.max(to)), index as u32, slot as u8));
        }
    }
    sides.sort_unstable();
    let mut across = vec![[NO_NEIGHBOUR; 3]; facets.len()];
    for shared in sides.chunk_by(|a, b| a.0 == b.0) {
        for (position, &(_, facet, slot)) in shared.iter().enumerate() {
            let (_, next, _) = shared[(position + 1) % shared.len()];
            if next != facet {
                across[facet as usize][slot as usize] = next;
            }
        }
    }
    across
}

/// Whether `candidate` lies on the plane of `seed`.
fn is_on_plane(candidate: &Facet, seed: &Facet, points: &[[f64; 3]]) -> bool {
    let anchor = points[seed.corners[0] as usize];
    let is_near = candidate.corners.iter().all(|&corner| {
        dot(sub(points[corner as usize], anchor), seed.normal).abs() <= MAX_PLANE_DISTANCE_MM
    });
    let is_noisy = candidate.altitude < MIN_RELIABLE_ALTITUDE_MM;
    is_near && (is_noisy || dot(candidate.normal, seed.normal) >= MIN_NORMAL_COSINE)
}

/// The facets reachable from `seed` across shared edges without leaving its
/// plane. Marks them taken.
fn grow(
    seed: u32,
    facets: &[Option<Facet>],
    across: &[[u32; 3]],
    points: &[[f64; 3]],
    is_taken: &mut [bool],
) -> Region {
    let mut region = Region {
        area: 0.0,
        members: Vec::new(),
    };
    let Some(seed_facet) = facets[seed as usize].as_ref() else {
        return region;
    };
    is_taken[seed as usize] = true;
    region.area = seed_facet.area;
    region.members.push(seed);
    let mut pending = vec![seed];
    while let Some(index) = pending.pop() {
        for next in across[index as usize] {
            if next == NO_NEIGHBOUR || is_taken[next as usize] {
                continue;
            }
            let Some(candidate) = facets[next as usize].as_ref() else {
                continue;
            };
            if is_on_plane(candidate, seed_facet, points) {
                is_taken[next as usize] = true;
                region.area += candidate.area;
                region.members.push(next);
                pending.push(next);
            }
        }
    }
    region
}

/// The face `region` makes, or `None` when it encloses nothing to outline,
/// as two facets lying back to back do.
fn describe(region: &Region, facets: &[Option<Facet>], points: &[[f64; 3]]) -> Option<PlanarFace> {
    let in_face: Vec<&Facet> = region
        .members
        .iter()
        .filter_map(|&index| facets[index as usize].as_ref())
        .collect();
    let area = region.area;
    let weighted_normal = in_face.iter().fold([0.0; 3], |sum, f| {
        [0, 1, 2].map(|axis| sum[axis] + f.normal[axis] * f.area)
    });
    let length = norm(weighted_normal);
    if length <= 0.0 {
        return None;
    }
    let normal = weighted_normal.map(|value| value / length);
    let origin = in_face.iter().fold([0.0; 3], |sum, f| {
        let [a, b, c] = f.corners.map(|index| points[index as usize]);
        [0, 1, 2].map(|axis| sum[axis] + (a[axis] + b[axis] + c[axis]) / 3.0 * f.area / area)
    });

    let mut loops: Vec<(f64, Vec<[f64; 3]>)> = boundary_loops(&in_face)
        .into_iter()
        .map(|walk| simplified(walk.iter().map(|&index| points[index as usize]).collect()))
        .filter(|loop_points| loop_points.len() >= 3)
        .map(|loop_points| (signed_area(&loop_points, normal), loop_points))
        .collect();
    loops.sort_by(|a, b| b.0.abs().total_cmp(&a.0.abs()));
    let mut remaining = loops.into_iter().map(|(_, loop_points)| loop_points);
    let outline = remaining.next()?;
    Some(PlanarFace {
        normal,
        origin,
        plane_offset_mm: dot(origin, normal),
        area_mm2: area,
        triangle_count: in_face.len(),
        outline,
        holes: remaining.collect(),
    })
}

/// The face's boundary, as closed walks over vertex ids. A directed edge is on
/// the boundary when no facet of the face runs it the other way, so what is
/// left is balanced at every vertex and every walk closes. Ordered maps keep
/// the walks, and so the answer, the same from run to run.
fn boundary_loops(in_face: &[&Facet]) -> Vec<Vec<u32>> {
    let mut open: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    for facet in in_face {
        for (from, to) in edges(facet.corners) {
            match open.get_mut(&(to, from)) {
                Some(1) => {
                    open.remove(&(to, from));
                }
                Some(count) => *count -= 1,
                None => *open.entry((from, to)).or_insert(0) += 1,
            }
        }
    }
    let mut next: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for ((from, to), count) in open {
        next.entry(from)
            .or_default()
            .extend(std::iter::repeat_n(to, count as usize));
    }

    let mut walks = Vec::new();
    while let Some(&start) = next.keys().next() {
        let mut walk = vec![start];
        let mut at = start;
        while let Some(to) = take_step(&mut next, at) {
            at = to;
            if at == start {
                break;
            }
            walk.push(at);
        }
        walks.push(walk);
    }
    walks
}

fn take_step(next: &mut BTreeMap<u32, Vec<u32>>, from: u32) -> Option<u32> {
    let targets = next.get_mut(&from)?;
    let to = targets.pop();
    if targets.is_empty() {
        next.remove(&from);
    }
    to
}

/// `points` without the ones that only split a straight edge.
fn simplified(points: Vec<[f64; 3]>) -> Vec<[f64; 3]> {
    let count = points.len();
    (0..count)
        .filter(|&index| {
            let previous = points[(index + count - 1) % count];
            let here = points[index];
            let following = points[(index + 1) % count];
            let (into, out) = (sub(here, previous), sub(following, here));
            norm(cross(into, out)) > MIN_CORNER_SINE * norm(into) * norm(out)
        })
        .map(|index| points[index])
        .collect()
}

/// Area of a closed loop in a plane of the given `normal`, positive when it
/// runs counter-clockwise about that normal.
fn signed_area(points: &[[f64; 3]], normal: [f64; 3]) -> f64 {
    let anchor = points[0];
    let mut doubled = [0.0; 3];
    for pair in points.windows(2) {
        let spanned = cross(sub(pair[0], anchor), sub(pair[1], anchor));
        doubled = [0, 1, 2].map(|axis| doubled[axis] + spanned[axis]);
    }
    dot(doubled, normal) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::Triangle;

    fn quad(a: [f64; 3], b: [f64; 3], c: [f64; 3], d: [f64; 3]) -> Vec<Triangle> {
        vec![[a, b, c], [a, c, d]]
    }

    /// A box from the origin to `size`, wound outward.
    fn box_triangles(size: [f64; 3]) -> Vec<Triangle> {
        let [x, y, z] = size;
        let v = |a: f64, b: f64, c: f64| [a * x, b * y, c * z];
        [
            quad(v(0., 0., 0.), v(0., 1., 0.), v(1., 1., 0.), v(1., 0., 0.)),
            quad(v(0., 0., 1.), v(1., 0., 1.), v(1., 1., 1.), v(0., 1., 1.)),
            quad(v(0., 0., 0.), v(1., 0., 0.), v(1., 0., 1.), v(0., 0., 1.)),
            quad(v(0., 1., 0.), v(0., 1., 1.), v(1., 1., 1.), v(1., 1., 0.)),
            quad(v(0., 0., 0.), v(0., 0., 1.), v(0., 1., 1.), v(0., 1., 0.)),
            quad(v(1., 0., 0.), v(1., 1., 0.), v(1., 1., 1.), v(1., 0., 1.)),
        ]
        .concat()
    }

    /// `triangles` moved by `offset`.
    fn moved(triangles: Vec<Triangle>, offset: [f64; 3]) -> Vec<Triangle> {
        TriangleMesh::new(triangles).translated(offset).triangles
    }

    /// Every face of `mesh`.
    fn all_faces(mesh: &TriangleMesh) -> Vec<PlanarFace> {
        planar_faces(mesh, usize::MAX).faces
    }

    /// The one face whose normal is `normal`.
    fn facing(faces: &[PlanarFace], normal: [f64; 3]) -> &PlanarFace {
        faces
            .iter()
            .find(|f| dot(f.normal, normal) > 0.999)
            .unwrap_or_else(|| panic!("no face towards {normal:?}: {faces:?}"))
    }

    /// A plate 40×30×4 with a square window through it, its top and bottom
    /// faces split the way a boolean splits them.
    fn plate_with_window() -> TriangleMesh {
        let (x0, y0, x1, y1) = (10.0, 10.0, 20.0, 20.0);
        let ring = |z: f64, flip: bool| -> Vec<Triangle> {
            let v = |x: f64, y: f64| [x, y, z];
            let strips = [
                quad(v(0., 0.), v(40., 0.), v(x1, y0), v(x0, y0)),
                quad(v(40., 0.), v(40., 30.), v(x1, y1), v(x1, y0)),
                quad(v(40., 30.), v(0., 30.), v(x0, y1), v(x1, y1)),
                quad(v(0., 30.), v(0., 0.), v(x0, y0), v(x0, y1)),
            ]
            .concat();
            if flip {
                strips.iter().map(|[a, b, c]| [*a, *c, *b]).collect()
            } else {
                strips
            }
        };
        let wall = |a: [f64; 2], b: [f64; 2]| {
            let (lo, hi) = ([a[0], a[1], 0.0], [b[0], b[1], 0.0]);
            quad(lo, hi, [b[0], b[1], 4.0], [a[0], a[1], 4.0])
        };
        TriangleMesh::new(
            [
                ring(4.0, false),
                ring(0.0, true),
                wall([0., 0.], [40., 0.]),
                wall([40., 0.], [40., 30.]),
                wall([40., 30.], [0., 30.]),
                wall([0., 30.], [0., 0.]),
                wall([x0, y0], [x0, y1]),
                wall([x0, y1], [x1, y1]),
                wall([x1, y1], [x1, y0]),
                wall([x1, y0], [x0, y0]),
            ]
            .concat(),
        )
    }

    #[test]
    fn a_box_has_six_faces_with_the_right_planes_and_areas() {
        let mesh = TriangleMesh::new(box_triangles([10.0, 20.0, 30.0]));

        let faces = all_faces(&mesh);

        assert_eq!(faces.len(), 6);
        let top = facing(&faces, [0.0, 0.0, 1.0]);
        assert!((top.area_mm2 - 200.0).abs() < 1e-9);
        assert!((top.plane_offset_mm - 30.0).abs() < 1e-9);
        for (found, wanted) in top.origin.iter().zip([5.0, 10.0, 30.0]) {
            assert!((found - wanted).abs() < 1e-9, "{:?}", top.origin);
        }
        assert_eq!(top.triangle_count, 2);
        let bottom = facing(&faces, [0.0, 0.0, -1.0]);
        assert!(bottom.plane_offset_mm.abs() < 1e-9, "n·p = 0 at z = 0");
    }

    #[test]
    fn faces_come_largest_first() {
        let mesh = TriangleMesh::new(box_triangles([10.0, 20.0, 30.0]));

        let areas: Vec<f64> = all_faces(&mesh).iter().map(|f| f.area_mm2).collect();

        assert!(areas.windows(2).all(|pair| pair[0] >= pair[1]), "{areas:?}");
    }

    /// The diagonal between a face's two triangles is no part of its outline.
    #[test]
    fn an_outline_is_the_corners_of_the_face() {
        let mesh = TriangleMesh::new(box_triangles([10.0, 20.0, 30.0]));

        let faces = all_faces(&mesh);

        let top = facing(&faces, [0.0, 0.0, 1.0]);
        assert_eq!(top.outline.len(), 4);
        assert!(top.holes.is_empty());
        for corner in &top.outline {
            assert_eq!(corner[2], 30.0);
        }
    }

    #[test]
    fn an_outline_runs_counter_clockwise_seen_from_outside() {
        let mesh = TriangleMesh::new(box_triangles([10.0, 20.0, 30.0]));

        for face in all_faces(&mesh) {
            let closed: Vec<[f64; 3]> = face
                .outline
                .iter()
                .chain(face.outline.first())
                .copied()
                .collect();
            assert!(signed_area(&closed, face.normal) > 0.0, "{face:?}");
        }
    }

    /// A window through a plate is a hole in the top face, not its outline.
    #[test]
    fn a_face_with_a_window_keeps_it_as_a_hole() {
        let faces = all_faces(&plate_with_window());

        let top = facing(&faces, [0.0, 0.0, 1.0]);
        assert!((top.area_mm2 - (40.0 * 30.0 - 100.0)).abs() < 1e-9);
        assert_eq!(top.outline.len(), 4);
        assert_eq!(top.holes.len(), 1);
        assert_eq!(top.holes[0].len(), 4);
        assert!(top.outline.iter().any(|p| p[0] == 40.0));
    }

    /// A step's two levels and two pads at one height are different faces.
    #[test]
    fn coplanar_faces_that_do_not_touch_stay_apart() {
        let triangles = [
            box_triangles([5.0, 5.0, 5.0]),
            moved(box_triangles([5.0, 5.0, 5.0]), [20.0, 0.0, 0.0]),
        ]
        .concat();

        let faces = all_faces(&TriangleMesh::new(triangles));

        let tops = faces
            .iter()
            .filter(|f| dot(f.normal, [0.0, 0.0, 1.0]) > 0.999)
            .count();
        assert_eq!(tops, 2);
    }

    /// `n` facets around a cylinder of radius `r`, walls only.
    fn cylinder_wall(r: f64, height: f64, n: u32) -> Vec<Triangle> {
        let at = |i: u32, z: f64| {
            let angle = std::f64::consts::TAU * f64::from(i) / f64::from(n);
            [r * angle.cos(), r * angle.sin(), z]
        };
        (0..n)
            .flat_map(|i| quad(at(i, 0.0), at(i + 1, 0.0), at(i + 1, height), at(i, height)))
            .collect()
    }

    /// The reason each neighbour is judged against the seed: stepping from
    /// facet to facet, a 0.05° turn per facet would pass all the way round.
    #[test]
    fn a_fine_cylinder_does_not_come_back_as_one_flat_face() {
        let wall = cylinder_wall(100.0, 20.0, 7200);

        let faces = all_faces(&TriangleMesh::new(wall));

        let biggest = &faces[0];
        let whole = std::f64::consts::TAU * 100.0 * 20.0;
        assert!(biggest.area_mm2 < whole / 90.0, "{}", biggest.area_mm2);
    }

    #[test]
    fn a_coarse_cylinder_is_its_facets() {
        let faces = all_faces(&TriangleMesh::new(cylinder_wall(10.0, 20.0, 36)));

        assert_eq!(faces.len(), 36);
    }

    /// What survives a round trip through `f32`: the same face a few bits off.
    #[test]
    fn rounding_noise_does_not_split_a_face() {
        let noisy = |x: f64, y: f64, bits: f64| [x, y, 85.0 + bits * 7.6e-6];
        let triangles = [
            [noisy(0., 0., 0.), noisy(40., 0., 1.), noisy(40., 40., -1.)],
            [noisy(0., 0., 0.), noisy(40., 40., -1.), noisy(0., 40., 1.)],
        ];

        let faces = all_faces(&TriangleMesh::new(triangles.to_vec()));

        assert_eq!(faces.len(), 1);
    }

    /// A sliver's own normal says nothing; its corners still do.
    #[test]
    fn a_sliver_on_a_face_joins_it() {
        let z = 5.0;
        let triangles = vec![
            [[0., 0., z], [10., 0., z], [10., 10., z]],
            [[0., 0., z], [10., 10., z], [0., 10., z]],
            // Fills the 2 µm gap under the first edge.
            [[0., 0., z], [5.0, -2e-3, z + 1e-5], [10., 0., z]],
        ];

        let faces = all_faces(&TriangleMesh::new(triangles));

        assert_eq!(faces[0].triangle_count, 3);
    }

    #[test]
    fn a_mesh_wound_inside_out_still_faces_out() {
        let inside_out: Vec<Triangle> = box_triangles([10.0, 20.0, 30.0])
            .iter()
            .map(|[a, b, c]| [*a, *c, *b])
            .collect();

        let faces = all_faces(&TriangleMesh::new(inside_out));

        let top = facing(&faces, [0.0, 0.0, 1.0]);
        assert!((top.plane_offset_mm - 30.0).abs() < 1e-9, "{top:?}");
    }

    /// Its signed volume depends on where it sits, so it says nothing about
    /// which way a sheet was meant to face.
    #[test]
    fn an_open_sheet_keeps_the_winding_it_came_with() {
        let sheet = |z: f64| {
            vec![
                [[0., 0., z], [10., 0., z], [10., 10., z]],
                [[0., 0., z], [10., 10., z], [0., 10., z]],
            ]
        };

        for z in [-5.0, 5.0] {
            let faces = all_faces(&TriangleMesh::new(sheet(z)));
            assert_eq!(faces[0].normal, [0.0, 0.0, 1.0], "at z = {z}");
        }
    }

    #[test]
    fn a_tilted_neighbour_is_a_face_of_its_own() {
        let triangles = vec![
            [[0., 0., 0.], [10., 0., 0.], [10., 10., 0.]],
            [[0., 0., 0.], [10., 10., 0.], [0., 10., 0.]],
            [[10., 0., 0.], [20., 0., 1.], [20., 10., 1.]],
            [[10., 0., 0.], [20., 10., 1.], [10., 10., 0.]],
        ];

        assert_eq!(all_faces(&TriangleMesh::new(triangles)).len(), 2);
    }

    #[test]
    fn triangles_with_no_area_are_left_out() {
        let mut triangles = box_triangles([10.0, 10.0, 10.0]);
        triangles.push([[0., 0., 0.], [0., 0., 0.], [10., 0., 0.]]);
        triangles.push([[0., 0., 0.], [5., 0., 0.], [10., 0., 0.]]);

        assert_eq!(all_faces(&TriangleMesh::new(triangles)).len(), 6);
    }

    /// The limit cuts the answer, not the count: an agent has to be able to
    /// tell it was shown the largest faces and not all of them.
    #[test]
    fn a_limit_keeps_the_largest_faces_and_the_full_count() {
        let mesh = TriangleMesh::new(box_triangles([10.0, 20.0, 30.0]));

        let found = planar_faces(&mesh, 2);

        assert_eq!(found.total, 6);
        assert_eq!(found.faces.len(), 2);
        assert_eq!(found.faces[0].area_mm2, 600.0);
        assert_eq!(found.faces[1].area_mm2, 600.0);
    }

    #[test]
    fn an_empty_mesh_has_no_faces() {
        assert!(all_faces(&TriangleMesh::default()).is_empty());
    }
}
