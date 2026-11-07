//! Coordinate frames of a part's bodies. Three meet here:
//!
//! - **file** — the coordinates inside a body's mesh or STEP file;
//! - **design** — the part's own frame: file + `design_offset`;
//! - **recentred** — what every consumer actually places: the file moved so
//!   its bounds' XY centre and bottom sit at the origin, then the object's
//!   transform. The slicer (`apply_object_transform`), the UI
//!   (`recenterGeometryToLocalBbox`) and `compute_union_bounds` all do this.
//!
//! Object transforms are 4×4 matrices stored by columns, translation in
//! elements 12–14 — the layout the bridge reads.

use slicer_types::{BodyGeometry, Bounds, FeatureOp};

/// The point a consumer moves to the origin before applying a transform.
pub fn anchor(bounds: &Bounds) -> [f64; 3] {
    [
        (f64::from(bounds.min[0]) + f64::from(bounds.max[0])) * 0.5,
        (f64::from(bounds.min[1]) + f64::from(bounds.max[1])) * 0.5,
        f64::from(bounds.min[2]),
    ]
}

/// A body's anchor in the part's design frame.
pub fn design_anchor(geometry: &BodyGeometry) -> [f64; 3] {
    let a = anchor(&geometry.bounds);
    [
        a[0] + f64::from(geometry.design_offset[0]),
        a[1] + f64::from(geometry.design_offset[1]),
        a[2] + f64::from(geometry.design_offset[2]),
    ]
}

/// `transform · translate(delta)`: the same placement for geometry whose
/// anchor moved by `delta` in its own frame.
pub fn shift_transform(transform: &[f32; 16], delta: [f64; 3]) -> [f32; 16] {
    let mut out = *transform;
    for row in 0..3 {
        let moved = f64::from(transform[row]) * delta[0]
            + f64::from(transform[4 + row]) * delta[1]
            + f64::from(transform[8 + row]) * delta[2];
        out[12 + row] = (f64::from(transform[12 + row]) + moved) as f32;
    }
    out
}

/// `a − b` per axis. The order is the sign of the result: for the shift of an
/// anchor, pass `difference(new, old)` — the vector [`shift_transform`] needs
/// to keep the body where it was.
pub fn difference(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// How far, in millimetres, a body's anchor may move on each axis before its
/// placement has to follow.
///
/// A rebuild that leaves the bounds alone still moves the anchor by `f32`
/// rounding noise: `apply_operations` recentres the mesh in `f32`
/// (`recenter_to_local_bbox`) and hands back `f32` bounds and offset.
/// Replaying those steps on 20 000 random no-op rebuilds gave drifts up to
/// 5.7e-6 mm, so a limit of 1e-6 would call about half of them a move. 1e-3
/// is about 175 times that worst drift and far below any step a printer can
/// take.
pub const NEGLIGIBLE_SHIFT_MM: f64 = 1e-3;

/// Whether `delta` is within rounding noise on every axis
/// ([`NEGLIGIBLE_SHIFT_MM`]), so the placement can stay as it is.
pub fn is_negligible(delta: [f64; 3]) -> bool {
    delta.iter().all(|d| d.abs() < NEGLIGIBLE_SHIFT_MM)
}

/// A hole from the part's design frame into the frame `apply_operations`
/// cuts `input` in: that function recentres the mesh it loads on its own
/// bounds first, so a design point moves by the input's design anchor.
pub fn hole_in_apply_frame(op: &FeatureOp, input: &BodyGeometry) -> FeatureOp {
    let a = design_anchor(input);
    FeatureOp {
        origin: [
            (f64::from(op.origin[0]) - a[0]) as f32,
            (f64::from(op.origin[1]) - a[1]) as f32,
            (f64::from(op.origin[2]) - a[2]) as f32,
        ],
        ..op.clone()
    }
}

/// The design offset of what `apply_operations` writes for `input`: its
/// file is in the recentred frame, so design = file + the input's anchor.
pub fn hole_output_offset(input: &BodyGeometry) -> [f32; 3] {
    let a = design_anchor(input);
    [a[0] as f32, a[1] as f32, a[2] as f32]
}
