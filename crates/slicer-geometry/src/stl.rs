use manifold_rust::manifold::Manifold;

/// Serialize a solid as binary STL.
///
/// The 80-byte header is left zeroed rather than stamped with a name or a
/// date: this output is content-addressed by the caller and compared
/// byte-for-byte by the determinism tests.
///
/// Facet normals are recomputed from the winding rather than taken from
/// the kernel, so the file is self-consistent even where manifold merged
/// coplanar faces.
pub fn to_binary_stl(solid: &Manifold) -> Vec<u8> {
    let mesh = solid.get_mesh_gl64(-1);
    let stride = mesh.num_prop as usize;
    let tri_count = mesh.tri_verts.len() / 3;

    let mut out = vec![0u8; 80];
    out.reserve(4 + tri_count * 50);
    out.extend_from_slice(&(tri_count as u32).to_le_bytes());

    let vertex = |index: u64| -> [f64; 3] {
        let at = index as usize * stride;
        [
            mesh.vert_properties[at],
            mesh.vert_properties[at + 1],
            mesh.vert_properties[at + 2],
        ]
    };

    for tri in mesh.tri_verts.as_chunks::<3>().0 {
        let (a, b, c) = (vertex(tri[0]), vertex(tri[1]), vertex(tri[2]));
        for value in facet_normal(a, b, c) {
            out.extend_from_slice(&(value as f32).to_le_bytes());
        }
        for point in [a, b, c] {
            for value in point {
                out.extend_from_slice(&(value as f32).to_le_bytes());
            }
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out
}

/// Unit normal of the triangle, or the zero vector for a degenerate one.
/// Zero is the conventional STL "normal unknown" and every reader we care
/// about recomputes from the winding anyway.
fn facet_normal(a: [f64; 3], b: [f64; 3], c: [f64; 3]) -> [f64; 3] {
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let n = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if length > 0.0 {
        [n[0] / length, n[1] / length, n[2] / length]
    } else {
        [0.0, 0.0, 0.0]
    }
}
