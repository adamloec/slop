//! The pass that draws the environment itself.
//!
//! `docs/PLAN.md` §9.7 E6e, and the shader is `passes/scene/skybox.slang` —
//! which carries the reasoning about *where in the frame* this sits, since that
//! is a property of the depth comparison rather than of anything here.
//!
//! # What it does not own
//!
//! The cube. [`Sky`](crate::Sky) uploads that and puts it in the heap, and this
//! pass never names it: the fragment reads the environment buffer that
//! [`View::environment`](crate::View::environment) already points at, and takes
//! the cube's slot from there. So there is one answer to "which environment is
//! this frame lit by", and the sky and the reflections read it.
//!
//! That is also why this takes no `Sky` argument. A caller decides whether to
//! *declare* the pass — there is nothing to draw on a checkout that has fetched
//! no panorama — and the shader's degenerate case is black rather than a branch.

use std::sync::Arc;

use slop_asset::Reflection;
use slop_rhi::{
    BindlessHeap, Blend, Device, Format, GraphicsPipeline, GraphicsPipelineConfig, PipelineLayout,
    PipelineLayoutConfig, ShaderModule, ShaderStage,
};

use crate::{RenderError, View};

/// Per-draw constants, matching `PushConstants` in `skybox.slang`.
///
/// A matrix and one index. The camera *position* is deliberately absent — the
/// shader recovers a ray direction from two points on it, both unprojected by
/// this matrix, which is better conditioned than subtracting the eye from a
/// point on the near plane.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PushConstants {
    inverse_view_projection: [f32; 16],
    environment: u32,
}

/// Draws the environment where the depth buffer says nothing else was.
pub struct Skybox {
    pipeline: GraphicsPipeline,
    push_constant_bytes: u32,
}

impl Skybox {
    /// Build the pass.
    ///
    /// `depth_format` is required rather than optional: the depth test is not a
    /// detail of this pass, it is the whole mechanism by which the sky stays
    /// behind the scene. A pipeline built without one would paint over
    /// everything.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if a GPU object cannot be created, or
    /// [`RenderError::Layout`] if the shader writes more push constants than
    /// this passes.
    pub fn new(
        device: &Arc<Device>,
        heap: &mut BindlessHeap,
        module: &ShaderModule,
        reflection: &Reflection,
        color_format: Format,
        depth_format: Format,
    ) -> Result<Self, RenderError> {
        let push_constant_bytes = reflection.push_constant_bytes;

        if push_constant_bytes as usize > size_of::<PushConstants>() {
            return Err(RenderError::Layout {
                what: "the skybox shader's push constant block is larger than the pass writes",
            });
        }

        let layout = Arc::new(PipelineLayout::new(
            device,
            &PipelineLayoutConfig {
                heap: Some(heap),
                push_constant_bytes,
            },
        )?);

        let pipeline = GraphicsPipeline::new(
            device,
            &layout,
            &GraphicsPipelineConfig {
                vertex: ShaderStage {
                    module,
                    entry: c"vertexMain",
                },
                fragment: Some(ShaderStage {
                    module,
                    entry: c"fragmentMain",
                }),
                color_format: Some(color_format),
                depth_format: Some(depth_format),
                // Positions come from `SV_VertexID`.
                vertex_layout: None,
                // The fullscreen triangle is larger than the viewport, so two of
                // its corners are outside and their winding is not worth
                // reasoning about.
                cull_back_faces: false,
                // Opaque. The sky is not composited over the background — where
                // it draws at all, it *is* the background.
                blend: Blend::Opaque,
            },
        )?;

        Ok(Self {
            pipeline,
            push_constant_bytes,
        })
    }

    /// Record the fullscreen draw into a pass the caller opened.
    ///
    /// Takes the same [`View`] the scene was drawn with, and inverts its matrix
    /// here rather than asking for a second one: two matrices at a call site
    /// that were meant to be inverses of each other is a thing that can be got
    /// wrong, and it would show as a sky that does not move with the camera.
    ///
    /// **Records a draw and nothing else** — no pass is opened and no barrier is
    /// emitted, because the depth attachment this reads is declared to the
    /// [`Graph`](crate::Graph) by the caller.
    pub fn draw(&self, pass: &mut slop_rhi::Pass<'_>, heap: &BindlessHeap, view: &View) {
        pass.bind_pipeline(&self.pipeline);
        pass.bind_heap(heap);

        let push = PushConstants {
            inverse_view_projection: view.view_projection.inverse().to_cols_array(),
            environment: view.environment,
        };

        pass.push_constants(&bytemuck::bytes_of(&push)[..self.push_constant_bytes as usize]);
        pass.draw(3, 1);
    }
}

impl std::fmt::Debug for Skybox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Skybox")
            .field("push_constant_bytes", &self.push_constant_bytes)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use slop_math::{Mat4, Vec3};

    use super::*;

    #[test]
    fn the_push_block_is_a_matrix_and_one_index() {
        // 64 bytes of matrix and a single `uint`. A field added here without one
        // in the shader would be read as whatever followed in the block, and the
        // matrix is what the ray direction comes out of — so the failure would
        // be a sky pointing somewhere arbitrary rather than a missing value.
        assert_eq!(size_of::<PushConstants>(), 68);
    }

    #[test]
    fn inverting_the_view_projection_returns_the_ray_it_came_from() {
        // What `draw` hands the shader, checked without one. The shader
        // unprojects two depths at the same pixel and subtracts; this asserts
        // the matrix it is given actually inverts the camera, which is the part
        // that would silently produce a sky rotated away from the scene.
        let eye = Vec3::new(0.0, 2.0, 6.0);
        let view_projection = slop_math::perspective(0.9, 1.0, 0.1)
            * slop_math::look_at(eye, Vec3::ZERO, slop_math::UP);

        let inverse = Mat4::from_cols_array(
            &PushConstants {
                inverse_view_projection: view_projection.inverse().to_cols_array(),
                environment: 0,
            }
            .inverse_view_projection,
        );

        // The centre of the screen, at two depths, exactly as the shader does it.
        let near = inverse * slop_math::Vec4::new(0.0, 0.0, 1.0, 1.0);
        let far = inverse * slop_math::Vec4::new(0.0, 0.0, 1.0 / 1024.0, 1.0);

        let direction = (far.truncate() / far.w - near.truncate() / near.w).normalize();

        // The centre pixel looks at whatever the camera is pointed at, which is
        // the origin. Loose, because this is checking a direction rather than a
        // matrix element — and stated as "towards the target" rather than as an
        // axis, since the camera is above the origin as well as in front of it.
        assert!(
            direction.dot((Vec3::ZERO - eye).normalize()) > 0.999,
            "the centre of the screen does not look where the camera does: {direction}"
        );
    }
}
