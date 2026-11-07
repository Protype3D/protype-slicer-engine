//! Shared handling of the 2D outlines `extrude`, `revolve` and `loft`
//! are written from.
//!
//! Both kernels read the same outline, so anything done to one of them
//! here is done to both. That matters more than it sounds: the two
//! kernels disagreed on an outline whose last point repeated the first
//! — OCCT absorbed it, and manifold produced a surface with eight
//! unpaired edges — which meant one spelling of the same shape built as
//! a chamfered part and failed as a plain one. Measured 2026-08-21.

use crate::tree::{CsgNode, LoftSection};

/// Two outline points closer than this are the same point.
///
/// An outline is authored, not measured, so this is about spelling
/// rather than tolerance: a generator writing the closing point again
/// writes the same literal coordinates, and a generator computing them
/// from an angle lands within a rounding error of them.
const SAME_POINT_MM: f64 = 1e-9;

fn same_point(a: [f64; 2], b: [f64; 2]) -> bool {
    (a[0] - b[0]).abs() <= SAME_POINT_MM && (a[1] - b[1]).abs() <= SAME_POINT_MM
}

/// Drop repeated points, including a final one that repeats the first.
///
/// Outlines are implicitly closed, so writing the first point again at
/// the end is a redundant spelling of the same loop rather than a
/// different shape — and it is the spelling a generator reaches for by
/// default. Left in, it becomes a zero-length edge.
pub fn closed_loop(points: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut out: Vec<[f64; 2]> = Vec::with_capacity(points.len());
    for point in points {
        if out.last().is_some_and(|last| same_point(*last, *point)) {
            continue;
        }
        out.push(*point);
    }
    // The closing repeat is the same test against the other end, and it
    // only applies while something is left to close.
    while out.len() > 1 && same_point(out[0], out[out.len() - 1]) {
        out.pop();
    }
    out
}

/// The first pair of non-adjacent edges that cross, if any.
///
/// A crossing outline is not a shape either kernel can build honestly:
/// OCCT sums the two lobes with opposite signs, so a symmetric bowtie
/// comes back enclosing -0 mm³ and an asymmetric one comes back
/// enclosing a plausible volume of the wrong part. Both were measured
/// 2026-08-21; the second is the dangerous one, because nothing
/// downstream can tell it from a part that was asked for.
pub fn first_crossing(points: &[[f64; 2]]) -> Option<(usize, usize)> {
    let n = points.len();
    if n < 4 {
        return None;
    }
    for i in 0..n {
        for j in (i + 1)..n {
            // Edges sharing an endpoint always "cross" at it. Edge n-1
            // wraps to point 0, so it is adjacent to edge 0 as well.
            if j == i + 1 || (i == 0 && j == n - 1) {
                continue;
            }
            if segments_cross(
                points[i],
                points[(i + 1) % n],
                points[j],
                points[(j + 1) % n],
            ) {
                return Some((i, j));
            }
        }
    }
    None
}

/// Do the open segments `p→p2` and `q→q2` share a point?
///
/// Collinear overlap is deliberately not a crossing: an outline that
/// doubles back along itself is degenerate for other reasons, and both
/// kernels already refuse the zero volume it produces.
fn segments_cross(p: [f64; 2], p2: [f64; 2], q: [f64; 2], q2: [f64; 2]) -> bool {
    let d1 = cross(q, q2, p);
    let d2 = cross(q, q2, p2);
    let d3 = cross(p, p2, q);
    let d4 = cross(p, p2, q2);
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

/// Twice the signed area of the triangle `a, b, c`.
fn cross(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

/// Rewrite every outline in `node` into its canonical closed loop.
///
/// A rewrite rather than a refusal: the redundant closing point is the
/// most common thing a generator gets wrong about an outline, it means
/// exactly what the loop without it means, and refusing costs a round
/// trip to say so.
pub fn normalize(node: &CsgNode) -> CsgNode {
    match node {
        CsgNode::Extrude { outline, height } => CsgNode::Extrude {
            outline: closed_loop(outline),
            height: *height,
        },
        CsgNode::Revolve {
            outline,
            angle,
            segments,
        } => CsgNode::Revolve {
            outline: closed_loop(outline),
            angle: *angle,
            segments: *segments,
        },
        CsgNode::Loft { sections } => CsgNode::Loft {
            sections: sections
                .iter()
                .map(|section| LoftSection {
                    outline: closed_loop(&section.outline),
                    z: section.z,
                })
                .collect(),
        },
        other => other.map_children(normalize),
    }
}
