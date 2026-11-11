//! In-process geometry creation for the daemon — backend-2 of the
//! pluggable geometry contract.
//!
//! A client sends a CSG tree; this crate validates it, evaluates it with
//! `manifold-rust`, and hands back a binary STL. Deliberately free of
//! every other workspace crate so it builds without libslic3r and its
//! tests run on every pull request.

pub mod error;
pub mod eval;
pub mod import;
pub mod measure;
pub mod mesh;
pub mod outline;
pub mod pattern;
pub mod planar;
pub mod section;
pub mod serial;
pub mod shared_edge;
pub mod stl;
pub mod surface;
mod surface_formula;
pub mod thickness;
pub mod tree;

pub use error::{GeometryError, MeshError};
pub use eval::{evaluate, evaluate_with};
pub use import::{MAX_IMPORT_FILE_BYTES, MAX_IMPORT_TRIANGLES};
pub use measure::{centre_bore, free_circle, free_circle_over, inscribed_circle, profile};
pub use mesh::{Measurement, TriangleMesh};
pub use pattern::{expand_patterns, prepare};
pub use planar::{planar_faces, PlanarFace, PlanarFaces};
pub use section::{section_at_z, Section};
pub use stl::to_binary_stl;
pub use thickness::wall_thickness;
pub use tree::{
    brep_islands, extrude_divisions, kernel_need, Axis, CsgNode, EdgeSelector, KernelNeed,
    LoftSection, OpenFace, DEFAULT_TWIST_STEP_DEG, MAX_DEPTH, MAX_DIMENSION_MM, MAX_DIVISIONS,
    MAX_EXTRUDE_VERTICES, MAX_IMPORT_REF_BYTES, MAX_NODES, MAX_OUTLINE_POINTS, MAX_PATTERN_COUNT,
    MAX_SEGMENTS, MAX_TWIST_DEG, MAX_TWIST_EDGE_BEND_MM, MAX_TWIST_PER_DIVISION_DEG, MIN_DIVISIONS,
    MIN_LOFT_SECTIONS, MIN_OUTLINE_POINTS, MIN_SEGMENTS,
};

/// A finished part: the STL bytes plus the facts the RPC layer needs for
/// its `ObjectAdded` event, so callers never have to re-open the file or
/// re-derive them from the mesh.
#[derive(Debug, Clone)]
pub struct BuiltGeometry {
    pub stl: Vec<u8>,
    pub triangle_count: u32,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub volume_mm3: f64,
}

/// Validate, evaluate and serialize in one call — the whole public
/// surface the daemon needs.
pub fn build_stl(tree: &CsgNode) -> Result<BuiltGeometry, GeometryError> {
    build_stl_with(tree, &import::Imports::new())
}

/// [`build_stl`] for a tree with `import` nodes, given what they stand for.
pub fn build_stl_with(
    tree: &CsgNode,
    imports: &import::Imports,
) -> Result<BuiltGeometry, GeometryError> {
    let solid = evaluate_with(tree, imports)?;
    let bounds = solid.bounding_box();
    Ok(BuiltGeometry {
        stl: to_binary_stl(&solid),
        triangle_count: solid.num_tri() as u32,
        bounds_min: [
            bounds.min.x as f32,
            bounds.min.y as f32,
            bounds.min.z as f32,
        ],
        bounds_max: [
            bounds.max.x as f32,
            bounds.max.y as f32,
            bounds.max.z as f32,
        ],
        volume_mm3: solid.volume(),
    })
}
