use thiserror::Error;

/// Everything that can go wrong between a client's JSON and a finished
/// STL. Every variant names the offending value so the message can be
/// shown to a user without a debugger.
#[derive(Debug, Error)]
pub enum GeometryError {
    #[error("surface relief: {0}")]
    Surface(String),
    #[error("{field} must be greater than 0 and at most {max} mm, got {value}")]
    Dimension {
        field: &'static str,
        value: f64,
        max: f64,
    },
    #[error("{field} must be a finite number, got {value}")]
    NotFinite { field: &'static str, value: f64 },
    // Not "cylinder segments" any more: spheres and revolves count their
    // facets with the same field and the same limits.
    #[error("segments must be between {min} and {max}, got {value}")]
    Segments { value: u32, min: u32, max: u32 },
    #[error("a pattern makes {min} to {max} copies, got {value}")]
    PatternCount { value: u32, min: u32, max: u32 },
    #[error("a loft needs at least {min} sections, got {count}")]
    LoftSections { count: usize, min: usize },
    #[error("loft sections must climb: z {next} does not come above z {previous}")]
    LoftOrder { previous: f64, next: f64 },
    #[error(
        "`{brep}` cannot stand over `{mesh}`: it works on the faces and edges of a B-rep solid, \
         and `{mesh}` gives a mesh, which has neither. Keep `{brep}` only around the new geometry \
         and join the result to `{mesh}` outside it: `{brep}` of a block inside a `union` with \
         `{mesh}` is fine. The edges of a mesh itself can only be shaped by subtracting a solid \
         from it with `difference`"
    )]
    BrepOverMesh {
        brep: &'static str,
        mesh: &'static str,
    },
    #[error("`{op}` needs the B-rep kernel — the mesh kernel has no such operation")]
    MeshUnsupported { op: &'static str },
    #[error(
        "`import` names exactly one of `source_path` and `object_id`, a string of 1 to {max} bytes"
    )]
    ImportTarget { max: usize },
    #[error("nothing was read for the `import` of {reference}")]
    ImportUnresolved { reference: String },
    #[error(
        "this tree imports {count} triangles, counting every copy, and takes at most {max}: \
         fewer copies, or a coarser mesh"
    )]
    ImportTooLarge { count: usize, max: usize },
    #[error("`{op}` needs at least one child")]
    EmptyChildren { op: &'static str },
    #[error(
        "the outline crosses itself: the edge from point {first} meets the edge from point \
         {second}. An outline has to run around its shape once without touching itself — \
         reorder the points so the edges no longer cross"
    )]
    OutlineCrosses { first: usize, second: usize },
    #[error("an outline needs {min} to {max} points, got {value}")]
    Outline {
        value: usize,
        min: usize,
        max: usize,
    },
    #[error("tree is deeper than {max} levels")]
    TooDeep { max: usize },
    #[error("tree has more than {max} nodes")]
    TooLarge { max: usize },
    #[error("the result is empty — every solid was subtracted away")]
    EmptyResult,
    #[error(
        "the result encloses no volume — check for a degenerate outline or a zero-thickness solid"
    )]
    ZeroVolume,
    #[error(
        "the kernel produced a surface that is not closed ({unpaired} unpaired edges) — a \
         manifold defect on near-coincident geometry, not bad input; nudging the offending \
         features apart usually clears it"
    )]
    NotWatertight { unpaired: usize },
    #[error("manifold reported {0:?} while evaluating the tree")]
    Kernel(manifold_rust::types::Error),
}

/// Everything that can go wrong measuring a mesh that already exists.
///
/// Kept apart from [`GeometryError`], which is about building one: the
/// two are asked at different moments and a caller measuring a
/// downloaded STL should not have to reason about extrusion outlines.
#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("cannot read a mesh from {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("this does not parse as binary or ASCII STL")]
    NotStl,
    #[error("the mesh has no triangles")]
    Empty,
    #[error("{field} must be a finite number, got {value}")]
    NotFinite { field: &'static str, value: f64 },
    #[error(
        "the plane at Z{z} does not cut the mesh, whose Z range is {min_z} to {max_z} — there is \
         nothing to measure there"
    )]
    NoSection { z: f64, min_z: f64, max_z: f64 },
    #[error(
        "the axis at ({x}, {y}) is inside material at Z{z}, so there is no free circle around it \
         to measure — check the axis, not the number"
    )]
    AxisInsideMaterial { x: f64, y: f64, z: f64 },
    #[error("a scan needs a positive step, got {0}")]
    BadStep(f64),
}
