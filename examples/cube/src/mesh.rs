//! How a vertex is laid out in memory, for the pipeline's vertex binding.
//!
//! The geometry itself is not here. It is `assets/cube.gltf`, cooked to
//! `meshes/cube.Cube.0.mesh` and loaded through the VFS — see `scene.rs`. What
//! remains is the one thing a *file* cannot carry: how the shader expects those
//! bytes to be arranged when the GPU reads them.
//!
//! Shader reflection at M2 is what removes even this, by reading the layout out
//! of the cooked SPIR-V instead of restating it here.

use slop_rhi::vk;

/// One vertex, laid out exactly as the shader's `VertexIn` declares it.
///
/// Structurally identical to [`slop_asset::Vertex`], and deliberately not an
/// alias for it: this one exists to describe what the *pipeline* is configured
/// for. The two matching is an assertion, made by the
/// `the_stride_matches_the_cooked_vertex` test below, not an assumption — the cooked
/// format is free to grow a field, and this must then fail rather than feed the
/// shader a stride it does not expect.
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
    fn the_stride_matches_the_cooked_vertex() {
        // The buffer uploaded to the GPU is a slice of `slop_asset::Vertex`,
        // while the pipeline is configured from the type above. They are two
        // declarations of one layout, and nothing but this connects them: a
        // field added to the cooked format renders scrambled geometry rather
        // than failing anywhere.
        assert_eq!(size_of::<slop_asset::Vertex>(), size_of::<Vertex>());
        assert_eq!(align_of::<slop_asset::Vertex>(), align_of::<Vertex>());
    }
}
