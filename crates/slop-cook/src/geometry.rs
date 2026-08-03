//! Vertex attributes derived from geometry, for sources that omit them.
//!
//! Pure functions over positions, UVs and indices — **no glTF anywhere in this
//! module**. That is the reason it is separate: the derivations are properties
//! of triangle meshes, not of the file format one arrived in, and a second
//! importer (FBX, OBJ, a procedural generator) needs exactly these and none of
//! the glTF machinery around them.
//!
//! Both derivations are fallbacks. A file that carries normals or tangents is
//! believed, because an exporter's agree with whatever the artist authored
//! against them and a reconstruction that disagrees lights the surface subtly
//! wrongly in a way nobody can point at.

use slop_asset::mesh::Vertex;

/// Derive per-vertex tangents from positions, UVs and normals.
///
/// Only used when the source has none. An exporter's tangents agree with the
/// normal maps the artist baked; derived ones are a reconstruction, and the two
/// can differ where a mesh has hard edges or seams.
///
/// # How
///
/// A tangent is the direction in which **u increases** across the surface. For a
/// triangle, the two edges are known in both object space and UV space, so the
/// tangent is the solution of a 2×2 system relating them. Each triangle's result
/// is accumulated onto all three of its vertices and normalised at the end,
/// which is what makes the frame continuous across a smooth surface instead of
/// faceted per triangle.
///
/// The accumulated tangent is then made perpendicular to the vertex normal by
/// Gram-Schmidt: the normal is authoritative — it is what lighting uses — so the
/// tangent bends to meet it rather than the other way round.
///
/// Handedness comes from comparing the accumulated bitangent against
/// `cross(normal, tangent)`. If they point opposite ways the UVs are mirrored
/// here, and `w` is `-1`.
pub(crate) fn generate_tangents(vertices: &mut [Vertex], indices: &[u32]) {
    let mut tangents = vec![[0.0f32; 3]; vertices.len()];
    let mut bitangents = vec![[0.0f32; 3]; vertices.len()];

    for triangle in indices.chunks_exact(3) {
        let [a, b, c] = [
            triangle[0] as usize,
            triangle[1] as usize,
            triangle[2] as usize,
        ];

        let edge1 = subtract(vertices[b].position, vertices[a].position);
        let edge2 = subtract(vertices[c].position, vertices[a].position);

        let duv1 = [
            vertices[b].uv[0] - vertices[a].uv[0],
            vertices[b].uv[1] - vertices[a].uv[1],
        ];
        let duv2 = [
            vertices[c].uv[0] - vertices[a].uv[0],
            vertices[c].uv[1] - vertices[a].uv[1],
        ];

        // Zero when the triangle's UVs are degenerate — collapsed to a point or
        // a line, which happens on untextured filler geometry. Dividing would
        // produce infinities that then poison every vertex this triangle
        // touches, so the triangle contributes nothing instead.
        let determinant = duv1[0] * duv2[1] - duv2[0] * duv1[1];
        if determinant.abs() < f32::EPSILON {
            continue;
        }

        let scale = 1.0 / determinant;
        let tangent = [
            scale * (duv2[1] * edge1[0] - duv1[1] * edge2[0]),
            scale * (duv2[1] * edge1[1] - duv1[1] * edge2[1]),
            scale * (duv2[1] * edge1[2] - duv1[1] * edge2[2]),
        ];
        let bitangent = [
            scale * (duv1[0] * edge2[0] - duv2[0] * edge1[0]),
            scale * (duv1[0] * edge2[1] - duv2[0] * edge1[1]),
            scale * (duv1[0] * edge2[2] - duv2[0] * edge1[2]),
        ];

        // Unnormalised on purpose: a larger triangle contributes proportionally
        // more, which weights the average by area and is what keeps a big smooth
        // face from being dragged around by a sliver beside it.
        for &vertex in &[a, b, c] {
            tangents[vertex] = add(tangents[vertex], tangent);
            bitangents[vertex] = add(bitangents[vertex], bitangent);
        }
    }

    for (index, vertex) in vertices.iter_mut().enumerate() {
        let normal = vertex.normal;
        let accumulated = tangents[index];

        // Gram-Schmidt: remove the part of the tangent that lies along the
        // normal, leaving the part in the surface plane.
        let projection = dot(accumulated, normal);
        let orthogonal = [
            accumulated[0] - normal[0] * projection,
            accumulated[1] - normal[1] * projection,
            accumulated[2] - normal[2] * projection,
        ];

        // `normalize` returns the zero vector for anything too short to have a
        // direction, which is what this checks for.
        let unit = normalize(orthogonal);
        if unit == [0.0, 0.0, 0.0] {
            // Every triangle touching this vertex was degenerate, or the tangent
            // was parallel to the normal. Zero rather than an arbitrary guess:
            // `Vertex::has_tangent` is false, and the shader falls back to the
            // interpolated normal instead of lighting with a fabricated frame.
            vertex.tangent = [0.0; 4];
            continue;
        }

        // Mirrored UVs give a bitangent pointing against `cross(normal,
        // tangent)`, which is exactly what `w` records.
        let handedness = if dot(cross(normal, unit), bitangents[index]) < 0.0 {
            -1.0
        } else {
            1.0
        };

        vertex.tangent = [unit[0], unit[1], unit[2], handedness];
    }
}

fn add(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] + right[0], left[1] + right[1], left[2] + right[2]]
}

fn dot(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

pub(crate) fn generate_flat_normals(vertices: &mut Vec<Vertex>, indices: &mut Vec<u32>) {
    let mut split = Vec::with_capacity(indices.len());

    for triangle in indices.chunks_exact(3) {
        let corners = [
            vertices[triangle[0] as usize],
            vertices[triangle[1] as usize],
            vertices[triangle[2] as usize],
        ];

        let edge_one = subtract(corners[1].position, corners[0].position);
        let edge_two = subtract(corners[2].position, corners[0].position);
        let normal = normalize(cross(edge_one, edge_two));

        for corner in corners {
            split.push(Vertex { normal, ..corner });
        }
    }

    *indices = (0..split.len() as u32).collect();
    *vertices = split;
}

fn subtract(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn cross(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

/// Scale to unit length, or return the zero vector for a degenerate triangle.
///
/// A zero normal is wrong for lighting and is the correct thing to produce for a
/// triangle that has no plane — inventing a direction would hide the degenerate
/// geometry rather than making it visible.
fn normalize(vector: [f32; 3]) -> [f32; 3] {
    let length = (vector[0] * vector[0] + vector[1] * vector[1] + vector[2] * vector[2]).sqrt();

    if length == 0.0 {
        return [0.0, 0.0, 0.0];
    }

    [vector[0] / length, vector[1] / length, vector[2] / length]
}
#[cfg(test)]
mod tangent_tests {
    use super::*;

    /// A quad in the XY plane, facing +Z, with UVs running with the axes.
    ///
    /// u increases with x and v increases with y, so the correct tangent is +X.
    fn quad(flip_u: bool) -> (Vec<Vertex>, Vec<u32>) {
        let corners = [
            ([0.0, 0.0, 0.0], [0.0, 0.0]),
            ([1.0, 0.0, 0.0], [1.0, 0.0]),
            ([1.0, 1.0, 0.0], [1.0, 1.0]),
            ([0.0, 1.0, 0.0], [0.0, 1.0]),
        ];

        let vertices = corners
            .iter()
            .map(|(position, uv)| Vertex {
                position: *position,
                normal: [0.0, 0.0, 1.0],
                uv: if flip_u { [1.0 - uv[0], uv[1]] } else { *uv },
                tangent: [0.0; 4],
            })
            .collect();

        (vertices, vec![0, 1, 2, 0, 2, 3])
    }

    #[test]
    fn a_tangent_points_the_way_u_increases() {
        let (mut vertices, indices) = quad(false);
        generate_tangents(&mut vertices, &indices);

        for vertex in &vertices {
            assert!(vertex.has_tangent());
            assert!(
                (vertex.tangent[0] - 1.0).abs() < 1e-5,
                "u runs along +X, so the tangent must too: {:?}",
                vertex.tangent
            );
        }
    }

    #[test]
    fn a_tangent_is_unit_length_and_perpendicular_to_the_normal() {
        // Both are what the shader assumes when it builds the frame, and neither
        // is free: the accumulated tangent is unnormalised and generally not
        // perpendicular until Gram-Schmidt runs.
        let (mut vertices, indices) = quad(false);
        generate_tangents(&mut vertices, &indices);

        for vertex in &vertices {
            let tangent = [vertex.tangent[0], vertex.tangent[1], vertex.tangent[2]];

            assert!(
                (dot(tangent, tangent).sqrt() - 1.0).abs() < 1e-5,
                "unit length"
            );
            assert!(dot(tangent, vertex.normal).abs() < 1e-5, "perpendicular");
        }
    }

    #[test]
    fn mirrored_uvs_are_recorded_as_negative_handedness() {
        // The whole reason `w` exists. Artists mirror half a symmetrical model
        // to halve the texture budget, and a bitangent computed without this
        // lights every mirrored surface as though lit from the other side.
        let (mut normal_uvs, indices) = quad(false);
        generate_tangents(&mut normal_uvs, &indices);

        let (mut mirrored, indices) = quad(true);
        generate_tangents(&mut mirrored, &indices);

        assert_eq!(normal_uvs[0].tangent[3], 1.0, "unmirrored is right-handed");
        assert_eq!(mirrored[0].tangent[3], -1.0, "mirrored is left-handed");
    }

    #[test]
    fn degenerate_uvs_produce_no_tangent_rather_than_a_nan() {
        // Every UV identical, so the 2x2 system is singular. Dividing by its
        // determinant would give infinities that spread to every vertex the
        // triangle touches; the shader checks `has_tangent` and falls back.
        let mut vertices = vec![
            Vertex {
                position: [0.0, 0.0, 0.0],
                normal: [0.0, 0.0, 1.0],
                uv: [0.5, 0.5],
                tangent: [0.0; 4],
            },
            Vertex {
                position: [1.0, 0.0, 0.0],
                normal: [0.0, 0.0, 1.0],
                uv: [0.5, 0.5],
                tangent: [0.0; 4],
            },
            Vertex {
                position: [0.0, 1.0, 0.0],
                normal: [0.0, 0.0, 1.0],
                uv: [0.5, 0.5],
                tangent: [0.0; 4],
            },
        ];

        generate_tangents(&mut vertices, &[0, 1, 2]);

        for vertex in &vertices {
            assert!(!vertex.has_tangent(), "{:?}", vertex.tangent);
            assert!(
                vertex.tangent.iter().all(|value| value.is_finite()),
                "a degenerate triangle must not produce NaN: {:?}",
                vertex.tangent
            );
        }
    }
}
