//! Patterns are expanded into plain unions before either kernel sees the
//! tree. Neither evaluator knows that repetition exists, which is what
//! keeps the two kernels from drifting on the one operation whose result
//! is defined by arithmetic rather than by geometry.

use crate::error::GeometryError;
use crate::tree::{Axis, CsgNode};

/// Put a written tree into the form both kernels build from.
///
/// The one entry point for everything that has to happen to a tree
/// before a kernel sees it, so that the two kernels cannot end up
/// preparing it differently — which they did: an outline whose last
/// point repeated the first built in OCCT and produced eight unpaired
/// edges in manifold, so the same part built or failed depending on
/// whether it also carried a chamfer.
pub fn prepare(node: &CsgNode) -> Result<CsgNode, GeometryError> {
    expand_patterns(&crate::outline::normalize(node))
}

/// Rewrite every `pattern_*` node into the union it stands for.
///
/// Validation runs twice on purpose: once on the written tree, which is
/// what caps `count`, and once on the expanded one, where the node budget
/// finally sees the real size of the work. A tree that fits the budget as
/// written can blow it once expanded, and that is exactly the case the
/// budget exists for.
pub fn expand_patterns(node: &CsgNode) -> Result<CsgNode, GeometryError> {
    node.validate()?;
    let expanded = expand(node);
    expanded.validate()?;
    Ok(expanded)
}

fn expand(node: &CsgNode) -> CsgNode {
    match node {
        CsgNode::PatternLinear { count, step, child } => {
            let inner = expand(child);
            CsgNode::Union {
                children: (0..*count)
                    .map(|index| {
                        let at = f64::from(index);
                        CsgNode::Translate {
                            offset: [step[0] * at, step[1] * at, step[2] * at],
                            child: std::boxed::Box::new(inner.clone()),
                        }
                    })
                    .collect(),
            }
        }
        CsgNode::PatternCircular {
            count,
            angle,
            axis,
            child,
        } => {
            let inner = expand(child);
            // A full turn puts the last copy back on the first, so the
            // step divides by `count`; a partial sweep is meant to reach
            // its far end, so it divides by `count - 1`. Validation keeps
            // `count` at two or more, so neither divisor is zero.
            let span = if (angle.abs() - 360.0).abs() < 1e-9 {
                *angle / f64::from(*count)
            } else {
                *angle / f64::from(*count - 1)
            };
            CsgNode::Union {
                children: (0..*count)
                    .map(|index| {
                        let turn = span * f64::from(index);
                        let degrees = match axis {
                            Axis::X => [turn, 0.0, 0.0],
                            Axis::Y => [0.0, turn, 0.0],
                            Axis::Z => [0.0, 0.0, turn],
                        };
                        CsgNode::Rotate {
                            degrees,
                            child: std::boxed::Box::new(inner.clone()),
                        }
                    })
                    .collect(),
            }
        }
        // The validated relief child has no patterns (only a primitive
        // and translations), so cloning cannot hide unexpanded work.
        CsgNode::SurfaceRelief { .. } => node.clone(),
        CsgNode::Translate { offset, child } => CsgNode::Translate {
            offset: *offset,
            child: std::boxed::Box::new(expand(child)),
        },
        CsgNode::Rotate { degrees, child } => CsgNode::Rotate {
            degrees: *degrees,
            child: std::boxed::Box::new(expand(child)),
        },
        CsgNode::Mirror { normal, child } => CsgNode::Mirror {
            normal: *normal,
            child: std::boxed::Box::new(expand(child)),
        },
        CsgNode::Fillet {
            radius,
            edges,
            child,
        } => CsgNode::Fillet {
            radius: *radius,
            edges: *edges,
            child: std::boxed::Box::new(expand(child)),
        },
        CsgNode::Chamfer {
            distance,
            edges,
            child,
        } => CsgNode::Chamfer {
            distance: *distance,
            edges: *edges,
            child: std::boxed::Box::new(expand(child)),
        },
        CsgNode::Shell {
            thickness,
            open,
            child,
        } => CsgNode::Shell {
            thickness: *thickness,
            open: *open,
            child: std::boxed::Box::new(expand(child)),
        },
        CsgNode::Union { children } => CsgNode::Union {
            children: children.iter().map(expand).collect(),
        },
        CsgNode::Difference { children } => CsgNode::Difference {
            children: children.iter().map(expand).collect(),
        },
        CsgNode::Intersection { children } => CsgNode::Intersection {
            children: children.iter().map(expand).collect(),
        },
        CsgNode::Hull { children } => CsgNode::Hull {
            children: children.iter().map(expand).collect(),
        },
        // Leaves are listed one by one rather than caught by `_`: a node
        // that carries a child and falls into a catch-all keeps its
        // patterns folded inside, and an evaluator promised it would
        // never meet a `pattern_*` then meets one. Spelling the leaves
        // out turns that into a compile error the day a node is added.
        CsgNode::Box { .. }
        | CsgNode::Cylinder { .. }
        | CsgNode::Sphere { .. }
        | CsgNode::Extrude { .. }
        | CsgNode::Revolve { .. }
        | CsgNode::Import { .. }
        | CsgNode::Loft { .. } => node.clone(),
    }
}
