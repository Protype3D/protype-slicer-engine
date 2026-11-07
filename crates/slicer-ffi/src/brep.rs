//! The B-rep evaluator: the same `CsgNode` tree the mesh kernel takes,
//! built in OCCT and written out as a STEP file.
//!
//! Lives here rather than next to `slicer-geometry`'s `eval.rs` because
//! that crate is deliberately free of every other workspace crate, so it
//! builds without libslic3r and its tests run on every pull request. OCCT
//! lives behind this crate's `with-libslic3r` feature, and dragging it
//! into `slicer-geometry` would take that property away.
//!
//! The recursion is here, in Rust, and C++ holds nothing but a table of
//! shapes: the tree walk is already written and tested against the mesh
//! kernel, and duplicating it across the FFI boundary would mean two
//! walks to keep in agreement.

use std::path::{Path, PathBuf};

use slicer_geometry::{CsgNode, GeometryError};

use crate::{
    brep_boolean, brep_box, brep_chamfer, brep_cylinder, brep_fillet, brep_loft, brep_mirror,
    brep_prism, brep_release, brep_revolve, brep_shell, brep_sphere, brep_transform, brep_volume,
    brep_write_step,
};

/// The one operation with no honest OCCT equivalent. Named once so the
/// message the caller matches on and the message it logs cannot drift.
const HULL: &str = "hull";

/// Why a tree could not be built as B-rep.
///
/// The variants are different answers, not different wordings of one:
/// `Unsupported` is a gap this kernel has and the mesh kernel does not,
/// so the caller should fall back; `Kernel` is OCCT refusing this
/// particular geometry, so the caller should fall back and say why;
/// `Invalid` is a tree neither kernel would accept, so falling back would
/// only fail again; `Write` is a body that was built and could not be
/// stored, which is a disk problem wearing a geometry costume.
#[derive(Debug)]
pub enum BrepError {
    /// An operation OCCT cannot express: `hull`, or a mesh to work on.
    Unsupported(&'static str),
    /// The tree failed `CsgNode::validate` — the limits are shared with
    /// the mesh kernel on purpose, so this is not a reason to retry there.
    Invalid(GeometryError),
    /// OCCT refused a node, or built a body enclosing nothing.
    Kernel(String),
    /// The solid exists but the STEP file does not.
    Write(String),
}

impl std::fmt::Display for BrepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrepError::Unsupported(op) => write!(f, "the B-rep kernel cannot build `{op}`"),
            BrepError::Invalid(e) => write!(f, "{e}"),
            BrepError::Kernel(why) => write!(f, "{why}"),
            BrepError::Write(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for BrepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BrepError::Invalid(e) => Some(e),
            _ => None,
        }
    }
}

/// A finished B-rep part on disk, plus the volume OCCT measured on the
/// solid itself — analytic, not summed over triangles.
#[derive(Debug, Clone)]
pub struct BuiltBrep {
    pub step_path: PathBuf,
    pub volume_mm3: f64,
}

/// Does this tree contain a `hull` anywhere?
///
/// Asked before building rather than discovered during it: OCCT has no
/// convex hull, so a tree carrying one is going to the mesh kernel no
/// matter how much of it OCCT could have built first.
///
/// Call it on a validated tree. `validate` is what caps nesting at
/// `MAX_DEPTH`, and this walk recurses without a guard of its own.
pub fn tree_contains_hull(node: &CsgNode) -> bool {
    match node {
        CsgNode::Hull { .. } => true,
        CsgNode::SurfaceRelief { child, .. }
        | CsgNode::Translate { child, .. }
        | CsgNode::Rotate { child, .. }
        | CsgNode::Mirror { child, .. }
        | CsgNode::PatternLinear { child, .. }
        | CsgNode::PatternCircular { child, .. }
        | CsgNode::Fillet { child, .. }
        | CsgNode::Chamfer { child, .. }
        | CsgNode::Shell { child, .. } => tree_contains_hull(child),
        CsgNode::Union { children }
        | CsgNode::Difference { children }
        | CsgNode::Intersection { children } => children.iter().any(tree_contains_hull),
        CsgNode::Box { .. }
        | CsgNode::Cylinder { .. }
        | CsgNode::Sphere { .. }
        | CsgNode::Extrude { .. }
        | CsgNode::Revolve { .. }
        | CsgNode::Import { .. }
        | CsgNode::Loft { .. } => false,
    }
}

/// Build `tree` as a B-rep solid and write it to `out_path` as STEP.
///
/// Validation comes first and is not the caller's job, for the same
/// reason it is not in the mesh evaluator: the recursion below has no
/// depth guard, and the limits are shared between the kernels so that no
/// input is legal for one and illegal for the other.
pub fn build_step(tree: &CsgNode, out_path: &Path) -> Result<BuiltBrep, BrepError> {
    // Preparation first, and it validates both the written tree and the
    // expanded one — the mesh kernel calls the same function, so a tree
    // too large once expanded, or with an outline spelled closed, is
    // treated identically by both.
    let tree = &slicer_geometry::prepare(tree).map_err(BrepError::Invalid)?;
    if tree_contains_hull(tree) {
        return Err(BrepError::Unsupported(HULL));
    }

    let solid = build(tree)?;

    // Checked separately from the per-node handles, and for a different
    // failure: every node can succeed and still leave nothing behind,
    // because a `difference` that removes everything is a valid boolean
    // with an empty result. `brep_volume` answers 0.0 for that, the same
    // as for a handle nobody holds.
    let volume = brep_volume(solid.handle);
    if !volume.is_finite() || volume <= 0.0 {
        return Err(BrepError::Kernel(format!(
            "OCCT built a body enclosing {volume} mm³ — every solid was subtracted away, or the \
             geometry was degenerate"
        )));
    }

    let path = out_path
        .to_str()
        .ok_or_else(|| BrepError::Write(format!("{} is not valid UTF-8", out_path.display())))?;
    if !brep_write_step(solid.handle, path) {
        return Err(BrepError::Write(format!(
            "the STEP writer refused {}",
            out_path.display()
        )));
    }

    Ok(BuiltBrep {
        step_path: out_path.to_path_buf(),
        volume_mm3: volume,
    })
}

/// A shape in the C++ table, released when it leaves scope.
///
/// A build is a recursion with a failure point at every node, and the
/// table outlives any one build — so releasing by hand would mean
/// remembering to do it on every early return, and leaking every shape
/// built so far the one time it was forgotten.
struct Shape {
    handle: u64,
}

impl Shape {
    /// `None` for the reserved handle 0, which the bridge returns for
    /// "this operation failed" — OCCT reports by throwing, and an
    /// exception must not cross the cxx boundary.
    fn new(handle: u64) -> Option<Shape> {
        (handle != 0).then_some(Shape { handle })
    }
}

impl Drop for Shape {
    fn drop(&mut self) {
        brep_release(self.handle);
    }
}

fn shape(handle: u64, what: &str) -> Result<Shape, BrepError> {
    Shape::new(handle).ok_or_else(|| BrepError::Kernel(format!("OCCT refused to build `{what}`")))
}

/// The recursion proper. Runs only on a validated, hull-free tree, so
/// every arithmetic input below is already known finite and in range.
fn build(node: &CsgNode) -> Result<Shape, BrepError> {
    match node {
        CsgNode::Box { size } => shape(brep_box(size[0], size[1], size[2]), "box"),
        CsgNode::Cylinder {
            diameter,
            diameter_top,
            height,
            segments: _,
        } => {
            // `segments` is ignored on purpose, and this is the whole
            // point of the backend: it tells the mesh kernel how many
            // chords to approximate a circle with, and a B-rep cylinder
            // is the circle.
            let radius = diameter / 2.0;
            let radius_top = diameter_top.unwrap_or(*diameter) / 2.0;
            shape(brep_cylinder(radius, radius_top, *height), "cylinder")
        }
        CsgNode::Sphere {
            diameter,
            segments: _,
        } => {
            // `segments` ignored for the reason the cylinder ignores it:
            // this kernel builds the sphere, not an approximation of one.
            shape(brep_sphere(diameter / 2.0), "sphere")
        }
        CsgNode::Translate { offset, child } => {
            let inner = build(child)?;
            shape(
                brep_transform(inner.handle, offset[0], offset[1], offset[2], 0.0, 0.0, 0.0),
                "translate",
            )
        }
        CsgNode::Rotate { degrees, child } => {
            let inner = build(child)?;
            shape(
                brep_transform(
                    inner.handle,
                    0.0,
                    0.0,
                    0.0,
                    degrees[0],
                    degrees[1],
                    degrees[2],
                ),
                "rotate",
            )
        }
        CsgNode::Mirror { normal, child } => {
            let inner = build(child)?;
            shape(
                brep_mirror(inner.handle, normal[0], normal[1], normal[2]),
                "mirror",
            )
        }
        CsgNode::Union { children } => fold(children, 0, "union"),
        CsgNode::Difference { children } => fold(children, 1, "difference"),
        CsgNode::Intersection { children } => fold(children, 2, "intersection"),
        CsgNode::Hull { .. } => Err(BrepError::Unsupported(HULL)),
        CsgNode::SurfaceRelief { .. } => Err(BrepError::Unsupported(
            "surface_relief requires the mesh kernel",
        )),
        CsgNode::Import { .. } => Err(BrepError::Unsupported("import")),
        // `build_step` expands patterns before recursing, so these cannot
        // arrive. Answered rather than panicked: this is a library, and a
        // caller reaching `build` by another road deserves an error.
        CsgNode::PatternLinear { .. } | CsgNode::PatternCircular { .. } => Err(BrepError::Kernel(
            "patterns must be expanded before building".to_string(),
        )),
        CsgNode::Extrude { outline, height } => {
            let flat: Vec<f64> = outline.iter().flat_map(|p| [p[0], p[1]]).collect();
            shape(brep_prism(&flat, *height), "extrude")
        }
        CsgNode::Fillet {
            radius,
            edges,
            child,
        } => {
            let inner = build(child)?;
            shape(brep_fillet(inner.handle, *radius, edges.code()), "fillet")
        }
        CsgNode::Chamfer {
            distance,
            edges,
            child,
        } => {
            let inner = build(child)?;
            shape(
                brep_chamfer(inner.handle, *distance, edges.code()),
                "chamfer",
            )
        }
        CsgNode::Shell {
            thickness,
            open,
            child,
        } => {
            let inner = build(child)?;
            shape(brep_shell(inner.handle, *thickness, open.code()), "shell")
        }
        CsgNode::Loft { sections } => {
            let mut flat: Vec<f64> = Vec::new();
            let mut lengths: Vec<u32> = Vec::new();
            let mut heights: Vec<f64> = Vec::new();
            for section in sections {
                lengths.push(section.outline.len() as u32);
                heights.push(section.z);
                flat.extend(section.outline.iter().flat_map(|p| [p[0], p[1]]));
            }
            shape(brep_loft(&flat, &lengths, &heights), "loft")
        }
        CsgNode::Revolve {
            outline,
            angle,
            segments: _,
        } => {
            // `segments` ignored as everywhere else in this kernel: the
            // swept surface is exact, not chorded.
            let flat: Vec<f64> = outline.iter().flat_map(|p| [p[0], p[1]]).collect();
            shape(brep_revolve(&flat, *angle), "revolve")
        }
    }
}

/// Left-fold the children with one boolean, mirroring the mesh
/// evaluator's `fold` node for node. `children` is non-empty —
/// validation rejects an empty list — so the first child is always
/// available as the seed, which is what makes a one-child `difference`
/// the identity in both kernels.
fn fold(children: &[CsgNode], op: u8, what: &str) -> Result<Shape, BrepError> {
    let mut iter = children.iter();
    let mut acc = build(iter.next().expect("validated: children is non-empty"))?;
    for child in iter {
        let next = build(child)?;
        // The operands are released as `acc` and `next` fall out of
        // scope. OCCT's result holds its own references to the geometry
        // it needs, so dropping the table's entries does not take it
        // away.
        acc = shape(brep_boolean(op, acc.handle, next.handle), what)?;
    }
    Ok(acc)
}
