//! The cooked shader reflection, checked against what it has to agree with.
//!
//! These assertions used to be a hand-written attribute table in
//! `src/mesh.rs` — `VERTEX_ATTRIBUTES`, `VERTEX_STRIDE`, and a test that they
//! matched each other. The table is gone: the pipeline now derives its layout
//! from `shaders/passes/cube.refl`, cooked from the same compile that produced
//! the SPIR-V.
//!
//! What is left is the join that reflection *cannot* check on its own. The
//! shader says it reads 32 bytes per vertex; the cooked mesh format says it
//! writes 32 bytes per vertex. Nothing connects those two statements except
//! this file, and a disagreement is not a compile error — it makes the GPU read
//! each vertex at the wrong offset, and the symptom is scrambled geometry.

use std::path::PathBuf;

use slop_asset::shader::VertexFormat;
use slop_asset::{Reflection, Vfs};

/// The cube shader's reflection, or `None` if it has not been cooked.
fn cooked() -> Option<Reflection> {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");

    match Vfs::for_project(&project).read("shaders/passes/cube.refl") {
        Ok(bytes) => Some(Reflection::read(&bytes).expect("cooked reflection must decode")),
        Err(error) => {
            eprintln!("skipping: {error} — run `cargo run -p slop-cli -- cook`");
            None
        }
    }
}

#[test]
fn the_shaders_vertex_matches_the_cooked_meshs_vertex() {
    // The join. Two independently produced descriptions of one layout: the
    // shader's, via `slangc` reflection, and the mesh format's, via
    // `size_of::<Vertex>()`. Adding a field to either without the other is what
    // this catches.
    let Some(reflection) = cooked() else { return };
    let (_, stride) = reflection.interleaved();

    assert_eq!(
        stride as usize,
        size_of::<slop_asset::Vertex>(),
        "the shader reads a different vertex size than the mesh format writes"
    );
}

#[test]
fn the_shader_reads_position_normal_uv_and_tangent_in_that_order() {
    // Order matters as much as size: swapping normal and uv keeps the stride
    // identical and renders a cube lit by its texture coordinates.
    //
    // The tangent is declared by the cube's shader and never read by it. That is
    // required rather than sloppy: this reflection *is* the vertex layout, so
    // omitting the tangent would compute a 32-byte stride for a buffer whose
    // vertices are 48, and every vertex after the first would be read from the
    // middle of its predecessor. This assertion is what keeps the two in step.
    let Some(reflection) = cooked() else { return };

    let formats: Vec<VertexFormat> = reflection
        .vertex_inputs
        .iter()
        .map(|input| input.format)
        .collect();

    assert_eq!(
        formats,
        vec![
            VertexFormat::Float32x3,
            VertexFormat::Float32x3,
            VertexFormat::Float32x2,
            VertexFormat::Float32x4,
        ]
    );
}

#[test]
fn the_reflected_stride_matches_the_cooked_vertex() {
    // The invariant the test above protects, stated directly and against the
    // real number rather than against a restatement of it.
    let Some(reflection) = cooked() else { return };

    let stride: usize = reflection
        .vertex_inputs
        .iter()
        .map(|input| match input.format {
            VertexFormat::Float32 => 4,
            VertexFormat::Float32x2 => 8,
            VertexFormat::Float32x3 => 12,
            VertexFormat::Float32x4 => 16,
        })
        .sum();

    assert_eq!(
        stride,
        slop_asset::mesh::VERTEX_SIZE,
        "the shader's vertex layout must be exactly the cooked mesh's vertex"
    );
}

#[test]
fn the_locations_are_contiguous_from_zero() {
    // What `VertexBinding::interleaved` requires, asserted against the real
    // artifact rather than only against a synthetic one in a unit test.
    let Some(reflection) = cooked() else { return };

    let locations: Vec<u32> = reflection
        .vertex_inputs
        .iter()
        .map(|input| input.location)
        .collect();

    assert_eq!(locations, vec![0, 1, 2, 3]);
}

#[test]
fn the_push_constant_block_is_two_matrices_and_two_indices() {
    // 64 + 64 + 4 + 4. `Scene::new` compares this against
    // `size_of::<PushConstants>()` at startup and refuses to run if they
    // disagree; this says what the number should actually be, so that changing
    // both sides to something wrong still fails.
    let Some(reflection) = cooked() else { return };

    assert_eq!(reflection.push_constant_bytes, 136);
}
