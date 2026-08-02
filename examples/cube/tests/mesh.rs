//! The cooked geometry, checked against what the renderer needs of it.
//!
//! These assertions used to live beside `VERTICES` and `INDICES` consts in
//! `src/mesh.rs`. The consts are gone — the geometry is `assets/cube.gltf` now —
//! so the checks moved to the artifact the renderer actually draws.
//!
//! That is the better place for them, for the same reason the texture's checks
//! moved: a `const` can only be wrong about itself, while a cooked asset can be
//! wrong because the glTF changed, because the importer mangled an accessor, or
//! because the cache served something stale. Every one of those produces a cube
//! that still *draws*, which is what makes them worth asserting.

use std::path::PathBuf;

use slop_asset::{Mesh, Vfs};

/// The cooked cube, or `None` with an explanation if it has not been cooked.
fn cooked() -> Option<Mesh> {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");

    match Vfs::for_project(&project).read("meshes/cube.Cube.0.mesh") {
        Ok(bytes) => Some(Mesh::read(&bytes).expect("a cooked mesh must decode")),
        Err(error) => {
            eprintln!("skipping: {error} — run `cargo run -p slop-cli -- cook`");
            None
        }
    }
}

#[test]
fn the_cube_has_twenty_four_vertices_and_twelve_triangles() {
    // Twenty-four, not eight. A cube's corners each meet three faces with three
    // different normals and three different texture coordinates, and a vertex
    // carries exactly one of each. An importer that welded them would round the
    // lighting off and make a wrong normal impossible to see.
    let Some(mesh) = cooked() else { return };

    assert_eq!(mesh.vertices.len(), 24);
    assert_eq!(mesh.indices.len(), 36);
}

#[test]
fn every_face_has_four_vertices_sharing_one_normal() {
    let Some(mesh) = cooked() else { return };

    for face in 0..6 {
        let normals: Vec<[f32; 3]> = mesh.vertices[face * 4..face * 4 + 4]
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
    let Some(mesh) = cooked() else { return };

    let mut normals: Vec<[i32; 3]> = mesh
        .vertices
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
    // Half-extent 0.5 in every axis, so the cube is one unit across. This is
    // also the check that catches a unit or scale mistake in the import path —
    // a cube ten times too large still renders, just filling the frame.
    let Some(mesh) = cooked() else { return };

    for vertex in &mesh.vertices {
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
    // The check that catches an inward-facing normal, which lights the cube as
    // though it were hollow — plausible-looking and wrong.
    let Some(mesh) = cooked() else { return };

    for vertex in &mesh.vertices {
        let dot = vertex.position[0] * vertex.normal[0]
            + vertex.position[1] * vertex.normal[1]
            + vertex.position[2] * vertex.normal[2];

        assert!(dot > 0.0, "a normal points inward: {vertex:?}");
    }
}

#[test]
fn every_triangle_winds_counter_clockwise_seen_from_outside() {
    // The invariant back-face culling enforces, checked here so a reversed face
    // is a test failure rather than a face that silently vanishes.
    //
    // The cross product of two edges points along the outward normal exactly
    // when the winding is counter-clockwise viewed from outside. glTF specifies
    // counter-clockwise too, so an importer that flipped the winding — or a
    // handedness conversion applied where it did not belong — fails here.
    let Some(mesh) = cooked() else { return };

    for triangle in mesh.indices.chunks_exact(3) {
        let [a, b, c] = [
            mesh.vertices[triangle[0] as usize],
            mesh.vertices[triangle[1] as usize],
            mesh.vertices[triangle[2] as usize],
        ];

        let edge1 = subtract(b.position, a.position);
        let edge2 = subtract(c.position, a.position);
        let cross = [
            edge1[1] * edge2[2] - edge1[2] * edge2[1],
            edge1[2] * edge2[0] - edge1[0] * edge2[2],
            edge1[0] * edge2[1] - edge1[1] * edge2[0],
        ];

        let alignment = cross[0] * a.normal[0] + cross[1] * a.normal[1] + cross[2] * a.normal[2];

        assert!(
            alignment > 0.0,
            "triangle {triangle:?} winds the wrong way (alignment {alignment})"
        );
    }
}

#[test]
fn the_indices_cover_every_vertex_exactly_once_per_face() {
    let Some(mesh) = cooked() else { return };

    for face in 0..6_u32 {
        let base = face * 4;
        let used = &mesh.indices[face as usize * 6..face as usize * 6 + 6];

        for corner in base..base + 4 {
            assert!(used.contains(&corner), "corner {corner} is never drawn");
        }
    }
}

#[test]
fn every_index_is_in_range() {
    // An out-of-range index is undefined behaviour on the GPU rather than an
    // error, so it can produce anything from a stray triangle to a device loss.
    // The format's own reader does not check this — it decodes bytes.
    let Some(mesh) = cooked() else { return };

    let count = u32::try_from(mesh.vertices.len()).expect("24 fits in a u32");
    for index in &mesh.indices {
        assert!(*index < count, "index {index} is past the last vertex");
    }
}

fn subtract(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

#[test]
fn the_cube_names_the_material_its_gltf_gives_it() {
    // The link the renderer will follow to find a surface's textures. A
    // primitive that lost its material still draws — untextured, or with
    // whatever was bound last — so nothing else would notice it went missing.
    let Some(mesh) = cooked() else { return };

    assert_eq!(mesh.material.as_deref(), Some("materials/cube.Checker.mat"));
}

#[test]
fn that_material_is_cooked_and_names_its_texture() {
    // The other half: a material naming a texture that was never cooked is a
    // dangling reference, and the cache is exactly where that would hide.
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let vfs = Vfs::for_project(&project);

    let Ok(bytes) = vfs.read("materials/cube.Checker.mat") else {
        eprintln!("skipping: run `cargo run -p slop-cli -- cook`");
        return;
    };

    let material = slop_asset::Material::read(&bytes).expect("a cooked material must decode");

    assert_eq!(material.metallic, 0.0);
    assert_eq!(material.roughness, 0.85);
    assert_eq!(material.alpha_mode, slop_asset::AlphaMode::Opaque);
    assert!(!material.double_sided);

    let albedo = material
        .texture(slop_asset::TextureSlot::BaseColor)
        .expect("the material declares a base colour texture");

    assert!(
        vfs.exists(albedo),
        "the material names '{albedo}', which is not cooked"
    );
}
