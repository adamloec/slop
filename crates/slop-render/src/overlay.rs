//! Drawing what an immediate-mode UI tessellated — `docs/DESIGN.md` §10.2.
//!
//! This is the *renderer* half of the debug UI and nothing else. It takes the
//! triangles egui produced and puts them on screen; it does not own an egui
//! context, read input, or know what a window is. Those belong to the
//! application, which is where the platform already put them
//! (`docs/DESIGN.md` §1.2 principle 4).
//!
//! ```ignore
//! let output = context.run(raw_input, |ui| { /* declare the UI */ });
//! let primitives = context.tessellate(output.shapes, output.pixels_per_point);
//!
//! overlay.update_textures(&output.textures_delta)?;
//! overlay.draw(frame.command, frame.target, &primitives, output.pixels_per_point)?;
//! ```
//!
//! # Why write the backend rather than take one
//!
//! `egui-ash-renderer` exists and works. It also brings its own descriptor pool,
//! its own sampler, and its own pipeline management — all of which this engine
//! already has, in a bindless form (`docs/DESIGN.md` §2.2) that a general-purpose
//! backend cannot assume. Taking it would mean two descriptor models in one
//! frame and a second answer to "where do textures live".
//!
//! What is actually being written here is an upload, a pipeline, and a draw loop
//! that sets a scissor rectangle. `docs/DESIGN.md` §3's write/take line puts a
//! *font rasterizer and layout engine* firmly on the take side — which is why
//! egui itself is a dependency — and puts three hundred lines of Vulkan that has
//! to match our descriptor model on the write side.
//!
//! # The vertex format reflection cannot see
//!
//! egui's vertex is `[f32; 2]` position, `[f32; 2]` UV, and **four normalized
//! bytes** of colour, which the shader reads as a `float4`. Reflection reports
//! what the *shader* reads, so it says `Float32x4` — the buffer format is a
//! separate decision. See [`crate::vertex`] on why that split is real rather
//! than a gap, and why this module states its layout explicitly.

use std::sync::Arc;

use slop_asset::Reflection;
use slop_rhi::{
    Blend, Buffer, BufferConfig, Device, GraphicsPipeline, GraphicsPipelineConfig, Image,
    ImageConfig, ImageState, MemoryLocation, PipelineLayout, PipelineLayoutConfig, SampledImage,
    Sampler, ShaderModule, ShaderStage, VertexLayout, vk,
};

use crate::RenderError;

/// Bytes one egui vertex occupies: two floats, two floats, four bytes.
const VERTEX_STRIDE: u32 = 20;

/// The buffer-side attribute formats, in shader location order.
///
/// Stated rather than derived, and the third entry is why: the colour arrives as
/// `R8G8B8A8_UNORM` and is read as a `float4`. Reflection describes the shader
/// side and cannot see that. The two are checked against each other in
/// [`Overlay::new`].
const ATTRIBUTES: [(vk::Format, u32); 3] = [
    (vk::Format::R32G32_SFLOAT, 0),
    (vk::Format::R32G32_SFLOAT, 8),
    (vk::Format::R8G8B8A8_UNORM, 16),
];

/// Per-draw constants, matching `PushConstants` in `shaders/passes/overlay.slang`.
///
/// Its size is checked against the shader's reflected block in [`Overlay::new`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PushConstants {
    screen_size: [f32; 2],
    texture: u32,
    sampler: u32,
}

/// One texture egui asked to be kept.
struct Managed {
    #[expect(dead_code, reason = "held so the heap's descriptor stays valid")]
    image: Image,
    slot: slop_core::Handle<SampledImage>,
    width: u32,
    height: u32,
    /// Kept so a partial update can patch and re-upload. See `set_texture`.
    pixels: Vec<u8>,
}

/// Draws tessellated immediate-mode UI over a target.
///
/// Owns its pipeline, its sampler, the textures egui manages, and the per-frame
/// vertex and index buffers.
pub struct Overlay {
    // Declared in drop order: things built from the device, then the device.
    pipeline: GraphicsPipeline,
    textures: slop_core::FxHashMap<u64, Managed>,
    sampler: vk::Sampler,
    sampler_slot: slop_core::Handle<Sampler>,
    /// One set of dynamic buffers per in-flight slot. See `Frame::slot`.
    meshes: Vec<MeshBuffers>,
    push_constant_bytes: u32,
    device: Arc<Device>,
}

impl Overlay {
    /// Build the overlay against an existing bindless heap.
    ///
    /// The heap is borrowed rather than owned: the overlay's textures live in
    /// the same table as everything else's, which is the point of a bindless
    /// model. It inserts its font atlas and sampler there and holds the handles.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if a GPU object cannot be created,
    /// [`RenderError::OverlayLayout`] if the cooked shader does not describe the
    /// vertex format this module writes.
    pub fn new(
        device: &Arc<Device>,
        heap: &mut slop_rhi::BindlessHeap,
        module: &ShaderModule,
        reflection: &Reflection,
        color_format: vk::Format,
    ) -> Result<Self, RenderError> {
        check_layout(reflection)?;

        let push_constant_bytes = reflection.push_constant_bytes;
        if push_constant_bytes as usize > size_of::<PushConstants>() {
            return Err(RenderError::OverlayLayout {
                what: "the shader's push constant block is larger than the overlay writes",
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
                fragment: ShaderStage {
                    module,
                    entry: c"fragmentMain",
                },
                color_format,
                // No depth. The overlay draws last, over everything, and a depth
                // test would let the scene occlude the interface reading it.
                depth_format: None,
                vertex_layout: Some(VertexLayout {
                    stride: VERTEX_STRIDE,
                    attributes: &ATTRIBUTES,
                }),
                // Off. egui emits both windings and culling would drop half of
                // every glyph.
                cull_back_faces: false,
                blend: Blend::PremultipliedAlpha,
            },
        )?;

        let sampler = create_sampler(device)?;
        let sampler_slot = heap
            .insert_sampler(sampler)
            .ok_or(RenderError::OverlayLayout {
                what: "the bindless heap had no room for the overlay's sampler",
            })?;

        Ok(Self {
            pipeline,
            textures: slop_core::FxHashMap::default(),
            sampler,
            sampler_slot,
            meshes: Vec::new(),
            push_constant_bytes,
            device: Arc::clone(device),
        })
    }

    /// Apply the texture changes egui asked for.
    ///
    /// Call before [`Overlay::draw`] and before the frame that uses them: this
    /// waits for its uploads, so it must not run inside a recorded frame.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if an image cannot be created or uploaded.
    pub fn update_textures(
        &mut self,
        heap: &mut slop_rhi::BindlessHeap,
        allocator: &Arc<slop_rhi::Allocator>,
        delta: &egui::TexturesDelta,
    ) -> Result<(), RenderError> {
        for (id, image) in &delta.set {
            self.set_texture(heap, allocator, *id, image)?;
        }

        // After the sets, and after this frame's draw would have used them —
        // egui only frees an id it will not reference again.
        for id in &delta.free {
            if let Some(managed) = self.textures.remove(&key(*id)) {
                heap.remove_sampled_image(managed.slot);
            }
        }

        Ok(())
    }

    /// Create or replace one texture.
    ///
    /// A partial update — egui sends one when a glyph is added to an existing
    /// atlas — is applied by reading the current image back, patching it, and
    /// re-uploading. That is wasteful and correct; a `vkCmdCopyBufferToImage`
    /// into a sub-region would avoid the readback, and `docs/PLAN.md` §6.1
    /// records it. Font atlases settle within a few frames of startup, so this
    /// runs a handful of times and then never again.
    fn set_texture(
        &mut self,
        heap: &mut slop_rhi::BindlessHeap,
        allocator: &Arc<slop_rhi::Allocator>,
        id: egui::TextureId,
        delta: &egui::epaint::ImageDelta,
    ) -> Result<(), RenderError> {
        let egui::epaint::ImageData::Color(source) = &delta.image;

        let [patch_width, patch_height] = source.size;
        let patch: Vec<u8> = source
            .pixels
            .iter()
            .flat_map(|pixel| pixel.to_array())
            .collect();

        let (width, height, pixels) = match delta.pos {
            None => (patch_width as u32, patch_height as u32, patch),
            Some([x, y]) => {
                let existing = self
                    .textures
                    .get(&key(id))
                    .ok_or(RenderError::OverlayLayout {
                        what: "a patch arrived for a texture that was never set",
                    })?;

                let (width, height) = (existing.width, existing.height);
                let mut whole = existing.pixels.clone();

                for row in 0..patch_height {
                    let from = row * patch_width * 4;
                    let to = ((y + row) * width as usize + x) * 4;

                    whole[to..to + patch_width * 4]
                        .copy_from_slice(&patch[from..from + patch_width * 4]);
                }

                (width, height, whole)
            }
        };

        let image = upload_image(&self.device, allocator, width, height, &pixels)?;
        let slot = heap
            .insert_sampled_image(image.view(), vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .ok_or(RenderError::OverlayLayout {
                what: "the bindless heap had no room for an overlay texture",
            })?;

        // Replacing frees the old slot, or a session of font changes exhausts
        // the heap one glyph update at a time.
        if let Some(previous) = self.textures.insert(
            key(id),
            Managed {
                image,
                slot,
                width,
                height,
                pixels,
            },
        ) {
            heap.remove_sampled_image(previous.slot);
        }

        Ok(())
    }

    /// Record the overlay's draws into `command`.
    ///
    /// **Opens its own render pass**, so it must be called after the caller's
    /// has ended. Two reasons it cannot share one: a pipeline inside a pass has
    /// to declare the same depth format the pass carries, and the overlay wants
    /// no depth at all — sharing would depth-test the interface against the
    /// scene and let geometry occlude the readout describing it. The pass loads
    /// rather than clears, so the scene underneath survives.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if this frame's buffers cannot be allocated.
    pub fn draw(
        &mut self,
        heap: &slop_rhi::BindlessHeap,
        allocator: &Arc<slop_rhi::Allocator>,
        frame: &crate::Frame<'_>,
        primitives: &[egui::ClippedPrimitive],
        pixels_per_point: f32,
    ) -> Result<(), RenderError> {
        let command = frame.command;
        // One set of buffers per in-flight slot. Writing a single shared buffer
        // would corrupt the frame still reading it — see `Frame::slot`.
        if self.meshes.len() < frame.slots {
            self.meshes.resize_with(frame.slots, MeshBuffers::default);
        }

        let mut vertices: Vec<u8> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut draws = Vec::new();

        for primitive in primitives {
            let egui::epaint::Primitive::Mesh(mesh) = &primitive.primitive else {
                // A paint callback asks the application to record its own draws
                // mid-UI. Nothing uses it, and silently skipping is the right
                // behaviour until something does.
                continue;
            };

            if mesh.indices.is_empty() {
                continue;
            }

            let Some(texture) = self.textures.get(&key(mesh.texture_id)) else {
                continue;
            };

            let base_vertex = vertices.len() as u32 / VERTEX_STRIDE;
            let first_index = indices.len() as u32;

            for vertex in &mesh.vertices {
                vertices.extend_from_slice(&vertex.pos.x.to_ne_bytes());
                vertices.extend_from_slice(&vertex.pos.y.to_ne_bytes());
                vertices.extend_from_slice(&vertex.uv.x.to_ne_bytes());
                vertices.extend_from_slice(&vertex.uv.y.to_ne_bytes());
                vertices.extend_from_slice(&vertex.color.to_array());
            }

            indices.extend(mesh.indices.iter().copied());

            draws.push(Draw {
                first_index,
                index_count: mesh.indices.len() as u32,
                base_vertex,
                texture: texture.slot.index(),
                scissor: scissor(primitive.clip_rect, frame.target.extent, pixels_per_point),
            });
        }

        if draws.is_empty() {
            return Ok(());
        }

        let buffers = &mut self.meshes[frame.slot];
        buffers.write(allocator, &vertices, &indices)?;

        let raw = self.device.raw();
        let handle = command.handle();
        let (Some(vertex_buffer), Some(index_buffer)) = (&buffers.vertices, &buffers.indices)
        else {
            return Ok(());
        };

        let attachments = [vk::RenderingAttachmentInfo::default()
            .image_view(frame.target.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            // LOAD, never CLEAR: the scene is already in this attachment and the
            // overlay composites over it.
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)];

        let rendering = vk::RenderingInfo::default()
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: frame.target.extent,
            })
            .layer_count(1)
            .color_attachments(&attachments);

        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: frame.target.extent.width as f32,
            height: frame.target.extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];

        // SAFETY: the buffer is recording and outside a pass, every handle
        // belongs to this device, and each borrowed array outlives the call.
        unsafe {
            raw.cmd_begin_rendering(handle, &rendering);
            raw.cmd_set_viewport(handle, 0, &viewports);

            // Re-bound with *this* pipeline's layout. Two layouts are compatible
            // only if their push constant ranges match as well as their set
            // layouts, and the overlay's block is a different size from the
            // scene's — so the scene's binding does not carry over.
            heap.bind(
                handle,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.layout().handle(),
            );

            raw.cmd_bind_pipeline(
                handle,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.handle(),
            );
            raw.cmd_bind_vertex_buffers(handle, 0, &[vertex_buffer.handle()], &[0]);
            raw.cmd_bind_index_buffer(handle, index_buffer.handle(), 0, vk::IndexType::UINT32);

            for draw in &draws {
                let push = PushConstants {
                    // In *points*, not physical pixels. egui's vertex positions
                    // are logical, so mapping them to clip space has to divide
                    // by the logical size — while the scissor below is physical,
                    // because Vulkan's is. Dividing geometry by the physical size
                    // draws the interface at 1/scale while its clip rectangles
                    // stay full size, which shaves the left edge off every
                    // label and is invisible at 100% display scaling.
                    screen_size: [
                        frame.target.extent.width as f32 / pixels_per_point,
                        frame.target.extent.height as f32 / pixels_per_point,
                    ],
                    texture: draw.texture,
                    sampler: self.sampler_slot.index(),
                };

                raw.cmd_set_scissor(handle, 0, &[draw.scissor]);
                raw.cmd_push_constants(
                    handle,
                    self.pipeline.layout().handle(),
                    vk::ShaderStageFlags::ALL,
                    0,
                    &as_bytes(&push)[..self.push_constant_bytes as usize],
                );
                raw.cmd_draw_indexed(
                    handle,
                    draw.index_count,
                    1,
                    draw.first_index,
                    draw.base_vertex as i32,
                    0,
                );
            }

            raw.cmd_end_rendering(handle);
        }

        Ok(())
    }
}

/// One recorded draw's parameters.
struct Draw {
    first_index: u32,
    index_count: u32,
    base_vertex: u32,
    texture: u32,
    scissor: vk::Rect2D,
}

/// Per-slot vertex and index buffers, grown as needed.
#[derive(Default)]
struct MeshBuffers {
    vertices: Option<Buffer>,
    indices: Option<Buffer>,
}

impl MeshBuffers {
    /// Write this frame's geometry, reallocating only when it does not fit.
    ///
    /// Host-visible rather than staged through a device-local copy. UI geometry
    /// is small, changes every frame, and is read once — the copy would cost
    /// more than the slower reads do. A mesh that persists across frames wants
    /// the opposite trade, which is why the scene's uploads are staged.
    fn write(
        &mut self,
        allocator: &Arc<slop_rhi::Allocator>,
        vertices: &[u8],
        indices: &[u32],
    ) -> Result<(), RenderError> {
        let index_bytes: Vec<u8> = indices
            .iter()
            .flat_map(|index| index.to_ne_bytes())
            .collect();

        grow(
            &mut self.vertices,
            allocator,
            "overlay vertices",
            vertices.len(),
            vk::BufferUsageFlags::VERTEX_BUFFER,
        )?;
        grow(
            &mut self.indices,
            allocator,
            "overlay indices",
            index_bytes.len(),
            vk::BufferUsageFlags::INDEX_BUFFER,
        )?;

        if let Some(buffer) = &mut self.vertices {
            buffer.mapped_mut()?[..vertices.len()].copy_from_slice(vertices);
        }
        if let Some(buffer) = &mut self.indices {
            buffer.mapped_mut()?[..index_bytes.len()].copy_from_slice(&index_bytes);
        }

        Ok(())
    }
}

/// Allocate `slot` if it is missing or too small.
///
/// Grown to the next power of two rather than to exactly what is needed, so a
/// UI that gains one widget per frame does not reallocate every frame.
fn grow(
    slot: &mut Option<Buffer>,
    allocator: &Arc<slop_rhi::Allocator>,
    name: &str,
    needed: usize,
    usage: vk::BufferUsageFlags,
) -> Result<(), RenderError> {
    if slot
        .as_ref()
        .is_some_and(|buffer| buffer.size() >= needed as u64)
    {
        return Ok(());
    }

    let size = (needed as u64).next_power_of_two().max(4096);

    *slot = Some(Buffer::new(
        allocator,
        &BufferConfig {
            name,
            size,
            usage,
            location: MemoryLocation::Upload,
        },
    )?);

    Ok(())
}

/// egui's clip rectangle, in points, as a Vulkan scissor in pixels.
///
/// Clamped to the target: egui can emit a rectangle extending past the edge, and
/// a scissor outside the framebuffer is a validation error rather than a clamp.
fn scissor(clip: egui::Rect, extent: vk::Extent2D, pixels_per_point: f32) -> vk::Rect2D {
    let min_x = (clip.min.x * pixels_per_point).round().max(0.0) as u32;
    let min_y = (clip.min.y * pixels_per_point).round().max(0.0) as u32;
    let max_x = (clip.max.x * pixels_per_point).round().max(0.0) as u32;
    let max_y = (clip.max.y * pixels_per_point).round().max(0.0) as u32;

    let min_x = min_x.min(extent.width);
    let min_y = min_y.min(extent.height);

    vk::Rect2D {
        offset: vk::Offset2D {
            x: min_x as i32,
            y: min_y as i32,
        },
        extent: vk::Extent2D {
            width: max_x.min(extent.width) - min_x,
            height: max_y.min(extent.height) - min_y,
        },
    }
}

/// Upload RGBA8 pixels into a sampled image, waiting for the copy.
fn upload_image(
    device: &Arc<Device>,
    allocator: &Arc<slop_rhi::Allocator>,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<Image, RenderError> {
    let mut staging = Buffer::new(
        allocator,
        &BufferConfig {
            name: "overlay texture staging",
            size: pixels.len() as u64,
            usage: vk::BufferUsageFlags::TRANSFER_SRC,
            location: MemoryLocation::Upload,
        },
    )?;

    staging.mapped_mut()?[..pixels.len()].copy_from_slice(pixels);

    let image = Image::new(
        allocator,
        &ImageConfig {
            name: "overlay texture",
            extent: vk::Extent2D { width, height },
            // UNORM: egui's colours are already in the space its blending
            // expects, and an sRGB view would convert them a second time.
            format: vk::Format::R8G8B8A8_UNORM,
            usage: vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
        },
    )?;

    slop_rhi::submit_and_wait(device, |command| {
        command.transition_image(
            image.handle(),
            image.aspect(),
            ImageState::UNDEFINED,
            ImageState::TRANSFER_DST,
        );
        command.copy_buffer_to_image(
            staging.handle(),
            image.handle(),
            image.aspect(),
            vk::Extent2D { width, height },
        );
        command.transition_image(
            image.handle(),
            image.aspect(),
            ImageState::TRANSFER_DST,
            ImageState::SHADER_READ,
        );
    })?;

    Ok(image)
}

/// egui's texture id as a hashable key.
///
/// `TextureId` is `Managed(u64)` or `User(u64)` and the two spaces are distinct,
/// so the discriminant has to survive. The high bit does that without a second
/// map.
fn key(id: egui::TextureId) -> u64 {
    match id {
        egui::TextureId::Managed(index) => index,
        egui::TextureId::User(index) => index | (1 << 63),
    }
}

/// A `Copy` value's bytes, for a push constant block.
fn as_bytes<T: Copy>(value: &T) -> &[u8] {
    // SAFETY: `T` is `Copy`, the slice covers exactly the value, and it borrows
    // from `value` so it cannot outlive it. Reading padding bytes as `u8` is
    // defined; reading them as their own type would not be.
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(value).cast::<u8>(), size_of::<T>()) }
}

/// Refuse a shader whose inputs are not the ones this module writes.
///
/// Reflection cannot check the *buffer* formats — it describes the shader side —
/// but it can check that there are three inputs at locations 0, 1 and 2 reading
/// two, two and four components. A shader that gained an input, or reordered
/// them, fails here rather than reading the wrong bytes per vertex.
fn check_layout(reflection: &Reflection) -> Result<(), RenderError> {
    use slop_asset::shader::VertexFormat;

    let expected = [
        VertexFormat::Float32x2,
        VertexFormat::Float32x2,
        VertexFormat::Float32x4,
    ];

    if reflection.vertex_inputs.len() != expected.len() {
        return Err(RenderError::OverlayLayout {
            what: "the overlay shader does not read exactly three vertex inputs",
        });
    }

    for (index, input) in reflection.vertex_inputs.iter().enumerate() {
        if input.location != index as u32 || input.format != expected[index] {
            return Err(RenderError::OverlayLayout {
                what: "the overlay shader's vertex inputs are not position, uv and colour",
            });
        }
    }

    Ok(())
}

/// Nearest-neighbour is wrong here: egui's atlas is sampled at fractional
/// positions and a nearest filter makes text shimmer as a window moves.
fn create_sampler(device: &Arc<Device>) -> Result<vk::Sampler, RenderError> {
    let info = vk::SamplerCreateInfo::default()
        .mag_filter(vk::Filter::LINEAR)
        .min_filter(vk::Filter::LINEAR)
        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);

    // SAFETY: `info` is fully initialized and borrows nothing that outlives it.
    let sampler =
        unsafe { device.raw().create_sampler(&info, None) }.map_err(slop_rhi::RhiError::Vulkan)?;

    Ok(sampler)
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: the device is idle — `FrameRenderer::drop` and the
        // application's own teardown both wait — and the sampler was created
        // from this device.
        unsafe { self.device.raw().destroy_sampler(self.sampler, None) };
    }
}

impl std::fmt::Debug for Overlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Overlay")
            .field("textures", &self.textures.len())
            .field("push_constant_bytes", &self.push_constant_bytes)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_asset::shader::{VertexFormat, VertexInput};

    fn reflection(formats: &[(u32, VertexFormat)]) -> Reflection {
        Reflection {
            push_constant_bytes: 16,
            vertex_inputs: formats
                .iter()
                .map(|(location, format)| VertexInput {
                    location: *location,
                    format: *format,
                })
                .collect(),
        }
    }

    fn overlay_shader() -> Reflection {
        reflection(&[
            (0, VertexFormat::Float32x2),
            (1, VertexFormat::Float32x2),
            (2, VertexFormat::Float32x4),
        ])
    }

    #[test]
    fn the_real_overlay_shader_is_accepted() {
        assert!(check_layout(&overlay_shader()).is_ok());
    }

    #[test]
    fn the_attribute_table_covers_one_egui_vertex() {
        // 8 + 8 + 4 = 20, and the last attribute must end exactly at the stride.
        // A mismatch reads each vertex at the wrong offset — scrambled text
        // rather than an error.
        let (_, last_offset) = ATTRIBUTES[ATTRIBUTES.len() - 1];

        assert_eq!(last_offset + 4, VERTEX_STRIDE);
        assert_eq!(VERTEX_STRIDE, 20);
    }

    #[test]
    fn the_colour_is_four_bytes_in_the_buffer_and_four_floats_in_the_shader() {
        // The case reflection cannot see, asserted so that deriving the layout
        // from reflection later — which would produce R32G32B32A32_SFLOAT —
        // fails here first.
        assert_eq!(ATTRIBUTES[2].0, vk::Format::R8G8B8A8_UNORM);
        assert_eq!(
            overlay_shader().vertex_inputs[2].format,
            VertexFormat::Float32x4
        );
    }

    #[test]
    fn a_shader_with_an_extra_input_is_refused() {
        let extra = reflection(&[
            (0, VertexFormat::Float32x2),
            (1, VertexFormat::Float32x2),
            (2, VertexFormat::Float32x4),
            (3, VertexFormat::Float32x3),
        ]);

        assert!(check_layout(&extra).is_err());
    }

    #[test]
    fn a_shader_with_reordered_inputs_is_refused() {
        // Same count, same total size, wrong meaning — the failure that renders
        // an interface which is present but wrong.
        let swapped = reflection(&[
            (0, VertexFormat::Float32x4),
            (1, VertexFormat::Float32x2),
            (2, VertexFormat::Float32x2),
        ]);

        assert!(check_layout(&swapped).is_err());
    }
}
