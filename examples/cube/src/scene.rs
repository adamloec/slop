//! Everything needed to draw the cube into a caller-supplied target.

use std::path::PathBuf;
use std::sync::Arc;

use slop_asset::{Assets, Mesh, Texture, Vfs};
use slop_core::Handle;
use slop_core::diagnostics::tracing::{info, warn};
use slop_math::{Mat4, Quat, Vec3};
use slop_render::{Overlay, VertexBinding};
use slop_rhi::{
    Allocator, BindlessHeap, BindlessHeapConfig, Blend, Buffer, BufferConfig, BufferState,
    DEPTH_CLEAR, Device, GraphicsPipeline, GraphicsPipelineConfig, Image, ImageConfig, ImageState,
    MemoryLocation, PipelineLayout, PipelineLayoutConfig, SampledImage, Sampler, ShaderModule,
    ShaderStage, vk,
};

/// Per-draw data, matching `PushConstants` in `shaders/passes/cube.slang`.
///
/// `#[repr(C)]` is load-bearing: Rust may reorder the fields of a default-layout
/// struct, and the shader reads this block by offset.
///
/// Its *size* is checked against the shader's reflected block at startup, which
/// is what catches a field added on one side and not the other. The field order
/// is not checked and cannot be from reflection alone — that is what a generic
/// material parameter writer would fix, and `docs/PLAN.md` §6.1 records it.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PushConstants {
    /// Model, view and projection combined.
    pub model_view_projection: Mat4,
    /// Model alone, for transforming normals to world space.
    pub model: Mat4,
    /// Slot of the albedo texture in the bindless heap.
    pub texture: u32,
    /// Slot of the sampler.
    pub sampler: u32,
    /// Makes the struct's size equal the sum of its fields.
    ///
    /// Without it `Mat4`'s sixteen-byte alignment rounds `size_of` from 136 up
    /// to 144, and the last eight bytes are padding no field ever writes.
    /// Casting the struct to bytes then *reads uninitialised memory*, which is
    /// undefined behaviour — the same bug the `Blittable` derive was fixed for
    /// earlier, arriving by a different door. `#[derive(Pod)]` refuses to
    /// compile without this, which is how it stopped being invisible.
    pub padding: [u32; 2],
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
    /// Held so the heap's descriptor stays valid, and replaced on hot reload.
    texture: Image,
    depth: Image,
    indices: Buffer,
    vertices: Buffer,
    texture_slot: Handle<SampledImage>,
    sampler_slot: Handle<Sampler>,
    /// How many indices the cooked mesh has, for the draw call.
    index_count: u32,
    /// Bytes the shader's push constant block occupies, from reflection.
    push_constant_bytes: u32,
    /// The debug overlay.
    ///
    /// Owned here because the bindless heap is: `Overlay` inserts its font atlas
    /// and sampler into the same table everything else uses, which is the point
    /// of a bindless model. At M3 the renderer owns the heap and the overlay
    /// sits beside the scene rather than inside it.
    overlay: Overlay,
    /// The CPU-side assets, kept so [`Scene::reload_changed`] has something to
    /// compare against and something to re-upload from.
    meshes: Assets<Mesh>,
    textures: Assets<Texture>,
    mesh: Handle<Mesh>,
    albedo: Handle<Texture>,
    /// What was last uploaded to the GPU.
    ///
    /// The registry's revision counter says the *asset* changed; these say
    /// whether the change has reached the GPU yet. Without them a reload would
    /// re-upload on every frame after the first change, because "the asset has
    /// been reloaded once" stays true forever.
    uploaded_mesh: u32,
    uploaded_albedo: u32,
    allocator: Arc<Allocator>,
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

        // What the shader says about itself, cooked from the same compile that
        // produced the SPIR-V above. Everything below that used to be restated
        // in Rust — the attribute formats, their offsets, the stride, the push
        // constant size — is derived from this instead.
        let reflection = load_reflection()?;
        let vertices_layout = VertexBinding::interleaved(&reflection)
            .map_err(|error| format!("the cube shader's vertex inputs: {error}"))?;

        // The shader's block, which is what the layout is sized to and what gets
        // pushed — not `size_of::<PushConstants>()`.
        //
        // Those two differ, and finding out why is the reason this is written
        // down. `PushConstants` is 136 bytes of fields and `size_of` reports
        // **144**: `Mat4` has 16-byte alignment, so Rust rounds the struct up to
        // a multiple of it, and the last eight bytes are tail padding no field
        // occupies. Pushing `size_of` bytes therefore sends eight bytes of
        // nothing past the end of what the shader declared. It was harmless —
        // the range was simply larger than the block — but it meant the engine's
        // idea of the block and the shader's had never actually agreed, and
        // nothing would have said so.
        //
        // Reflection is the authority now: the layout is sized from the shader,
        // and the push below sends exactly that many bytes.
        let push_constant_bytes = reflection.push_constant_bytes;
        if push_constant_bytes as usize > size_of::<PushConstants>() {
            return Err(format!(
                "the shader's push constant block is {push_constant_bytes} bytes and \
                 `PushConstants` is only {}; the shader would read past the end of what \
                 this pushes",
                size_of::<PushConstants>()
            ));
        }

        let mut heap = BindlessHeap::new(device, &BindlessHeapConfig::default())
            .map_err(|error| error.to_string())?;

        let layout = Arc::new(
            PipelineLayout::new(
                device,
                &PipelineLayoutConfig {
                    heap: Some(&heap),
                    push_constant_bytes,
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
                vertex_layout: Some(vertices_layout.layout()),
                // On. With culling off a reversed face still renders, and the
                // cube looks right from outside while being wrong.
                cull_back_faces: true,
                blend: Blend::Opaque,
            },
        )
        .map_err(|error| error.to_string())?;

        // Content comes from the registries, not from `const`s in this crate.
        // Both source assets were generated from the code they replaced, so the
        // golden image is the oracle for the whole path — parse, cook, key,
        // cache, VFS, decode, upload.
        //
        // The registries are kept rather than dropped after upload, which is
        // what [`Scene::reload_changed`] needs: something to compare against,
        // and something to re-upload from.
        let project = project_root();
        let mut meshes = Assets::<Mesh>::for_project(&project);
        let mut textures = Assets::<Texture>::for_project(&project);

        let mesh = meshes.load("meshes/cube.Cube.0.mesh").map_err(cook_first)?;
        let albedo = textures.load("textures/checker.tex").map_err(cook_first)?;

        let cooked = meshes.get(mesh).expect("just loaded");

        let vertices = upload_buffer(
            device,
            allocator,
            "cube vertices",
            bytemuck::cast_slice(&cooked.vertices),
            vk::BufferUsageFlags::VERTEX_BUFFER,
            BufferState::VERTEX_INPUT,
        )?;
        let indices = upload_buffer(
            device,
            allocator,
            "cube indices",
            bytemuck::cast_slice(&cooked.indices),
            vk::BufferUsageFlags::INDEX_BUFFER,
            BufferState::INDEX_INPUT,
        )?;
        let index_count = u32::try_from(cooked.indices.len())
            .map_err(|_| String::from("the cube has more indices than a draw call can take"))?;

        let texture = upload_texture(
            device,
            allocator,
            textures.get(albedo).expect("just loaded"),
        )?;
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

        let overlay_module = load_module(device, "shaders/passes/overlay.spv")?;
        let overlay = Overlay::new(
            device,
            &mut heap,
            &overlay_module,
            &load_reflection_at("shaders/passes/overlay.refl")?,
            color_format,
        )
        .map_err(|error| error.to_string())?;

        Ok(Self {
            pipeline,
            overlay,
            heap,
            sampler,
            texture,
            depth,
            indices,
            vertices,
            texture_slot,
            sampler_slot,
            index_count,
            push_constant_bytes,
            meshes,
            textures,
            mesh,
            albedo,
            uploaded_mesh: 0,
            uploaded_albedo: 0,
            allocator: Arc::clone(allocator),
            device: Arc::clone(device),
        })
    }

    /// Recook-aware reload: pick up any cooked asset that changed on disk.
    ///
    /// The runtime half of hot reload. Run `cargo run -p slop-cli -- cook
    /// --watch` beside the demo and editing `assets/checker.png` or
    /// `assets/cube.gltf` changes what is on screen without a restart.
    ///
    /// Returns whether anything was re-uploaded.
    ///
    /// **Not called from `Scene::record`**, and that is deliberate: the golden
    /// test renders by frame number and must stay a pure function of it
    /// (`docs/DESIGN.md` §2.14). Only the windowed demo polls, so a test can
    /// never race a file on disk.
    ///
    /// # Errors
    ///
    /// Fails if a replacement GPU resource cannot be created or uploaded. An
    /// asset that fails to *decode* is logged and skipped instead — a broken
    /// save mid-edit is expected, and killing the demo over it would defeat the
    /// point of hot reload.
    /// Apply the debug overlay's texture changes.
    ///
    /// Separate from `record` because it uploads, and uploading waits — which
    /// must happen outside a recorded frame.
    ///
    /// # Errors
    ///
    /// Fails if a texture cannot be created or uploaded.
    pub fn update_overlay_textures(
        &mut self,
        allocator: &Arc<Allocator>,
        delta: &egui::TexturesDelta,
    ) -> Result<(), String> {
        self.overlay
            .update_textures(&mut self.heap, allocator, delta)
            .map_err(|error| error.to_string())
    }

    pub fn reload_changed(&mut self) -> Result<bool, String> {
        for (handle, outcome) in self.meshes.reload_changed() {
            if let Err(error) = outcome {
                warn!(asset = self.meshes.path(handle), %error, "mesh reload failed");
            }
        }
        for (handle, outcome) in self.textures.reload_changed() {
            if let Err(error) = outcome {
                warn!(asset = self.textures.path(handle), %error, "texture reload failed");
            }
        }

        let mesh_revision = self
            .meshes
            .revision(self.mesh)
            .unwrap_or(self.uploaded_mesh);
        let albedo_revision = self
            .textures
            .revision(self.albedo)
            .unwrap_or(self.uploaded_albedo);

        let mesh_stale = mesh_revision != self.uploaded_mesh;
        let albedo_stale = albedo_revision != self.uploaded_albedo;

        if !mesh_stale && !albedo_stale {
            return Ok(false);
        }

        // Everything below frees a resource the GPU may still be reading from a
        // frame that has not finished. Waiting is the blunt answer and the right
        // one here: hot reload happens when a human saves a file, so a stall
        // nobody can perceive costs nothing. A renderer doing this per frame
        // would need deferred deletion instead.
        self.device.wait_idle().map_err(|error| error.to_string())?;

        if mesh_stale {
            let cooked = self.meshes.get(self.mesh).expect("loaded");

            self.vertices = upload_buffer(
                &self.device,
                &self.allocator,
                "cube vertices",
                bytemuck::cast_slice(&cooked.vertices),
                vk::BufferUsageFlags::VERTEX_BUFFER,
                BufferState::VERTEX_INPUT,
            )?;
            self.indices = upload_buffer(
                &self.device,
                &self.allocator,
                "cube indices",
                bytemuck::cast_slice(&cooked.indices),
                vk::BufferUsageFlags::INDEX_BUFFER,
                BufferState::INDEX_INPUT,
            )?;
            self.index_count = u32::try_from(cooked.indices.len())
                .map_err(|_| String::from("the cube has more indices than a draw call can take"))?;

            self.uploaded_mesh = mesh_revision;
            info!(revision = mesh_revision, "reloaded the cube mesh");
        }

        if albedo_stale {
            let cooked = self.textures.get(self.albedo).expect("loaded");
            let texture = upload_texture(&self.device, &self.allocator, cooked)?;

            // The slot is released and retaken rather than left alone: the
            // descriptor names an image view, and the new image has a different
            // one. Freeing first is what lets the allocator hand back the same
            // index, so a session of reloads does not exhaust the heap.
            self.heap.remove_sampled_image(self.texture_slot);
            self.texture_slot = self
                .heap
                .insert_sampled_image(texture.view(), vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .ok_or_else(|| String::from("the bindless heap had no room for one texture"))?;
            self.texture = texture;

            self.uploaded_albedo = albedo_revision;
            info!(revision = albedo_revision, "reloaded the cube albedo");
        }

        Ok(true)
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

    /// Record one frame.
    ///
    /// `primitives` is what the debug overlay tessellated, drawn last and inside
    /// the same pass so it composites over the scene without a second load and
    /// store of the whole attachment. An empty slice draws no overlay, which is
    /// what the golden test passes — see the crate docs on determinism.
    ///
    /// Takes `&mut self` because the overlay writes this slot's vertex buffer.
    pub fn record(
        &mut self,
        frame: &slop_render::Frame<'_>,
        primitives: &[egui::ClippedPrimitive],
        pixels_per_point: f32,
    ) {
        let command = frame.command;
        let target = frame.target;
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
        let draws = [
            Self::model_matrix(frame.number),
            Self::second_model_matrix(frame.number),
        ];

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
            raw.cmd_bind_index_buffer(buffer, self.indices.handle(), 0, vk::IndexType::UINT32);

            for model in draws {
                let push = PushConstants {
                    model_view_projection: view_projection * model,
                    model,
                    texture: self.texture_slot.index(),
                    sampler: self.sampler_slot.index(),
                    padding: [0; 2],
                };

                raw.cmd_push_constants(
                    buffer,
                    self.pipeline.layout().handle(),
                    vk::ShaderStageFlags::ALL,
                    0,
                    // Exactly the shader's block, not the Rust struct's
                    // `size_of` — see `Scene::new` on the eight bytes of tail
                    // padding that `Mat4` alignment adds.
                    &bytemuck::bytes_of(&push)[..self.push_constant_bytes as usize],
                );
                raw.cmd_draw_indexed(buffer, self.index_count, 1, 0, 0, 0);
            }
        }

        // SAFETY: the buffer is recording and the pass was begun above.
        unsafe {
            self.device.raw().cmd_end_rendering(command.handle());
        }

        // After the scene's pass ends, in one of its own. The overlay wants no
        // depth, and a pipeline used inside a pass must declare the depth format
        // that pass carries — sharing would depth-test the interface against the
        // cube and let geometry occlude the readout describing it.
        //
        // Errors are logged rather than propagated: a debug overlay that fails
        // to allocate must not take the frame with it.
        if let Err(error) = self.overlay.draw(
            &self.heap,
            &self.allocator,
            frame,
            primitives,
            pixels_per_point,
        ) {
            warn!(%error, "the debug overlay could not be drawn");
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

    slop_rhi::submit_and_wait(device, |command| {
        command.barrier_buffer(
            staging.handle(),
            BufferState::HOST_WRITE,
            BufferState::TRANSFER_SRC,
        );
        command.copy_buffer(staging.handle(), destination.handle(), size);
        command.barrier_buffer(destination.handle(), BufferState::TRANSFER_DST, final_state);
    })
    .map_err(|error| error.to_string())?;

    Ok(destination)
}

/// Upload the checkerboard and leave it ready for sampling.
fn upload_texture(
    device: &Arc<Device>,
    allocator: &Arc<Allocator>,
    cooked: &Texture,
) -> Result<Image, String> {
    let pixels = &cooked.pixels;

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
        .copy_from_slice(pixels);

    let texture = Image::new(
        allocator,
        &ImageConfig {
            name: "cube albedo",
            extent: vk::Extent2D {
                width: cooked.width,
                height: cooked.height,
            },
            // UNORM rather than SRGB, so the shader reads the bytes that were
            // uploaded. The golden image then compares shader output rather
            // than the result of a colour space conversion.
            format: vulkan_format(cooked.format),
            usage: vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
        },
    )
    .map_err(|error| error.to_string())?;

    slop_rhi::submit_and_wait(device, |command| {
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
    })
    .map_err(|error| error.to_string())?;

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
/// The Vulkan format a cooked texture's bytes are in.
///
/// UNORM rather than the `_SRGB` variants for both, so the shader reads the
/// bytes that were uploaded and the golden image compares shader output rather
/// than a colour space conversion the driver performed. The cooked format
/// deliberately does not encode that choice — see `slop_asset::texture::Format`.
///
/// A BC7 image is uploaded as blocks and never expanded: the copy below hands
/// the GPU exactly the bytes on disk, and the texture units decompress at sample
/// time. That is the whole point — the saving is in VRAM and bandwidth, not just
/// on disk.
fn vulkan_format(format: slop_asset::Format) -> vk::Format {
    match format {
        slop_asset::Format::Rgba8 => vk::Format::R8G8B8A8_UNORM,
        slop_asset::Format::Bc7 => vk::Format::BC7_UNORM_BLOCK,
    }
}

/// The project this example's assets were cooked into.
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// Turn a load failure into a message that says what to do about it.
///
/// The useful half of an asset error is in its source chain — `Display` on an
/// error shows only its own message by convention, so `AssetError` alone says
/// "reading mesh 'meshes/cube.Cube.0.mesh'" and never says where it looked or
/// why that failed. Walking the chain is what turns that into something
/// actionable.
fn cook_first(error: slop_asset::AssetError) -> String {
    use std::error::Error;

    let mut message = error.to_string();
    let mut cause = error.source();

    while let Some(error) = cause {
        message.push_str(": ");
        message.push_str(&error.to_string());
        cause = error.source();
    }

    format!("{message}. Run `cargo run -p slop-cli -- cook` first")
}

/// Load the cooked cube shader's reflection.
///
/// Beside the SPIR-V and from the same compile, so the two cannot describe
/// different shaders.
fn load_reflection() -> Result<slop_asset::Reflection, String> {
    load_reflection_at("shaders/passes/cube.refl")
}

/// Load any cooked reflection by logical path.
fn load_reflection_at(logical: &str) -> Result<slop_asset::Reflection, String> {
    let bytes = cooked(logical)?;

    slop_asset::Reflection::read(&bytes).map_err(|error| error.to_string())
}

/// Read cooked bytes, with the hint that says what to do when they are absent.
fn cooked(logical: &str) -> Result<Vec<u8>, String> {
    Vfs::for_project(&project_root())
        .read(logical)
        .map_err(|error| format!("{error}. Run `cargo run -p slop-cli -- cook` first"))
}

/// Load the cooked cube shader.
///
/// Through the asset VFS, so this names the shader rather than a path into the
/// cache. Where cooked bytes live is `slop-asset`'s business.
fn load_shader(device: &Arc<Device>) -> Result<ShaderModule, String> {
    load_module(device, "shaders/passes/cube.spv")
}

/// Load any cooked SPIR-V module by logical path.
fn load_module(device: &Arc<Device>, logical: &str) -> Result<ShaderModule, String> {
    let bytes = cooked(logical)?;

    ShaderModule::from_bytes(device, &bytes).map_err(|error| error.to_string())
}
