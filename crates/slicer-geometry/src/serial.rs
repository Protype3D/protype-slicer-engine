//! Deterministic serial engraving. The footprint and remaining wall are
//! checked against the solid, not inferred from a bounding box or AI output.
use std::collections::HashMap;

use manifold_rust::linalg::{Mat3x4, Vec3};
use manifold_rust::manifold::Manifold;
use manifold_rust::types::{Error, MeshGL64, OpType};
use serde::{Deserialize, Serialize};

use crate::mesh::TriangleMesh;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SerialLayout {
    #[default]
    Row,
    Column,
}
impl SerialLayout {
    pub fn is_row(&self) -> bool {
        *self == Self::Row
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SerialMark {
    pub serial: String,
    #[serde(default, skip_serializing_if = "SerialLayout::is_row")]
    pub layout: SerialLayout,
    pub origin: [f64; 3],
    pub normal: [f64; 3],
    pub tangent: [f64; 3],
    pub height_mm: f64,
    pub depth_mm: f64,
    pub min_wall_mm: f64,
}

pub const FONT: &str = "serial-grid-v2";

// Original 5×7 grid alphabet, restricted to the cloud's serial format.
fn glyph(c: u8) -> [u8; 7] {
    match c {
        // An open counter avoids the tiny islands created by a diagonal slash.
        b'0' => [14, 17, 17, 17, 17, 17, 14],
        b'1' => [4, 12, 4, 4, 4, 4, 14],
        b'2' => [14, 17, 1, 2, 4, 8, 31],
        b'3' => [30, 1, 1, 14, 1, 1, 30],
        b'4' => [2, 6, 10, 18, 31, 2, 2],
        b'5' => [31, 16, 16, 30, 1, 1, 30],
        b'6' => [14, 16, 16, 30, 17, 17, 14],
        b'7' => [31, 1, 2, 4, 8, 8, 8],
        b'8' => [14, 17, 17, 14, 17, 17, 14],
        b'9' => [14, 17, 17, 15, 1, 1, 14],
        b'A' => [14, 17, 17, 31, 17, 17, 17],
        b'B' => [30, 17, 17, 30, 17, 17, 30],
        b'C' => [14, 17, 16, 16, 16, 17, 14],
        b'D' => [30, 17, 17, 17, 17, 17, 30],
        b'E' => [31, 16, 16, 30, 16, 16, 31],
        b'F' => [31, 16, 16, 30, 16, 16, 16],
        b'S' => [15, 16, 16, 14, 1, 1, 30],
        b'-' => [0, 0, 0, 31, 0, 0, 0],
        _ => [0; 7],
    }
}

fn vec(v: [f64; 3]) -> Vec3 {
    Vec3::new(v[0], v[1], v[2])
}
fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.iter().zip(b).map(|(a, b)| a * b).sum()
}

impl SerialMark {
    pub fn validate(&self) -> Result<(), String> {
        let digits = self.serial.strip_prefix("S-");
        let valid = if let Some(digits) = digits {
            digits.len() == 12
                && digits
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        } else {
            self.serial.len() == 6
                && self
                    .serial
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        };
        if !valid {
            return Err("Expected six uppercase hexadecimal digits or a legacy S- serial".into());
        }
        for (name, value, max) in [
            ("height", self.height_mm, 30.0),
            ("depth", self.depth_mm, 3.0),
            ("remaining wall", self.min_wall_mm, 20.0),
        ] {
            if !value.is_finite() || value <= 0.0 || value > max {
                return Err(format!("Invalid {name} in millimetres"));
            }
        }
        if self.height_mm < 0.7 {
            return Err("Serial height must be at least 0.7 mm".into());
        }
        if self
            .origin
            .iter()
            .chain(&self.normal)
            .chain(&self.tangent)
            .any(|v| !v.is_finite() || v.abs() > 10_000.0)
            || (dot(self.normal, self.normal) - 1.0).abs() > 1e-4
            || (dot(self.tangent, self.tangent) - 1.0).abs() > 1e-4
            || dot(self.normal, self.tangent).abs() > 1e-4
        {
            return Err("Marking frame must have finite, perpendicular unit axes".into());
        }
        Ok(())
    }

    pub fn width_mm(&self) -> f64 {
        match self.layout {
            SerialLayout::Row => (self.serial.len() * 6 - 1) as f64 * self.height_mm / 7.0,
            SerialLayout::Column => 5.0 * self.height_mm / 7.0,
        }
    }

    pub fn total_height_mm(&self) -> f64 {
        match self.layout {
            SerialLayout::Row => self.height_mm,
            SerialLayout::Column => (self.serial.len() * 8 - 1) as f64 * self.height_mm / 7.0,
        }
    }

    fn frame(&self) -> Mat3x4 {
        let x = vec(self.tangent);
        let z = vec(self.normal);
        let y = Vec3::new(
            z.y * x.z - z.z * x.y,
            z.z * x.x - z.x * x.z,
            z.x * x.y - z.y * x.x,
        );
        Mat3x4::from_cols(x, y, z, vec(self.origin))
    }

    fn footprint(&self, z: f64, height: f64) -> Manifold {
        Manifold::cube(
            Vec3::new(self.width_mm(), self.total_height_mm(), height),
            false,
        )
        .translate(Vec3::new(
            -self.width_mm() / 2.0,
            -self.total_height_mm() / 2.0,
            z,
        ))
        .transform(&self.frame())
    }

    fn cutter(&self) -> Manifold {
        let pixel = self.height_mm / 7.0;
        let (width, height) = match self.layout {
            SerialLayout::Row => ((self.serial.len() * 6 - 1) as f64, 7.0),
            SerialLayout::Column => (5.0, (self.serial.len() * 8 - 1) as f64),
        };
        let mut blocks = Vec::new();
        for (letter, c) in self.serial.bytes().enumerate() {
            let left = -width / 2.0
                + if self.layout == SerialLayout::Row {
                    letter as f64 * 6.0
                } else {
                    0.0
                };
            let top = height / 2.0
                - if self.layout == SerialLayout::Column {
                    letter as f64 * 8.0
                } else {
                    0.0
                };
            // Coordinates are in glyph cells, with y measured down from top.
            let mut rectangle = |x: f64, y: f64, width: f64, height: f64| {
                blocks.push(
                    Manifold::cube(Vec3::new(width, height, self.depth_mm + 0.05), false)
                        .translate(Vec3::new(left + x, top - y - height, -self.depth_mm)),
                );
            };
            let rows = glyph(c);
            for (row, bits) in rows.into_iter().enumerate() {
                // Full cells share faces; an inset here leaves unprintable
                // partitions across every vertical stroke.
                let mut col = 0;
                while col < 5 {
                    if bits & (1 << (4 - col)) == 0 {
                        col += 1;
                        continue;
                    }
                    let start = col;
                    while col < 5 && bits & (1 << (4 - col)) != 0 {
                        col += 1;
                    }
                    rectangle(start as f64, row as f64, (col - start) as f64, 1.0);
                }
            }
            for row in 0..6 {
                for col in 0..4 {
                    let upper = (rows[row] >> (3 - col)) & 3;
                    let lower = (rows[row + 1] >> (3 - col)) & 3;
                    if matches!((upper, lower), (2, 1) | (1, 2)) {
                        // Connect diagonal-only pixels with a full-cell square
                        // centered on their shared corner. The diagonal neck
                        // is pixel/sqrt(2), never a non-manifold corner touch.
                        // This stays inside the original 5×7 glyph footprint.
                        rectangle(col as f64 + 0.5, row as f64 + 0.5, 1.0, 1.0);
                    }
                }
            }
        }
        // Union exact integer/half-integer XY coordinates before scaling:
        // independently scaled cubes can leave numerical gaps at shared faces.
        Manifold::batch_boolean(&blocks, OpType::Add)
            .scale(Vec3::new(pixel, pixel, 1.0))
            .transform(&self.frame())
    }
}

fn solid(mesh: &TriangleMesh) -> Result<Manifold, String> {
    if mesh.triangles.len() > 500_000 {
        return Err("Mesh exceeds 500000 triangles".into());
    }
    let mut vertices = HashMap::new();
    let mut input = MeshGL64 {
        num_prop: 3,
        ..Default::default()
    };
    for tri in &mesh.triangles {
        for p in tri {
            if p.iter().any(|v| !v.is_finite() || v.abs() > 10_000.0) {
                return Err("Mesh has invalid coordinates".into());
            }
            // Exact welding preserves geometry; unclosed or noisy input is
            // rejected rather than silently repaired and reshaped.
            let key = p.map(|v| if v == 0.0 { 0 } else { v.to_bits() });
            let index = *vertices.entry(key).or_insert_with(|| {
                let index = input.vert_properties.len() as u64 / 3;
                input.vert_properties.extend_from_slice(p);
                index
            });
            input.tri_verts.push(index);
        }
    }
    let result = Manifold::from_mesh_gl64(&input);
    if result.status() != Error::NoError || result.is_empty() || result.volume() <= 0.0 {
        return Err("Marking requires a closed, consistently oriented solid".into());
    }
    Ok(result)
}

fn check_placement(source: &Manifold, mark: &SerialMark) -> Result<(), String> {
    let depth = mark.depth_mm + mark.min_wall_mm;
    let inside = mark.footprint(-depth, depth);
    let expected = mark.width_mm() * mark.total_height_mm() * depth;
    let occupied = source.intersection(&inside);
    if occupied.status() != Error::NoError
        || (occupied.volume() - expected).abs() > expected * 0.001
    {
        return Err(
            "The full label does not fit on a flat surface with enough remaining wall".into(),
        );
    }
    let outside = source.intersection(&mark.footprint(0.005, 0.05));
    if outside.status() != Error::NoError || outside.volume().abs() > expected * 0.00001 {
        return Err("The label must lie on the outside surface, facing outwards".into());
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct PlacementCandidate {
    pub id: String,
    pub origin: [f64; 3],
    pub normal: [f64; 3],
    pub tangent: [f64; 3],
    pub width_mm: f64,
    pub height_mm: f64,
    pub area_mm2: f64,
}

/// Planar patches nominate positions; a solid intersection then verifies the
/// entire rectangle and remaining wall, including holes and disconnected faces.
pub fn candidates(
    mesh: &TriangleMesh,
    height_mm: f64,
    depth_mm: f64,
    min_wall_mm: f64,
    layout: SerialLayout,
) -> Result<Vec<PlacementCandidate>, String> {
    let mut mark = SerialMark {
        serial: "000000".into(),
        layout,
        origin: [0.; 3],
        normal: [0., 0., 1.],
        tangent: [1., 0., 0.],
        height_mm,
        depth_mm,
        min_wall_mm,
    };
    mark.validate()?;
    let source = solid(mesh)?;
    let cross = |a: [f64; 3], b: [f64; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let sub = |a: [f64; 3], b: [f64; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    // Quantized plane keys avoid a quadratic triangle comparison. Keep the
    // original unit normal and offset for the actual geometric checks.
    type Patch = ([f64; 3], f64, f64, Vec<[f64; 3]>);
    let mut planes: std::collections::BTreeMap<[i64; 4], Patch> = std::collections::BTreeMap::new();
    for t in &mesh.triangles {
        let n = cross(sub(t[1], t[0]), sub(t[2], t[0]));
        let length = dot(n, n).sqrt();
        if length < 1e-8 {
            continue;
        }
        let n = n.map(|x| x / length);
        let d = dot(n, t[0]);
        let key = [
            (n[0] * 1e5).round() as i64,
            (n[1] * 1e5).round() as i64,
            (n[2] * 1e5).round() as i64,
            (d * 1e4).round() as i64,
        ];
        let patch = planes.entry(key).or_insert((n, d, 0., Vec::new()));
        patch.2 += length / 2.;
        patch.3.extend_from_slice(t);
    }
    let mut patches: Vec<_> = planes.into_values().collect();
    patches.sort_by(|a, b| b.2.total_cmp(&a.2));
    let mut result = Vec::new();
    // Bound kernel work even on a high-poly curved model.
    for (normal, d, area, vertices) in patches.into_iter().take(24) {
        let reference = if normal[2].abs() < 0.9 {
            [0., 0., 1.]
        } else {
            [0., 1., 0.]
        };
        let t = cross(reference, normal);
        let tangent = t.map(|v| v / dot(t, t).sqrt());
        for tangent in [tangent, cross(normal, tangent)] {
            let y = cross(normal, tangent);
            let mut lo = [f64::INFINITY; 2];
            let mut hi = [f64::NEG_INFINITY; 2];
            for v in &vertices {
                for (i, axis) in [tangent, y].iter().enumerate() {
                    let p = dot(*v, *axis);
                    lo[i] = lo[i].min(p);
                    hi[i] = hi[i].max(p);
                }
            }
            if hi[0] - lo[0] < mark.width_mm() || hi[1] - lo[1] < mark.total_height_mm() {
                continue;
            }
            for fraction in [0.5, 0.25, 0.75] {
                let x = lo[0] + mark.width_mm() / 2. + (hi[0] - lo[0] - mark.width_mm()) * fraction;
                let v = (lo[1] + hi[1]) / 2.;
                mark.origin = std::array::from_fn(|i| tangent[i] * x + y[i] * v + normal[i] * d);
                mark.normal = normal;
                mark.tangent = tangent;
                if check_placement(&source, &mark).is_err() {
                    continue;
                }
                if result.iter().any(|p: &PlacementCandidate| {
                    dot(sub(p.origin, mark.origin), sub(p.origin, mark.origin)) < 0.01
                        && dot(p.normal, normal) > 0.999
                        && dot(p.tangent, tangent).abs() > 0.999
                }) {
                    continue;
                }
                result.push(PlacementCandidate {
                    id: format!("face-{}", result.len() + 1),
                    origin: mark.origin,
                    normal,
                    tangent,
                    width_mm: mark.width_mm(),
                    height_mm: mark.total_height_mm(),
                    area_mm2: area,
                });
                break;
            }
            if result.len() >= 12 {
                return Ok(result);
            }
        }
    }
    Ok(result)
}

/// Facets created inside the shallow serial recess. Only these may suppress
/// automatic supports: the original surface and nearby overhangs stay automatic.
/// Call on the final STL, so the result follows its exact triangle order.
pub fn support_blocker_facets(mesh: &TriangleMesh, mark: &SerialMark) -> Vec<bool> {
    let up = [
        mark.normal[1] * mark.tangent[2] - mark.normal[2] * mark.tangent[1],
        mark.normal[2] * mark.tangent[0] - mark.normal[0] * mark.tangent[2],
        mark.normal[0] * mark.tangent[1] - mark.normal[1] * mark.tangent[0],
    ];
    // Binary STL stores f32 coordinates. Account for their precision while
    // keeping the tolerance small relative to the engraving depth.
    let epsilon =
        (mark.origin.iter().fold(1.0_f64, |a, v| a.max(v.abs())) * f32::EPSILON as f64 * 2.0)
            .max(0.0001)
            .min(mark.depth_mm / 100.0);
    mesh.triangles
        .iter()
        .map(|triangle| {
            let mut recessed = false;
            let inside = triangle.iter().all(|point| {
                let local = std::array::from_fn(|i| point[i] - mark.origin[i]);
                let depth = dot(local, mark.normal);
                recessed |= depth < -epsilon;
                dot(local, mark.tangent).abs() <= mark.width_mm() / 2.0 + epsilon
                    && dot(local, up).abs() <= mark.total_height_mm() / 2.0 + epsilon
                    && depth >= -mark.depth_mm - epsilon
                    && depth <= epsilon
            });
            inside && recessed
        })
        .collect()
}

pub fn engrave(mesh: &TriangleMesh, mark: &SerialMark) -> Result<Vec<u8>, String> {
    mark.validate()?;
    let source = solid(mesh)?;
    check_placement(&source, mark)?;
    let result = source.difference(&mark.cutter());
    if result.status() != Error::NoError || result.is_empty() || result.volume() >= source.volume()
    {
        return Err("Engraving failed to remove material".into());
    }
    let bytes = crate::stl::to_binary_stl(&result);
    let check = TriangleMesh::from_stl_bytes(&bytes).map_err(|e| e.to_string())?;
    if !check.measure().map_err(|e| e.to_string())?.is_manifold {
        return Err(format!(
            "Engraving produced an unclosed mesh: {:?}",
            check.measure()
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_glyph_is_one_connected_closed_cutter() {
        // Check the actual boolean result, including diagonal-only strokes in
        // 2/7 and the legacy S prefix, at the supported size boundaries.
        for height_mm in [0.7, 3.5, 30.0] {
            for c in "0123456789ABCDEFS-".chars() {
                let m = SerialMark {
                    serial: c.to_string(),
                    height_mm,
                    ..mark()
                };
                let cutter = m.cutter();
                assert_eq!(cutter.status(), Error::NoError, "glyph {c}");
                assert_eq!(
                    cutter.decompose().len(),
                    1,
                    "glyph {c} at height {height_mm} must have continuous strokes"
                );
                let mesh =
                    TriangleMesh::from_stl_bytes(&crate::stl::to_binary_stl(&cutter)).unwrap();
                assert!(mesh.measure().unwrap().is_manifold, "glyph {c}");
            }
        }
    }

    #[test]
    fn enclosed_glyph_counters_remain_open_and_zero_has_no_slash() {
        for height_mm in [0.7, 3.5, 30.0] {
            for (c, holes) in [('0', 1), ('4', 1), ('8', 2), ('A', 1), ('B', 2), ('D', 1)] {
                let m = SerialMark {
                    serial: c.to_string(),
                    height_mm,
                    ..mark()
                };
                let cutter = m.cutter();
                assert_eq!(cutter.genus(), holes, "glyph {c} at height {height_mm}");
                if c == '0' {
                    // A substantial central counter remains solid after
                    // engraving, instead of two small islands around a slash.
                    let pixel = height_mm / 7.;
                    let counter =
                        Manifold::cube(Vec3::new(2.0 * pixel, 4.0 * pixel, m.depth_mm), false)
                            .translate(Vec3::new(-pixel, -2.0 * pixel, m.origin[2] - m.depth_mm));
                    assert!(counter.intersection(&cutter).volume().abs() < 1e-10);
                }
            }
        }
    }

    #[test]
    fn strokes_have_no_row_partitions_or_corner_only_joints() {
        for layout in [SerialLayout::Row, SerialLayout::Column] {
            for height_mm in [0.7, 3.5] {
                // An interior row boundary in 1, and the diagonal between
                // rows 1/2 of 7. A finite square must be entirely recessed.
                for (serial, col, row) in [("111111", 2.5, 3.0), ("777777", 4.0, 2.0)] {
                    let m = SerialMark {
                        serial: serial.into(),
                        layout,
                        height_mm,
                        origin: [0.; 3],
                        ..mark()
                    };
                    let pixel = height_mm / 7.;
                    let side = pixel * 0.8;
                    let probe = Manifold::cube(Vec3::new(side, side, m.depth_mm), false).translate(
                        Vec3::new(
                            -m.width_mm() / 2. + col * pixel - side / 2.,
                            m.total_height_mm() / 2. - row * pixel - side / 2.,
                            -m.depth_mm,
                        ),
                    );
                    let partition = probe.difference(&m.cutter());
                    assert!(
                        partition.volume().abs() < 1e-10,
                        "{serial} {layout:?} at {height_mm}: unrecessed partition {} mm³",
                        partition.volume()
                    );
                }
            }
        }
    }

    #[test]
    fn all_glyphs_engrave_closed_meshes_in_both_layouts() {
        let source = TriangleMesh::from_stl_bytes(&crate::stl::to_binary_stl(&Manifold::cube(
            Vec3::new(100., 100., 5.),
            true,
        )))
        .unwrap();
        for layout in [SerialLayout::Row, SerialLayout::Column] {
            for serial in ["012345", "6789AB", "CDEF01", "S-012345ABCDEF"] {
                let m = SerialMark {
                    serial: serial.into(),
                    layout,
                    ..mark()
                };
                let cutter = m.cutter();
                // Glyphs connect internally, but never across the one-cell
                // letter spacing (the hyphen remains its own component).
                assert_eq!(
                    cutter.decompose().len(),
                    serial.len(),
                    "{serial} {layout:?}"
                );
                let output = TriangleMesh::from_stl_bytes(&engrave(&source, &m).unwrap()).unwrap();
                assert!(output.measure().unwrap().is_manifold);
                assert_eq!(output.bounds(), source.bounds());
                assert!(output.volume_mm3() < source.volume_mm3());
            }
        }
    }

    #[test]
    fn support_mask_excludes_surface_and_neighbouring_geometry() {
        let mark = SerialMark {
            layout: SerialLayout::Row,
            serial: "A7C42F".into(),
            origin: [20., 4., 30.],
            normal: [0., -1., 0.],
            tangent: [0., 0., 1.],
            height_mm: 6.6,
            depth_mm: 0.6,
            min_wall_mm: 1.,
        };
        let mesh = TriangleMesh::new(vec![
            // Recess floor and wall, expressed in the rotated marking frame.
            [[19., 4.6, 29.], [21., 4.6, 29.], [21., 4.6, 31.]],
            [[19., 4., 29.], [19., 4.6, 29.], [19., 4.6, 31.]],
            // Original flat face, a nearby overhang, deeper and outward faces.
            [[19., 4., 29.], [21., 4., 29.], [21., 4., 31.]],
            [[25., 4., 29.], [25., 4.6, 29.], [25., 4.6, 31.]],
            [[19., 5., 29.], [21., 5., 29.], [21., 5., 31.]],
            [[19., 3., 29.], [21., 3., 29.], [21., 3., 31.]],
        ]);
        assert_eq!(
            support_blocker_facets(&mesh, &mark),
            [true, true, false, false, false, false]
        );
    }

    #[test]
    fn support_mask_tracks_real_engraved_stl_for_rotated_marks() {
        for tangent in [[1., 0., 0.], [0., 1., 0.]] {
            let mark = SerialMark {
                layout: SerialLayout::Row,
                serial: "A7C42F".into(),
                origin: [0., 0., 2.5],
                normal: [0., 0., 1.],
                tangent,
                height_mm: 3.5,
                depth_mm: 0.4,
                min_wall_mm: 1.,
            };
            let source = block();
            assert!(!support_blocker_facets(&source, &mark).contains(&true));
            let bytes = engrave(&source, &mark).unwrap();
            let output = TriangleMesh::from_stl_bytes(&bytes).unwrap();
            let mask = support_blocker_facets(&output, &mark);
            assert_eq!(mask.len(), output.triangles.len());
            assert!(mask.contains(&true));
            assert!(mask.contains(&false));
            // Every selected triangle actually enters the cavity; the rear
            // face and untouched surface can never acquire a blocker.
            for (triangle, blocked) in output.triangles.iter().zip(mask) {
                if blocked {
                    assert!(triangle.iter().all(|p| p[2] >= 2.0999));
                    assert!(triangle.iter().any(|p| p[2] < 2.4999));
                }
            }
        }
    }

    fn block() -> TriangleMesh {
        TriangleMesh::from_stl_bytes(&crate::stl::to_binary_stl(&Manifold::cube(
            Vec3::new(80.0, 30.0, 5.0),
            true,
        )))
        .unwrap()
    }
    fn mark() -> SerialMark {
        SerialMark {
            layout: SerialLayout::Row,
            serial: "A7C42F".into(),
            origin: [0.0, 0.0, 2.5],
            normal: [0.0, 0.0, 1.0],
            tangent: [1.0, 0.0, 0.0],
            height_mm: 3.5,
            depth_mm: 0.4,
            min_wall_mm: 1.0,
        }
    }
    #[test]
    fn candidates_are_real_exterior_surfaces_and_every_proposal_engraves() {
        let proposals = candidates(&block(), 3.5, 0.4, 1.0, SerialLayout::Row).unwrap();
        assert!(proposals.len() >= 2);
        for p in proposals {
            let m = SerialMark {
                origin: p.origin,
                normal: p.normal,
                tangent: p.tangent,
                ..mark()
            };
            assert!(engrave(&block(), &m).is_ok());
        }
        assert!(candidates(&block(), 30., 0.4, 1., SerialLayout::Row)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn short_code_fits_a_small_part_while_legacy_code_remains_supported() {
        let small = TriangleMesh::from_stl_bytes(&crate::stl::to_binary_stl(&Manifold::cube(
            Vec3::new(20., 10., 5.),
            true,
        )))
        .unwrap();
        assert_eq!(mark().width_mm(), 17.5);
        assert!(engrave(&small, &mark()).is_ok());
        assert!(!candidates(&small, 3.5, 0.4, 1., SerialLayout::Row)
            .unwrap()
            .is_empty());
        let mut legacy = mark();
        legacy.serial = "S-012345ABCDEF".into();
        assert_eq!(legacy.width_mm(), 41.5);
        assert!(engrave(&small, &legacy).is_err());
        assert!(engrave(&block(), &legacy).is_ok());
    }
    #[test]
    fn column_fits_a_narrow_surface_with_upright_glyphs_and_full_height_mask() {
        let source = TriangleMesh::from_stl_bytes(&crate::stl::to_binary_stl(&Manifold::cube(
            Vec3::new(4., 26., 5.),
            true,
        )))
        .unwrap();
        let m = SerialMark {
            serial: "F11111".into(),
            layout: SerialLayout::Column,
            ..mark()
        };
        assert_eq!(m.width_mm(), 2.5);
        assert_eq!(m.total_height_mm(), 23.5);
        assert!(engrave(&source, &mark()).is_err());
        let output = TriangleMesh::from_stl_bytes(&engrave(&source, &m).unwrap()).unwrap();
        assert!(output.measure().unwrap().is_manifold);
        assert_eq!(output.bounds(), source.bounds());
        assert!(output.volume_mm3() < source.volume_mm3());
        let mask = support_blocker_facets(&output, &m);
        assert!(output
            .triangles
            .iter()
            .zip(mask)
            .any(|(t, blocked)| blocked && t.iter().all(|p| p[1] < -8.)));
        let cutter = TriangleMesh::from_stl_bytes(&crate::stl::to_binary_stl(&m.cutter())).unwrap();
        let width_at = |top: bool| {
            let points: Vec<_> = cutter
                .triangles
                .iter()
                .flatten()
                .filter(|p| if top { p[1] > 11.3 } else { p[1] < -11.3 })
                .collect();
            points
                .iter()
                .map(|p| p[0])
                .fold(f64::NEG_INFINITY, f64::max)
                - points.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min)
        };
        assert!((width_at(true) - 2.5).abs() < 0.001); // F at the top.
        assert!((width_at(false) - 1.5).abs() < 0.001); // 1 at the bottom.
        let choices = candidates(&source, 3.5, 0.4, 1., SerialLayout::Column).unwrap();
        assert!(!choices.is_empty());
        for c in choices {
            assert_eq!(c.height_mm, 23.5);
            assert!(engrave(
                &source,
                &SerialMark {
                    origin: c.origin,
                    normal: c.normal,
                    tangent: c.tangent,
                    ..m.clone()
                }
            )
            .is_ok());
        }
    }
    #[test]
    fn legacy_marks_default_to_row_and_layout_is_validated() {
        let mut value = serde_json::to_value(mark()).unwrap();
        assert!(value.get("layout").is_none());
        assert_eq!(
            serde_json::from_value::<SerialMark>(value.clone())
                .unwrap()
                .layout,
            SerialLayout::Row
        );
        value["layout"] = "column".into();
        assert_eq!(
            serde_json::from_value::<SerialMark>(value.clone())
                .unwrap()
                .layout,
            SerialLayout::Column
        );
        value["layout"] = "diagonal".into();
        assert!(serde_json::from_value::<SerialMark>(value).is_err());
    }
    #[test]
    fn creates_closed_physical_recess_and_preserves_bounds() {
        let source = block();
        let output = TriangleMesh::from_stl_bytes(&engrave(&source, &mark()).unwrap()).unwrap();
        assert!(output.volume_mm3() < source.volume_mm3() - 1.0);
        assert_eq!(output.bounds(), source.bounds());
        assert!(output.measure().unwrap().is_manifold);
    }
    #[test]
    fn different_serials_make_different_meshes() {
        let first = engrave(&block(), &mark()).unwrap();
        let mut other = mark();
        other.serial = "A7C420".into();
        assert_ne!(first, engrave(&block(), &other).unwrap());
    }
    #[test]
    fn refuses_thin_walls_off_surface_and_wrong_normal() {
        for (origin, normal, depth) in [
            ([0.0, 0.0, 2.5], [0.0, 0.0, 1.0], 5.0),
            ([35.0, 0.0, 2.5], [0.0, 0.0, 1.0], 1.0),
            ([0.0, 0.0, 0.0], [0.0, 0.0, 1.0], 1.0),
            ([0.0, 0.0, 2.5], [0.0, 0.0, -1.0], 1.0),
        ] {
            let mut m = mark();
            m.origin = origin;
            m.normal = normal;
            m.min_wall_mm = depth;
            assert!(engrave(&block(), &m).is_err());
        }
    }
    #[test]
    fn refuses_invalid_serial_and_open_mesh() {
        let mut m = mark();
        m.serial = "</script>".into();
        assert!(engrave(&block(), &m).is_err());
        let mut mesh = block();
        mesh.triangles.pop();
        assert!(engrave(&mesh, &mark()).is_err());
    }
}
