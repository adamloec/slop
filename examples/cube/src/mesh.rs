//! The cube's geometry, generated rather than loaded.
//!
//! Procedural because glTF import has landed but a cube asset has not — the
//! texture already comes from `assets/checker.png` through the cooked cache, and
//! `docs/PLAN.md` §6.1 records the geometry as following it.

use slop_rhi::vk;

/// One vertex, laid out exactly as the shader's `VertexIn` declares it.
///
/// `#[repr(C)]` is load-bearing, not decorative: Rust may reorder the fields of
/// a default-layout struct, and the offsets in [`VERTEX_ATTRIBUTES`] would then
/// describe a layout the compiler did not produce. The symptom is geometry that
/// looks scrambled rather than a compile error.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    /// Object-space position.
    pub position: [f32; 3],
    /// Object-space normal. Unit length.
    pub normal: [f32; 3],
    /// Texture coordinate, origin top-left.
    pub uv: [f32; 2],
}

/// Attribute formats and offsets, in shader location order.
///
/// The order here *is* the shader's `location` numbering, so it must match the
/// field order of `VertexIn` in `shaders/passes/cube.slang`.
pub const VERTEX_ATTRIBUTES: [(vk::Format, u32); 3] = [
    (vk::Format::R32G32B32_SFLOAT, 0),
    (vk::Format::R32G32B32_SFLOAT, 12),
    (vk::Format::R32G32_SFLOAT, 24),
];

/// Bytes per vertex, for the pipeline's vertex binding.
pub const VERTEX_STRIDE: u32 = size_of::<Vertex>() as u32;

/// Twenty-four vertices, not eight.
///
/// A cube's corners each meet three faces with three different normals and three
/// different texture coordinates, and a vertex carries exactly one of each. Eight
/// shared vertices would have to average the normals, which rounds the cube's
/// lighting off and makes a wrong normal impossible to see — the opposite of
/// what this example is for.
pub const VERTICES: [Vertex; 24] = {
    // Each face is built from its normal and two in-plane axes, so the winding
    // is derived once rather than typed out six times with a chance of getting
    // one backwards.
    const fn face(normal: [f32; 3], right: [f32; 3], up: [f32; 3]) -> [Vertex; 4] {
        // Corner = normal (the face's plane) ± right ± up, at half-extent 0.5.
        const fn corner(
            normal: [f32; 3],
            right: [f32; 3],
            up: [f32; 3],
            r: f32,
            u: f32,
        ) -> [f32; 3] {
            [
                (normal[0] + right[0] * r + up[0] * u) * 0.5,
                (normal[1] + right[1] * r + up[1] * u) * 0.5,
                (normal[2] + right[2] * r + up[2] * u) * 0.5,
            ]
        }

        // Counter-clockwise when viewed from outside, matching
        // `slop_rhi`'s front face. See the note on `INDICES`.
        [
            Vertex {
                position: corner(normal, right, up, -1.0, -1.0),
                normal,
                uv: [0.0, 1.0],
            },
            Vertex {
                position: corner(normal, right, up, 1.0, -1.0),
                normal,
                uv: [1.0, 1.0],
            },
            Vertex {
                position: corner(normal, right, up, 1.0, 1.0),
                normal,
                uv: [1.0, 0.0],
            },
            Vertex {
                position: corner(normal, right, up, -1.0, 1.0),
                normal,
                uv: [0.0, 0.0],
            },
        ]
    }

    // `right` and `up` are not free: the winding above makes the first
    // triangle's normal `cross(right, up)`, so each pair must satisfy
    // `cross(right, up) == normal` or that face points inward and is culled
    // away. The ±Y faces are the two where the obvious choice is wrong, and the
    // winding test below is what caught them.
    let px = face([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]);
    let nx = face([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]);
    let py = face([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]);
    let ny = face([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]);
    let pz = face([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
    let nz = face([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);

    [
        px[0], px[1], px[2], px[3], nx[0], nx[1], nx[2], nx[3], py[0], py[1], py[2], py[3], ny[0],
        ny[1], ny[2], ny[3], pz[0], pz[1], pz[2], pz[3], nz[0], nz[1], nz[2], nz[3],
    ]
};

/// Two triangles per face, wound counter-clockwise seen from outside.
///
/// Counter-clockwise is `slop_rhi`'s front face, and back-face culling is on —
/// so a face wound the wrong way vanishes rather than rendering. That is the
/// intent: with culling off, a reversed winding is invisible until something
/// depends on it, and the triangle's history in `docs/PLAN.md` §3 is what that
/// costs.
pub const INDICES: [u16; 36] = {
    let mut indices = [0_u16; 36];
    let mut face = 0;

    while face < 6 {
        let base = (face * 4) as u16;
        let out = face * 6;

        indices[out] = base;
        indices[out + 1] = base + 1;
        indices[out + 2] = base + 2;
        indices[out + 3] = base;
        indices[out + 4] = base + 2;
        indices[out + 5] = base + 3;

        face += 1;
    }

    indices
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stride_matches_the_attribute_offsets() {
        // The pairing that breaks silently. A field added to `Vertex` without a
        // matching attribute entry changes the stride and leaves the shader
        // reading the previous vertex's data — geometry that looks scrambled,
        // with no error anywhere.
        assert_eq!(VERTEX_STRIDE, 32);
        assert_eq!(size_of::<Vertex>(), 32);

        let (_, last_offset) = VERTEX_ATTRIBUTES[VERTEX_ATTRIBUTES.len() - 1];
        assert_eq!(last_offset + 8, VERTEX_STRIDE, "uv must end at the stride");
    }

    #[test]
    fn every_face_has_four_vertices_sharing_one_normal() {
        // Twenty-four vertices, six distinct normals, four vertices each. Eight
        // shared corners would fail this, which is the point.
        for face in 0..6 {
            let normals: Vec<[f32; 3]> = VERTICES[face * 4..face * 4 + 4]
                .iter()
                .map(|vertex| vertex.normal)
                .collect();

            assert!(
                normals.iter().all(|normal| *normal == normals[0]),
                "face {face} has mixed normals"
            );
        }
    }

    #[test]
    fn the_six_face_normals_are_the_six_axes() {
        let mut normals: Vec<[i32; 3]> = VERTICES
            .iter()
            .step_by(4)
            .map(|vertex| {
                [
                    vertex.normal[0] as i32,
                    vertex.normal[1] as i32,
                    vertex.normal[2] as i32,
                ]
            })
            .collect();
        normals.sort_unstable();

        assert_eq!(
            normals,
            vec![
                [-1, 0, 0],
                [0, -1, 0],
                [0, 0, -1],
                [0, 0, 1],
                [0, 1, 0],
                [1, 0, 0],
            ]
        );
    }

    #[test]
    fn every_vertex_sits_on_the_unit_cube() {
        // Half-extent 0.5 in every axis, so the cube is one unit across.
        for vertex in &VERTICES {
            for axis in vertex.position {
                assert!(
                    (axis.abs() - 0.5).abs() < 1e-6,
                    "position {axis} is off the cube"
                );
            }
        }
    }

    #[test]
    fn every_normal_points_away_from_the_centre() {
        // The check that catches an inward-facing normal, which lights the cube
        // as though it were hollow — plausible-looking and wrong.
        for vertex in &VERTICES {
            let dot = vertex.position[0] * vertex.normal[0]
                + vertex.position[1] * vertex.normal[1]
                + vertex.position[2] * vertex.normal[2];

            assert!(dot > 0.0, "a normal points inward: {vertex:?}");
        }
    }

    #[test]
    fn every_triangle_winds_counter_clockwise_seen_from_outside() {
        // The invariant back-face culling enforces, checked here so a reversed
        // face is a test failure rather than a face that silently vanishes.
        //
        // The cross product of two edges points along the outward normal
        // exactly when the winding is counter-clockwise viewed from outside.
        for triangle in INDICES.chunks_exact(3) {
            let [a, b, c] = [
                VERTICES[triangle[0] as usize],
                VERTICES[triangle[1] as usize],
                VERTICES[triangle[2] as usize],
            ];

            let edge1 = subtract(b.position, a.position);
            let edge2 = subtract(c.position, a.position);
            let cross = [
                edge1[1] * edge2[2] - edge1[2] * edge2[1],
                edge1[2] * edge2[0] - edge1[0] * edge2[2],
                edge1[0] * edge2[1] - edge1[1] * edge2[0],
            ];

            let alignment =
                cross[0] * a.normal[0] + cross[1] * a.normal[1] + cross[2] * a.normal[2];

            assert!(
                alignment > 0.0,
                "triangle {triangle:?} winds the wrong way (alignment {alignment})"
            );
        }
    }

    #[test]
    fn the_indices_cover_every_vertex_exactly_once_per_face() {
        assert_eq!(INDICES.len(), 36);

        for face in 0..6_u16 {
            let base = face * 4;
            let used = INDICES[face as usize * 6..face as usize * 6 + 6].to_vec();

            for corner in base..base + 4 {
                assert!(used.contains(&corner), "corner {corner} is never drawn");
            }
        }
    }

    // The texture's own checks moved to `tests/texture.rs`, which asserts them
    // against the cooked asset the renderer actually samples rather than against
    // a generator that no longer runs.

    fn subtract(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
        [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
    }
}
