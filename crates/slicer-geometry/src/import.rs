//! What an `import` node stands for by the time a tree is evaluated: a closed
//! solid made from triangles somebody else wrote.
//!
//! The bytes are the caller's to find. This crate reaches no file and no
//! project, so a tree only *names* a mesh, and whoever builds the tree reads
//! what it names into an [`Imports`] and hands that to
//! [`evaluate_with`](crate::eval::evaluate_with).
//!
//! A mesh that is not a solid is refused here rather than repaired. A boolean
//! on an open surface gives back something that looks like a part, and nothing
//! downstream can tell it from one. Pieces of a solid that touch along an edge
//! are a solid, though, only one the kernel cannot take as written, so that
//! edge is split here: see [`crate::shared_edge`].

use std::collections::HashMap;

use manifold_rust::manifold::Manifold;
use manifold_rust::types::{Error as KernelError, MeshGL64};

use crate::mesh::{count_unpaired, TriangleMesh};
use crate::shared_edge::{balanced, split_shared_edges};
use crate::tree::MAX_DIMENSION_MM;

/// Largest file an `import` reads. Binary STL takes 50 bytes a triangle and
/// ASCII STL about 250, so this holds [`MAX_IMPORT_TRIANGLES`] in either.
pub const MAX_IMPORT_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Most triangles imported into one tree, counted per use: a pattern of ten
/// copies costs ten times its mesh. Well inside the 90 s a client waits for
/// `geometry.build`: 524 288 triangles took 1.9 s to weld, cut and write on a
/// fully loaded M5 Max (measured 2026-10-30), and the cost grows with the mesh.
pub const MAX_IMPORT_TRIANGLES: usize = 1_000_000;

/// What an `import` node names. Two nodes naming one thing share one solid.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImportRef {
    Path(String),
    Object(String),
}

impl std::fmt::Display for ImportRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportRef::Path(path) => write!(f, "{path}"),
            ImportRef::Object(id) => write!(f, "object {id}"),
        }
    }
}

/// Everything a tree imports, read.
pub type Imports = HashMap<ImportRef, ImportedSolid>;

/// Why a mesh is not a solid an `import` can stand for.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("the mesh has no triangles")]
    Empty,
    #[error("the mesh has {count} triangles and an `import` takes at most {max}")]
    TooManyTriangles { count: usize, max: usize },
    #[error(
        "the mesh has a vertex coordinate {value}, which is not a number within \
         {MAX_DIMENSION_MM} mm of the origin"
    )]
    BadCoordinate { value: f64 },
    #[error(
        "the mesh is not closed: {open} edges have no single partner, from a hole or from faces \
         on one edge that do not pair up round it, so it has no inside for a boolean to work on. \
         Repair the mesh and import it again"
    )]
    NotClosed { open: usize },
    #[error(
        "the mesh is inside out: its surface encloses {volume} mm³, so what it calls its inside \
         is the rest of the world. Flip its normals and import it again"
    )]
    InsideOut { volume: f64 },
    #[error("the mesh encloses no volume")]
    ZeroVolume,
    #[error("manifold reported {0:?} on the mesh, which passed every check before it")]
    Kernel(KernelError),
}

/// A closed, outward-facing solid, ready for the booleans.
pub struct ImportedSolid {
    solid: Manifold,
}

impl ImportedSolid {
    /// Weld `mesh` the way `mesh.measure` does and take it for a solid, or say
    /// why not. A mesh `mesh.measure` calls closed is one this accepts, and so
    /// is one that only a sliver the weld collapsed keeps it from calling
    /// closed: an STL rounded to `f32` can hold those. So is one whose only
    /// unpaired edges are where pieces of it touch: those come out split and
    /// [`SHARED_EDGE_GAP_MM`](crate::shared_edge::SHARED_EDGE_GAP_MM) apart.
    pub fn from_mesh(mesh: &TriangleMesh) -> Result<Self, ImportError> {
        if mesh.is_empty() {
            return Err(ImportError::Empty);
        }
        if mesh.triangles.len() > MAX_IMPORT_TRIANGLES {
            return Err(ImportError::TooManyTriangles {
                count: mesh.triangles.len(),
                max: MAX_IMPORT_TRIANGLES,
            });
        }
        // Before welding: the weld hashes coordinates into a grid, and NaN has
        // no place on one.
        let mut coordinates = mesh.triangles.iter().flatten().flatten();
        if let Some(value) = coordinates.find(|v| !v.is_finite() || v.abs() > MAX_DIMENSION_MM) {
            return Err(ImportError::BadCoordinate { value: *value });
        }

        let (vertices, triangles) = mesh.indexed();
        let open = count_unpaired(triangles.iter().copied());
        let (vertices, triangles) = if open == 0 {
            (vertices, triangles)
        } else if let Some(split) = split_shared_edges(&vertices, &triangles) {
            split
        } else if balanced(&triangles) && mesh.volume_mm3() < 0.0 {
            // Wound inside out as a whole: round a shared edge the faces then
            // pair across the empty wedges, and nothing splits.
            return Err(ImportError::InsideOut {
                volume: mesh.volume_mm3(),
            });
        } else {
            return Err(ImportError::NotClosed { open });
        };
        // Only now does this number mean anything: the divergence theorem is
        // for closed surfaces.
        let volume = mesh.volume_mm3();
        if volume == 0.0 {
            return Err(ImportError::ZeroVolume);
        }
        if volume < 0.0 {
            return Err(ImportError::InsideOut { volume });
        }

        let solid = Manifold::from_mesh_gl64(&MeshGL64 {
            num_prop: 3,
            vert_properties: vertices.iter().flatten().copied().collect(),
            tri_verts: triangles.iter().flatten().map(|v| *v as u64).collect(),
            ..Default::default()
        });
        match solid.status() {
            KernelError::NoError => Ok(Self { solid }),
            status => Err(ImportError::Kernel(status)),
        }
    }

    pub fn triangle_count(&self) -> usize {
        self.solid.num_tri()
    }

    pub(crate) fn solid(&self) -> &Manifold {
        &self.solid
    }
}
