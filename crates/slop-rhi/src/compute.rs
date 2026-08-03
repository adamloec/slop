//! Compute pipelines, and recording work for them.
//!
//! `docs/PLAN.md` §9.5 E1b. The queues have been acquired since M0 and the
//! bindless heap has had a storage-image binding since M0; what was missing was
//! any way to build a compute pipeline or dispatch it. §9.4's cluster build is
//! the first consumer, and the post stack at E7 is the second.
//!
//! # Where dispatch lives, and what that does and does not buy
//!
//! `vkCmdDispatch` outside a render pass is the rule — dispatching inside one is
//! invalid, and there is no compute equivalent of [`Pass`](crate::Pass) because
//! there is no begin/end pair to balance.
//!
//! So compute recording hangs off [`CommandBuffer`] rather than off `Pass`, and
//! there is no `dispatch` on `Pass` to reach for. **That is discouragement by
//! shape, not enforcement.** `CommandBuffer::begin_rendering` takes `&self`, so
//! nothing in the type system stops a caller holding a live `Pass` and starting
//! a [`Compute`] beside it; making that a borrow error would mean `&mut self`,
//! which the frame loop cannot supply because `Frame` hands out a shared
//! reference. The render graph at E3 is what will actually order passes, and
//! until then this is a validation-layer error like any other.
//!
//! # Why a scope type rather than methods on the command buffer
//!
//! Push constants and heap binding both need the *layout* of the bound pipeline.
//! `Pass` tracks it in an `Option` that is `None` until something is bound, which
//! makes `bind_heap` and `push_constants` silent no-ops before then.
//!
//! [`Compute`] is constructed **from** the pipeline, so its layout is never
//! absent and neither method has a do-nothing path. That is the same problem
//! solved one step earlier, and it is worth the divergence: a silently skipped
//! heap binding is a shader reading descriptor slot zero of an unbound set,
//! which is undefined rather than obviously wrong.

use std::sync::Arc;

use ash::vk;

use crate::{BindlessHeap, CommandBuffer, Device, PipelineLayout, RhiError, ShaderStage};

/// A compiled compute pipeline.
///
/// Far less configurable than [`GraphicsPipeline`](crate::GraphicsPipeline),
/// and not because this is a simplification: a compute pipeline genuinely is one
/// shader and a layout. There is no fixed-function state to describe.
pub struct ComputePipeline {
    handle: vk::Pipeline,
    // Held for the same reason `GraphicsPipeline` holds it: Vulkan permits
    // destroying a layout after pipeline creation but not while it is used to
    // bind descriptors, and encoding the stricter rule costs nothing.
    layout: Arc<PipelineLayout>,
    device: Arc<Device>,
}

impl ComputePipeline {
    /// Compile a compute pipeline from one entry point.
    ///
    /// # Errors
    ///
    /// [`RhiError::Vulkan`] if the driver rejects it — most often a wrong entry
    /// point name, or a shader whose declared resources do not match `layout`.
    pub fn new(
        device: &Arc<Device>,
        layout: &Arc<PipelineLayout>,
        stage: ShaderStage<'_>,
    ) -> Result<Self, RhiError> {
        let stage_info = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(stage.module.handle())
            .name(stage.entry);

        let create_info = vk::ComputePipelineCreateInfo::default()
            .stage(stage_info)
            .layout(layout.handle());

        let create_infos = [create_info];

        // SAFETY: `create_info` borrows nothing that does not outlive this call,
        // and the shader module is alive because `stage` borrows it.
        let pipelines = unsafe {
            device
                .raw()
                .create_compute_pipelines(vk::PipelineCache::null(), &create_infos, None)
        }
        // Failure arrives as the partial results plus the code; only the code is
        // useful, matching how `GraphicsPipeline` handles the same shape.
        .map_err(|(_, error)| RhiError::Vulkan(error))?;

        let handle = pipelines
            .first()
            .copied()
            .ok_or(RhiError::Vulkan(vk::Result::ERROR_UNKNOWN))?;

        Ok(Self {
            handle,
            layout: Arc::clone(layout),
            device: Arc::clone(device),
        })
    }

    /// The underlying handle. The escape hatch — see [`crate::handle`].
    #[must_use]
    pub fn handle(&self) -> vk::Pipeline {
        self.handle
    }

    /// The layout this pipeline was built with.
    #[must_use]
    pub fn layout(&self) -> &Arc<PipelineLayout> {
        &self.layout
    }
}

impl Drop for ComputePipeline {
    fn drop(&mut self) {
        // SAFETY: created from this device, destroyed exactly once, and the
        // device outlives this because we hold an `Arc` to it.
        unsafe { self.device.raw().destroy_pipeline(self.handle, None) };
    }
}

impl std::fmt::Debug for ComputePipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputePipeline").finish_non_exhaustive()
    }
}

/// Recording compute work with one pipeline bound.
///
/// Created by [`CommandBuffer::bind_compute`], which binds the pipeline — so
/// unlike [`Pass`](crate::Pass) there is no unbound state and no method here
/// quietly does nothing.
///
/// Not an RAII guard: nothing needs ending, because compute dispatch has no
/// begin/end pair. It exists to carry the layout.
#[derive(Debug)]
pub struct Compute<'a> {
    command: &'a CommandBuffer,
    device: Arc<Device>,
    layout: vk::PipelineLayout,
}

impl CommandBuffer {
    /// Bind a compute pipeline and return a scope for recording against it.
    ///
    /// **Must not be called while a [`Pass`](crate::Pass) is open** — dispatching
    /// inside a render pass is invalid. Nothing enforces that; see this module's
    /// documentation for why, and what E3 does about it.
    pub fn bind_compute<'a>(&'a self, pipeline: &ComputePipeline) -> Compute<'a> {
        // SAFETY: the buffer is recording and the pipeline belongs to this
        // device, which it proves by holding an `Arc` to the same one.
        unsafe {
            self.device().raw().cmd_bind_pipeline(
                self.handle(),
                vk::PipelineBindPoint::COMPUTE,
                pipeline.handle(),
            );
        }

        Compute {
            command: self,
            device: Arc::clone(self.device()),
            layout: pipeline.layout().handle(),
        }
    }
}

impl Compute<'_> {
    /// Bind the bindless heap for this pipeline's layout.
    ///
    /// Separate from the graphics binding rather than shared: descriptor
    /// bindings are per bind point, so a heap bound for `GRAPHICS` is not
    /// visible to a compute shader however recently it was bound.
    pub fn bind_heap(&self, heap: &BindlessHeap) {
        heap.bind(
            self.command.handle(),
            vk::PipelineBindPoint::COMPUTE,
            self.layout,
        );
    }

    /// Write the push-constant block.
    ///
    /// `ALL` stages, matching how the layout declared the range — see
    /// [`PipelineLayout::new`].
    pub fn push_constants(&self, bytes: &[u8]) {
        // SAFETY: the buffer is recording, the layout belongs to this device,
        // and `bytes` outlives the call.
        unsafe {
            self.device.raw().cmd_push_constants(
                self.command.handle(),
                self.layout,
                vk::ShaderStageFlags::ALL,
                0,
                bytes,
            );
        }
    }

    /// Run `x × y × z` workgroups.
    ///
    /// **Workgroups, not threads.** The shader declares its own workgroup size,
    /// so covering an image means dividing its dimensions by that size and
    /// rounding *up* — and rounding up means the last group runs partly outside
    /// the image, which the shader must handle by bounds-checking its own
    /// invocation index. Passing pixel counts here instead launches the
    /// workgroup size squared too much work, which reads as a hang rather than
    /// as a mistake.
    ///
    /// A count of zero in any dimension is legal and does nothing.
    pub fn dispatch(&self, x: u32, y: u32, z: u32) {
        // SAFETY: the buffer is recording and a compute pipeline is bound,
        // because constructing this type is what bound it.
        unsafe {
            self.device
                .raw()
                .cmd_dispatch(self.command.handle(), x, y, z);
        }
    }
}

/// How many workgroups cover `extent` items at `group` items per group.
///
/// Integer ceiling division. Written out because the alternative spelling
/// — `(extent + group - 1) / group` — overflows for an `extent` near `u32::MAX`,
/// and because getting it wrong by rounding *down* leaves the last strip of an
/// image unwritten, which looks like a cropped result rather than a maths error.
///
/// # Panics
///
/// If `group` is zero, which is a shader declaring a zero-sized workgroup and
/// cannot be recovered from.
#[must_use]
pub fn workgroups(extent: u32, group: u32) -> u32 {
    assert!(group > 0, "a workgroup size of zero covers nothing");

    extent / group + u32::from(!extent.is_multiple_of(group))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_multiple_needs_no_extra_group() {
        assert_eq!(workgroups(64, 8), 8);
        assert_eq!(workgroups(8, 8), 1);
    }

    /// The case rounding down gets wrong. A 1920-wide image at 16 per group is
    /// exact; 1921 is not, and the last pixel is what a floor would drop.
    #[test]
    fn a_remainder_gets_its_own_group() {
        assert_eq!(workgroups(65, 8), 9);
        assert_eq!(workgroups(1, 8), 1);
        assert_eq!(workgroups(1921, 16), 121);
    }

    #[test]
    fn nothing_needs_no_groups() {
        assert_eq!(workgroups(0, 8), 0);
    }

    /// The overflow the naive spelling has. `(u32::MAX + 8 - 1)` wraps, and the
    /// result is 0 — dispatching nothing for the largest possible extent.
    #[test]
    fn an_extent_near_the_maximum_does_not_overflow() {
        assert_eq!(workgroups(u32::MAX, 1), u32::MAX);
        assert_eq!(workgroups(u32::MAX, 8), 536_870_912);
    }
}
