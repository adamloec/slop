//! Graphics pipelines and their layouts.
//!
//! # No render pass objects
//!
//! `dynamic_rendering` is part of the required feature tier, so pipelines
//! declare the attachment formats they target directly and there are no
//! `VkRenderPass` or `VkFramebuffer` objects anywhere in the engine. Those
//! would otherwise need caching, keying, and invalidating — machinery the render
//! graph at §4.2 would have to own for no benefit.
//!
//! # Viewport and scissor are always dynamic
//!
//! Baking them into the pipeline would mean recreating every pipeline on window
//! resize. Dynamic state costs nothing measurable and removes an entire class of
//! resize bug, so it is unconditional rather than an option.

use std::ffi::CStr;
use std::sync::Arc;

use ash::vk;

use crate::{Device, RhiError, ShaderModule};

/// Front faces wind counter-clockwise.
///
/// glTF's convention, and glTF is the import format (`docs/DESIGN.md` §2.8), so
/// matching it means imported meshes need no winding flip — the same reasoning
/// that picked right-handed Y-up in `slop-math`. Vulkan's default agrees.
const FRONT_FACE: vk::FrontFace = vk::FrontFace::COUNTER_CLOCKWISE;

/// What a shader stage is, and where to find it.
///
/// One cooked module carries every entry point its source declared, so a vertex
/// and fragment pair names the same module twice with different entry points.
#[derive(Debug, Clone, Copy)]
pub struct ShaderStage<'a> {
    /// The module holding the entry point.
    pub module: &'a ShaderModule,
    /// The entry point's name, as written in the shader source.
    pub entry: &'a CStr,
}

/// How to build a [`GraphicsPipeline`].
#[derive(Debug, Clone, Copy)]
pub struct GraphicsPipelineConfig<'a> {
    /// The vertex stage.
    pub vertex: ShaderStage<'a>,
    /// The fragment stage.
    pub fragment: ShaderStage<'a>,
    /// Format of the single colour attachment this pipeline renders into.
    ///
    /// Must match the swapchain's, or the driver rejects the draw.
    pub color_format: vk::Format,
    /// Whether to discard back faces. Off is useful while debugging geometry
    /// whose winding is in doubt.
    pub cull_back_faces: bool,
}

/// The resources a pipeline's shaders can access.
///
/// Empty for now. `docs/DESIGN.md` §2.2's bindless model means this eventually
/// holds one global descriptor set layout shared by nearly every pipeline,
/// rather than a bespoke layout per material — but that needs shader reflection
/// (§2.11) to derive, so it waits for M2.
pub struct PipelineLayout {
    handle: vk::PipelineLayout,
    device: Arc<Device>,
}

impl PipelineLayout {
    /// A layout with no descriptor sets and no push constants.
    ///
    /// # Errors
    ///
    /// Fails if the driver rejects creation.
    pub fn empty(device: &Arc<Device>) -> Result<Self, RhiError> {
        let create_info = vk::PipelineLayoutCreateInfo::default();

        // SAFETY: `create_info` is a fully initialized default with nothing
        // borrowed to outlive.
        let handle = unsafe { device.raw().create_pipeline_layout(&create_info, None) }?;

        Ok(Self {
            handle,
            device: Arc::clone(device),
        })
    }

    /// The underlying handle.
    pub fn handle(&self) -> vk::PipelineLayout {
        self.handle
    }
}

impl Drop for PipelineLayout {
    fn drop(&mut self) {
        // SAFETY: created from this device, destroyed exactly once, and the
        // device outlives this because we hold an `Arc` to it.
        unsafe {
            self.device.raw().destroy_pipeline_layout(self.handle, None);
        }
    }
}

impl std::fmt::Debug for PipelineLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineLayout").finish_non_exhaustive()
    }
}

/// A compiled graphics pipeline.
pub struct GraphicsPipeline {
    handle: vk::Pipeline,
    // Held so the layout cannot be destroyed while this pipeline can still be
    // bound. Vulkan permits destroying a layout after pipeline creation, but not
    // while it is used to bind descriptors, and encoding the stricter rule
    // costs nothing.
    layout: Arc<PipelineLayout>,
    device: Arc<Device>,
}

impl GraphicsPipeline {
    /// Compile a pipeline.
    ///
    /// # Errors
    ///
    /// Fails if the driver rejects the pipeline — most often because a shader's
    /// interface does not match, or the entry point name is wrong.
    pub fn new(
        device: &Arc<Device>,
        layout: &Arc<PipelineLayout>,
        config: &GraphicsPipelineConfig<'_>,
    ) -> Result<Self, RhiError> {
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(config.vertex.module.handle())
                .name(config.vertex.entry),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(config.fragment.module.handle())
                .name(config.fragment.entry),
        ];

        // No vertex buffers. The triangle's positions come from SV_VertexID, and
        // real geometry will arrive through storage buffers read by index rather
        // than fixed-function vertex input, which is what §4.2 stage B's
        // GPU-driven pipeline needs.
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();

        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);

        // Counts only; the values are supplied at record time.
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);

        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(if config.cull_back_faces {
                vk::CullModeFlags::BACK
            } else {
                vk::CullModeFlags::NONE
            })
            .front_face(FRONT_FACE)
            .line_width(1.0);

        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);

        // Opaque: write all four channels, blend nothing. Transparency arrives
        // with the material system.
        let attachments = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(false)];
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);

        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

        let color_formats = [config.color_format];
        let mut rendering =
            vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&color_formats);

        let create_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .color_blend_state(&color_blend)
            .dynamic_state(&dynamic_state)
            .layout(layout.handle())
            .push_next(&mut rendering);

        let create_infos = [create_info];

        // SAFETY: every structure `create_info` borrows outlives this call, the
        // shader modules are alive, and `dynamic_rendering` is part of the
        // required feature tier so the `PipelineRenderingCreateInfo` chain is
        // understood.
        let pipelines = unsafe {
            device
                .raw()
                .create_graphics_pipelines(vk::PipelineCache::null(), &create_infos, None)
        }
        // Pipeline creation reports failure as a tuple of the partial results
        // and the error code; only the code is useful here.
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

    /// The underlying handle.
    pub fn handle(&self) -> vk::Pipeline {
        self.handle
    }

    /// The layout this pipeline was built with.
    pub fn layout(&self) -> &Arc<PipelineLayout> {
        &self.layout
    }
}

impl Drop for GraphicsPipeline {
    fn drop(&mut self) {
        // SAFETY: created from this device, destroyed exactly once, and the
        // device outlives this because we hold an `Arc` to it.
        unsafe { self.device.raw().destroy_pipeline(self.handle, None) };
    }
}

impl std::fmt::Debug for GraphicsPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphicsPipeline").finish_non_exhaustive()
    }
}
