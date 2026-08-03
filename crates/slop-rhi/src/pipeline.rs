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

use crate::BindlessHeap;

use crate::{Device, Format, RhiError, ShaderModule};

/// Front faces wind counter-clockwise.
///
/// glTF's convention, and glTF is the import format (`docs/DESIGN.md` §2.8), so
/// matching it means imported meshes need no winding flip — the same reasoning
/// that picked right-handed Y-up in `slop-math`. Vulkan's default agrees.
const FRONT_FACE: vk::FrontFace = vk::FrontFace::COUNTER_CLOCKWISE;

/// The depth comparison, fixed by the reversed depth convention.
///
/// `slop-math` maps the near plane to 1.0 and the far plane to 0.0, so "closer"
/// means "larger" and a fragment passes when its depth is **greater** than what
/// is already there. The conventional `LESS` is silently wrong under reverse-Z:
/// nothing fails, the depth buffer simply keeps the furthest surface at every
/// pixel and the scene renders inside out.
///
/// Three things must agree, and they live in three files: this comparison, the
/// clear value ([`DEPTH_CLEAR`]), and the projection matrix in `slop-math`. Two
/// out of three produces a plausible-looking image that is wrong, which is why
/// `docs/DESIGN.md` §1.2 principle 6 called this a rewrite rather than a
/// refactor and why it was settled at M0.
pub const DEPTH_COMPARE: vk::CompareOp = vk::CompareOp::GREATER_OR_EQUAL;

/// The value a depth attachment is cleared to.
///
/// Zero, being the far plane under reverse-Z. Clearing to the conventional 1.0
/// would put the far plane at *near*, and every fragment would fail the test.
pub const DEPTH_CLEAR: f32 = 0.0;

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
    /// The fragment stage, or `None` for a pipeline that produces no fragments.
    ///
    /// `None` is what a depth prepass over opaque geometry wants: rasterization
    /// still runs and still writes depth, but no fragment shader is invoked, so
    /// the pass costs geometry throughput and nothing else. That is the entire
    /// point of a prepass (`docs/PLAN.md` §9.4) — running the real shader twice
    /// would cost more than the overdraw it saves.
    ///
    /// Alpha-masked geometry is the exception and needs `Some`: the `discard` is
    /// what decides whether the fragment exists at all, so a prepass that skips
    /// it writes depth for pixels that should have been cut away.
    pub fragment: Option<ShaderStage<'a>>,
    /// Format of the single colour attachment this pipeline renders into, or
    /// `None` for a pipeline that writes only depth.
    ///
    /// Must match the attachment's, or the driver rejects the draw.
    pub color_format: Option<Format>,
    /// Format of the depth attachment, or `None` for a pipeline that neither
    /// tests nor writes depth.
    ///
    /// Must match the format the depth image was created with, and must be
    /// `None` when rendering supplies no depth attachment. A mismatch either way
    /// is a validation error at draw time rather than at pipeline creation.
    ///
    /// When present, depth testing and writing are both **on**, comparing with
    /// [`DEPTH_COMPARE`]. There is no knob: a pipeline that wants depth read
    /// without write, or a different comparison, is a different pipeline, and
    /// inventing the configuration surface before a pass needs it would be
    /// designing against imagined requirements (`docs/PLAN.md` §4.1-D).
    pub depth_format: Option<Format>,
    /// Layout of the vertex buffer bound at binding 0, or `None` for a shader
    /// generating its own positions from `SV_VertexID`.
    pub vertex_layout: Option<VertexLayout<'a>>,
    /// Whether to discard back faces. Off is useful while debugging geometry
    /// whose winding is in doubt.
    pub cull_back_faces: bool,
    /// How fragments combine with what is already in the attachment.
    pub blend: Blend,
}

/// How a pipeline's output combines with the attachment.
///
/// Two named modes rather than the full Vulkan blend state. Both exist because
/// something needs them; exposing the twelve knobs Vulkan has before a pass asks
/// for them would be designing against imagined requirements
/// (`docs/PLAN.md` §4.1-D).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Blend {
    /// Overwrite. What every opaque pass wants.
    #[default]
    Opaque,

    /// Composite over what is already there, assuming **premultiplied** alpha.
    ///
    /// `source + destination × (1 − source.a)`. Premultiplied rather than
    /// straight alpha because that is what the debug overlay's tessellator
    /// produces, and because it composites correctly under filtering and
    /// repeated blending where straight alpha does not — the classic dark-fringe
    /// artifact around a blended edge is straight alpha being interpolated.
    PremultipliedAlpha,
}

impl Blend {
    /// The attachment state this mode describes.
    fn attachment(self) -> vk::PipelineColorBlendAttachmentState {
        let state = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA);

        match self {
            Self::Opaque => state.blend_enable(false),
            Self::PremultipliedAlpha => state
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                // Alpha accumulates the same way, so drawing into a transparent
                // target and compositing that later gives the same result as
                // drawing straight onto the destination.
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD),
        }
    }
}

/// How to read one interleaved vertex buffer.
///
/// Shader input locations are assigned by position in [`attributes`], so the
/// order here is the order the shader declares. Keeping one list rather than a
/// list plus a separate set of location numbers removes the pairing that would
/// otherwise have to be maintained by hand.
///
/// [`attributes`]: Self::attributes
#[derive(Debug, Clone, Copy)]
pub struct VertexLayout<'a> {
    /// Bytes between consecutive vertices.
    ///
    /// Must equal the Rust vertex struct's `size_of`, which is why the struct
    /// wants `#[repr(C)]` — Rust may otherwise reorder fields, and then the
    /// offsets below describe a layout the compiler did not produce.
    pub stride: u32,
    /// Each attribute's format and byte offset, in shader location order.
    pub attributes: &'a [(Format, u32)],
}

/// What a pipeline's shaders can reach.
///
/// `docs/DESIGN.md` §2.2's bindless model means nearly every pipeline in the
/// engine shares one layout: the global heap, plus a small block of push
/// constants carrying the indices into it. A bespoke layout per material is
/// what bindless exists to remove.
#[derive(Debug, Clone, Copy, Default)]
pub struct PipelineLayoutConfig<'a> {
    /// The bindless heap, bound at [`HEAP_SET`](crate::HEAP_SET).
    ///
    /// `None` produces a layout with no descriptor sets, which suits a shader
    /// reading nothing — the M0 triangle, and little else.
    pub heap: Option<&'a BindlessHeap>,
    /// Bytes of push constants, visible to all stages.
    ///
    /// Push constants rather than a uniform buffer for per-draw data, because
    /// they need no descriptor, no allocation, and no synchronization. The
    /// guaranteed minimum is 128 bytes and creation fails past the device's
    /// limit, so this is not the place for anything that grows.
    pub push_constant_bytes: u32,
}

/// The resources a pipeline's shaders can access.
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
        Self::new(device, &PipelineLayoutConfig::default())
    }

    /// A layout over the bindless heap and a push-constant block.
    ///
    /// # Errors
    ///
    /// Fails if the driver rejects creation — in practice, a
    /// `push_constant_bytes` above `maxPushConstantsSize`.
    pub fn new(device: &Arc<Device>, config: &PipelineLayoutConfig<'_>) -> Result<Self, RhiError> {
        let set_layouts: Vec<vk::DescriptorSetLayout> =
            config.heap.map(|heap| heap.layout()).into_iter().collect();

        // ALL stages, matching the heap's own visibility. Declaring push
        // constants per stage means a vertex and fragment shader reading the
        // same struct need two ranges that must agree, and a disagreement is a
        // silent read of the wrong offset.
        let ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::ALL)
            .offset(0)
            .size(config.push_constant_bytes)];
        let ranges: &[vk::PushConstantRange] = if config.push_constant_bytes == 0 {
            // A zero-sized range is invalid, so an empty slice is how "no push
            // constants" is expressed.
            &[]
        } else {
            &ranges
        };

        let create_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(ranges);

        // SAFETY: `create_info` is fully initialized, and both borrowed slices
        // outlive the call.
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
        let mut stages = vec![
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(config.vertex.module.handle())
                .name(config.vertex.entry),
        ];

        if let Some(fragment) = config.fragment {
            stages.push(
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(fragment.module.handle())
                    .name(fragment.entry),
            );
        }

        // One interleaved vertex buffer at binding 0, or none at all.
        //
        // Fixed-function vertex input is not where this ends up: §4.2 stage B's
        // GPU-driven pipeline reads geometry from storage buffers by index,
        // because a draw whose vertex data the CPU never bound cannot use a
        // vertex buffer binding. It is here because M0's cube is CPU-submitted
        // and this is the shortest path to it, not because it is the model.
        let bindings = config.vertex_layout.map(|layout| {
            [vk::VertexInputBindingDescription::default()
                .binding(0)
                .stride(layout.stride)
                .input_rate(vk::VertexInputRate::VERTEX)]
        });

        let attributes: Vec<vk::VertexInputAttributeDescription> = config
            .vertex_layout
            .map(|layout| {
                layout
                    .attributes
                    .iter()
                    .enumerate()
                    .map(|(location, &(format, offset))| {
                        vk::VertexInputAttributeDescription::default()
                            .binding(0)
                            // Locations are assigned by position in the slice,
                            // so the Rust struct's field order *is* the shader's
                            // location order. One list to keep in step instead
                            // of two.
                            .location(u32::try_from(location).unwrap_or(0))
                            .format(format.to_vk())
                            .offset(offset)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let vertex_input = match &bindings {
            Some(bindings) => vk::PipelineVertexInputStateCreateInfo::default()
                .vertex_binding_descriptions(bindings)
                .vertex_attribute_descriptions(&attributes),
            None => vk::PipelineVertexInputStateCreateInfo::default(),
        };

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

        // One blend state per colour attachment, and none when there are none:
        // `attachmentCount` must equal the `colorAttachmentCount` below, so
        // these two are derived from the same `Option` rather than written
        // twice.
        let attachments: Vec<vk::PipelineColorBlendAttachmentState> = config
            .color_format
            .map(|_| config.blend.attachment())
            .into_iter()
            .collect();
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);

        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

        // Reverse-Z throughout — see `DEPTH_COMPARE`. Depth bounds stay off:
        // they are a culling optimization needing a separate feature, and using
        // them with a reversed range inverts their sense too.
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(config.depth_format.is_some())
            .depth_write_enable(config.depth_format.is_some())
            .depth_compare_op(DEPTH_COMPARE)
            .depth_bounds_test_enable(false)
            .stencil_test_enable(false);

        let color_formats: Vec<vk::Format> =
            config.color_format.map(Format::to_vk).into_iter().collect();
        let mut rendering = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(
                config
                    .depth_format
                    .map_or(vk::Format::UNDEFINED, Format::to_vk),
            );

        let create_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth_stencil)
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
