use manifold_rust::linalg::{Vec2, Vec3};
use manifold_rust::manifold::Manifold;
use manifold_rust::types::Error as KernelError;

use crate::error::GeometryError;
use crate::import::{Imports, MAX_IMPORT_TRIANGLES};
use crate::tree::CsgNode;

/// Validate a tree and evaluate it into a solid.
///
/// Validation is not optional and not the caller's job: the recursion
/// below has no depth guard of its own, and `Manifold::cylinder` with a
/// non-finite radius produces NaN vertices rather than an error.
pub fn evaluate(node: &CsgNode) -> Result<Manifold, GeometryError> {
    evaluate_with(node, &Imports::new())
}

/// [`evaluate`] for a tree with `import` nodes, given what they stand for.
pub fn evaluate_with(node: &CsgNode, imports: &Imports) -> Result<Manifold, GeometryError> {
    // Expansion validates the written tree and then the expanded one, so
    // this stands in for the plain `validate` that used to be here — and
    // it is the only thing that makes `build` below safe to write without
    // `pattern_*` arms.
    let node = &crate::pattern::prepare(node)?;
    // Refused here rather than inside the recursion so the message names
    // the operation instead of the node that happened to be reached
    // first. `hull` is the mirror image of this: OCCT refuses it, and
    // this kernel is where it gets built.
    crate::tree::kernel_need(node)?;
    let brep_only = first_brep_only_op(node);
    if !brep_only.is_empty() {
        return Err(GeometryError::MeshUnsupported { op: brep_only });
    }
    check_imports(node, imports)?;
    let solid = if matches!(node, CsgNode::SurfaceRelief { .. }) {
        crate::surface::build(node)?
    } else {
        build(node, imports)
    };

    let status = solid.status();
    if status != KernelError::NoError {
        return Err(GeometryError::Kernel(status));
    }
    // Checked after status, and separately from it: a solid that was
    // wholly subtracted away comes back empty with status NoError.
    // Verified against manifold-rust 0.10.
    if solid.is_empty() {
        return Err(GeometryError::EmptyResult);
    }
    // A third distinct failure, and the one that hides best: a
    // degenerate outline (collinear or repeated points) extrudes into a
    // solid that is *not* empty — 8 triangles, status NoError — and
    // encloses nothing. Verified against manifold-rust 0.10. Writing it
    // out would produce an STL that slices to no toolpaths at all.
    if solid.volume() <= 0.0 {
        return Err(GeometryError::ZeroVolume);
    }
    // Last guard, and the one manifold cannot be trusted to raise itself.
    // Measured 2026-08-05 on the shoe-rack deck tile: the kernel returned
    // `status = NoError`, `matches_tri_normals = true`, zero degenerate
    // triangles — and a mesh with 8 unpaired edges. Slicing an unclosed
    // surface produces silent nonsense, so refuse it here instead.
    let unpaired = unpaired_edges(&solid);
    if unpaired > 0 {
        return Err(GeometryError::NotWatertight { unpaired });
    }
    Ok(solid)
}

/// Count directed edges that are not part of exactly one opposed pair.
///
/// Runs on the kernel's own f64 vertices — the defect this catches is
/// present before the STL writer narrows anything.
///
/// See [`crate::mesh::count_unpaired`] for why "occurs exactly once" is
/// only half the condition. An earlier version of this function checked
/// that half alone, which caught the doubled-edge defect it was written
/// for but would have passed a kernel result with a face missing.
fn unpaired_edges(solid: &Manifold) -> usize {
    let mesh = solid.get_mesh_gl64(-1);
    let stride = mesh.num_prop as usize;
    let key = |index: u64| -> [u64; 3] {
        let at = index as usize * stride;
        [
            mesh.vert_properties[at].to_bits(),
            mesh.vert_properties[at + 1].to_bits(),
            mesh.vert_properties[at + 2].to_bits(),
        ]
    };
    crate::mesh::count_unpaired(
        mesh.tri_verts
            .as_chunks::<3>()
            .0
            .iter()
            .map(|tri| [key(tri[0]), key(tri[1]), key(tri[2])]),
    )
}

/// Every `import` of an expanded tree has its solid, and all of them together
/// stay within the limit, counted per use: expansion is what makes a mesh
/// within the limit into a tree past it.
fn check_imports(node: &CsgNode, imports: &Imports) -> Result<(), GeometryError> {
    let count = imported_triangles(node, imports)?;
    if count > MAX_IMPORT_TRIANGLES {
        return Err(GeometryError::ImportTooLarge {
            count,
            max: MAX_IMPORT_TRIANGLES,
        });
    }
    Ok(())
}

fn imported_triangles(node: &CsgNode, imports: &Imports) -> Result<usize, GeometryError> {
    let own = match node.import_ref() {
        Some(reference) => imports
            .get(&reference)
            .ok_or_else(|| GeometryError::ImportUnresolved {
                reference: reference.to_string(),
            })?
            .triangle_count(),
        None => 0,
    };
    node.children().iter().try_fold(own, |sum, child| {
        Ok(sum + imported_triangles(child, imports)?)
    })
}

/// Name of the first B-rep-only operation in the tree, for the message
/// this kernel refuses with. Empty when there is none.
fn first_brep_only_op(node: &CsgNode) -> &'static str {
    if matches!(
        node,
        CsgNode::Fillet { .. }
            | CsgNode::Chamfer { .. }
            | CsgNode::Shell { .. }
            | CsgNode::Loft { .. }
    ) {
        return node.op_name();
    }
    for child in node.children() {
        let found = first_brep_only_op(child);
        if !found.is_empty() {
            return found;
        }
    }
    ""
}

/// The recursion proper. Runs only on a validated tree, so it can build
/// unconditionally — every arithmetic input is already known finite and
/// in range.
fn build(node: &CsgNode, imports: &Imports) -> Manifold {
    match node {
        CsgNode::SurfaceRelief { .. } => unreachable!("relief is handled at the validated root"),
        CsgNode::Box { size } => Manifold::cube(Vec3::new(size[0], size[1], size[2]), false),
        CsgNode::Cylinder {
            diameter,
            diameter_top,
            height,
            segments,
        } => {
            let radius = diameter / 2.0;
            let radius_top = diameter_top.unwrap_or(*diameter) / 2.0;
            // Segments are passed per call on purpose. The global
            // `Quality::set_circular_segments` would be shared by every
            // concurrent RPC in the daemon.
            Manifold::cylinder(*height, radius, radius_top, *segments as i32)
        }
        CsgNode::Sphere { diameter, segments } => {
            // Same reason as the cylinder above: segments per call, never
            // through the global quality setting, which every concurrent
            // RPC in the daemon would share.
            Manifold::sphere(diameter / 2.0, *segments as i32)
        }
        CsgNode::Import { .. } => node
            .import_ref()
            .and_then(|reference| imports.get(&reference))
            .expect("check_imports found a solid for every import")
            .solid()
            .clone(),
        CsgNode::Translate { offset, child } => {
            build(child, imports).translate(Vec3::new(offset[0], offset[1], offset[2]))
        }
        CsgNode::Rotate { degrees, child } => {
            build(child, imports).rotate(degrees[0], degrees[1], degrees[2])
        }
        CsgNode::Mirror { normal, child } => {
            build(child, imports).mirror(Vec3::new(normal[0], normal[1], normal[2]))
        }
        // Unreachable because `evaluate` expands patterns first, and
        // stated rather than folded into a wildcard so that a future
        // caller who skips the expansion gets a loud panic instead of a
        // part missing all but one of its copies.
        CsgNode::PatternLinear { .. } | CsgNode::PatternCircular { .. } => {
            unreachable!("patterns are expanded before build")
        }
        // Refused by `evaluate` above; stated here so that adding a
        // B-rep-only operation without teaching `evaluate` about it is a
        // panic in a test rather than a wrong part in a customer's hands.
        CsgNode::Fillet { .. }
        | CsgNode::Chamfer { .. }
        | CsgNode::Shell { .. }
        | CsgNode::Loft { .. } => {
            unreachable!("B-rep-only operations are refused before build")
        }
        CsgNode::Union { children } => fold(children, imports, |acc, next| acc.union(&next)),
        CsgNode::Difference { children } => {
            fold(children, imports, |acc, next| acc.difference(&next))
        }
        CsgNode::Intersection { children } => {
            fold(children, imports, |acc, next| acc.intersection(&next))
        }
        // Hull is n-ary in the kernel rather than a fold: the hull of a
        // set is not the pairwise hull accumulated, and manifold builds
        // it from the combined point cloud in one pass.
        CsgNode::Hull { children } => {
            let parts: Vec<Manifold> = children.iter().map(|c| build(c, imports)).collect();
            Manifold::hull_manifolds(&parts)
        }
        CsgNode::Extrude { outline, height } => {
            let ring: Vec<Vec2> = outline.iter().map(|p| Vec2::new(p[0], p[1])).collect();
            // n_divisions 0, no twist, unit scale at the top: a straight
            // prism. Tapered and twisted extrusions are a later call if
            // anything asks for them.
            Manifold::extrude(&vec![ring], *height, 0, 0.0, Vec2::new(1.0, 1.0))
        }
        CsgNode::Revolve {
            outline,
            angle,
            segments,
        } => {
            // manifold revolves a cross-section around its Y axis and
            // calls the result's axis Z, so `[r, z]` maps straight onto
            // `(x, y)` with no rotation afterwards. Verified against
            // manifold-rust 0.10's `constructors::revolve`.
            let ring: Vec<Vec2> = outline.iter().map(|p| Vec2::new(p[0], p[1])).collect();
            Manifold::revolve(&vec![ring], *segments as i32, *angle)
        }
    }
}

/// Left-fold the children with `op`. `children` is non-empty — validation
/// rejects an empty list — so the first child is always available as the
/// seed, which is what makes a one-child `difference` the identity.
fn fold(
    children: &[CsgNode],
    imports: &Imports,
    op: impl Fn(Manifold, Manifold) -> Manifold,
) -> Manifold {
    let mut iter = children.iter();
    let mut acc = build(
        iter.next().expect("validated: children is non-empty"),
        imports,
    );
    for child in iter {
        acc = op(acc, build(child, imports));
    }
    acc
}
