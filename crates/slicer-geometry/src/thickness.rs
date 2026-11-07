//! How much material is there between one surface and the next.
//!
//! The failure this exists to catch: a bore widened until it broke
//! through the outer wall on three separate bands of height. The part
//! stayed manifold, its volume stayed plausible, its bounds did not
//! move, and the bore itself measured exactly as designed. The only way
//! to see it is to walk outward from the axis and ask how much material
//! you cross.
//!
//! Rays are cast in the plane of a section, so a "wall" here means wall
//! measured horizontally — which is what a vertical bore threatens.

use crate::error::MeshError;
use crate::measure::{ray_segment_distance, DEFAULT_RAY_COUNT};
use crate::mesh::TriangleMesh;
use crate::section::{section_at_z, Point2, Section};
use serde::{Deserialize, Serialize};

/// Material crossed by one ray, in one direction from the axis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spoke {
    pub angle_deg: f64,
    /// Distance from the axis to the first material.
    pub enters_at_mm: f64,
    /// Distance from the axis to where material ends. `None` when the
    /// ray never leaves it — an unbounded or badly-wound mesh.
    pub exits_at_mm: Option<f64>,
    /// `exits - enters`. `None` when the ray never exits.
    pub thickness_mm: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WallThickness {
    pub z: f64,
    pub axis: Point2,
    pub ray_count: usize,
    /// Thinnest wall found, over the rays that crossed material at all.
    pub min_thickness_mm: Option<f64>,
    pub min_at_deg: Option<f64>,
    /// Directions in which the ray left the axis and never met material.
    /// A non-empty list on a part that is supposed to be a closed wall
    /// *is* the breakthrough.
    pub breakthrough_at_deg: Vec<f64>,
    pub spokes: Vec<Spoke>,
}

impl WallThickness {
    pub fn has_breakthrough(&self) -> bool {
        !self.breakthrough_at_deg.is_empty()
    }
}

/// Walk outward from `axis` at height `z` and report the wall crossed in
/// each direction.
pub fn wall_thickness(
    mesh: &TriangleMesh,
    axis: Point2,
    z: f64,
    ray_count: usize,
) -> Result<WallThickness, MeshError> {
    let section = section_at_z(mesh, z)?;
    wall_thickness_in(&section, axis, ray_count)
}

pub fn wall_thickness_in(
    section: &Section,
    axis: Point2,
    ray_count: usize,
) -> Result<WallThickness, MeshError> {
    let rays = if ray_count == 0 {
        DEFAULT_RAY_COUNT
    } else {
        ray_count
    };
    if section.contains(axis) {
        return Err(MeshError::AxisInsideMaterial {
            x: axis[0],
            y: axis[1],
            z: section.z,
        });
    }

    let segments = section.segments();
    let mut spokes = Vec::with_capacity(rays);
    let mut breakthrough = Vec::new();

    for index in 0..rays {
        let angle = std::f64::consts::TAU * index as f64 / rays as f64;
        let direction = [angle.cos(), angle.sin()];

        // Every crossing along this ray, in order. Starting outside
        // material, the first is where the wall begins and the second is
        // where it ends.
        let mut crossings: Vec<f64> = segments
            .iter()
            .filter_map(|(a, b)| ray_segment_distance(axis, direction, *a, *b))
            .collect();
        crossings.sort_by(f64::total_cmp);
        crossings.dedup_by(|a, b| (*a - *b).abs() < 1e-9);

        let degrees = angle.to_degrees();
        match crossings.first() {
            None => {
                breakthrough.push(degrees);
            }
            Some(&enters) => {
                let exits = crossings.get(1).copied();
                spokes.push(Spoke {
                    angle_deg: degrees,
                    enters_at_mm: enters,
                    exits_at_mm: exits,
                    thickness_mm: exits.map(|x| x - enters),
                });
            }
        }
    }

    let thinnest = spokes
        .iter()
        .filter_map(|s| s.thickness_mm.map(|t| (t, s.angle_deg)))
        .min_by(|a, b| a.0.total_cmp(&b.0));

    Ok(WallThickness {
        z: section.z,
        axis,
        ray_count: rays,
        min_thickness_mm: thinnest.map(|(t, _)| t),
        min_at_deg: thinnest.map(|(_, a)| a),
        breakthrough_at_deg: breakthrough,
        spokes,
    })
}
