//! Recording draws, without the caller writing `unsafe`.
//!
//! `docs/slop-rhi/README.md` §4 deferred the consumer-facing API until something
//! consumed it, on the grounds that an abstraction with no consumers is designed
//! against imagined requirements. `slop-render` is now that consumer — and it
//! was written against `device.raw()` first, which put `unsafe` in a fourth
//! crate while `docs/CONVENTIONS.md` §7 confines it to three. This is what
//! removes it again.
//!
//! # A pass is a scope, not a pair of calls
//!
//! ```ignore
//! let pass = command.begin_rendering(&Attachments { .. });
//! pass.bind_pipeline(&pipeline);
//! pass.draw_indexed(index_count, 1);
//! // `cmd_end_rendering` on drop
//! ```
//!
//! Rendering must be begun and ended in balance, and every draw must happen
//! between them. Two free functions leave both facts to discipline; a guard that
//! ends on drop and owns the draw methods makes them the type system's problem —
//! there is no way to draw outside a pass, and no way to forget to end one.
//!
//! # What is deliberately not here
//!
//! Anything with no caller. `docs/PLAN.md` §4.1-D is explicit that a guessed
//! shape gets rebuilt anyway, so this covers what the existing consumers do and
//! nothing else: at most one colour attachment, an optional depth attachment,
//! one vertex buffer binding. **Multiple** render targets, instanced draws with
//! a non-zero first instance, and indirect draws all arrive with the passes that
//! need them. Zero colour attachments is here because §9.4's depth prepass is
//! that pass.

use std::sync::Arc;

use ash::vk;

use crate::{
    BindlessHeap, Buffer, CommandBuffer, Device, Extent2D, GraphicsPipeline, ImageViewHandle,
    PipelineLayout, Rect2D,
};

/// What happens to an attachment's existing contents when a pass begins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Load {
    /// Replace them. Faster than preserving, and correct whenever the pass
    /// covers the whole attachment.
    Clear(ClearValue),
    /// Keep them, for a pass compositing over what is already there.
    Preserve,
    /// Neither clear nor load them.
    ///
    /// For a pass that writes every pixel of the attachment — a fullscreen
    /// resolve, a blit. Clearing first would write the target twice, and
    /// preserving would read contents about to be overwritten; both cost
    /// bandwidth for nothing.
    ///
    /// **Only correct when the coverage really is total.** Anything the pass
    /// does not write reads as whatever the driver left there, which varies by
    /// vendor and by frame — so a partial pass using this looks right on one
    /// machine and shows garbage on another.
    Discard,
}

/// What to clear an attachment to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClearValue {
    /// Linear RGBA.
    Color([f32; 4]),
    /// A depth value. [`DEPTH_CLEAR`](crate::DEPTH_CLEAR) under reverse-Z.
    Depth(f32),
}

/// The colour target a pass draws into.
#[derive(Debug, Clone, Copy)]
pub struct ColorAttachment {
    /// The view being rendered into.
    pub view: ImageViewHandle,
    /// What to do with its existing contents.
    pub load: Load,
}

/// The depth target a pass tests against.
#[derive(Debug, Clone, Copy)]
pub struct DepthAttachment {
    /// The view being tested and written.
    pub view: ImageViewHandle,
    /// What to do with its existing contents.
    pub load: Load,
    /// Whether the result is kept after the pass.
    ///
    /// Almost always `false`: a depth buffer is scratch space for one pass, and
    /// storing it costs bandwidth for something nothing reads. A pass feeding a
    /// later depth-based effect is what sets it.
    pub store: bool,
}

/// Everything a pass renders into.
#[derive(Debug, Clone, Copy)]
pub struct Attachments {
    /// The colour target, or `None` for a pass that writes only depth.
    ///
    /// `None` is the depth prepass (`docs/PLAN.md` §9.4). Must agree with the
    /// pipeline's `color_format` the same way `depth` must agree with its
    /// `depth_format`.
    pub color: Option<ColorAttachment>,
    /// The depth target, or `None` for a pass that neither tests nor writes it.
    ///
    /// Must agree with the pipeline's `depth_format`: a pipeline built with a
    /// depth format and used in a pass without one is a validation error at
    /// draw time rather than at pipeline creation.
    pub depth: Option<DepthAttachment>,
    /// The area being drawn, which is also the initial viewport and scissor.
    pub extent: Extent2D,
}

/// A render pass in progress.
///
/// Ends when dropped, so it cannot be left open. Every draw method lives here
/// rather than on [`CommandBuffer`], which is what makes "draws happen inside a
/// pass" a fact the compiler enforces rather than a convention.
pub struct Pass<'a> {
    command: &'a CommandBuffer,
    device: Arc<Device>,
    /// The layout of the currently bound pipeline, for push constants and for
    /// binding the heap. `None` until something is bound, which is why both of
    /// those are no-ops before then rather than undefined.
    layout: Option<vk::PipelineLayout>,
}

impl CommandBuffer {
    /// Begin rendering into `attachments`.
    ///
    /// The viewport and scissor are set to the whole render area, since that is
    /// what every pass but a clipped one wants; [`Pass::set_scissor`] narrows it.
    ///
    /// The command buffer must be recording and not already inside a pass.
    pub fn begin_rendering<'a>(&'a self, attachments: &Attachments) -> Pass<'a> {
        // An array plus a slice rather than a `Vec`: this is a per-frame path
        // and `docs/CONVENTIONS.md` §8 says it allocates nothing. Zero or one
        // colour attachment is all `Attachments` can express, so the storage is
        // a fixed size.
        let one;
        let color: &[vk::RenderingAttachmentInfo<'_>] = match attachments.color {
            Some(color) => {
                one = [rendering_attachment(
                    color.view,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    color.load,
                    true,
                )];
                &one
            }
            None => &[],
        };

        let depth = attachments.depth.map(|depth| {
            rendering_attachment(
                depth.view,
                vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
                depth.load,
                depth.store,
            )
        });

        let mut info = vk::RenderingInfo::default()
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: attachments.extent.to_vk(),
            })
            .layer_count(1)
            .color_attachments(color);

        if let Some(depth) = &depth {
            info = info.depth_attachment(depth);
        }

        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: attachments.extent.width as f32,
            height: attachments.extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [full(attachments.extent)];

        // SAFETY: the buffer is recording, every borrowed structure outlives
        // these calls, and `dynamic_rendering` is in the required feature tier.
        unsafe {
            let raw = self.device().raw();

            raw.cmd_begin_rendering(self.handle(), &info);
            raw.cmd_set_viewport(self.handle(), 0, &viewports);
            raw.cmd_set_scissor(self.handle(), 0, &scissors);
        }

        Pass {
            command: self,
            device: Arc::clone(self.device()),
            layout: None,
        }
    }
}

impl Pass<'_> {
    /// Bind the pipeline subsequent draws use.
    ///
    /// Also remembers its layout, which is what [`Pass::push_constants`] and
    /// [`Pass::bind_heap`] need — Vulkan takes the layout rather than the
    /// pipeline for both, and passing a different one is a compatibility error
    /// the driver reports in terms of neither.
    pub fn bind_pipeline(&mut self, pipeline: &GraphicsPipeline) {
        self.layout = Some(pipeline.layout().handle());

        // SAFETY: the buffer is recording inside a pass, and the pipeline
        // belongs to this device.
        unsafe {
            self.device.raw().cmd_bind_pipeline(
                self.command.handle(),
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.handle(),
            );
        }
    }

    /// Bind the bindless heap for the current pipeline's layout.
    ///
    /// Must follow [`Pass::bind_pipeline`], and must be repeated after binding a
    /// pipeline whose layout differs. Two layouts are compatible only if their
    /// push constant ranges match as well as their set layouts, so a binding
    /// made for one does not carry to another — a mistake validation catches and
    /// nothing else does.
    pub fn bind_heap(&self, heap: &BindlessHeap) {
        let Some(layout) = self.layout else {
            return;
        };

        heap.bind(
            self.command.handle(),
            vk::PipelineBindPoint::GRAPHICS,
            layout,
        );
    }

    /// Narrow which pixels subsequent draws may touch.
    ///
    /// Clamped to nothing: a scissor outside the framebuffer is a validation
    /// error rather than a clamp, and the caller knows its own bounds.
    pub fn set_scissor(&self, scissor: Rect2D) {
        // SAFETY: the buffer is recording and `scissors` outlives the call.
        unsafe {
            self.device
                .raw()
                .cmd_set_scissor(self.command.handle(), 0, &[scissor.to_vk()]);
        }
    }

    /// Bind the vertex buffer at binding zero.
    pub fn bind_vertex_buffer(&self, buffer: &Buffer) {
        // SAFETY: the buffer is recording, the vertex buffer belongs to this
        // device, and both arrays outlive the call.
        unsafe {
            self.device.raw().cmd_bind_vertex_buffers(
                self.command.handle(),
                0,
                &[buffer.handle().0],
                &[0],
            );
        }
    }

    /// Bind the index buffer, as 32-bit indices.
    ///
    /// Only `UINT32`, because that is what the cooked mesh format writes. A
    /// 16-bit path halves index bandwidth and is worth having once something
    /// produces one.
    pub fn bind_index_buffer(&self, buffer: &Buffer) {
        // SAFETY: the buffer is recording and the index buffer belongs to this
        // device.
        unsafe {
            self.device.raw().cmd_bind_index_buffer(
                self.command.handle(),
                buffer.handle().0,
                0,
                vk::IndexType::UINT32,
            );
        }
    }

    /// Set the push constant block for subsequent draws.
    ///
    /// A no-op before a pipeline is bound, since there is no layout to set them
    /// against. `bytes` must be no longer than the layout declared.
    pub fn push_constants(&self, bytes: &[u8]) {
        let Some(layout) = self.layout else {
            return;
        };

        // SAFETY: the buffer is recording, the layout belongs to this device,
        // and `bytes` outlives the call.
        unsafe {
            self.device.raw().cmd_push_constants(
                self.command.handle(),
                layout,
                // Every stage: the bindless model has one layout shared by
                // vertex and fragment, and splitting visibility would mean a
                // range per stage for no benefit.
                vk::ShaderStageFlags::ALL,
                0,
                bytes,
            );
        }
    }

    /// Draw `vertices` vertices without an index buffer.
    ///
    /// What a shader generating its own positions from `SV_VertexID` uses.
    pub fn draw(&self, vertices: u32, instances: u32) {
        // SAFETY: the buffer is recording inside a pass with a pipeline bound.
        unsafe {
            self.device
                .raw()
                .cmd_draw(self.command.handle(), vertices, instances, 0, 0);
        }
    }

    /// Draw `indices` indices from the bound index buffer.
    ///
    /// `first_index` and `base_vertex` are what let several meshes share one
    /// pair of buffers, which is how the overlay batches its draws.
    pub fn draw_indexed(&self, indices: u32, instances: u32, first_index: u32, base_vertex: i32) {
        // SAFETY: the buffer is recording inside a pass with a pipeline and
        // index buffer bound.
        unsafe {
            self.device.raw().cmd_draw_indexed(
                self.command.handle(),
                indices,
                instances,
                first_index,
                base_vertex,
                0,
            );
        }
    }
}

impl Drop for Pass<'_> {
    fn drop(&mut self) {
        // SAFETY: the buffer is recording and this pass began it. Ending on drop
        // is what makes an unbalanced pass unrepresentable.
        unsafe { self.device.raw().cmd_end_rendering(self.command.handle()) };
    }
}

impl std::fmt::Debug for Pass<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pass")
            .field("pipeline_bound", &self.layout.is_some())
            .finish()
    }
}

/// A scissor covering the whole extent.
fn full(extent: Extent2D) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: extent.to_vk(),
    }
}

/// One attachment's rendering info.
fn rendering_attachment(
    view: ImageViewHandle,
    layout: vk::ImageLayout,
    load: Load,
    store: bool,
) -> vk::RenderingAttachmentInfo<'static> {
    let info = vk::RenderingAttachmentInfo::default()
        .image_view(view.0)
        .image_layout(layout)
        .store_op(if store {
            vk::AttachmentStoreOp::STORE
        } else {
            vk::AttachmentStoreOp::DONT_CARE
        });

    match load {
        Load::Preserve => info.load_op(vk::AttachmentLoadOp::LOAD),
        Load::Discard => info.load_op(vk::AttachmentLoadOp::DONT_CARE),
        Load::Clear(value) => info
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .clear_value(match value {
                ClearValue::Color(color) => vk::ClearValue {
                    color: vk::ClearColorValue { float32: color },
                },
                ClearValue::Depth(depth) => vk::ClearValue {
                    depth_stencil: vk::ClearDepthStencilValue { depth, stencil: 0 },
                },
            }),
    }
}

/// Unused, but the type must be nameable for `PipelineLayout` to appear in a
/// signature above.
#[expect(dead_code, reason = "imported for the doc link in `bind_pipeline`")]
type LayoutForDocs = PipelineLayout;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cleared_attachment_carries_its_value() {
        let info = rendering_attachment(
            ImageViewHandle(vk::ImageView::null()),
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            Load::Clear(ClearValue::Color([0.25, 0.5, 0.75, 1.0])),
            true,
        );

        assert_eq!(info.load_op, vk::AttachmentLoadOp::CLEAR);
        assert_eq!(info.store_op, vk::AttachmentStoreOp::STORE);

        // SAFETY: the union was written as `float32` two lines above, by the
        // `ClearValue::Color` arm of `rendering_attachment`.
        let written = unsafe { info.clear_value.color.float32 };

        assert_eq!(written, [0.25, 0.5, 0.75, 1.0]);
    }

    #[test]
    fn a_preserved_attachment_loads_rather_than_clears() {
        // The overlay depends on this: it composites over a scene already in the
        // attachment, and clearing would erase the frame it draws on top of.
        let info = rendering_attachment(
            ImageViewHandle(vk::ImageView::null()),
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            Load::Preserve,
            true,
        );

        assert_eq!(info.load_op, vk::AttachmentLoadOp::LOAD);
    }

    #[test]
    fn a_depth_attachment_that_is_not_stored_says_so() {
        // Depth is scratch for one pass; storing it costs bandwidth for
        // something nothing reads.
        let info = rendering_attachment(
            ImageViewHandle(vk::ImageView::null()),
            vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
            Load::Clear(ClearValue::Depth(crate::DEPTH_CLEAR)),
            false,
        );

        assert_eq!(info.store_op, vk::AttachmentStoreOp::DONT_CARE);
    }

    #[test]
    fn the_default_scissor_covers_the_whole_target() {
        let extent = Extent2D::new(1920, 1080);
        let scissor = full(extent);

        assert_eq!(scissor.offset.x, 0);
        assert_eq!(scissor.offset.y, 0);
        assert_eq!(scissor.extent, extent.to_vk());
    }
}
