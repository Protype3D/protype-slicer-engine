use serde::{Deserialize, Serialize};

use crate::error::GeometryError;
use crate::import::ImportRef;

/// Upper bound on total nodes in one tree. Generous enough for a
/// pattern-generated part (the shoe-rack deck tile is ~55 nodes) and low
/// enough that a runaway generator fails fast instead of exhausting
/// memory.
pub const MAX_NODES: usize = 4096;
/// Nesting limit. `validate` and `eval` both recurse, so this is what
/// keeps a hostile tree from overflowing the stack.
pub const MAX_DEPTH: usize = 64;
/// Largest accepted length in millimetres — two orders of magnitude past
/// any printable bed.
pub const MAX_DIMENSION_MM: f64 = 10_000.0;
/// Bounds on an `extrude` outline. Three points is the least that
/// encloses area; the ceiling keeps one node from carrying an unbounded
/// point cloud, since the node budget counts nodes and not their size.
pub const MIN_OUTLINE_POINTS: usize = 3;
pub const MAX_OUTLINE_POINTS: usize = 1024;
pub const MIN_SEGMENTS: u32 = 3;
pub const MAX_SEGMENTS: u32 = 512;
/// Largest `extrude` twist, either way: a hundred turns, enough for any
/// thread a part carries and far short of a runaway value.
pub const MAX_TWIST_DEG: f64 = 36_000.0;
/// Bounds on the layers an `extrude` is cut into along its height.
pub const MIN_DIVISIONS: u32 = 1;
pub const MAX_DIVISIONS: u32 = 4096;
/// Turn per layer when a twist comes without `divisions`.
pub const DEFAULT_TWIST_STEP_DEG: f64 = 10.0;
/// Most an outline may turn between two layers. Past it the twist stops
/// reading as a helix: one layer of a full turn lands the top on the
/// bottom and the result is a plain prism, with no error to say so. It is
/// that guard and not an accuracy bound: at 20° a point 30 mm off the axis
/// already dips 0.46 mm towards it between layers (`docs/extrude-twist.md`,
/// «Точность»), so the caller picks the turn per layer by radius.
pub const MAX_TWIST_PER_DIVISION_DEG: f64 = 20.0;
/// Most an outline edge may stray from the twisted surface between two
/// layers. One diagonal per quad joins a layer to the next, and halfway up
/// it stands ¼·L·sin θ off the surface for an edge of length L turning θ
/// per layer — out for one twist sign, in for the other, so a long edge
/// either bulges or cuts across its neighbour. The mesh kernel cuts longer
/// edges into equal pieces until each stays within this; 0.05 mm is well
/// under one FDM line.
pub const MAX_TWIST_EDGE_BEND_MM: f64 = 0.05;
/// Ceiling on outline points × (divisions + 1) for one `extrude` node,
/// counting the points after long edges are cut for a twist
/// (`MAX_TWIST_EDGE_BEND_MM`). The side walls cost about two triangles per
/// vertex, so one node stays within the million triangles the engine
/// already accepts from `import`; a pattern of such nodes multiplies it.
pub const MAX_EXTRUDE_VERTICES: usize = 500_000;
/// Longest `source_path` or `object_id` an `import` carries, for the reason
/// outlines have a ceiling: the node budget counts nodes and not their size.
pub const MAX_IMPORT_REF_BYTES: usize = 4096;

fn default_segments() -> u32 {
    48
}

/// Layers along the height of an `extrude`: the given number, else 1
/// without a twist and `ceil(|twist| / DEFAULT_TWIST_STEP_DEG)` with one,
/// kept within `MIN_DIVISIONS..=MAX_DIVISIONS`. Call it with a twist that
/// has already passed validation, so it is finite.
///
/// An out-of-range value is clamped, not refused: validation refuses it
/// first, so callers reach this with values already in range.
pub fn extrude_divisions(twist_deg: Option<f64>, divisions: Option<u32>) -> u32 {
    let wanted = match (divisions, twist_deg) {
        (Some(d), _) => d,
        (None, Some(t)) if t != 0.0 => (t.abs() / DEFAULT_TWIST_STEP_DEG).ceil() as u32,
        (None, _) => MIN_DIVISIONS,
    };
    wanted.clamp(MIN_DIVISIONS, MAX_DIVISIONS)
}

/// Turn of an `extrude` outline between two of its `layers`, in degrees,
/// either way; zero without a twist. What `check_extrude_twist` caps and
/// what the mesh kernel cuts long edges by, so the two cannot drift apart.
pub(crate) fn twist_per_division(twist_deg: Option<f64>, layers: u32) -> f64 {
    twist_deg.map_or(0.0, |twist| twist.abs() / f64::from(layers))
}

/// A sweep with no angle given is a whole turn — the common case by far,
/// and the one a generator omits without meaning to.
fn default_full_turn() -> f64 {
    360.0
}

fn default_axis() -> Axis {
    Axis::Z
}

/// Axis of a circular pattern. A free vector would be more general and
/// much harder for a generator to get right; three named axes cover the
/// parts people actually ask for.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    X,
    Y,
    Z,
}

/// Largest number of copies one pattern may ask for. Well below
/// `MAX_NODES` on purpose: expansion multiplies the child, so the child's
/// own size still has to fit the budget afterwards.
pub const MAX_PATTERN_COUNT: u32 = 512;

/// Which edges an edge operation touches.
///
/// A generator cannot name edges by identifier — it has never seen the
/// solid. Words are what it can produce, and these five cover what
/// actually gets asked for: rounded corners (`vertical`), a rounded top
/// lip (`top`), everything (`all`). `bottom` exists mostly to be left
/// alone: a rounded bottom edge shrinks the footprint holding the part to
/// the bed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeSelector {
    All,
    Vertical,
    Top,
    Bottom,
    Horizontal,
}

impl EdgeSelector {
    /// Wire encoding shared with the bridge. A method rather than two
    /// hand-written matches so `fillet` and `chamfer` cannot disagree.
    pub fn code(self) -> u8 {
        match self {
            EdgeSelector::All => 0,
            EdgeSelector::Vertical => 1,
            EdgeSelector::Top => 2,
            EdgeSelector::Bottom => 3,
            EdgeSelector::Horizontal => 4,
        }
    }
}

/// Which face a shelled body is left open at.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenFace {
    Top,
    Bottom,
    None,
}

impl OpenFace {
    pub fn code(self) -> u8 {
        match self {
            OpenFace::None => 0,
            OpenFace::Top => 1,
            OpenFace::Bottom => 2,
        }
    }
}

/// One cross-section of a loft: a closed outline in XY at height `z`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoftSection {
    pub outline: Vec<[f64; 2]>,
    pub z: f64,
}

/// Fewest sections a loft can be built from.
pub const MIN_LOFT_SECTIONS: usize = 2;

/// Which kernel a tree can be built by.
///
/// Not every operation exists in both: OCCT has no convex hull and no idea
/// what to do with a mesh, and manifold has no fillet and nothing to fake one
/// with. `MeshOnly` still allows B-rep operations *beside* the mesh ones: see
/// [`brep_islands`]. Over them it is impossible rather than hard, and
/// [`kernel_need`] says so before anything is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelNeed {
    Any,
    BrepOnly,
    MeshOnly,
}

/// One node of a constructive-solid-geometry tree.
///
/// Primitives sit at the origin and are placed by wrapping them in
/// `Translate`/`Rotate` — the OpenSCAD arrangement. That keeps every
/// primitive's own parameters to pure dimensions, so validation of a
/// shape and of its placement never get tangled.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum CsgNode {
    /// Final, sampled outward relief. Supported bases and limits are checked
    /// before allocation; this operation must be the root of the tree.
    SurfaceRelief {
        faces: crate::surface::SurfaceFaces,
        pattern: crate::surface::SurfacePattern,
        amplitude: f64,
        pitch: f64,
        resolution: f64,
        #[serde(default)]
        angle: f64,
        #[serde(default)]
        seed: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        formula: Option<String>,
        child: std::boxed::Box<CsgNode>,
    },
    /// Axis-aligned box with its minimum corner at the origin.
    Box {
        size: [f64; 3],
    },
    /// Z-axis cylinder with its base centre at the origin. Give
    /// `diameter_top` to taper it into a frustum, or 0.0 for a cone —
    /// the same job OpenSCAD's `d1`/`d2` do.
    Cylinder {
        diameter: f64,
        #[serde(default)]
        diameter_top: Option<f64>,
        height: f64,
        #[serde(default = "default_segments")]
        segments: u32,
    },
    /// Sphere centred on the origin.
    ///
    /// Centred, unlike `cylinder`, which sits on its base: a ball has no
    /// natural resting point, and centring it is what makes `translate`
    /// place the centre — which is how "a 20 mm ball at x = 30" is meant.
    Sphere {
        diameter: f64,
        #[serde(default = "default_segments")]
        segments: u32,
    },
    Translate {
        offset: [f64; 3],
        child: std::boxed::Box<CsgNode>,
    },
    /// Rotation in degrees about X, then Y, then Z.
    Rotate {
        degrees: [f64; 3],
        child: std::boxed::Box<CsgNode>,
    },
    /// Mirror across the plane through the origin with this normal.
    /// Placing the result stays the job of `translate`, as with every
    /// other operation here.
    Mirror {
        normal: [f64; 3],
        child: std::boxed::Box<CsgNode>,
    },
    /// `count` copies of the child, each `step` further along than the
    /// last. The first copy sits where the child already is.
    PatternLinear {
        count: u32,
        step: [f64; 3],
        child: std::boxed::Box<CsgNode>,
    },
    /// `count` copies spread over `degrees` around `axis` through the
    /// origin. The child moves itself off the axis with its own
    /// `translate`, the way OpenSCAD does it.
    PatternCircular {
        count: u32,
        #[serde(default = "default_full_turn")]
        angle: f64,
        #[serde(default = "default_axis")]
        axis: Axis,
        child: std::boxed::Box<CsgNode>,
    },
    Union {
        children: Vec<CsgNode>,
    },
    /// First child minus every later child. A single child is the
    /// identity, which keeps generated trees simple.
    Difference {
        children: Vec<CsgNode>,
    },
    Intersection {
        children: Vec<CsgNode>,
    },
    /// Convex hull of every child taken together.
    Hull {
        children: Vec<CsgNode>,
    },
    /// A closed 2D outline in the XY plane pulled up along +Z from z=0 —
    /// OpenSCAD's `linear_extrude(height) polygon(outline)`. Winding is
    /// normalized by the kernel, so either direction works. The outline
    /// may be concave; for a profile with a hole, subtract.
    ///
    /// `twist_deg` turns the top outline against the bottom one over the
    /// whole height, counter-clockwise seen from +Z looking down (a
    /// right-hand thread going up); `divisions` is the layer count along
    /// the height. Both are left out of the JSON when unset.
    Extrude {
        outline: Vec<[f64; 2]>,
        height: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        twist_deg: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        divisions: Option<u32>,
    },
    /// Round the selected edges of the child with a constant radius.
    Fillet {
        radius: f64,
        edges: EdgeSelector,
        child: std::boxed::Box<CsgNode>,
    },
    /// Cut the selected edges of the child back by a constant distance.
    /// On a bottom edge this is what a fillet cannot be: a chamfer leaves
    /// the first layer flat, so the part still holds to the bed.
    Chamfer {
        distance: f64,
        edges: EdgeSelector,
        child: std::boxed::Box<CsgNode>,
    },
    /// A solid that already exists, in one of two places: a mesh or STEP file
    /// (`source_path`), or an object of the scene (`object_id`). Exactly one of
    /// them.
    ///
    /// An object is taken as it prints, with the holes of its feature stack, in
    /// the frame of its source file: the frame `mesh.*` answers in, and not
    /// where it stands on the plate. Placing it is the job of `translate`, as
    /// with every other leaf.
    ///
    /// This crate reads neither files nor projects, so a tree only names what
    /// it imports: see [`crate::import`].
    Import {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        object_id: Option<String>,
    },
    /// Hollow the child out, leaving walls `thickness` thick. `open`
    /// names the face to remove — a closed shell encloses a void nothing
    /// can reach, which is rarely what was meant.
    Shell {
        thickness: f64,
        open: OpenFace,
        child: std::boxed::Box<CsgNode>,
    },
    /// A solid stretched through a stack of cross-sections, bottom
    /// upwards. Two sections make a frustum; more make a shape `extrude`
    /// cannot. Walls run straight from one section to the next, so flat
    /// sections give flat faces and the solid never bulges past them.
    Loft {
        sections: Vec<LoftSection>,
    },
    /// A closed outline in the XZ half-plane swept around the Z axis.
    /// Each point is `[r, z]`: `r` is the distance from the axis and may
    /// not be negative. Everything turned on a lathe is this operation —
    /// tubes, knobs, nozzles, vases.
    Revolve {
        outline: Vec<[f64; 2]>,
        #[serde(default = "default_full_turn")]
        angle: f64,
        #[serde(default = "default_segments")]
        segments: u32,
    },
}

impl CsgNode {
    /// Reject anything the evaluator is not prepared to meet: non-finite
    /// numbers, non-positive or absurd dimensions, empty operand lists,
    /// and trees too deep or too large.
    pub fn validate(&self) -> Result<(), GeometryError> {
        let mut nodes = 0usize;
        self.validate_at(1, &mut nodes)
    }

    fn validate_at(&self, depth: usize, nodes: &mut usize) -> Result<(), GeometryError> {
        if depth > MAX_DEPTH {
            return Err(GeometryError::TooDeep { max: MAX_DEPTH });
        }
        *nodes += 1;
        if *nodes > MAX_NODES {
            return Err(GeometryError::TooLarge { max: MAX_NODES });
        }

        match self {
            CsgNode::SurfaceRelief { child, .. } => {
                if depth != 1 {
                    return Err(GeometryError::Surface(
                        "surface_relief must be the final root operation".into(),
                    ));
                }
                child.validate_at(depth + 1, nodes)?;
                crate::surface::validate(self)
            }
            CsgNode::Box { size } => {
                for (axis, value) in ["size.x", "size.y", "size.z"].iter().zip(size) {
                    check_dimension(axis, *value)?;
                }
                Ok(())
            }
            CsgNode::Cylinder {
                diameter,
                diameter_top,
                height,
                segments,
            } => {
                check_dimension("diameter", *diameter)?;
                check_dimension("height", *height)?;
                // A zero top diameter is a cone tip, so this one is
                // allowed to be 0 where every other length is not.
                if let Some(top) = diameter_top {
                    check_finite("diameter_top", *top)?;
                    if *top < 0.0 || *top > MAX_DIMENSION_MM {
                        return Err(GeometryError::Dimension {
                            field: "diameter_top",
                            value: *top,
                            max: MAX_DIMENSION_MM,
                        });
                    }
                }
                check_segments(*segments)
            }
            CsgNode::Sphere { diameter, segments } => {
                check_dimension("diameter", *diameter)?;
                check_segments(*segments)
            }
            CsgNode::Translate { offset, child } => {
                for (axis, value) in ["offset.x", "offset.y", "offset.z"].iter().zip(offset) {
                    check_finite(axis, *value)?;
                }
                child.validate_at(depth + 1, nodes)
            }
            CsgNode::Rotate { degrees, child } => {
                for (axis, value) in ["degrees.x", "degrees.y", "degrees.z"].iter().zip(degrees) {
                    check_finite(axis, *value)?;
                }
                child.validate_at(depth + 1, nodes)
            }
            CsgNode::Mirror { normal, child } => {
                for (axis, value) in ["normal.x", "normal.y", "normal.z"].iter().zip(normal) {
                    check_finite(axis, *value)?;
                }
                // A zero normal describes no plane at all, and manifold
                // answers it with an empty solid rather than an error.
                if normal.iter().all(|value| *value == 0.0) {
                    return Err(GeometryError::NotFinite {
                        field: "normal",
                        value: 0.0,
                    });
                }
                child.validate_at(depth + 1, nodes)
            }
            CsgNode::PatternLinear { count, step, child } => {
                check_count(*count)?;
                for (axis, value) in ["step.x", "step.y", "step.z"].iter().zip(step) {
                    check_finite(axis, *value)?;
                }
                child.validate_at(depth + 1, nodes)
            }
            CsgNode::PatternCircular {
                count,
                angle,
                axis: _,
                child,
            } => {
                check_count(*count)?;
                check_finite("angle", *angle)?;
                child.validate_at(depth + 1, nodes)
            }
            CsgNode::Union { children } => validate_children("union", children, depth, nodes),
            CsgNode::Difference { children } => {
                validate_children("difference", children, depth, nodes)
            }
            CsgNode::Intersection { children } => {
                validate_children("intersection", children, depth, nodes)
            }
            CsgNode::Hull { children } => validate_children("hull", children, depth, nodes),
            CsgNode::Extrude {
                outline,
                height,
                twist_deg,
                divisions,
            } => {
                check_outline(outline, ["outline.x", "outline.y"])?;
                check_dimension("height", *height)?;
                check_extrude_twist(outline, *twist_deg, *divisions)
            }
            CsgNode::Import {
                source_path,
                object_id,
            } => check_import(source_path.as_deref(), object_id.as_deref()),
            CsgNode::Fillet {
                radius,
                edges: _,
                child,
            } => {
                check_dimension("radius", *radius)?;
                child.validate_at(depth + 1, nodes)
            }
            CsgNode::Chamfer {
                distance,
                edges: _,
                child,
            } => {
                check_dimension("distance", *distance)?;
                child.validate_at(depth + 1, nodes)
            }
            CsgNode::Shell {
                thickness,
                open: _,
                child,
            } => {
                check_dimension("thickness", *thickness)?;
                child.validate_at(depth + 1, nodes)
            }
            CsgNode::Loft { sections } => {
                if sections.len() < MIN_LOFT_SECTIONS {
                    return Err(GeometryError::LoftSections {
                        count: sections.len(),
                        min: MIN_LOFT_SECTIONS,
                    });
                }
                let mut previous: Option<f64> = None;
                for section in sections {
                    check_outline(
                        &section.outline,
                        ["sections.outline.x", "sections.outline.y"],
                    )?;
                    check_finite("sections.z", section.z)?;
                    // Sections have to climb: two at the same height give
                    // OCCT a zero-length sweep, and one below the last
                    // turns the solid inside out at that step.
                    if let Some(last) = previous {
                        if section.z <= last {
                            return Err(GeometryError::LoftOrder {
                                previous: last,
                                next: section.z,
                            });
                        }
                    }
                    previous = Some(section.z);
                }
                Ok(())
            }
            CsgNode::Revolve {
                outline,
                angle,
                segments,
            } => {
                check_outline(outline, ["outline.r", "outline.z"])?;
                for point in outline {
                    // A profile crossing the axis is a modelling mistake,
                    // and the mesh kernel answers it by silently clipping
                    // the negative side — building a part nobody asked
                    // for. Refuse it while it is still a tree.
                    if point[0] < 0.0 {
                        return Err(GeometryError::Dimension {
                            field: "outline.r",
                            value: point[0],
                            max: MAX_DIMENSION_MM,
                        });
                    }
                }
                if !(*angle > 0.0 && *angle <= 360.0) {
                    return Err(GeometryError::Dimension {
                        field: "angle",
                        value: *angle,
                        max: 360.0,
                    });
                }
                check_segments(*segments)
            }
        }
    }
}

impl CsgNode {
    /// The operation's name as it appears in JSON. Used in refusals, so
    /// it has to match the serde tag exactly.
    pub fn op_name(&self) -> &'static str {
        match self {
            CsgNode::SurfaceRelief { .. } => "surface_relief",
            CsgNode::Box { .. } => "box",
            CsgNode::Cylinder { .. } => "cylinder",
            CsgNode::Sphere { .. } => "sphere",
            CsgNode::Translate { .. } => "translate",
            CsgNode::Rotate { .. } => "rotate",
            CsgNode::Mirror { .. } => "mirror",
            CsgNode::PatternLinear { .. } => "pattern_linear",
            CsgNode::PatternCircular { .. } => "pattern_circular",
            CsgNode::Union { .. } => "union",
            CsgNode::Difference { .. } => "difference",
            CsgNode::Intersection { .. } => "intersection",
            CsgNode::Hull { .. } => "hull",
            CsgNode::Extrude { .. } => "extrude",
            CsgNode::Revolve { .. } => "revolve",
            CsgNode::Import { .. } => "import",
            CsgNode::Fillet { .. } => "fillet",
            CsgNode::Chamfer { .. } => "chamfer",
            CsgNode::Shell { .. } => "shell",
            CsgNode::Loft { .. } => "loft",
        }
    }

    /// Every child of this node, in written order.
    pub fn children(&self) -> Vec<&CsgNode> {
        match self {
            CsgNode::SurfaceRelief { child, .. }
            | CsgNode::Translate { child, .. }
            | CsgNode::Rotate { child, .. }
            | CsgNode::Mirror { child, .. }
            | CsgNode::PatternLinear { child, .. }
            | CsgNode::PatternCircular { child, .. }
            | CsgNode::Fillet { child, .. }
            | CsgNode::Chamfer { child, .. }
            | CsgNode::Shell { child, .. } => vec![child],
            CsgNode::Union { children }
            | CsgNode::Difference { children }
            | CsgNode::Intersection { children }
            | CsgNode::Hull { children } => children.iter().collect(),
            CsgNode::Box { .. }
            | CsgNode::Cylinder { .. }
            | CsgNode::Sphere { .. }
            | CsgNode::Extrude { .. }
            | CsgNode::Revolve { .. }
            | CsgNode::Import { .. }
            | CsgNode::Loft { .. } => Vec::new(),
        }
    }

    /// What this one node imports, if it is an `import`. A validated node names
    /// exactly one thing.
    pub fn import_ref(&self) -> Option<ImportRef> {
        match self {
            CsgNode::Import {
                source_path: Some(path),
                ..
            } => Some(ImportRef::Path(path.clone())),
            CsgNode::Import {
                object_id: Some(id),
                ..
            } => Some(ImportRef::Object(id.clone())),
            _ => None,
        }
    }

    /// Everything this tree imports, each thing once, in written order.
    pub fn imports(&self) -> Vec<ImportRef> {
        let mut found = Vec::new();
        self.collect_imports(&mut found);
        found
    }

    fn collect_imports(&self, found: &mut Vec<ImportRef>) {
        if let Some(reference) = self.import_ref().filter(|r| !found.contains(r)) {
            found.push(reference);
        }
        for child in self.children() {
            child.collect_imports(found);
        }
    }

    /// This node with every child replaced by `f(child)`, and
    /// everything else about it kept.
    ///
    /// Written once here rather than as an eighteen-arm match in each
    /// tree-rewriting pass: a pass that only cares about two node kinds
    /// should not have to enumerate the other sixteen, and adding a
    /// nineteenth operation should not silently skip a pass that forgot
    /// it.
    pub fn map_children(&self, f: impl Fn(&CsgNode) -> CsgNode + Copy) -> CsgNode {
        let one = |child: &CsgNode| std::boxed::Box::new(f(child));
        let many = |children: &Vec<CsgNode>| children.iter().map(f).collect();
        match self {
            CsgNode::SurfaceRelief {
                faces,
                pattern,
                amplitude,
                pitch,
                resolution,
                angle,
                seed,
                formula,
                child,
            } => CsgNode::SurfaceRelief {
                faces: *faces,
                pattern: *pattern,
                amplitude: *amplitude,
                pitch: *pitch,
                resolution: *resolution,
                angle: *angle,
                seed: *seed,
                formula: formula.clone(),
                child: one(child),
            },
            CsgNode::Translate { offset, child } => CsgNode::Translate {
                offset: *offset,
                child: one(child),
            },
            CsgNode::Rotate { degrees, child } => CsgNode::Rotate {
                degrees: *degrees,
                child: one(child),
            },
            CsgNode::Mirror { normal, child } => CsgNode::Mirror {
                normal: *normal,
                child: one(child),
            },
            CsgNode::PatternLinear { count, step, child } => CsgNode::PatternLinear {
                count: *count,
                step: *step,
                child: one(child),
            },
            CsgNode::PatternCircular {
                count,
                angle,
                axis,
                child,
            } => CsgNode::PatternCircular {
                count: *count,
                angle: *angle,
                axis: *axis,
                child: one(child),
            },
            CsgNode::Fillet {
                radius,
                edges,
                child,
            } => CsgNode::Fillet {
                radius: *radius,
                edges: *edges,
                child: one(child),
            },
            CsgNode::Chamfer {
                distance,
                edges,
                child,
            } => CsgNode::Chamfer {
                distance: *distance,
                edges: *edges,
                child: one(child),
            },
            CsgNode::Shell {
                thickness,
                open,
                child,
            } => CsgNode::Shell {
                thickness: *thickness,
                open: *open,
                child: one(child),
            },
            CsgNode::Union { children } => CsgNode::Union {
                children: many(children),
            },
            CsgNode::Difference { children } => CsgNode::Difference {
                children: many(children),
            },
            CsgNode::Intersection { children } => CsgNode::Intersection {
                children: many(children),
            },
            CsgNode::Hull { children } => CsgNode::Hull {
                children: many(children),
            },
            leaf @ (CsgNode::Box { .. }
            | CsgNode::Cylinder { .. }
            | CsgNode::Sphere { .. }
            | CsgNode::Extrude { .. }
            | CsgNode::Revolve { .. }
            | CsgNode::Import { .. }
            | CsgNode::Loft { .. }) => leaf.clone(),
        }
    }

    /// This node's own requirement, ignoring its children.
    fn own_need(&self) -> KernelNeed {
        match self {
            CsgNode::Hull { .. } | CsgNode::SurfaceRelief { .. } | CsgNode::Import { .. } => {
                KernelNeed::MeshOnly
            }
            // OCCT has no twisted extrude here; a zero twist is a prism.
            CsgNode::Extrude {
                twist_deg: Some(t), ..
            } if *t != 0.0 => KernelNeed::MeshOnly,
            CsgNode::Fillet { .. }
            | CsgNode::Chamfer { .. }
            | CsgNode::Shell { .. }
            | CsgNode::Loft { .. } => KernelNeed::BrepOnly,
            _ => KernelNeed::Any,
        }
    }
}

/// The first operation only one kernel has, of each kind, in a subtree.
#[derive(Debug, Clone, Copy, Default)]
struct ExclusiveOps {
    brep: Option<&'static str>,
    mesh: Option<&'static str>,
}

impl CsgNode {
    fn exclusive_ops(&self) -> ExclusiveOps {
        let own = ExclusiveOps {
            brep: (self.own_need() == KernelNeed::BrepOnly).then(|| self.op_name()),
            mesh: (self.own_need() == KernelNeed::MeshOnly).then(|| self.op_name()),
        };
        self.children().iter().fold(own, |seen, child| {
            let below = child.exclusive_ops();
            ExclusiveOps {
                brep: seen.brep.or(below.brep),
                mesh: seen.mesh.or(below.mesh),
            }
        })
    }
}

/// Which kernel this tree needs, or a refusal if a B-rep operation stands over
/// a mesh.
///
/// Call it on a validated tree: the walk recurses without a depth guard
/// of its own, the same arrangement every other walk here uses.
pub fn kernel_need(node: &CsgNode) -> Result<KernelNeed, GeometryError> {
    refuse_brep_over_mesh(node)?;
    let seen = node.exclusive_ops();
    Ok(match (seen.brep, seen.mesh) {
        (_, Some(_)) => KernelNeed::MeshOnly,
        (Some(_), None) => KernelNeed::BrepOnly,
        (None, None) => KernelNeed::Any,
    })
}

/// A fillet, a chamfer or a shell works on the faces and edges of its child,
/// and a mesh has neither. Nothing can build that.
fn refuse_brep_over_mesh(node: &CsgNode) -> Result<(), GeometryError> {
    if let (KernelNeed::BrepOnly, Some(mesh)) = (node.own_need(), node.exclusive_ops().mesh) {
        return Err(GeometryError::BrepOverMesh {
            brep: node.op_name(),
            mesh,
        });
    }
    node.children()
        .into_iter()
        .try_for_each(refuse_brep_over_mesh)
}

/// The parts of a tree that need OCCT while the tree as a whole needs the mesh
/// kernel, each as large as it can be: nothing mesh-only inside, and the whole
/// subtree goes to OCCT rather than the one fillet in it.
///
/// The kernels can only meet at a mesh, so this is where they do: whoever
/// builds the tree builds these in OCCT, tessellates them, and puts the result
/// back as an `import`. Empty when the tree needs no mesh kernel, or has
/// nothing for OCCT. Call it on a tree [`kernel_need`] accepted.
pub fn brep_islands(node: &CsgNode) -> Vec<&CsgNode> {
    if node.exclusive_ops().mesh.is_none() {
        return Vec::new();
    }
    let mut islands = Vec::new();
    collect_brep_islands(node, &mut islands);
    islands
}

fn collect_brep_islands<'tree>(node: &'tree CsgNode, islands: &mut Vec<&'tree CsgNode>) {
    let seen = node.exclusive_ops();
    match (seen.brep, seen.mesh) {
        (None, _) => {}
        (Some(_), None) => islands.push(node),
        (Some(_), Some(_)) => {
            for child in node.children() {
                collect_brep_islands(child, islands);
            }
        }
    }
}

fn validate_children(
    op: &'static str,
    children: &[CsgNode],
    depth: usize,
    nodes: &mut usize,
) -> Result<(), GeometryError> {
    if children.is_empty() {
        return Err(GeometryError::EmptyChildren { op });
    }
    for child in children {
        child.validate_at(depth + 1, nodes)?;
    }
    Ok(())
}

/// An `import` names one thing, and a tree stays a small thing to pass around.
fn check_import(source_path: Option<&str>, object_id: Option<&str>) -> Result<(), GeometryError> {
    let named = match (source_path, object_id) {
        (Some(one), None) | (None, Some(one)) => one,
        _ => "",
    };
    if named.is_empty() || named.len() > MAX_IMPORT_REF_BYTES {
        return Err(GeometryError::ImportTarget {
            max: MAX_IMPORT_REF_BYTES,
        });
    }
    Ok(())
}

/// A closed 2D outline: enough points to enclose area, few enough to
/// stay within the node budget's spirit, and every coordinate finite and
/// inside the build volume's order of magnitude. `fields` names the two
/// axes for the error message, because the same check serves `extrude`
/// (x, y) and `revolve` (r, z) and a message naming the wrong pair sends
/// the reader looking in the wrong place.
fn check_outline(outline: &[[f64; 2]], fields: [&'static str; 2]) -> Result<(), GeometryError> {
    if !(MIN_OUTLINE_POINTS..=MAX_OUTLINE_POINTS).contains(&outline.len()) {
        return Err(GeometryError::Outline {
            value: outline.len(),
            min: MIN_OUTLINE_POINTS,
            max: MAX_OUTLINE_POINTS,
        });
    }
    for point in outline {
        // Outline coordinates are positions, not extents, so they may be
        // negative — but they still have to stay inside the build
        // volume's order of magnitude.
        for (axis, value) in fields.iter().zip(point) {
            check_finite(axis, *value)?;
            if value.abs() > MAX_DIMENSION_MM {
                return Err(GeometryError::Dimension {
                    field: axis,
                    value: *value,
                    max: MAX_DIMENSION_MM,
                });
            }
        }
    }
    // Last, and only once the coordinates are known to be numbers: a
    // crossing outline is legal JSON describing an illegal shape, and
    // neither kernel says so on its own. OCCT sums the two lobes with
    // opposite signs, so a symmetric bowtie comes back enclosing -0 mm³
    // — reported as "every solid was subtracted away", which sends the
    // reader looking at the booleans — and an asymmetric one comes back
    // enclosing a plausible volume of a part nobody asked for.
    if let Some((first, second)) = crate::outline::first_crossing(outline) {
        return Err(GeometryError::OutlineCrosses { first, second });
    }
    Ok(())
}

/// Facet count for a round primitive. Shared by every primitive that has
/// one, so that a sphere and a cylinder cannot end up with different
/// ideas of how coarse is too coarse.
fn check_segments(value: u32) -> Result<(), GeometryError> {
    if (MIN_SEGMENTS..=MAX_SEGMENTS).contains(&value) {
        Ok(())
    } else {
        Err(GeometryError::Segments {
            value,
            min: MIN_SEGMENTS,
            max: MAX_SEGMENTS,
        })
    }
}

/// Copies in one pattern. Two is the floor because one copy is not a
/// repetition — it is the child, written the long way, and accepting it
/// would hide a generator that meant to write a number and wrote a
/// placeholder.
fn check_count(value: u32) -> Result<(), GeometryError> {
    if (2..=MAX_PATTERN_COUNT).contains(&value) {
        Ok(())
    } else {
        Err(GeometryError::PatternCount {
            value,
            min: 2,
            max: MAX_PATTERN_COUNT,
        })
    }
}

/// Twist, layer count, turn per layer and vertex budget of an `extrude`,
/// in that order, so the message names the first thing to change. The
/// budget counts the outline as the mesh kernel builds it, long edges cut
/// for the twist (`outline::split_for_twist`).
fn check_extrude_twist(
    outline: &[[f64; 2]],
    twist_deg: Option<f64>,
    divisions: Option<u32>,
) -> Result<(), GeometryError> {
    if let Some(twist) = twist_deg {
        check_finite("twist_deg", twist)?;
        if twist.abs() > MAX_TWIST_DEG {
            return Err(GeometryError::Twist {
                value: twist,
                max: MAX_TWIST_DEG,
            });
        }
    }
    if let Some(value) = divisions {
        if !(MIN_DIVISIONS..=MAX_DIVISIONS).contains(&value) {
            return Err(GeometryError::Divisions {
                value,
                min: MIN_DIVISIONS,
                max: MAX_DIVISIONS,
            });
        }
    }
    let layers = extrude_divisions(twist_deg, divisions);
    let per_division = twist_per_division(twist_deg, layers);
    if let Some(twist) = twist_deg {
        if per_division > MAX_TWIST_PER_DIVISION_DEG {
            return Err(GeometryError::TwistTooCoarse {
                per_division,
                max: MAX_TWIST_PER_DIVISION_DEG,
                needed: (twist.abs() / MAX_TWIST_PER_DIVISION_DEG).ceil() as u32,
            });
        }
    }
    // Counted, not built: a hostile outline of long edges would split into
    // tens of millions of points before this refused it.
    let points = crate::outline::split_for_twist_len(outline, per_division);
    let vertices = points.saturating_mul(layers as usize + 1);
    if vertices > MAX_EXTRUDE_VERTICES {
        return Err(GeometryError::ExtrudeTooDense {
            vertices,
            max: MAX_EXTRUDE_VERTICES,
        });
    }
    Ok(())
}

fn check_finite(field: &'static str, value: f64) -> Result<(), GeometryError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(GeometryError::NotFinite { field, value })
    }
}

fn check_dimension(field: &'static str, value: f64) -> Result<(), GeometryError> {
    check_finite(field, value)?;
    if value > 0.0 && value <= MAX_DIMENSION_MM {
        Ok(())
    } else {
        Err(GeometryError::Dimension {
            field,
            value,
            max: MAX_DIMENSION_MM,
        })
    }
}
