//! Everything needed to draw the cube into a caller-supplied target.

use std::sync::Arc;

use slop_asset::{Assets, Mesh, Texture, Vfs};
use slop_core::Handle;
use slop_core::diagnostics::tracing::{info, warn};
use slop_math::{Mat4, Quat, Vec3};
use slop_render::VertexBinding;
use slop_rhi::{
    Allocator, Attachments, BindlessHeap, BindlessHeapConfig, Blend, Buffer, BufferConfig,
    BufferState, BufferUsage, ClearValue, ColorAttachment, DEPTH_CLEAR, DepthAttachment, Device,
    Extent2D, Format, GraphicsPipeline, GraphicsPipelineConfig, Image, ImageAspect, ImageConfig,
    ImageState, ImageUsage, Load, MemoryLocation, PipelineLayout, PipelineLayoutConfig,
    SampledImage, Sampler, SamplerConfig, ShaderModule, ShaderStage, TextureSampler,
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
    /// Held so the heap's descriptor stays valid; destroyed on drop by
    /// `TextureSampler` rather than by hand.
    #[expect(dead_code, reason = "the heap references this sampler")]
    sampler: TextureSampler,
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
        extent: Extent2D,
        color_format: Format,
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
        let mut meshes = Assets::<Mesh>::new(assets());
        let mut textures = Assets::<Texture>::new(assets());

        let mesh = meshes.load("meshes/cube.Cube.0.mesh").map_err(cook_first)?;
        let albedo = textures.load("textures/checker.tex").map_err(cook_first)?;

        let cooked = meshes.get(mesh).expect("just loaded");

        let vertices = upload_buffer(
            device,
            allocator,
            "cube vertices",
            bytemuck::cast_slice(&cooked.vertices),
            BufferUsage::VERTEX,
            BufferState::VERTEX_INPUT,
        )?;
        let indices = upload_buffer(
            device,
            allocator,
            "cube indices",
            bytemuck::cast_slice(&cooked.indices),
            BufferUsage::INDEX,
            BufferState::INDEX_INPUT,
        )?;
        let index_count = u32::try_from(cooked.indices.len())
            .map_err(|_| String::from("the cube has more indices than a draw call can take"))?;

        let texture = upload_texture(
            device,
            allocator,
            textures.get(albedo).expect("just loaded"),
        )?;
        // Nearest filtering, deliberately. A linear filter would blur the
        // checkerboard differently depending on sub-pixel coverage, making the
        // golden image far more sensitive to a driver's rounding than to
        // anything worth catching. Nearest also makes a texture-orientation
        // mistake sharper to look at.
        let sampler = TextureSampler::new(
            device,
            &SamplerConfig {
                filter: slop_rhi::Filter::Nearest,
                ..SamplerConfig::default()
            },
        )
        .map_err(|error| error.to_string())?;

        let depth = Image::new(
            allocator,
            &ImageConfig {
                name: "cube depth",
                extent,
                format: depth_format,
                usage: ImageUsage::DEPTH_STENCIL_ATTACHMENT,
                mip_levels: 1,
            },
        )
        .map_err(|error| error.to_string())?;

        let texture_slot = heap
            .insert_sampled_image(texture.view(), ImageState::SHADER_READ)
            .ok_or_else(|| String::from("the bindless heap had no room for one texture"))?;
        let sampler_slot = heap
            .insert_sampler(sampler.handle())
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
    /// The bindless heap everything this scene draws is indexed through.
    ///
    /// Exposed because the debug overlay's font atlas belongs in the *same*
    /// table as the scene's textures — that is what a bindless model is for —
    /// while the overlay itself belongs to the application (`slop_editor::debug`)
    /// rather than in here. At M3 the renderer owns the heap and this accessor
    /// goes with it; see `docs/PLAN.md` §6.1.
    #[must_use]
    pub fn heap(&self) -> &BindlessHeap {
        &self.heap
    }

    /// The heap, mutably, for inserting an atlas into.
    pub fn heap_mut(&mut self) -> &mut BindlessHeap {
        &mut self.heap
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
                BufferUsage::VERTEX,
                BufferState::VERTEX_INPUT,
            )?;
            self.indices = upload_buffer(
                &self.device,
                &self.allocator,
                "cube indices",
                bytemuck::cast_slice(&cooked.indices),
                BufferUsage::INDEX,
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
                .insert_sampled_image(texture.view(), ImageState::SHADER_READ)
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
    pub fn resize(&mut self, allocator: &Arc<Allocator>, extent: Extent2D) -> Result<(), String> {
        // Waiting first because the old depth image is about to be dropped and
        // frames referencing it may still be in flight.
        self.device.wait_idle().map_err(|error| error.to_string())?;

        self.depth = Image::new(
            allocator,
            &ImageConfig {
                name: "cube depth",
                extent,
                format: self.depth.format(),
                usage: ImageUsage::DEPTH_STENCIL_ATTACHMENT,
                mip_levels: 1,
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
    /// Draws the scene and nothing else. The debug overlay is the application's,
    /// drawn after this in a pass of its own — see `slop_editor::debug`.
    ///
    /// **Leaves the colour attachment in `COLOR_ATTACHMENT`, not in the frame's
    /// final state.** Only the last thing to draw may transition, and a scene
    /// cannot know whether an overlay follows it. Callers end with
    /// [`slop_render::Frame::finish`].
    pub fn record(&self, frame: &slop_render::Frame<'_>) {
        let command = frame.command;
        let target = frame.target;
        let extent = target.extent;

        command.transition_image(
            target.image,
            ImageAspect::Color,
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

        let mut pass = command.begin_rendering(&Attachments {
            color: ColorAttachment {
                view: target.view,
                load: Load::Clear(ClearValue::Color([0.02, 0.02, 0.03, 1.0])),
            },
            depth: Some(DepthAttachment {
                view: self.depth.view(),
                load: Load::Clear(ClearValue::Depth(DEPTH_CLEAR)),
                // Scratch for this pass only.
                store: false,
            }),
            extent,
        });

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

        pass.bind_pipeline(&self.pipeline);
        pass.bind_heap(&self.heap);

        // Bound once, outside the loop. Both cubes are the same mesh — the
        // bindless heap and push constants are what make them different, which
        // is the model §4.2 stage B generalizes.
        pass.bind_vertex_buffer(&self.vertices);
        pass.bind_index_buffer(&self.indices);

        for model in draws {
            let push = PushConstants {
                model_view_projection: view_projection * model,
                model,
                texture: self.texture_slot.index(),
                sampler: self.sampler_slot.index(),
                padding: [0; 2],
            };

            // Exactly the shader's block, not the Rust struct's `size_of` — see
            // `Scene::new` on the eight bytes of tail padding `Mat4` alignment
            // adds.
            pass.push_constants(&bytemuck::bytes_of(&push)[..self.push_constant_bytes as usize]);
            pass.draw_indexed(self.index_count, 1, 0, 0);
        }

        // Ends the pass, so the overlay can open its own.
        //
        // The overlay wants no depth, and a pipeline used inside a pass must
        // declare the depth format that pass carries — sharing one pass would
        // depth-test the interface against the cube and let geometry occlude the
        // readout describing it.
        //
        // No transition to `target.to` here. See this method's docs.
        drop(pass);
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

        // The sampler used to be destroyed here by hand. It is a
        // `TextureSampler` now, so its own `Drop` does it — which is what took
        // the last `unsafe` out of this file.
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
    usage: BufferUsage,
    final_state: BufferState,
) -> Result<Buffer, String> {
    let size = data.len() as u64;

    let mut staging = Buffer::new(
        allocator,
        &BufferConfig {
            name: "upload staging",
            size,
            usage: BufferUsage::TRANSFER_SRC,
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
            usage: usage | BufferUsage::TRANSFER_DST,
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
            usage: BufferUsage::TRANSFER_SRC,
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
            extent: Extent2D {
                width: cooked.width,
                height: cooked.height,
            },
            // UNORM rather than SRGB, so the shader reads the bytes that were
            // uploaded. The golden image then compares shader output rather
            // than the result of a colour space conversion.
            format: vulkan_format(cooked.format),
            usage: ImageUsage::SAMPLED | ImageUsage::TRANSFER_DST,
            mip_levels: cooked.mip_levels,
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

        // One copy per level, out of one staging buffer holding the chain.
        for (index, level) in cooked.levels().enumerate() {
            command.copy_buffer_to_image_level(
                staging.handle(),
                level.offset as u64,
                texture.handle(),
                texture.aspect(),
                Extent2D {
                    width: level.width,
                    height: level.height,
                },
                u32::try_from(index).expect("a mip chain is far shorter than u32::MAX"),
            );
        }
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
fn vulkan_format(format: slop_asset::Format) -> Format {
    match format {
        slop_asset::Format::Rgba8 => Format::Rgba8Unorm,
        slop_asset::Format::Bc7 => Format::Bc7Unorm,
    }
}

/// Cooked assets, found by walking up from wherever this was run.
///
/// Not `CARGO_MANIFEST_DIR`: that is baked in at compile time and points into a
/// source tree, so it is correct only for a binary run from the build that
/// produced it. Discovery works the same in a source tree and beside a shipped
/// binary — see [`Vfs::discover`].
fn assets() -> Vfs {
    let here = std::env::current_dir().expect("the current directory must be readable");

    Vfs::discover(&here).unwrap_or_else(|failure| panic!("{failure}"))
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
    assets()
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
