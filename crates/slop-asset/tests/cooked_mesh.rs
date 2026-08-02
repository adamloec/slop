//! Reading the cooked mesh this repository actually ships.
//!
//! `assets/triangle.gltf` is hand-written rather than exported, so its expected
//! output can be checked by reading the source file — three vertices at the unit
//! corners, all facing +Z, wound counter-clockwise. An exported model would test
//! the same code and prove nothing about what the numbers should be.
//!
//! Skipped with an explanation when the cook step has not been run, the same way
//! the shader tests are: a test that fails because a build step is missing
//! teaches nothing.

use std::path::PathBuf;

use slop_asset::{Mesh, Vfs};

/// The project root, two levels above this crate.
fn project() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// The cooked triangle, or `None` with an explanation.
fn cooked() -> Option<Mesh> {
    let vfs = Vfs::for_project(&project());

    match vfs.read("meshes/triangle.Triangle.0.mesh") {
        Ok(bytes) => Some(Mesh::read(&bytes).expect("a cooked mesh must decode")),
        Err(error) => {
            eprintln!("skipping: {error} — run `cargo run -p slop-cli -- cook`");
            None
        }
    }
}

#[test]
fn the_cooked_triangle_has_the_geometry_the_source_declares() {
    let Some(mesh) = cooked() else { return };

    assert_eq!(mesh.vertices.len(), 3);
    assert_eq!(mesh.indices, vec![0, 1, 2]);
    assert_eq!(mesh.triangles(), 1);

    // Exactly the positions in `assets/triangle.gltf`.
    assert_eq!(mesh.vertices[0].position, [0.0, 0.0, 0.0]);
    assert_eq!(mesh.vertices[1].position, [1.0, 0.0, 0.0]);
    assert_eq!(mesh.vertices[2].position, [0.0, 1.0, 0.0]);
}

#[test]
fn the_attributes_survive_the_trip_through_the_cache() {
    // Positions alone would pass with normals and UVs dropped, which is the
    // quietest way an importer goes wrong.
    let Some(mesh) = cooked() else { return };

    for vertex in &mesh.vertices {
        assert_eq!(vertex.normal, [0.0, 0.0, 1.0], "the source declares +Z");
    }

    assert_eq!(mesh.vertices[0].uv, [0.0, 0.0]);
    assert_eq!(mesh.vertices[1].uv, [1.0, 0.0]);
    assert_eq!(mesh.vertices[2].uv, [0.0, 1.0]);
}

#[test]
fn the_artifact_is_addressed_by_logical_path_alone() {
    // The point of the VFS: nothing here knows about `.slop/cache`.
    let vfs = Vfs::for_project(&project());

    if !vfs.exists("meshes/triangle.Triangle.0.mesh") {
        eprintln!("skipping: not cooked");
        return;
    }

    assert!(!vfs.exists("meshes/triangle.Nothing.0.mesh"));
}
