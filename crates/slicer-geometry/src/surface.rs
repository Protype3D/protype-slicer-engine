//! Bounded outward height fields on explicit primitives. Never substitutes an
//! arbitrary child's bounding box: unsupported topology is a visible error.
use crate::surface_formula::Expr;
use crate::{CsgNode, GeometryError};
use manifold_rust::{manifold::Manifold, types::MeshGL64};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::f64::consts::TAU;

pub const MAX_SURFACE_VERTICES: usize = 200_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceFaces {
    Sides,
    All,
    Top,
    Bottom,
    XMin,
    XMax,
    YMin,
    YMax,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SurfacePattern {
    Ribs,
    Waves,
    Roughness,
    Formula,
}

fn error(message: &str) -> GeometryError {
    GeometryError::Surface(message.into())
}

struct Settings {
    faces: SurfaceFaces,
    pattern: SurfacePattern,
    amplitude: f64,
    pitch: f64,
    resolution: f64,
    angle: f64,
    seed: u32,
    formula: Option<Expr>,
}
impl Settings {
    fn from_node(node: &CsgNode) -> Result<Self, GeometryError> {
        let CsgNode::SurfaceRelief {
            faces,
            pattern,
            amplitude,
            pitch,
            resolution,
            angle,
            seed,
            formula,
            ..
        } = node
        else {
            unreachable!()
        };
        if !amplitude.is_finite() || *amplitude <= 0.0 || *amplitude > 20.0 {
            return Err(error("amplitude must be > 0 and <= 20 mm"));
        }
        if !pitch.is_finite() || !(0.4..=1000.0).contains(pitch) {
            return Err(error("pitch must be 0.4..1000 mm"));
        }
        if !resolution.is_finite()
            || !(0.05..=10.0).contains(resolution)
            || *resolution > pitch / 8.0
        {
            return Err(error("resolution must be 0.05..10 mm and <= pitch / 8"));
        }
        if !angle.is_finite() {
            return Err(error("angle must be finite"));
        }
        let expression = match (*pattern, formula) {
            (SurfacePattern::Formula, Some(source)) => Some(Expr::parse(source)?),
            (SurfacePattern::Formula, None) => {
                return Err(error("formula pattern requires formula"))
            }
            (_, Some(_)) => return Err(error("formula is only allowed with the formula pattern")),
            (_, None) => None,
        };
        Ok(Self {
            faces: *faces,
            pattern: *pattern,
            amplitude: *amplitude,
            pitch: *pitch,
            resolution: *resolution,
            angle: angle.rem_euclid(360.0).to_radians(),
            seed: *seed,
            formula: expression,
        })
    }
    fn selected(&self, axis: usize, upper: bool) -> bool {
        match self.faces {
            SurfaceFaces::All => true,
            SurfaceFaces::Sides => axis != 2,
            SurfaceFaces::Top => axis == 2 && upper,
            SurfaceFaces::Bottom => axis == 2 && !upper,
            SurfaceFaces::XMin => axis == 0 && !upper,
            SurfaceFaces::XMax => axis == 0 && upper,
            SurfaceFaces::YMin => axis == 1 && !upper,
            SurfaceFaces::YMax => axis == 1 && upper,
        }
    }
    fn height(&self, x: f64, y: f64, width: f64, height: f64) -> Result<f64, GeometryError> {
        let (sin, cos) = self.angle.sin_cos();
        let (u, v) = (
            (cos * x + sin * y) / self.pitch,
            (-sin * x + cos * y) / self.pitch,
        );
        let value = match self.pattern {
            SurfacePattern::Ribs => (TAU * u).cos().max(0.0),
            SurfacePattern::Waves => (1.0 + (TAU * u).sin() * (TAU * v).cos()) / 2.0,
            SurfacePattern::Roughness => noise(u, v, self.seed),
            SurfacePattern::Formula => self.formula.as_ref().unwrap().value(u, v)?,
        };
        if !value.is_finite() || !(-1e-12..=1.0 + 1e-12).contains(&value) {
            return Err(error(
                "sampled formula height must be finite and in [0,1] before amplitude scaling",
            ));
        }
        // Zero at common edges/caps/seam gives identical shared vertices.
        // Each selected face stays a graph outside its own primitive face,
        // so even adjacent relief faces cannot fold through one another.
        let edge = x.min(width - x).min(y).min(height - y).max(0.0);
        let taper = (edge / (self.pitch / 4.0)).min(1.0);
        Ok(self.amplitude * value.clamp(0.0, 1.0) * taper)
    }
}

// Stable value noise, independent of process RNG, platform and mesh resolution.
fn noise(u: f64, v: f64, seed: u32) -> f64 {
    fn hash(x: i64, y: i64, seed: u32) -> f64 {
        let mut n = (x as u64).wrapping_mul(0x9e3779b97f4a7c15)
            ^ (y as u64).wrapping_mul(0xbf58476d1ce4e5b9)
            ^ u64::from(seed);
        n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
        n ^= n >> 31;
        (n >> 11) as f64 / ((1u64 << 53) - 1) as f64
    }
    let (x, y) = (u.floor() as i64, v.floor() as i64);
    let (a, b) = (u - u.floor(), v - v.floor());
    let (a, b) = (a * a * (3.0 - 2.0 * a), b * b * (3.0 - 2.0 * b));
    let lower = hash(x, y, seed) * (1.0 - a) + hash(x + 1, y, seed) * a;
    let upper = hash(x, y + 1, seed) * (1.0 - a) + hash(x + 1, y + 1, seed) * a;
    lower * (1.0 - b) + upper * b
}

enum Base {
    Box([f64; 3]),
    Cylinder {
        radius: f64,
        height: f64,
        segments: u32,
    },
}
fn base(node: &CsgNode, offset: &mut [f64; 3]) -> Result<Base, GeometryError> {
    match node {
        CsgNode::Translate { offset: add, child } => {
            for i in 0..3 {
                offset[i] += add[i];
                if !offset[i].is_finite() || offset[i].abs() > 10_000.0 { return Err(error("combined translation must stay within +/-10000 mm")); }
            }
            base(child, offset)
        }
        CsgNode::Box { size } => Ok(Base::Box(*size)),
        CsgNode::Cylinder { diameter, diameter_top, height, segments }
            if diameter_top.is_none_or(|top| top == *diameter) =>
            Ok(Base::Cylinder { radius:diameter/2.0, height:*height, segments:*segments }),
        _ => Err(error("supported child: box or straight cylinder, optionally wrapped in translate; curved, tapered, rotated, boolean and B-rep bodies are not supported")),
    }
}
fn divisions(length: f64, resolution: f64) -> usize {
    (length / resolution).ceil().max(2.0) as usize
}
fn budget(base: &Base, s: &Settings) -> Result<(), GeometryError> {
    let vertices = match base {
        Base::Box(size) => {
            let n = size.map(|v| divisions(v, s.resolution) + 1);
            2 * (n[0] * n[1] + n[1] * n[2] + n[2] * n[0])
        }
        Base::Cylinder {
            radius,
            height,
            segments,
        } => {
            if s.faces != SurfaceFaces::Sides {
                return Err(error(
                    "cylinder relief supports faces=sides only; caps remain flat",
                ));
            }
            divisions(TAU * radius, s.resolution).max(*segments as usize)
                * (divisions(*height, s.resolution) + 1)
                + 2
        }
    };
    if vertices > MAX_SURFACE_VERTICES {
        return Err(error(
            "sampling exceeds 200000 vertices; increase resolution or reduce the part dimensions",
        ));
    }
    Ok(())
}
pub(crate) fn validate(node: &CsgNode) -> Result<(), GeometryError> {
    let s = Settings::from_node(node)?;
    let CsgNode::SurfaceRelief { child, .. } = node else {
        unreachable!()
    };
    budget(&base(child, &mut [0.0; 3])?, &s)
}

pub(crate) fn build(node: &CsgNode) -> Result<Manifold, GeometryError> {
    let s = Settings::from_node(node)?;
    let CsgNode::SurfaceRelief { child, .. } = node else {
        unreachable!()
    };
    let mut offset = [0.0; 3];
    let primitive = base(child, &mut offset)?;
    budget(&primitive, &s)?;
    let mut mesh = MeshGL64 {
        num_prop: 3,
        ..Default::default()
    };
    match primitive {
        Base::Box(size) => box_mesh(&mut mesh, size, &s)?,
        Base::Cylinder {
            radius,
            height,
            segments,
        } => cylinder_mesh(&mut mesh, radius, height, segments, &s)?,
    }
    for p in mesh.vert_properties.as_chunks_mut::<3>().0 {
        for i in 0..3 {
            p[i] += offset[i];
        }
    }
    // Exact shared index topology, before Manifold can simplify anything.
    let unpaired = crate::mesh::count_unpaired(mesh.tri_verts.as_chunks::<3>().0.iter().copied());
    if unpaired != 0 {
        return Err(GeometryError::NotWatertight { unpaired });
    }
    Ok(Manifold::from_mesh_gl64(&mesh))
}
fn vertex(mesh: &mut MeshGL64, p: [f64; 3]) -> u64 {
    let index = (mesh.vert_properties.len() / 3) as u64;
    mesh.vert_properties.extend(p);
    index
}
fn quad(mesh: &mut MeshGL64, a: u64, b: u64, c: u64, d: u64, reverse: bool) {
    if reverse {
        mesh.tri_verts.extend([a, c, b, a, d, c]);
    } else {
        mesh.tri_verts.extend([a, b, c, a, c, d]);
    }
}
fn box_mesh(mesh: &mut MeshGL64, size: [f64; 3], s: &Settings) -> Result<(), GeometryError> {
    let n = size.map(|v| divisions(v, s.resolution));
    let mut shared = HashMap::<[usize; 3], u64>::new();
    for axis in 0..3 {
        let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
        for upper in [false, true] {
            let mut grid = Vec::with_capacity((n[u] + 1) * (n[v] + 1));
            for j in 0..=n[v] {
                for i in 0..=n[u] {
                    let mut key = [0; 3];
                    key[axis] = if upper { n[axis] } else { 0 };
                    key[u] = i;
                    key[v] = j;
                    let mut p = [0.0; 3];
                    for k in 0..3 {
                        p[k] = size[k] * key[k] as f64 / n[k] as f64;
                    }
                    if s.selected(axis, upper) {
                        // Both vertical wall pairs use horizontal U and Z as V.
                        // The cyclic grid axes above remain for outward winding.
                        let (hu, hv) = if axis == 1 { (0, 2) } else { (u, v) };
                        let h = s.height(p[hu], p[hv], size[hu], size[hv])?;
                        p[axis] += if upper { h } else { -h };
                    }
                    grid.push(*shared.entry(key).or_insert_with(|| vertex(mesh, p)));
                }
            }
            let stride = n[u] + 1;
            for j in 0..n[v] {
                for i in 0..n[u] {
                    let a = j * stride + i;
                    quad(
                        mesh,
                        grid[a],
                        grid[a + 1],
                        grid[a + stride + 1],
                        grid[a + stride],
                        !upper,
                    );
                }
            }
        }
    }
    Ok(())
}
fn cylinder_mesh(
    mesh: &mut MeshGL64,
    radius: f64,
    height: f64,
    segments: u32,
    s: &Settings,
) -> Result<(), GeometryError> {
    let width = TAU * radius;
    let n = divisions(width, s.resolution).max(segments as usize);
    let nz = divisions(height, s.resolution);
    for j in 0..=nz {
        let z = height * j as f64 / nz as f64;
        for i in 0..n {
            let x = width * i as f64 / n as f64;
            let angle = TAU * i as f64 / n as f64;
            let r = radius + s.height(x, z, width, height)?;
            vertex(mesh, [r * angle.cos(), r * angle.sin(), z]);
        }
    }
    for j in 0..nz {
        for i in 0..n {
            let next = (i + 1) % n;
            quad(
                mesh,
                (j * n + i) as u64,
                (j * n + next) as u64,
                ((j + 1) * n + next) as u64,
                ((j + 1) * n + i) as u64,
                false,
            );
        }
    }
    let bottom = vertex(mesh, [0.0, 0.0, 0.0]);
    let top = vertex(mesh, [0.0, 0.0, height]);
    for i in 0..n {
        let next = (i + 1) % n;
        mesh.tri_verts.extend([
            bottom,
            next as u64,
            i as u64,
            top,
            (nz * n + i) as u64,
            (nz * n + next) as u64,
        ]);
    }
    Ok(())
}
