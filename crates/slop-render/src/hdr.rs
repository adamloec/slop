//! The floating-point target a scene is rendered into, and the pass that
//! resolves it to the swapchain.
//!
//! `docs/PLAN.md` §9.5 E2, and §9.4 for why the format is what it is.
//!
//! # Why the scene stops drawing straight into the swapchain
//!
//! Radiance has no upper bound. Sunlit stone and a shadowed arch in the same
//! frame differ by orders of magnitude, and an eight-bit target clips the top of
//! that at the moment the fragment shader writes it — before anything can decide
//! what the bright end should become. Every part of §9.4's Stage A that follows
//! (bloom reading what is above white, TAA accumulating without banding, a
//! tonemap curve at all) needs the unclipped values to still exist.
//!
//! # This is also the engine's first real pass dependency
//!
//! One pass writes an image and another reads it. Nothing before this had that:
//! the mesh renderer and the debug overlay both wrote the swapchain in sequence,
//! which is an ordering rather than a dependency. Barriers here are hand-written
//! and correct; deriving them is `docs/PLAN.md` §9.5 E3's job, and this is the
//! frame it will be derived *from*.
//!
//! # What it does not yet do
//!
//! [`Tonemap`] applies the identity. §9.4 puts the curve at E7 — it is a look
//! decision that wants lit content to judge it against. The identity is what
//! makes this change checkable: the approved golden images were rendered
//! straight into the swapchain, and routing the same colours through a float
//! target must reproduce them.

use std::sync::Arc;

use slop_asset::Reflection;
use slop_core::Handle;
use slop_rhi::{
    Allocator, BindlessHeap, Blend, Device, Extent2D, Format, GraphicsPipeline,
    GraphicsPipelineConfig, Image, ImageConfig, ImageState, ImageUsage, ImageViewHandle,
    PipelineLayout, PipelineLayoutConfig, SampledImage, Sampler, SamplerConfig, ShaderModule,
    ShaderStage, TextureSampler,
};

use crate::RenderError;

/// The format §9.4 chose, and the one a pipeline drawing into this must declare.
///
/// `Rgba16Float` rather than the half-bandwidth `R11G11B10Float`: correctness
/// first, one variable at a time. `docs/PLAN.md` §6.1 carries the row for
/// swapping, and a test in `slop-rhi` already asserts the cheaper format is a
/// usable sampled colour attachment so the swap stays a one-line change.
pub const FORMAT: Format = Format::Rgba16Float;

/// Where a scene is drawn before it is resolved to the swapchain.
///
/// Owns the image, the view, and the heap slot the tonemap pass reads it
/// through. Created by the application and handed to both halves, because
/// neither owns it — the renderer writes it and the tonemap reads it, which is
/// exactly the relationship E3's graph will take over describing.
pub struct HdrTarget {
    image: Image,
    slot: Handle<SampledImage>,
    extent: Extent2D,
}

impl HdrTarget {
    /// Allocate a target at `extent` and place it in the heap.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if the image cannot be allocated, or
    /// [`RenderError::Layout`] if the bindless heap is full.
    pub fn new(
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        extent: Extent2D,
    ) -> Result<Self, RenderError> {
        let image = Image::new(
            allocator,
            &ImageConfig {
                name: "hdr target",
                extent,
                format: FORMAT,
                // Written as an attachment, read as a texture. Neither implies
                // the other, and `Image::new` refuses a format that cannot do
                // both rather than letting the driver accept it and misbehave.
                usage: ImageUsage::COLOR_ATTACHMENT | ImageUsage::SAMPLED,
                // No chain: nothing samples this at a distance, it is read
                // one-to-one by the pass that resolves it.
                mip_levels: 1,
            },
        )?;

        let slot = heap
            .insert_sampled_image(image.view(), ImageState::SHADER_READ)
            .ok_or(RenderError::Layout {
                what: "the bindless heap had no room for the HDR target",
            })?;

        Ok(Self {
            image,
            slot,
            extent,
        })
    }

    /// Reallocate at a new size, reusing nothing.
    ///
    /// Two things this does that are easy to leave out, both learned from the
    /// depth buffer doing exactly that:
    ///
    /// - **Waits for the device.** Assigning over `self.image` drops the old
    ///   one, which destroys a `VkImage` a frame in flight may still be reading.
    /// - **Returns the heap slot.** `insert_sampled_image` allocates a new slot
    ///   each time; without the matching remove, every resize costs one and a
    ///   window dragged around long enough exhausts the heap.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if the device cannot be waited on or the image
    /// cannot be allocated, or [`RenderError::Layout`] if the heap is full.
    pub fn resize(
        &mut self,
        device: &Arc<Device>,
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        extent: Extent2D,
    ) -> Result<(), RenderError> {
        device.wait_idle()?;

        let replacement = Self::new(allocator, heap, extent)?;

        // After the new one is in, so a failure above leaves this target intact
        // rather than holding a freed slot.
        heap.remove_sampled_image(self.slot);

        *self = replacement;

        Ok(())
    }

    /// The view a pass renders into.
    #[must_use]
    pub fn view(&self) -> ImageViewHandle {
        self.image.view()
    }

    /// The image itself, for [`Graph::import`](crate::Graph::import).
    #[must_use]
    pub fn image(&self) -> slop_rhi::ImageHandle {
        self.image.handle()
    }

    /// Which aspects a barrier over it must name.
    #[must_use]
    pub fn aspect(&self) -> slop_rhi::ImageAspect {
        self.image.aspect()
    }

    /// This target's slot in the bindless heap, for a pass that samples it.
    #[must_use]
    pub fn slot(&self) -> u32 {
        self.slot.index()
    }

    /// The size this was allocated at.
    #[must_use]
    pub fn extent(&self) -> Extent2D {
        self.extent
    }

    // **`begin_writing` and `end_writing` used to be here, and are gone.**
    //
    // They bracketed the scene's writes and made them visible to the sampler,
    // and the second was the barrier this whole target exists around. Both were
    // correct and both were a convention: a caller could forget the second and
    // the tonemap would read whatever had been flushed so far, which desktop
    // hardware usually gets away with.
    //
    // `Graph` derives both from the declaration — the scene pass says it writes
    // this and the tonemap says it samples it, and the difference between those
    // two states *is* the barrier. Keeping the methods alongside would leave the
    // convention available, which is how one comes back.
}

impl std::fmt::Debug for HdrTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HdrTarget")
            .field("extent", &self.extent)
            .field("slot", &self.slot.index())
            .finish()
    }
}

/// Per-draw constants, matching `PushConstants` in `tonemap.slang`.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PushConstants {
    source: u32,
    sampler: u32,
}

/// Resolves an [`HdrTarget`] onto whatever the frame is presenting.
pub struct Tonemap {
    pipeline: GraphicsPipeline,
    /// Held so the heap's descriptor stays valid; destroyed on drop.
    #[expect(dead_code, reason = "the heap references this sampler")]
    sampler: TextureSampler,
    sampler_slot: Handle<Sampler>,
    push_constant_bytes: u32,
}

impl Tonemap {
    /// Build the pass.
    ///
    /// `color_format` is the swapchain's, not [`FORMAT`] — this pass *reads* the
    /// HDR target and writes the presentable image.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if a GPU object cannot be created, or
    /// [`RenderError::Layout`] if the shader writes more push constants than
    /// this passes or the heap is full.
    pub fn new(
        device: &Arc<Device>,
        heap: &mut BindlessHeap,
        module: &ShaderModule,
        reflection: &Reflection,
        color_format: Format,
    ) -> Result<Self, RenderError> {
        let push_constant_bytes = reflection.push_constant_bytes;

        if push_constant_bytes as usize > size_of::<PushConstants>() {
            return Err(RenderError::Layout {
                what: "the tonemap shader's push constant block is larger than the pass writes",
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
                // No depth at all. The triangle covers the viewport and there is
                // nothing to test against; declaring a depth format here would
                // mean the pass had to be given one.
                depth_format: None,
                // Positions come from `SV_VertexID`, so there is no vertex
                // buffer and nothing to describe.
                vertex_layout: None,
                // The fullscreen triangle is deliberately larger than the
                // viewport, so two of its corners are outside and their winding
                // is not worth reasoning about.
                cull_back_faces: false,
                // Opaque: this replaces the swapchain's contents rather than
                // compositing over them.
                blend: Blend::Opaque,
            },
        )?;

        // Nearest, not linear. The target is the size of the thing it is being
        // resolved onto, so every fragment reads exactly one texel — filtering
        // would blur a one-to-one copy. Clamped so the fullscreen triangle's
        // out-of-range corners cannot wrap.
        let sampler = TextureSampler::new(
            device,
            &SamplerConfig {
                filter: slop_rhi::Filter::Nearest,
                wrap: slop_rhi::Wrap::ClampToEdge,
                ..SamplerConfig::default()
            },
        )?;
        let sampler_slot = heap
            .insert_sampler(sampler.handle())
            .ok_or(RenderError::Layout {
                what: "the bindless heap had no room for the tonemap sampler",
            })?;

        Ok(Self {
            pipeline,
            sampler,
            sampler_slot,
            push_constant_bytes,
        })
    }

    /// Record the fullscreen resolve into a pass the caller opened.
    ///
    /// `source` is the HDR target's heap slot — [`HdrTarget::slot`].
    ///
    /// **Records draws and nothing else.** It opens no pass and emits no
    /// barrier, because a [`Graph`](crate::Graph) pass declaring
    /// `samples: &[(hdr, Stage::Fragment)]` is what makes the scene's writes
    /// visible here. This used to do both, and the barrier was the part a caller
    /// could forget.
    pub fn draw(&self, pass: &mut slop_rhi::Pass<'_>, heap: &BindlessHeap, source: u32) {
        pass.bind_pipeline(&self.pipeline);
        pass.bind_heap(heap);

        let push = PushConstants {
            source,
            sampler: self.sampler_slot.index(),
        };

        pass.push_constants(&bytemuck::bytes_of(&push)[..self.push_constant_bytes as usize]);
        pass.draw(3, 1);
    }
}

impl std::fmt::Debug for Tonemap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tonemap")
            .field("sampler_slot", &self.sampler_slot.index())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_push_block_is_two_indices_and_nothing_else() {
        // Both are heap slots, and the shader reads them as a pair of `uint`.
        // A third field added here without one in the shader would be read as
        // whatever followed in the block.
        assert_eq!(size_of::<PushConstants>(), 8);
    }

    /// The scene's target and the pass that reads it must agree on the format,
    /// or the pipeline is built against one and handed the other.
    #[test]
    fn the_target_format_is_the_one_the_plan_chose() {
        assert_eq!(FORMAT, Format::Rgba16Float);
    }
}
