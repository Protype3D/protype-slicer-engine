//! A closed mesh where two pieces of one solid touch along an edge.
//!
//! Four faces on one edge, or six, are not a hole. When their windings
//! alternate going round the edge, the surface still encloses a volume: two
//! pieces of it meet along that line, the way a sliver of the Tarot box in
//! spec #1596 meets its wall. The kernel takes only edges of two faces, so each
//! such edge is split here into one edge for each pair of faces with solid
//! between them, and the vertices at its ends are split with it.
//!
//! Splitting the indices is not enough. An STL has no indices, and the build
//! refuses its own output when two edges fall on one set of coordinates, so
//! each copy of a split vertex also moves half of [`SHARED_EDGE_GAP_MM`],
//! across the edge and towards the solid between the faces of its piece. Two
//! copies then stand up to the whole gap apart, less when their pieces lie in
//! nearly the same direction from the edge; copies that would not survive
//! being written as `f32` and welded again are refused instead.
//!
//! Everything is walked in index order, not hash order: the sums of directions
//! below are floating point, and the same file must give the same part.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::f64::consts::TAU;

use crate::mesh::{count_unpaired, cross, dot, norm, sub, WELD_TOLERANCE_MM};

/// How far apart the copies of a shared edge end up when their pieces lie on
/// opposite sides of it: ten times the weld tolerance, and four hundred times
/// finer than a 0.4 mm nozzle draws. Each copy moves half of it.
pub const SHARED_EDGE_GAP_MM: f64 = 1e-3;

/// Closest two copies of one vertex may stand once written as `f32`: twice the
/// distance at which the weld would join them again on reading the part back.
const MIN_WRITTEN_GAP_MM: f64 = 2.0 * WELD_TOLERANCE_MM;

/// Narrowest wedge between two faces round a shared edge that still tells one
/// face from the other. Below it two faces lie on one side of the edge, and
/// which of them is nearer the solid is a guess.
const MIN_WEDGE_RAD: f64 = 1e-3;

type Vertex = [f64; 3];
type Edge = (usize, usize);

/// One face on an edge, and whether its winding runs from the edge's lower
/// vertex index to its higher one.
#[derive(Clone, Copy)]
struct Side {
    face: usize,
    forward: bool,
}

/// Two faces round a shared edge with solid between them, and the direction
/// from the edge into that solid.
struct Piece {
    faces: [usize; 2],
    into_solid: Vertex,
}

/// The mesh with every edge of more than two faces split, or `None` when some
/// edge cannot be: a hole, faces that do not alternate in winding, two faces on
/// one side of an edge, an edge whose ends belong to one piece each way, or
/// copies of a vertex that would land too close to tell apart in an STL.
pub(crate) fn split_shared_edges(
    vertices: &[Vertex],
    triangles: &[[usize; 3]],
) -> Option<(Vec<Vertex>, Vec<[usize; 3]>)> {
    let edges = sides_of_edges(triangles);
    let mut pieces: BTreeMap<Edge, Vec<Piece>> = BTreeMap::new();
    for (&edge, sides) in &edges {
        if sides.len() != 2 {
            pieces.insert(edge, pair_round_edge(vertices, triangles, edge, sides)?);
        }
    }
    if pieces.is_empty() {
        return None;
    }
    let groups = corner_groups(triangles, &edges, &pieces);
    let (vertices, triangles) = rebuild(vertices, triangles, &groups, &pieces)?;
    (count_unpaired(triangles.iter().copied()) == 0).then_some((vertices, triangles))
}

fn sides_of_edges(triangles: &[[usize; 3]]) -> BTreeMap<Edge, Vec<Side>> {
    let mut edges: BTreeMap<Edge, Vec<Side>> = BTreeMap::new();
    for (face, [a, b, c]) in triangles.iter().copied().enumerate() {
        for (from, to) in [(a, b), (b, c), (c, a)] {
            edges
                .entry((from.min(to), from.max(to)))
                .or_default()
                .push(Side {
                    face,
                    forward: from < to,
                });
        }
    }
    edges
}

/// Sort the faces round the edge and pair each with the next one across solid.
///
/// A face whose winding runs along the edge has its outside ahead of it going
/// round the edge (right hand, thumb along the edge) and its solid behind; a
/// face wound the other way has its solid ahead. So the solid lies from a
/// backward face to the next face round, which must be a forward one.
fn pair_round_edge(
    vertices: &[Vertex],
    triangles: &[[usize; 3]],
    (low, high): Edge,
    sides: &[Side],
) -> Option<Vec<Piece>> {
    let forward = sides.iter().filter(|side| side.forward).count();
    if !sides.len().is_multiple_of(2) || forward * 2 != sides.len() {
        return None;
    }
    let along = unit(sub(vertices[high], vertices[low]))?;
    let across = unit(perpendicular(along))?;
    let up = cross(along, across);
    let mut round = sides
        .iter()
        .map(|side| {
            let third = triangles[side.face]
                .into_iter()
                .find(|v| *v != low && *v != high)?;
            let out = sub(vertices[third], vertices[low]);
            let out = sub(out, scale(along, dot(out, along)));
            if norm(out) <= WELD_TOLERANCE_MM {
                return None;
            }
            Some((dot(out, up).atan2(dot(out, across)), *side))
        })
        .collect::<Option<Vec<_>>>()?;
    round.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut pieces = Vec::new();
    for (index, &(angle, side)) in round.iter().enumerate() {
        let (next_angle, next) = round[(index + 1) % round.len()];
        let next_angle = if index + 1 == round.len() {
            next_angle + TAU
        } else {
            next_angle
        };
        if next_angle - angle < MIN_WEDGE_RAD || side.forward == next.forward {
            return None;
        }
        if !side.forward {
            let middle = (angle + next_angle) / 2.0;
            let into_solid = add(scale(across, middle.cos()), scale(up, middle.sin()));
            pieces.push(Piece {
                faces: [side.face, next.face],
                into_solid,
            });
        }
    }
    Some(pieces)
}

/// Which corners stay one vertex: those whose faces meet across an edge of two
/// faces, or across a shared edge as one piece. Each corner maps to the root of
/// its group.
fn corner_groups(
    triangles: &[[usize; 3]],
    edges: &BTreeMap<Edge, Vec<Side>>,
    pieces: &BTreeMap<Edge, Vec<Piece>>,
) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..triangles.len() * 3).collect();
    for (&(low, high), sides) in edges {
        let pairs: Vec<[usize; 2]> = match pieces.get(&(low, high)) {
            Some(round) => round.iter().map(|piece| piece.faces).collect(),
            None => vec![[sides[0].face, sides[1].face]],
        };
        for [a, b] in pairs {
            for vertex in [low, high] {
                let (a, b) = (corner(triangles, a, vertex), corner(triangles, b, vertex));
                let (a, b) = (root(&mut parent, a), root(&mut parent, b));
                parent[a] = b;
            }
        }
    }
    (0..parent.len()).map(|c| root(&mut parent, c)).collect()
}

/// New vertices and triangles: each vertex at the end of a shared edge becomes
/// one vertex per group of its corners, each moved into its own piece of solid.
/// A vertex whose corners stay one group stays where it is.
fn rebuild(
    vertices: &[Vertex],
    triangles: &[[usize; 3]],
    groups: &[usize],
    pieces: &BTreeMap<Edge, Vec<Piece>>,
) -> Option<(Vec<Vertex>, Vec<[usize; 3]>)> {
    let (push, ends) = directions_into_solid(triangles, groups, pieces);
    let copies = groups_at(&ends, triangles, groups);
    let (vertices, index_of) = place_copies(vertices, &copies, &push)?;
    let triangles = triangles
        .iter()
        .enumerate()
        .map(|(face, tri)| {
            std::array::from_fn(|slot| {
                index_of
                    .get(&groups[face * 3 + slot])
                    .copied()
                    .unwrap_or(tri[slot])
            })
        })
        .collect();
    Some((vertices, triangles))
}

/// For each group of corners on a shared edge, the sum of the directions into
/// the solid of the pieces it belongs to; and the vertices that end such edges.
fn directions_into_solid(
    triangles: &[[usize; 3]],
    groups: &[usize],
    pieces: &BTreeMap<Edge, Vec<Piece>>,
) -> (HashMap<usize, Vertex>, BTreeSet<usize>) {
    let mut push: HashMap<usize, Vertex> = HashMap::new();
    let mut ends = BTreeSet::new();
    for (&(low, high), round) in pieces {
        for piece in round {
            for vertex in [low, high] {
                let group = groups[corner(triangles, piece.faces[0], vertex)];
                let sum = push.entry(group).or_insert([0.0; 3]);
                *sum = add(*sum, piece.into_solid);
                ends.insert(vertex);
            }
        }
    }
    (push, ends)
}

/// The groups of corners at each of `ends`, in the order the triangles first
/// reach them.
fn groups_at(
    ends: &BTreeSet<usize>,
    triangles: &[[usize; 3]],
    groups: &[usize],
) -> BTreeMap<usize, Vec<usize>> {
    let mut copies: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (face, tri) in triangles.iter().enumerate() {
        for (slot, vertex) in tri.iter().enumerate().filter(|(_, v)| ends.contains(v)) {
            let group = groups[face * 3 + slot];
            let seen = copies.entry(*vertex).or_default();
            if !seen.contains(&group) {
                seen.push(group);
            }
        }
    }
    copies
}

/// One vertex for each group of a vertex that splits: the first keeps the
/// vertex's index, the rest are appended. `None` when two copies would still
/// be one point.
fn place_copies(
    vertices: &[Vertex],
    copies: &BTreeMap<usize, Vec<usize>>,
    push: &HashMap<usize, Vertex>,
) -> Option<(Vec<Vertex>, HashMap<usize, usize>)> {
    let mut out = vertices.to_vec();
    let mut index_of = HashMap::new();
    for (&vertex, groups_here) in copies.iter().filter(|(_, g)| g.len() > 1) {
        let mut placed = Vec::with_capacity(groups_here.len());
        for (nth, group) in groups_here.iter().enumerate() {
            let at = match push.get(group).and_then(|d| unit(*d)) {
                Some(d) => add(vertices[vertex], scale(d, SHARED_EDGE_GAP_MM / 2.0)),
                None => vertices[vertex],
            };
            let index = if nth == 0 {
                out[vertex] = at;
                vertex
            } else {
                out.push(at);
                out.len() - 1
            };
            index_of.insert(*group, index);
            placed.push(at);
        }
        if !apart_as_written(&placed) {
            return None;
        }
    }
    Some((out, index_of))
}

fn corner(triangles: &[[usize; 3]], face: usize, vertex: usize) -> usize {
    let slot = triangles[face]
        .iter()
        .position(|v| *v == vertex)
        .expect("a corner of the face");
    face * 3 + slot
}

fn root(parent: &mut [usize], mut at: usize) -> usize {
    while parent[at] != at {
        parent[at] = parent[parent[at]];
        at = parent[at];
    }
    at
}

/// Whether each two of `points` stay [`MIN_WRITTEN_GAP_MM`] apart after the
/// rounding to `f32` an STL makes. Near 10 m an `f32` step is about a micron,
/// so the check is on the written numbers, not on these.
fn apart_as_written(points: &[Vertex]) -> bool {
    let written: Vec<Vertex> = points
        .iter()
        .map(|p| p.map(|value| f64::from(value as f32)))
        .collect();
    written.iter().enumerate().all(|(i, a)| {
        written[i + 1..]
            .iter()
            .all(|b| norm(sub(*a, *b)) > MIN_WRITTEN_GAP_MM)
    })
}

/// Whether every edge has as many faces wound along it as against it: a
/// closed surface in that sense even where it cannot be split, so the sign of
/// its volume still says whether it is inside out.
pub(crate) fn balanced(triangles: &[[usize; 3]]) -> bool {
    sides_of_edges(triangles)
        .values()
        .all(|sides| sides.iter().filter(|side| side.forward).count() * 2 == sides.len())
}

fn perpendicular(v: Vertex) -> Vertex {
    let axis = if v[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    cross(v, axis)
}

fn unit(v: Vertex) -> Option<Vertex> {
    let length = norm(v);
    (length > f64::EPSILON).then(|| scale(v, 1.0 / length))
}

fn scale(v: Vertex, by: f64) -> Vertex {
    [v[0] * by, v[1] * by, v[2] * by]
}

fn add(a: Vertex, b: Vertex) -> Vertex {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
