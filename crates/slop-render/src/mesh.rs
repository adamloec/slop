//! Drawing a cooked model — many meshes, each with its own material.
//!
//! What `examples/cube`'s hand-written scene is *not*. That draws one mesh with
//! one hardcoded texture and a pipeline built around it, which was the right
//! shape for proving the stack works end to end and the wrong one for a level.
//! This loads whatever a [`Model`] names and draws all of it through one
//! pipeline.
//!
//! ```ignore
//! let mut meshes = MeshRenderer::new(&device, &mut heap, &module, &reflection, formats)?;
//! meshes.load(&allocator, &mut heap, &vfs, "models/sponza.model")?;
//!
//! renderer.render(|frame| meshes.record(frame, view_projection))?;
//! ```
//!
//! # One pipeline, however many materials
//!
//! A pipeline per material is the shape an engine grows by accident, and it
//! costs a bind and a barrier per surface. Here every material is a row in a
//! storage buffer and every texture a slot in the bindless heap, so a draw
//! differs from its neighbour only by two integers in its push constants
//! (`docs/DESIGN.md` §2.2). That is also what makes §4.2 stage B possible later:
//! a GPU-built draw list has nothing per-draw to bind.
//!
//! # What is loaded, and once
//!
//! A model places the same mesh many times — a pillar, a floor tile — so meshes,
//! materials and textures are each uploaded once and referenced by index. The
//! instance list is what repeats.

use std::sync::Arc;

use slop_asset::{AlphaMode, Material, Mesh, Model, Reflection, TextureSlot, Vfs};
use slop_core::FxHashMap;
use slop_core::diagnostics::tracing::warn;
use slop_math::Mat4;
use slop_rhi::{
    Allocator, Attachments, BindlessHeap, Blend, Buffer, BufferConfig, BufferState, ClearValue,
    ColorAttachment, DepthAttachment, Device, GraphicsPipeline, GraphicsPipelineConfig, Image,
    ImageConfig, ImageState, Load, MemoryLocation, PipelineLayout, PipelineLayoutConfig,
    SampledImage, Sampler, SamplerConfig, ShaderModule, ShaderStage, StorageBuffer, TextureSampler,
    vk,
};

use crate::{RenderError, VertexBinding};

/// What a material's texture index holds when it has no texture.
///
/// Not zero: zero is a perfectly good heap slot, and a material with no albedo
/// would sample whichever texture happened to land there. Matches `NO_TEXTURE`
/// in `shaders/passes/model.slang`.
const NO_TEXTURE: u32 = u32::MAX;

/// One material as the shader reads it.
///
/// Mirrors `MaterialGpu` in `shaders/passes/model.slang`. `#[repr(C)]` is
/// load-bearing for the same reason a vertex struct needs it, and the field
/// order is chosen so std430 and Rust agree without padding on either side: the
/// `[f32; 4]` needs sixteen-byte alignment and comes first, and the eight
/// scalars after it fill two more sixteen-byte rows exactly.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MaterialGpu {
    base_color: [f32; 4],
    metallic: f32,
    roughness: f32,
    alpha_cutoff: f32,
    flags: u32,
    base_color_texture: u32,
    normal_texture: u32,
    metallic_roughness_texture: u32,
    sampler: u32,
}

/// One uploaded mesh.
struct GpuMesh {
    vertices: Buffer,
    indices: Buffer,
    index_count: u32,
    /// Row in the material buffer this mesh is drawn with.
    material: u32,
}

/// One placement of one mesh.
struct Placement {
    mesh: usize,
    transform: Mat4,
}

/// Per-draw constants, matching `PushConstants` in `model.slang`.
///
/// The trailing pad is not decoration. `Mat4` forces sixteen-byte alignment, so
/// without it `size_of` rounds up past the declared fields and the last eight
/// bytes are uninitialised padding — which `bytemuck::bytes_of` would then read.
/// `#[derive(Pod)]` refuses to compile a struct in that state, which is how the
/// gap becomes visible.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PushConstants {
    model_view_projection: Mat4,
    normal_rows: [[f32; 4]; 3],
    materials: u32,
    material: u32,
    _pad: [u32; 2],
}

/// Loads a cooked model and draws it.
pub struct MeshRenderer {
    pipeline: GraphicsPipeline,
    meshes: Vec<GpuMesh>,
    placements: Vec<Placement>,
    /// Held so the heap's descriptors stay valid. Never read after upload —
    /// the heap holds the view, not a reference the borrow checker can see.
    images: Vec<Image>,
    materials: Option<Buffer>,
    materials_slot: Option<slop_core::Handle<StorageBuffer>>,
    texture_slots: Vec<slop_core::Handle<SampledImage>>,
    /// Destroyed when this drops, which is `TextureSampler`'s job rather than a
    /// hand-written `Drop` here.
    /// Held so the heap's descriptor stays valid; destroyed on drop.
    #[expect(dead_code, reason = "the heap references this sampler")]
    sampler: TextureSampler,
    sampler_slot: slop_core::Handle<Sampler>,
    /// Owned rather than borrowed: its format is what the pipeline was built
    /// against, so nothing else can supply one that agrees by accident.
    depth: Option<Image>,
    depth_format: vk::Format,
    push_constant_bytes: u32,
    device: Arc<Device>,
}

impl MeshRenderer {
    /// Build the pipeline a model is drawn with.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if a GPU object cannot be created, or
    /// [`RenderError::VertexLocationGap`] if the shader's inputs are not
    /// contiguous.
    pub fn new(
        device: &Arc<Device>,
        heap: &mut BindlessHeap,
        module: &ShaderModule,
        reflection: &Reflection,
        color_format: vk::Format,
        depth_format: vk::Format,
    ) -> Result<Self, RenderError> {
        let vertices = VertexBinding::interleaved(reflection)?;
        let push_constant_bytes = reflection.push_constant_bytes;

        if push_constant_bytes as usize > size_of::<PushConstants>() {
            return Err(RenderError::Layout {
                what: "the model shader's push constant block is larger than the renderer writes",
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
                depth_format: Some(depth_format),
                vertex_layout: Some(vertices.layout()),
                // Off, for now, and this is a real limitation rather than an
                // oversight: `double_sided` is per material and culling is per
                // pipeline, so honouring it needs two pipelines and a sort.
                // Culling everything would erase Sponza's foliage, which is
                // single-sided geometry meant to be seen from behind.
                // `docs/PLAN.md` §6.1 records the pair.
                cull_back_faces: false,
                blend: Blend::Opaque,
            },
        )?;

        // Anisotropic and repeating — what a surface texture wants, and the
        // difference between a floor that reads sharp at a grazing angle and one
        // that smears.
        let sampler = TextureSampler::new(
            device,
            &SamplerConfig {
                anisotropy: Some(16.0),
                ..SamplerConfig::default()
            },
        )?;
        let sampler_slot = heap
            .insert_sampler(sampler.handle())
            .ok_or(RenderError::Layout {
                what: "the bindless heap had no room for the model sampler",
            })?;

        Ok(Self {
            pipeline,
            meshes: Vec::new(),
            placements: Vec::new(),
            images: Vec::new(),
            materials: None,
            materials_slot: None,
            texture_slots: Vec::new(),
            sampler,
            sampler_slot,
            depth: None,
            depth_format,
            push_constant_bytes,
            device: Arc::clone(device),
        })
    }

    /// How many draws recording a frame will issue.
    pub fn draw_count(&self) -> usize {
        self.placements.len()
    }

    /// How many distinct meshes are uploaded.
    pub fn mesh_count(&self) -> usize {
        self.meshes.len()
    }

    /// Load everything a cooked model names and upload it.
    ///
    /// Each distinct mesh, material and texture is uploaded once however many
    /// times the model places it.
    ///
    /// # Errors
    ///
    /// [`RenderError::Asset`] if anything the model names cannot be read or
    /// decoded, [`RenderError::Rhi`] if an upload fails.
    pub fn load(
        &mut self,
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        vfs: &Vfs,
        logical: &str,
    ) -> Result<(), RenderError> {
        let model: Model = read_asset(vfs, logical)?;

        // Meshes first, in first-seen order, so the index a placement stores is
        // stable and matches the order the model lists them.
        let mut index_of = FxHashMap::default();
        let mut materials = Vec::new();
        let mut material_of: FxHashMap<String, u32> = FxHashMap::default();

        for name in model.meshes() {
            let mesh: Mesh = read_asset(vfs, name)?;

            let material = match &mesh.material {
                None => self.material_row(&mut materials, &Material::default()),
                Some(path) => match material_of.get(path) {
                    Some(row) => *row,
                    None => {
                        let material: Material = read_asset(vfs, path)?;
                        let resolved =
                            self.resolve_textures(allocator, heap, vfs, &material, path)?;
                        let row = self.material_row(&mut materials, &material);

                        materials[row as usize] = resolved(materials[row as usize]);
                        material_of.insert(path.clone(), row);

                        row
                    }
                },
            };

            index_of.insert(name.to_owned(), self.meshes.len());
            self.meshes
                .push(upload_mesh(&self.device, allocator, &mesh, material)?);
        }

        for instance in &model.instances {
            let Some(mesh) = index_of.get(&instance.mesh) else {
                continue;
            };

            self.placements.push(Placement {
                mesh: *mesh,
                transform: Mat4::from_cols_array(&instance.transform),
            });
        }

        self.upload_materials(allocator, heap, &materials)?;

        Ok(())
    }

    /// Add a material's factors to the buffer, returning its row.
    fn material_row(&self, rows: &mut Vec<MaterialGpu>, material: &Material) -> u32 {
        let row = rows.len() as u32;

        rows.push(MaterialGpu {
            base_color: material.base_color,
            metallic: material.metallic,
            roughness: material.roughness,
            alpha_cutoff: material.alpha_cutoff,
            flags: match material.alpha_mode {
                AlphaMode::Opaque => 0,
                AlphaMode::Mask => 1,
                AlphaMode::Blend => 2,
            },
            base_color_texture: NO_TEXTURE,
            normal_texture: NO_TEXTURE,
            metallic_roughness_texture: NO_TEXTURE,
            sampler: self.sampler_slot.index(),
        });

        row
    }

    /// Upload a material's textures and return a patch setting their slots.
    ///
    /// Returned as a closure rather than applied directly because the row does
    /// not exist yet — the caller creates it and then applies this, which keeps
    /// "upload the textures" and "record where they landed" from having to share
    /// a mutable borrow of the row vector.
    fn resolve_textures(
        &mut self,
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        vfs: &Vfs,
        material: &Material,
        name: &str,
    ) -> Result<impl Fn(MaterialGpu) -> MaterialGpu + use<>, RenderError> {
        let mut slots = [NO_TEXTURE; 3];

        for (index, slot) in [
            TextureSlot::BaseColor,
            TextureSlot::Normal,
            TextureSlot::MetallicRoughness,
        ]
        .into_iter()
        .enumerate()
        {
            let Some(path) = material.texture(slot) else {
                continue;
            };

            // A missing texture is logged and skipped rather than fatal: one
            // absent file should not stop a level loading, and the material's
            // factors are a defined fallback.
            let texture = match read_asset(vfs, path) {
                Ok(texture) => texture,
                Err(error) => {
                    warn!(material = name, texture = path, %error, "skipping a texture");
                    continue;
                }
            };

            let image = upload_texture(&self.device, allocator, &texture)?;
            let Some(handle) =
                heap.insert_sampled_image(image.view(), vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            else {
                warn!(material = name, "the bindless heap is full");
                continue;
            };

            slots[index] = handle.index();
            self.texture_slots.push(handle);
            self.images.push(image);
        }

        Ok(move |row: MaterialGpu| MaterialGpu {
            base_color_texture: slots[0],
            normal_texture: slots[1],
            metallic_roughness_texture: slots[2],
            ..row
        })
    }

    /// Put the material rows in a storage buffer the shader can index.
    fn upload_materials(
        &mut self,
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
        rows: &[MaterialGpu],
    ) -> Result<(), RenderError> {
        if rows.is_empty() {
            return Ok(());
        }

        let bytes: &[u8] = bytemuck::cast_slice(rows);
        let mut buffer = Buffer::new(
            allocator,
            &BufferConfig {
                name: "model materials",
                size: bytes.len() as u64,
                usage: vk::BufferUsageFlags::STORAGE_BUFFER,
                // Host-visible rather than staged: this is written once at load
                // and read every frame, and a scene's materials are kilobytes.
                // Staging would cost a copy to save reads that are already
                // cached after the first frame.
                location: MemoryLocation::Upload,
            },
        )?;

        buffer.mapped_mut()?[..bytes.len()].copy_from_slice(bytes);

        let slot = heap
            .insert_storage_buffer(buffer.handle())
            .ok_or(RenderError::Layout {
                what: "the bindless heap had no room for the material buffer",
            })?;

        self.materials_slot = Some(slot);
        self.materials = Some(buffer);

        Ok(())
    }

    /// Rebuild the depth buffer for a new target size.
    ///
    /// Must be called before the first frame and after every resize: a depth
    /// attachment whose size differs from the colour target is a validation
    /// error on the first frame after a resize.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if the image cannot be allocated.
    pub fn resize(
        &mut self,
        allocator: &Arc<Allocator>,
        extent: vk::Extent2D,
    ) -> Result<(), RenderError> {
        self.depth = Some(Image::new(
            allocator,
            &ImageConfig {
                name: "model depth",
                extent,
                format: self.depth_format,
                usage: vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT,
                // No chain: nothing samples a depth buffer at a distance.
                mip_levels: 1,
            },
        )?);

        Ok(())
    }

    /// Open a pass over the frame's target and record every draw.
    ///
    /// Owns the pass rather than joining the caller's, for the same reason
    /// `slop_editor::Overlay` does: the attachments and their formats are
    /// this renderer's business, and a caller assembling them would be
    /// duplicating what the pipeline already declares.
    ///
    /// Does nothing if no model is loaded or [`MeshRenderer::resize`] has not
    /// run.
    pub fn record(&self, heap: &BindlessHeap, frame: &crate::Frame<'_>, view_projection: Mat4) {
        let (Some(materials), Some(depth)) = (self.materials_slot, self.depth.as_ref()) else {
            return;
        };

        frame.command.transition_image(
            frame.target.image,
            vk::ImageAspectFlags::COLOR,
            frame.target.from,
            ImageState::COLOR_ATTACHMENT,
        );
        frame.command.transition_image(
            depth.handle(),
            depth.aspect(),
            // From UNDEFINED every frame: the depth buffer is cleared, so its
            // previous contents are worth nothing and discarding is faster.
            ImageState::UNDEFINED,
            ImageState::DEPTH_ATTACHMENT,
        );

        let mut pass = frame.command.begin_rendering(&Attachments {
            color: ColorAttachment {
                view: frame.target.view,
                load: Load::Clear(ClearValue::Color([0.02, 0.02, 0.03, 1.0])),
            },
            depth: Some(DepthAttachment {
                view: depth.view(),
                load: Load::Clear(ClearValue::Depth(slop_rhi::DEPTH_CLEAR)),
                // Scratch for this pass only, so storing it would cost
                // bandwidth for something nothing reads.
                store: false,
            }),
            extent: frame.target.extent,
        });

        pass.bind_pipeline(&self.pipeline);
        pass.bind_heap(heap);

        for placement in &self.placements {
            let mesh = &self.meshes[placement.mesh];

            let push = PushConstants {
                model_view_projection: view_projection * placement.transform,
                normal_rows: normal_rows(placement.transform),
                materials: materials.index(),
                material: mesh.material,
                _pad: [0; 2],
            };

            pass.push_constants(&bytemuck::bytes_of(&push)[..self.push_constant_bytes as usize]);
            pass.bind_vertex_buffer(&mesh.vertices);
            pass.bind_index_buffer(&mesh.indices);
            pass.draw_indexed(mesh.index_count, 1, 0, 0);
        }

        // Ends the pass, so the transition below is outside it.
        drop(pass);

        // **Left in `COLOR_ATTACHMENT`, deliberately not in `frame.target.to`.**
        //
        // This used to transition to the frame's final state here, which is
        // correct only when nothing draws afterwards. The moment a debug overlay
        // was added, its pass began on an image already in `PRESENT_SRC` and
        // validation objected on every frame — two renderers in one frame, both
        // believing they were last.
        //
        // Only the last writer may perform the final transition, and a mesh
        // renderer cannot know whether it is the last. So it leaves the image
        // ready for the next pass, and the caller ends the frame with
        // [`Frame::finish`]. The render graph (`docs/PLAN.md` §9.2 item E) is
        // what will derive this rather than leaving it to a convention.
    }
}

impl std::fmt::Debug for MeshRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshRenderer")
            .field("meshes", &self.meshes.len())
            .field("draws", &self.placements.len())
            .field("textures", &self.texture_slots.len())
            .finish()
    }
}

/// The rows of `transpose(inverse(mat3(model)))`, padded to `float4`.
///
/// The model matrix would do for a rigid transform and is wrong under
/// non-uniform scale, where it tilts every normal — which reads as a lighting
/// bug rather than a transform one. glTF scenes scale non-uniformly often
/// enough that this is not a theoretical case.
fn normal_rows(model: Mat4) -> [[f32; 4]; 3] {
    let normal = Mat4::from_mat3(slop_math::Mat3::from_mat4(model).inverse().transpose());
    let rows = normal.transpose();

    [
        rows.x_axis.to_array(),
        rows.y_axis.to_array(),
        rows.z_axis.to_array(),
    ]
}

/// Read and decode one cooked asset.
fn read_asset<T: slop_asset::Asset>(vfs: &Vfs, logical: &str) -> Result<T, RenderError> {
    let bytes = vfs.read(logical).map_err(|source| RenderError::Asset {
        logical: logical.to_owned(),
        source: Box::new(source),
    })?;

    T::decode(&bytes).map_err(|source| RenderError::Asset {
        logical: logical.to_owned(),
        source,
    })
}

/// Upload one mesh's vertex and index buffers.
fn upload_mesh(
    device: &Arc<Device>,
    allocator: &Arc<Allocator>,
    mesh: &Mesh,
    material: u32,
) -> Result<GpuMesh, RenderError> {
    let vertices = upload_buffer(
        device,
        allocator,
        "model vertices",
        bytemuck::cast_slice(&mesh.vertices),
        vk::BufferUsageFlags::VERTEX_BUFFER,
        BufferState::VERTEX_INPUT,
    )?;
    let indices = upload_buffer(
        device,
        allocator,
        "model indices",
        bytemuck::cast_slice(&mesh.indices),
        vk::BufferUsageFlags::INDEX_BUFFER,
        BufferState::INDEX_INPUT,
    )?;

    Ok(GpuMesh {
        vertices,
        indices,
        index_count: mesh.indices.len() as u32,
        material,
    })
}

/// Stage bytes into a device-local buffer and wait for the copy.
fn upload_buffer(
    device: &Arc<Device>,
    allocator: &Arc<Allocator>,
    name: &str,
    bytes: &[u8],
    usage: vk::BufferUsageFlags,
    state: BufferState,
) -> Result<Buffer, RenderError> {
    let mut staging = Buffer::new(
        allocator,
        &BufferConfig {
            name: "model staging",
            size: bytes.len() as u64,
            usage: vk::BufferUsageFlags::TRANSFER_SRC,
            location: MemoryLocation::Upload,
        },
    )?;

    staging.mapped_mut()?[..bytes.len()].copy_from_slice(bytes);

    let buffer = Buffer::new(
        allocator,
        &BufferConfig {
            name,
            size: bytes.len() as u64,
            usage: usage | vk::BufferUsageFlags::TRANSFER_DST,
            location: MemoryLocation::DeviceOnly,
        },
    )?;

    slop_rhi::submit_and_wait(device, |command| {
        command.copy_buffer(staging.handle(), buffer.handle(), bytes.len() as u64);
        command.barrier_buffer(buffer.handle(), BufferState::TRANSFER_DST, state);
    })?;

    Ok(buffer)
}

/// Upload a cooked texture into a sampled image.
fn upload_texture(
    device: &Arc<Device>,
    allocator: &Arc<Allocator>,
    texture: &slop_asset::Texture,
) -> Result<Image, RenderError> {
    let mut staging = Buffer::new(
        allocator,
        &BufferConfig {
            name: "model texture staging",
            size: texture.pixels.len() as u64,
            usage: vk::BufferUsageFlags::TRANSFER_SRC,
            location: MemoryLocation::Upload,
        },
    )?;

    staging.mapped_mut()?[..texture.pixels.len()].copy_from_slice(&texture.pixels);

    let image = Image::new(
        allocator,
        &ImageConfig {
            name: "model texture",
            extent: vk::Extent2D {
                width: texture.width,
                height: texture.height,
            },
            format: vulkan_format(texture.format),
            usage: vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            mip_levels: texture.mip_levels,
        },
    )?;

    slop_rhi::submit_and_wait(device, |command| {
        // Covers every level: `transition_image` names the whole chain, which
        // is what leaves no level behind in UNDEFINED.
        command.transition_image(
            image.handle(),
            image.aspect(),
            ImageState::UNDEFINED,
            ImageState::TRANSFER_DST,
        );

        // One copy per level, all out of the same staging buffer.
        for (index, level) in texture.levels().enumerate() {
            command.copy_buffer_to_image_level(
                staging.handle(),
                level.offset as u64,
                image.handle(),
                image.aspect(),
                vk::Extent2D {
                    width: level.width,
                    height: level.height,
                },
                u32::try_from(index).expect("a mip chain is far shorter than u32::MAX"),
            );
        }
        command.transition_image(
            image.handle(),
            image.aspect(),
            ImageState::TRANSFER_DST,
            ImageState::SHADER_READ,
        );
    })?;

    Ok(image)
}

/// The Vulkan format a cooked texture's bytes are in.
///
/// UNORM rather than the `_SRGB` variants, matching what the cube does and for
/// the same reason: the shader reads the bytes that were uploaded. Applying the
/// sRGB transfer where a material says to is `TextureSlot::is_srgb`'s job and
/// arrives with real shading at M3 — `docs/PLAN.md` §6.1 records it.
fn vulkan_format(format: slop_asset::Format) -> vk::Format {
    match format {
        slop_asset::Format::Rgba8 => vk::Format::R8G8B8A8_UNORM,
        slop_asset::Format::Bc7 => vk::Format::BC7_UNORM_BLOCK,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_material_row_matches_what_the_shader_reads() {
        // std430 and `#[repr(C)]` agreeing is what makes the storage buffer
        // readable at all, and a mismatch shifts every field after the first
        // rather than failing.
        assert_eq!(size_of::<MaterialGpu>(), 48);
        assert_eq!(align_of::<MaterialGpu>(), 4);
    }

    #[test]
    fn the_push_block_fits_what_vulkan_guarantees() {
        // 128 bytes is the floor across desktop hardware, and §2.1 buys one
        // feature tier rather than branching on the device's actual limit. This
        // is why materials live in a buffer.
        assert!(size_of::<PushConstants>() <= 128);
    }

    #[test]
    fn a_rigid_transform_leaves_normals_alone() {
        let rotated = Mat4::from_rotation_y(0.7);
        let rows = normal_rows(rotated);

        // For a rotation the normal matrix *is* the rotation, so recomposing the
        // rows must give the original basis back.
        for (index, row) in rows.iter().enumerate() {
            let expected = rotated.row(index);

            for axis in 0..3 {
                assert!(
                    (row[axis] - expected[axis]).abs() < 1e-5,
                    "row {index} axis {axis}: {} vs {}",
                    row[axis],
                    expected[axis]
                );
            }
        }
    }

    #[test]
    fn a_non_uniform_scale_inverts_rather_than_copying_the_model_matrix() {
        // The case the model matrix gets wrong. Scaling X by four must *divide*
        // the normal's X by four, not multiply it — using the model matrix tilts
        // every normal on a stretched surface and reads as a lighting bug.
        let stretched = Mat4::from_scale(slop_math::Vec3::new(4.0, 1.0, 1.0));
        let rows = normal_rows(stretched);

        assert!(
            (rows[0][0] - 0.25).abs() < 1e-5,
            "expected 0.25, got {}",
            rows[0][0]
        );
        assert!((rows[1][1] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn a_material_with_no_textures_says_so_rather_than_pointing_at_slot_zero() {
        // Zero is a real slot. A material defaulting to it would sample whatever
        // texture happened to load first, which looks like a content mistake.
        assert_eq!(NO_TEXTURE, u32::MAX);
    }
}
