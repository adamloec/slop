//! Everything needed to draw the cube into a caller-supplied target.

use std::path::PathBuf;
use std::sync::Arc;

use slop_asset::Vfs;
use slop_core::Handle;
use slop_math::{Mat4, Quat, Vec3};
use slop_rhi::{
    Allocator, BindlessHeap, BindlessHeapConfig, Buffer, BufferConfig, BufferState, CommandBuffer,
    CommandPool, DEPTH_CLEAR, Device, GraphicsPipeline, GraphicsPipelineConfig, Image, ImageConfig,
    ImageState, MemoryLocation, PipelineLayout, PipelineLayoutConfig, RhiError, SampledImage,
    Sampler, ShaderModule, ShaderStage, TimelineSemaphore, VertexLayout, vk,
};

use crate::mesh;

/// Where a frame is drawn.
///
/// A borrowed handle rather than an owned image, because the two callers own
/// their targets differently: the windowed demo's belongs to the swapchain and
/// changes every frame, while the golden test's is one image it keeps. Taking
/// either would have forced the other to work around it.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    /// The colour image being rendered into.
    pub image: vk::Image,
    /// A view of it.
    pub view: vk::ImageView,
    /// Its size in pixels.
    pub extent: vk::Extent2D,
    /// The state the image is in on entry.
    ///
    /// Almost always [`ImageState::UNDEFINED`]: the frame clears it, so
    /// discarding the previous contents is both correct and faster than
    /// preserving them.
    pub from: ImageState,
    /// The state the image must be left in.
    ///
    /// [`ImageState::PRESENT`] for a swapchain image,
    /// [`ImageState::TRANSFER_SRC`] for one about to be read back.
    pub to: ImageState,
}

/// Per-draw data, matching `PushConstants` in `shaders/passes/cube.slang`.
///
/// `#[repr(C)]` for the same reason [`mesh::Vertex`] needs it: Rust may reorder
/// fields otherwise, and the shader reads by offset. A field added on one side
/// and not the other reads garbage rather than failing to compile — shader
/// reflection at M2 is what removes the duplication.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PushConstants {
    /// Model, view and projection combined.
    pub model_view_projection: Mat4,
    /// Model alone, for transforming normals to world space.
    pub model: Mat4,
    /// Slot of the albedo texture in the bindless heap.
    pub texture: u32,
    /// Slot of the sampler.
    pub sampler: u32,
}

/// Vertical field of view. A moderate value: a wide one distorts the cube's
/// edges enough to hide a projection mistake.
const FIELD_OF_VIEW: f32 = 50.0;

/// Distance from the camera to the cube's centre.
const CAMERA_DISTANCE: f32 = 3.0;

/// Radians of rotation per frame.
///
/// Per *frame*, not per second — see the crate docs on determinism. A little
/// over one degree, so consecutive frames differ visibly without the cube
/// spinning too fast to look at.
const RADIANS_PER_FRAME: f32 = 0.02;

/// Where the second cube sits relative to the first.
///
/// Overlapping in screen space and further from the camera, which is what makes
/// the depth comparison decide the result. See [`Scene::second_model_matrix`].
const SECOND_CUBE_OFFSET: Vec3 = Vec3::new(0.55, -0.35, -0.8);

/// The cube, its GPU resources, and the pipeline that draws it.
pub struct Scene {
    // Declared in drop order: things built from the device first, then the
    // resources, then the device itself. The heap must outlive the pipeline
    // layout that names its set layout, which the `Arc` in `GraphicsPipeline`
    // handles.
    pipeline: GraphicsPipeline,
    heap: BindlessHeap,
    sampler: vk::Sampler,
    #[expect(dead_code, reason = "held so the heap's descriptor stays valid")]
    texture: Image,
    depth: Image,
    indices: Buffer,
    vertices: Buffer,
    texture_slot: Handle<SampledImage>,
    sampler_slot: Handle<Sampler>,
    device: Arc<Device>,
}

impl Scene {
    /// Build everything, uploading geometry and the texture.
    ///
    /// `extent` sizes the depth buffer, which must match the colour target.
    ///
    /// # Errors
    ///
    /// Fails if any GPU resource cannot be created, if the cooked shader is
    /// missing, or if an upload cannot be submitted.
    pub fn new(
        device: &Arc<Device>,
        allocator: &Arc<Allocator>,
        extent: vk::Extent2D,
        color_format: vk::Format,
    ) -> Result<Self, String> {
        let module = load_shader(device)?;

        let mut heap = BindlessHeap::new(device, &BindlessHeapConfig::default())
            .map_err(|error| error.to_string())?;

        let layout = Arc::new(
            PipelineLayout::new(
                device,
                &PipelineLayoutConfig {
                    heap: Some(&heap),
                    push_constant_bytes: u32::try_from(size_of::<PushConstants>())
                        .expect("the push constant block is far under 4 GiB"),
                },
            )
            .map_err(|error| error.to_string())?,
        );

        let depth_format = slop_rhi::preferred_depth_format(device);

        let pipeline = GraphicsPipeline::new(
            device,
            &layout,
            &GraphicsPipelineConfig {
                vertex: ShaderStage {
                    module: &module,
                    entry: c"vertexMain",
                },
                fragment: ShaderStage {
                    module: &module,
                    entry: c"fragmentMain",
                },
                color_format,
                depth_format: Some(depth_format),
                vertex_layout: Some(VertexLayout {
                    stride: mesh::VERTEX_STRIDE,
                    attributes: &mesh::VERTEX_ATTRIBUTES,
                }),
                // On. With culling off a reversed face still renders, and the
                // cube looks right from outside while being wrong.
                cull_back_faces: true,
            },
        )
        .map_err(|error| error.to_string())?;

        let vertices = upload_buffer(
            device,
            allocator,
            "cube vertices",
            bytes_of(&mesh::VERTICES),
            vk::BufferUsageFlags::VERTEX_BUFFER,
            BufferState::VERTEX_INPUT,
        )?;
        let indices = upload_buffer(
            device,
            allocator,
            "cube indices",
            bytes_of(&mesh::INDICES),
            vk::BufferUsageFlags::INDEX_BUFFER,
            BufferState::INDEX_INPUT,
        )?;

        let texture = upload_texture(device, allocator)?;
        let sampler = create_sampler(device)?;

        let depth = Image::new(
            allocator,
            &ImageConfig {
                name: "cube depth",
                extent,
                format: depth_format,
                usage: vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT,
            },
        )
        .map_err(|error| error.to_string())?;

        let texture_slot = heap
            .insert_sampled_image(texture.view(), vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .ok_or_else(|| String::from("the bindless heap had no room for one texture"))?;
        let sampler_slot = heap
            .insert_sampler(sampler)
            .ok_or_else(|| String::from("the bindless heap had no room for one sampler"))?;

        Ok(Self {
            pipeline,
            heap,
            sampler,
            texture,
            depth,
            indices,
            vertices,
            texture_slot,
            sampler_slot,
            device: Arc::clone(device),
        })
    }

    /// Rebuild the depth buffer for a new size.
    ///
    /// # Errors
    ///
    /// Fails if the image cannot be allocated.
    pub fn resize(
        &mut self,
        allocator: &Arc<Allocator>,
        extent: vk::Extent2D,
    ) -> Result<(), String> {
        // Waiting first because the old depth image is about to be dropped and
        // frames referencing it may still be in flight.
        self.device.wait_idle().map_err(|error| error.to_string())?;

        self.depth = Image::new(
            allocator,
            &ImageConfig {
                name: "cube depth",
                extent,
                format: self.depth.format(),
                usage: vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT,
            },
        )
        .map_err(|error| error.to_string())?;

        Ok(())
    }

    /// The transform for a given frame.
    ///
    /// Public so a test can assert the animation is a function of the frame
    /// number and nothing else — the property `docs/DESIGN.md` §2.14 requires
    /// and the reason a golden image of a rotating object is possible.
    pub fn model_matrix(frame: u64) -> Mat4 {
        // Two axes at different rates, so the cube presents several faces over
        // a short run rather than spinning about one axis and hiding four of
        // them.
        let angle = frame as f32 * RADIANS_PER_FRAME;
        let rotation = Quat::from_rotation_y(angle) * Quat::from_rotation_x(angle * 0.6);

        Mat4::from_quat(rotation)
    }

    /// The transform for the second cube.
    ///
    /// **This exists to make the depth test observable.** A single convex cube
    /// with back-face culling renders correctly whether or not depth works —
    /// culling alone leaves exactly one surface per pixel, so a broken
    /// comparison, a wrong clear value, or no depth buffer at all would all
    /// produce the same image. `docs/PLAN.md` §4.2 claims the cube proves the
    /// stack works, and without a second overlapping object that claim would be
    /// false for depth specifically.
    ///
    /// So: a second cube, smaller, offset, and partly *behind* the first where
    /// they overlap on screen. Under reverse-Z the nearer surface has the larger
    /// depth and wins. Reverse the comparison and the far cube punches a hole
    /// through the near one, which is impossible to miss.
    pub fn second_model_matrix(frame: u64) -> Mat4 {
        let angle = frame as f32 * RADIANS_PER_FRAME;
        // Counter-rotating, so the two are never in the same orientation and a
        // transform accidentally shared between them would be visible.
        let rotation = Quat::from_rotation_z(-angle * 0.8) * Quat::from_rotation_x(angle * 0.4);

        Mat4::from_translation(SECOND_CUBE_OFFSET)
            * Mat4::from_quat(rotation)
            * Mat4::from_scale(Vec3::splat(0.7))
    }

    /// Record one frame into `target`.
    ///
    /// The command buffer must already be recording.
    pub fn record(&self, command: &CommandBuffer, target: Target, frame: u64) {
        let extent = target.extent;

        command.transition_image(
            target.image,
            vk::ImageAspectFlags::COLOR,
            target.from,
            ImageState::COLOR_ATTACHMENT,
        );
        command.transition_image(
            self.depth.handle(),
            self.depth.aspect(),
            // From UNDEFINED every frame: the depth buffer is cleared, so its
            // previous contents are worth nothing and discarding is faster.
            ImageState::UNDEFINED,
            ImageState::DEPTH_ATTACHMENT,
        );

        let color_attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(target.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.02, 0.02, 0.03, 1.0],
                },
            })];

        let depth_attachment = vk::RenderingAttachmentInfo::default()
            .image_view(self.depth.view())
            .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            // DONT_CARE: nothing reads depth after the pass. A depth prepass or
            // screen-space effect would need STORE, and this is where that
            // changes.
            .store_op(vk::AttachmentStoreOp::DONT_CARE)
            .clear_value(vk::ClearValue {
                depth_stencil: vk::ClearDepthStencilValue {
                    // Zero, the far plane under reverse-Z. Clearing to 1.0
                    // would put the far plane at *near* and reject every
                    // fragment.
                    depth: DEPTH_CLEAR,
                    stencil: 0,
                },
            });

        let rendering = vk::RenderingInfo::default()
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            })
            .layer_count(1)
            .color_attachments(&color_attachment)
            .depth_attachment(&depth_attachment);

        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: extent.width as f32,
            height: extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        }];

        let view = slop_math::look_at(
            Vec3::new(0.0, 0.0, CAMERA_DISTANCE),
            Vec3::ZERO,
            slop_math::UP,
        );
        let projection = slop_math::perspective(
            FIELD_OF_VIEW.to_radians(),
            extent.width as f32 / extent.height as f32,
            0.1,
        );
        let view_projection = projection * view;

        // Two draws sharing one pipeline, one heap, and one vertex buffer,
        // differing only in push constants — the shape a real frame takes.
        //
        // The **near cube is drawn first**, and that order is the entire point.
        // Far-then-near would produce a correct image by draw order alone,
        // proving nothing: the painter's algorithm and a working depth test are
        // indistinguishable when geometry arrives back to front. With the near
        // cube first, the far one appears only where depth *lets* it, so a
        // reversed comparison or a wrong clear value punches the far cube
        // straight through the near one.
        let draws = [Self::model_matrix(frame), Self::second_model_matrix(frame)];

        let raw = self.device.raw();
        let buffer = command.handle();
        let vertex_buffers = [self.vertices.handle()];
        let offsets = [0_u64];

        // SAFETY: the command buffer is recording, every borrowed structure
        // outlives these calls, `dynamic_rendering` is in the required feature
        // tier, and the push constant block matches the layout the pipeline was
        // built with.
        unsafe {
            raw.cmd_begin_rendering(buffer, &rendering);
            raw.cmd_set_viewport(buffer, 0, &viewports);
            raw.cmd_set_scissor(buffer, 0, &scissors);
            raw.cmd_bind_pipeline(
                buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.handle(),
            );

            self.heap.bind(
                buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.layout().handle(),
            );

            // Bound once, outside the loop. Both cubes are the same mesh — the
            // bindless heap and push constants are what make them different,
            // which is the model §4.2 stage B generalizes.
            raw.cmd_bind_vertex_buffers(buffer, 0, &vertex_buffers, &offsets);
            raw.cmd_bind_index_buffer(buffer, self.indices.handle(), 0, vk::IndexType::UINT16);

            for model in draws {
                let push = PushConstants {
                    model_view_projection: view_projection * model,
                    model,
                    texture: self.texture_slot.index(),
                    sampler: self.sampler_slot.index(),
                };

                raw.cmd_push_constants(
                    buffer,
                    self.pipeline.layout().handle(),
                    vk::ShaderStageFlags::ALL,
                    0,
                    as_bytes(&push),
                );
                raw.cmd_draw_indexed(
                    buffer,
                    u32::try_from(mesh::INDICES.len()).expect("36 fits in a u32"),
                    1,
                    0,
                    0,
                    0,
                );
            }

            raw.cmd_end_rendering(buffer);
        }

        command.transition_image(
            target.image,
            vk::ImageAspectFlags::COLOR,
            ImageState::COLOR_ATTACHMENT,
            target.to,
        );
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        // Every Vulkan object below is destroyed as this struct's fields drop,
        // which happens *after* this function returns, and the GPU may still be
        // executing. `Device::drop` also waits, but far too late for fields
        // declared before it. See `docs/slop-rhi/README.md` invariant 22.
        if let Err(error) = self.device.wait_idle() {
            slop_core::diagnostics::tracing::error!(%error, "device did not go idle");
        }

        // The sampler is a raw handle rather than an owning type, so it is
        // destroyed by hand. It has no `slop-rhi` wrapper because a sampler is
        // a small immutable descriptor an engine has a handful of, and the
        // sampler cache that owns them properly belongs with the material
        // system at M2.
        //
        // SAFETY: created from this device, destroyed exactly once, and the
        // wait above means no submitted work still references it.
        unsafe { self.device.raw().destroy_sampler(self.sampler, None) };
    }
}

impl std::fmt::Debug for Scene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scene")
            .field("texture_slot", &self.texture_slot.index())
            .field("depth_format", &self.depth.format())
            .finish_non_exhaustive()
    }
}

/// Upload bytes into a device-local buffer and leave it in `final_state`.
///
/// Synchronous: it submits and waits. That is correct for startup and wrong for
/// streaming, which needs the async transfer queue and a staging ring — both of
/// which arrive with the asset system at M2 (`docs/DESIGN.md` §2.8).
fn upload_buffer(
    device: &Arc<Device>,
    allocator: &Arc<Allocator>,
    name: &str,
    data: &[u8],
    usage: vk::BufferUsageFlags,
    final_state: BufferState,
) -> Result<Buffer, String> {
    let size = data.len() as u64;

    let mut staging = Buffer::new(
        allocator,
        &BufferConfig {
            name: "upload staging",
            size,
            usage: vk::BufferUsageFlags::TRANSFER_SRC,
            location: MemoryLocation::Upload,
        },
    )
    .map_err(|error| error.to_string())?;

    staging
        .mapped_mut()
        .map_err(|error| error.to_string())?
        .copy_from_slice(data);

    let destination = Buffer::new(
        allocator,
        &BufferConfig {
            name,
            size,
            usage: usage | vk::BufferUsageFlags::TRANSFER_DST,
            location: MemoryLocation::DeviceOnly,
        },
    )
    .map_err(|error| error.to_string())?;

    submit_once(device, |command| {
        command.barrier_buffer(
            staging.handle(),
            BufferState::HOST_WRITE,
            BufferState::TRANSFER_SRC,
        );
        command.copy_buffer(staging.handle(), destination.handle(), size);
        command.barrier_buffer(destination.handle(), BufferState::TRANSFER_DST, final_state);
    })?;

    Ok(destination)
}

/// Upload the checkerboard and leave it ready for sampling.
fn upload_texture(device: &Arc<Device>, allocator: &Arc<Allocator>) -> Result<Image, String> {
    let pixels = mesh::checkerboard();

    let mut staging = Buffer::new(
        allocator,
        &BufferConfig {
            name: "texture staging",
            size: pixels.len() as u64,
            usage: vk::BufferUsageFlags::TRANSFER_SRC,
            location: MemoryLocation::Upload,
        },
    )
    .map_err(|error| error.to_string())?;

    staging
        .mapped_mut()
        .map_err(|error| error.to_string())?
        .copy_from_slice(&pixels);

    let texture = Image::new(
        allocator,
        &ImageConfig {
            name: "cube albedo",
            extent: vk::Extent2D {
                width: mesh::TEXTURE_SIZE,
                height: mesh::TEXTURE_SIZE,
            },
            // UNORM rather than SRGB, so the shader reads the bytes that were
            // uploaded. The golden image then compares shader output rather
            // than the result of a colour space conversion.
            format: vk::Format::R8G8B8A8_UNORM,
            usage: vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
        },
    )
    .map_err(|error| error.to_string())?;

    submit_once(device, |command| {
        command.transition_image(
            texture.handle(),
            texture.aspect(),
            ImageState::UNDEFINED,
            ImageState::TRANSFER_DST,
        );
        command.copy_buffer_to_image(
            staging.handle(),
            texture.handle(),
            texture.aspect(),
            texture.extent(),
        );
        command.transition_image(
            texture.handle(),
            texture.aspect(),
            ImageState::TRANSFER_DST,
            ImageState::SHADER_READ,
        );
    })?;

    Ok(texture)
}

/// A sampler for the cube's albedo.
///
/// Nearest filtering, deliberately. A linear filter would blur the checkerboard
/// differently depending on sub-pixel coverage, which makes the golden image
/// far more sensitive to a driver's rounding than to anything worth catching.
/// Nearest also makes a texture-orientation mistake sharper to look at.
fn create_sampler(device: &Arc<Device>) -> Result<vk::Sampler, String> {
    let create_info = vk::SamplerCreateInfo::default()
        .mag_filter(vk::Filter::NEAREST)
        .min_filter(vk::Filter::NEAREST)
        .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
        .address_mode_u(vk::SamplerAddressMode::REPEAT)
        .address_mode_v(vk::SamplerAddressMode::REPEAT)
        .address_mode_w(vk::SamplerAddressMode::REPEAT);

    // SAFETY: `create_info` is fully initialized and borrows nothing.
    unsafe { device.raw().create_sampler(&create_info, None) }.map_err(|error| error.to_string())
}

/// Record, submit, and wait. Startup only — see [`upload_buffer`].
fn submit_once(device: &Arc<Device>, record: impl FnOnce(&CommandBuffer)) -> Result<(), String> {
    let pool = CommandPool::new(device, device.queue_families().graphics)
        .map_err(|error| error.to_string())?;
    let command = pool
        .allocate(1)
        .map_err(|error| error.to_string())?
        .pop()
        .expect("one buffer was requested");

    command.begin().map_err(|error| error.to_string())?;
    record(&command);
    command.end().map_err(|error| error.to_string())?;

    let timeline = TimelineSemaphore::new(device, 0).map_err(|error| error.to_string())?;

    let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(command.handle())];
    let signals = [vk::SemaphoreSubmitInfo::default()
        .semaphore(timeline.handle())
        .value(1)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submits = [vk::SubmitInfo2::default()
        .command_buffer_infos(&commands)
        .signal_semaphore_infos(&signals)];

    // SAFETY: the buffer is recorded and not pending, the timeline belongs to
    // this device, and every borrowed array outlives the call.
    unsafe {
        device
            .raw()
            .queue_submit2(device.queues().graphics, &submits, vk::Fence::null())
    }
    .map_err(|error: vk::Result| RhiError::Vulkan(error).to_string())?;

    let finished = timeline
        .wait(1, std::time::Duration::from_secs(10))
        .map_err(|error| error.to_string())?;

    if !finished {
        return Err(String::from(
            "an upload did not complete within ten seconds",
        ));
    }

    Ok(())
}

/// Load the cooked cube shader.
///
/// Through the asset VFS, so this names the shader rather than a path into the
/// cache. Where cooked bytes live is `slop-asset`'s business.
fn load_shader(device: &Arc<Device>) -> Result<ShaderModule, String> {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");

    let bytes = Vfs::for_project(&project)
        .read("shaders/passes/cube.spv")
        .map_err(|error| format!("{error}. Run `cargo run -p slop-cli -- cook` first"))?;

    ShaderModule::from_bytes(device, &bytes).map_err(|error| error.to_string())
}

/// A `Copy` value's bytes, for a push constant block.
fn as_bytes<T: Copy>(value: &T) -> &[u8] {
    // SAFETY: `T` is `Copy` and therefore has no padding requiring
    // initialization, the slice covers exactly the value, and it borrows from
    // `value` so it cannot outlive it. Reading padding bytes as `u8` is
    // defined; only reading them as their own type would not be.
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(value).cast::<u8>(), size_of::<T>()) }
}

/// A slice's bytes, for an upload.
fn bytes_of<T: Copy>(values: &[T]) -> &[u8] {
    // SAFETY: same reasoning as `as_bytes`, over a contiguous slice.
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of_val(values)) }
}
