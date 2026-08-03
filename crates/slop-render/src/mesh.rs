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
//! // Two passes, declared to the graph. Neither names a barrier.
//! graph.add(&RenderPass { name: "depth prepass", color: None, depth: .., .. },
//!           |pass| meshes.draw_depth(pass, &heap, view_projection));
//! graph.add(&RenderPass { name: "scene", color: .., depth: .., .. },
//!           |pass| meshes.draw(pass, &heap, view_projection));
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
    Allocator, BindlessHeap, Blend, Buffer, BufferConfig, BufferState, BufferUsage, CommandBuffer,
    CommandPool, Device, Extent2D, Format, GraphicsPipeline, GraphicsPipelineConfig, Image,
    ImageConfig, ImageState, ImageUsage, MemoryLocation, PipelineLayout, PipelineLayoutConfig,
    SampledImage, Sampler, SamplerConfig, ShaderModule, ShaderStage, StorageBuffer, TextureSampler,
};

use crate::{RenderError, VertexBinding, View};

/// What a material's texture index holds when it has no texture.
///
/// Not zero: zero is a perfectly good heap slot, and a material with no albedo
/// would sample whichever texture happened to land there. Matches `NO_TEXTURE`
/// in `shaders/passes/model.slang`.
const NO_TEXTURE: u32 = u32::MAX;

/// Which bits of a material's `flags` hold its alpha mode.
///
/// Matches `ALPHA_MODE_MASK_BITS` in `shaders/passes/model.slang`. Two bits, so
/// the remaining thirty are free for the flags real shading will want.
const ALPHA_MODE_BITS: u32 = 3;

/// The alpha mode meaning "cut the fragment away below the cutoff".
///
/// Matches `ALPHA_MODE_MASK` in the shader and `AlphaMode::Mask`'s discriminant
/// below. The prepass selects a pipeline on this, so a disagreement between the
/// two sides shows as geometry that vanishes rather than as an error.
const ALPHA_MODE_MASK: u32 = 1;

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
    /// Whether this mesh's material cuts fragments away with `discard`.
    ///
    /// Selects which prepass pipeline draws it, and the two are not
    /// interchangeable: masked geometry *must* run a fragment shader in the
    /// prepass, opaque geometry must not.
    masked: bool,
}

/// One placement of one mesh.
struct Placement {
    mesh: usize,
    transform: Mat4,
}

/// Where one placement sits, matching `InstanceGpu` in `model.slang`.
///
/// **The model matrix is four explicit columns, not a `Mat4`.** A matrix in a
/// structured buffer has a layout convention on each side, and the two
/// disagreeing transposes every transform in the scene — which reads as broken
/// geometry rather than as a layout bug, and is the kind of thing that costs an
/// afternoon. Four `[f32; 4]` have exactly one interpretation, and the shader
/// writes the multiply out rather than asking a matrix type to do it.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct InstanceGpu {
    model_columns: [[f32; 4]; 4],
    normal_rows: [[f32; 4]; 3],
}

/// Per-draw constants, matching `PushConstants` in `model.slang`.
///
/// The trailing pad is not decoration. `Mat4` forces sixteen-byte alignment, so
/// without it `size_of` rounds up past the declared fields and the last eight
/// bytes are uninitialised padding — which `bytemuck::bytes_of` would then read.
/// `#[derive(Pod)]` refuses to compile a struct in that state, which is how the
/// gap becomes visible.
///
/// This used to carry the model-view-projection and the normal matrix, at 120 of
/// the 128 bytes Vulkan guarantees, and being full is what kept the model matrix
/// out of the shader — see [`InstanceGpu`]. What is left is the camera and five
/// indices.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PushConstants {
    view_projection: Mat4,
    instances: u32,
    instance: u32,
    materials: u32,
    material: u32,
    grid: u32,
    environment: u32,
    shadows: u32,
    _pad: u32,
}

/// Loads a cooked model and draws it.
pub struct MeshRenderer {
    pipeline: GraphicsPipeline,
    /// Depth only, no fragment shader. What the prepass draws opaque geometry
    /// with — see [`draw_depth`](MeshRenderer::draw_depth).
    prepass_opaque: GraphicsPipeline,
    /// Depth only, with the cutout fragment shader. What masked geometry needs,
    /// because its `discard` is what decides the fragment exists.
    prepass_masked: GraphicsPipeline,
    meshes: Vec<GpuMesh>,
    placements: Vec<Placement>,
    /// Held so the heap's descriptors stay valid. Never read after upload —
    /// the heap holds the view, not a reference the borrow checker can see.
    images: Vec<Image>,
    materials: Option<Buffer>,
    materials_slot: Option<slop_core::Handle<StorageBuffer>>,
    /// One row per placement, written once at load.
    ///
    /// Not one buffer per frame in flight, and that is a statement about the
    /// content rather than a shortcut: a placement's transform is fixed by the
    /// cooked model and nothing moves. The day something does, this becomes a
    /// ring like `Lights`' — a change behind an unchanged seam, since the shader
    /// already reads it by index.
    instances: Option<Buffer>,
    instances_slot: Option<slop_core::Handle<StorageBuffer>>,
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
    depth_format: Format,
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
        color_format: Format,
        depth_format: Format,
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

        // Every pipeline below shares this. Written once rather than three
        // times, because the vertex stage and the vertex layout agreeing across
        // them is what makes the prepass's depth match the forward pass's:
        // different position arithmetic in the two would leave the forward pass
        // failing its own depth test and geometry vanishing.
        let shared = GraphicsPipelineConfig {
            vertex: ShaderStage {
                module,
                entry: c"vertexMain",
            },
            fragment: None,
            color_format: None,
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
        };

        let pipeline = GraphicsPipeline::new(
            device,
            &layout,
            &GraphicsPipelineConfig {
                fragment: Some(ShaderStage {
                    module,
                    entry: c"fragmentMain",
                }),
                color_format: Some(color_format),
                ..shared
            },
        )?;

        // No fragment stage at all: rasterization writes depth and nothing is
        // shaded, which is the whole saving a prepass exists for.
        let prepass_opaque = GraphicsPipeline::new(device, &layout, &shared)?;

        let prepass_masked = GraphicsPipeline::new(
            device,
            &layout,
            &GraphicsPipelineConfig {
                fragment: Some(ShaderStage {
                    module,
                    entry: c"prepassMain",
                }),
                ..shared
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
            prepass_opaque,
            prepass_masked,
            meshes: Vec::new(),
            placements: Vec::new(),
            images: Vec::new(),
            materials: None,
            materials_slot: None,
            instances: None,
            instances_slot: None,
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

    /// Load everything a cooked model names and upload it, replacing whatever
    /// was loaded before.
    ///
    /// Each distinct mesh, material and texture is uploaded once however many
    /// times the model places it, and all of them travel in a single transfer
    /// rather than one submit-and-block each.
    ///
    /// **Replaces rather than adds.** Calling this twice leaves the second
    /// model loaded and nothing of the first. That is worth stating because the
    /// previous behaviour was neither: meshes accumulated while material rows
    /// restarted at zero, so the first model's meshes ended up pointing at the
    /// second model's material rows — or past the end of the buffer — and the
    /// superseded heap slot leaked. Loading a second model beside a first is a
    /// real thing to want, but it needs the material table to own itself, which
    /// is M3's decomposition rather than this function's job.
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

        self.unload(heap)?;

        let mut uploads = Uploads::new(&self.device)?;

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
                        let resolved = self.resolve_textures(
                            allocator,
                            heap,
                            &mut uploads,
                            vfs,
                            &material,
                            path,
                        )?;
                        let row = self.material_row(&mut materials, &material);

                        materials[row as usize] = resolved(materials[row as usize]);
                        material_of.insert(path.clone(), row);

                        row
                    }
                },
            };

            // Read back off the row rather than off the `Material`, so the flag
            // the prepass selects on and the flag the shader tests are the same
            // value. A mesh with no material takes the default row, which has
            // no `Material` to consult at all.
            let masked = materials[material as usize].flags & ALPHA_MODE_BITS == ALPHA_MODE_MASK;

            index_of.insert(name.to_owned(), self.meshes.len());
            self.meshes.push(upload_mesh(
                allocator,
                &mut uploads,
                &mesh,
                material,
                masked,
            )?);
        }

        for instance in &model.instances {
            let Some(mesh) = index_of.get(&instance.mesh) else {
                // A cooked model naming a mesh it does not contain is a cooker
                // bug, and dropping the instance silently is how it stays one.
                // Loud rather than fatal, for the same reason a missing texture
                // is: the rest of the level is still worth drawing.
                warn!(
                    model = logical,
                    mesh = instance.mesh,
                    "a placement names a mesh this model does not contain; skipping it"
                );
                continue;
            };

            self.placements.push(Placement {
                mesh: *mesh,
                transform: Mat4::from_cols_array(&instance.transform),
            });
        }

        self.upload_materials(allocator, heap, &materials)?;
        self.upload_instances(allocator, heap)?;

        // After every copy above is recorded, and before the first frame that
        // reads any of it.
        uploads.finish(&self.device)?;

        Ok(())
    }

    /// Drop everything a previous [`load`](Self::load) uploaded.
    ///
    /// Waits for the device first: the resources being freed here are ones a
    /// frame in flight may still be reading, and that is the same hazard
    /// [`resize`](Self::resize) documents.
    fn unload(&mut self, heap: &mut BindlessHeap) -> Result<(), RenderError> {
        if self.meshes.is_empty() && self.materials.is_none() {
            return Ok(());
        }

        self.device.wait_idle()?;

        for slot in self.texture_slots.drain(..) {
            heap.remove_sampled_image(slot);
        }

        if let Some(slot) = self.materials_slot.take() {
            heap.remove_storage_buffer(slot);
        }

        if let Some(slot) = self.instances_slot.take() {
            heap.remove_storage_buffer(slot);
        }

        self.meshes.clear();
        self.placements.clear();
        self.images.clear();
        self.materials = None;
        self.instances = None;

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
                AlphaMode::Mask => ALPHA_MODE_MASK,
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
        uploads: &mut Uploads,
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

            let image = upload_texture(allocator, uploads, &texture)?;
            let Some(handle) = heap.insert_sampled_image(image.view(), ImageState::SHADER_READ)
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
                usage: BufferUsage::STORAGE,
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

    /// Put every placement's transform in a storage buffer the shader indexes.
    ///
    /// Written once, here, rather than per frame: what a placement's transform
    /// is comes from the cooked model and does not change. The normal matrix is
    /// computed here too — it is `transpose(inverse(mat3(model)))`, the same
    /// value every frame, and doing it per draw on the CPU was work repeated
    /// sixty times a second for an answer that never moved.
    fn upload_instances(
        &mut self,
        allocator: &Arc<Allocator>,
        heap: &mut BindlessHeap,
    ) -> Result<(), RenderError> {
        if self.placements.is_empty() {
            return Ok(());
        }

        let rows: Vec<InstanceGpu> = self
            .placements
            .iter()
            .map(|placement| InstanceGpu {
                model_columns: placement.transform.to_cols_array_2d(),
                normal_rows: normal_rows(placement.transform),
            })
            .collect();

        let bytes: &[u8] = bytemuck::cast_slice(&rows);
        let mut buffer = Buffer::new(
            allocator,
            &BufferConfig {
                name: "model instances",
                size: bytes.len() as u64,
                usage: BufferUsage::STORAGE,
                // Host-visible, for the same reason the material buffer is:
                // written once at load, read every frame, and a scene's
                // transforms are kilobytes.
                location: MemoryLocation::Upload,
            },
        )?;

        buffer.mapped_mut()?[..bytes.len()].copy_from_slice(bytes);

        let slot = heap
            .insert_storage_buffer(buffer.handle())
            .ok_or(RenderError::Layout {
                what: "the bindless heap had no room for the instance buffer",
            })?;

        self.instances_slot = Some(slot);
        self.instances = Some(buffer);

        Ok(())
    }

    /// Rebuild the depth buffer for a new target size.
    ///
    /// Must be called before the first frame and after every resize: a depth
    /// attachment whose size differs from the colour target is a validation
    /// error on the first frame after a resize.
    ///
    /// Replacing an existing depth buffer destroys the old image, which frames
    /// still in flight may reference, so this waits for the device to go idle
    /// first. That is the same blunt instrument `Swapchain::recreate` uses and
    /// for the same reason: resizing is rare, and a per-frame fence is the
    /// eventual answer rather than this one's.
    ///
    /// The wait is not redundant with the swapchain's. Today the only caller
    /// reaches here through `FrameRenderer::prepare`, which has already waited —
    /// but that made this function correct by call order rather than by
    /// construction, with nothing to catch a caller that resizes the renderer
    /// without recreating a swapchain. Waiting here costs nothing on the
    /// existing path, where the device is already idle.
    ///
    /// # Errors
    ///
    /// [`RenderError::Rhi`] if the device cannot be waited on, or the image
    /// cannot be allocated.
    pub fn resize(
        &mut self,
        allocator: &Arc<Allocator>,
        extent: Extent2D,
    ) -> Result<(), RenderError> {
        if self.depth.is_some() {
            self.device.wait_idle()?;
        }

        self.depth = Some(Image::new(
            allocator,
            &ImageConfig {
                name: "model depth",
                extent,
                format: self.depth_format,
                usage: ImageUsage::DEPTH_STENCIL_ATTACHMENT,
                // No chain: nothing samples a depth buffer at a distance.
                mip_levels: 1,
                array_layers: 1,
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
    /// Does nothing if no model is loaded, which is a legitimate state — a
    /// renderer that has been constructed but not given anything to draw.
    ///
    /// A model *with* no depth buffer is not legitimate, and is the one this
    /// asserts on: it means [`resize`](Self::resize) was never called, whose
    /// only symptom is otherwise a black screen with no log, no error and no
    /// panic. That is the failure mode `docs/PLAN.md` §3.1 already records
    /// learning once from golden tests that skipped on setup failure, and a
    /// `debug_assert` puts the complaint where the mistake is rather than
    /// leaving it to be diagnosed from an empty window.
    pub fn draw(&self, pass: &mut slop_rhi::Pass<'_>, heap: &BindlessHeap, view: &View) {
        debug_assert!(
            !(self.materials_slot.is_some() && self.depth.is_none()),
            "a model is loaded but `MeshRenderer::resize` has never run, so there is \
             no depth buffer and nothing will be drawn"
        );

        let Some(shared) = self.shared(view) else {
            return;
        };

        pass.bind_pipeline(&self.pipeline);
        pass.bind_heap(heap);

        for index in 0..self.placements.len() {
            self.record(pass, index, shared);
        }

        // Ends the pass, so the transition below is outside it.
        // **No barrier here, and no pass opened here either.**
        //
        // This used to bracket its own writes and transition the target itself,
        // with a comment about only the last writer being allowed to. The graph
        // opens the pass, and derives the transitions from what the declaration
        // said this pass touches — so there is nothing left to forget.
        // `docs/PLAN.md` §9.5 E3.
    }

    /// Record the depth prepass — every mesh, depth only, no shading.
    ///
    /// `docs/PLAN.md` §9.4. The forward pass then tests against a depth buffer
    /// that already holds the nearest surface at every pixel, so a fragment
    /// hidden behind something else is rejected before its shader runs. That
    /// matters more the more expensive shading gets, which is the direction
    /// Stage A is going.
    ///
    /// Draws through the **same vertex entry point** as
    /// [`draw`](Self::draw) with the **same push constants**, which is not a
    /// tidiness point: the forward pass tests `GREATER_OR_EQUAL` against what
    /// this wrote, so position arithmetic that differed by one bit in the
    /// unfavourable direction would reject the very fragment that produced the
    /// depth, and the surface would disappear.
    ///
    /// # What is not covered by a test
    ///
    /// **The masked half.** Sponza has 14 masked meshes of 103, so the split
    /// runs — but drawing them through `prepass_opaque` instead, which is the
    /// mistake this exists to prevent, changes **0 of 65536 pixels** in the
    /// reference frame. Measured, not assumed. The reference camera sits in the
    /// arcade among columns and banners, and no cutout in it has anything
    /// behind it whose disappearance would show.
    ///
    /// So this path is correct by construction — the shader tests the same
    /// expression the forward pass does — and nothing independent checks it.
    /// `docs/PLAN.md` §6.1 carries the row; closing it wants a source asset
    /// shaped for the case rather than a camera hunt through Sponza, which was
    /// tried.
    pub fn draw_depth(&self, pass: &mut slop_rhi::Pass<'_>, heap: &BindlessHeap, view: &View) {
        let Some(shared) = self.shared(view) else {
            return;
        };

        // Walked twice, grouped by pipeline, rather than once with a bind per
        // draw. Which prepass pipeline a mesh needs is a property of its
        // material, and rebinding per placement would cost more than the
        // prepass saves — Sponza is 103 primitives.
        for (masked, pipeline) in [(false, &self.prepass_opaque), (true, &self.prepass_masked)] {
            pass.bind_pipeline(pipeline);
            // Bound for both, though only the masked pipeline samples anything:
            // the alternative is a rule about which pipeline needs it, and that
            // rule breaks silently the day the vertex shader reads the heap.
            pass.bind_heap(heap);

            for index in 0..self.placements.len() {
                if self.meshes[self.placements[index].mesh].masked != masked {
                    continue;
                }

                self.record(pass, index, shared);
            }
        }
    }

    /// The push constants every draw in a frame shares, or `None` when there is
    /// nothing loaded to draw.
    ///
    /// Built once per pass rather than per draw. Everything here but the
    /// instance and material indices is the same for all of them, and computing
    /// it inside the loop was how the model matrix ended up being multiplied by
    /// the camera on the CPU a hundred times a frame.
    fn shared(&self, view: &View) -> Option<PushConstants> {
        Some(PushConstants {
            view_projection: view.view_projection,
            instances: self.instances_slot?.index(),
            instance: 0,
            materials: self.materials_slot?.index(),
            material: 0,
            grid: view.grid,
            environment: view.environment,
            shadows: view.shadows,
            _pad: 0,
        })
    }

    /// One draw, identical whichever pipeline is bound.
    ///
    /// Shared by [`draw`](Self::draw) and [`draw_depth`](Self::draw_depth)
    /// rather than written twice, because the push constants the two send must
    /// be the same bytes — see `draw_depth` for what happens when they are not.
    fn record(&self, pass: &slop_rhi::Pass<'_>, placement: usize, shared: PushConstants) {
        let mesh = &self.meshes[self.placements[placement].mesh];

        let push = PushConstants {
            // The row `upload_instances` wrote for this placement. Placements
            // are uploaded in order, so the index is the position in the list.
            instance: placement as u32,
            material: mesh.material,
            ..shared
        };

        pass.push_constants(&bytemuck::bytes_of(&push)[..self.push_constant_bytes as usize]);
        pass.bind_vertex_buffer(&mesh.vertices);
        pass.bind_index_buffer(&mesh.indices);
        pass.draw_indexed(mesh.index_count, 1, 0, 0);
    }

    /// The depth buffer, for [`Graph::import`](crate::Graph::import).
    ///
    /// `None` before [`resize`](Self::resize) has run. Exposed rather than kept
    /// private because the graph is what declares it now, and a resource the
    /// graph cannot name is one it cannot barrier.
    #[must_use]
    pub fn depth(
        &self,
    ) -> Option<(
        slop_rhi::ImageHandle,
        slop_rhi::ImageViewHandle,
        slop_rhi::ImageAspect,
    )> {
        let depth = self.depth.as_ref()?;

        Some((depth.handle(), depth.view(), depth.aspect()))
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

/// Every transfer for one [`MeshRenderer::load`], recorded into one command
/// buffer and submitted once.
///
/// The shape this replaces submitted and blocked *per resource*: a queue submit
/// and a full `wait_idle` for each vertex buffer, each index buffer and each
/// texture, with a staging allocation created and freed around every one. Sponza
/// is 103 primitives and 25 materials, so that was several hundred round trips
/// to the GPU to move data that could travel together.
///
/// Staging buffers live in `staging` rather than being freed at the end of the
/// call that made them, because the copies reading them have not run yet. That
/// is the whole reason the per-resource version had to block: it had nowhere to
/// keep them.
///
/// Still one blocking submit at the end. `docs/PLAN.md` §6.1 records the real
/// answer — an async transfer queue with a staging ring — and this is not it;
/// it is the same blunt instrument used once instead of hundreds of times.
struct Uploads {
    command: CommandBuffer,
    staging: Vec<Buffer>,
    /// Declared after `command`, since the pool must outlive the buffer it
    /// allocated.
    _pool: CommandPool,
}

impl Uploads {
    /// Open a batch and begin recording.
    fn new(device: &Arc<Device>) -> Result<Self, RenderError> {
        let pool = CommandPool::new(device, device.queue_families().graphics)?;
        let command = pool
            .allocate(1)?
            .pop()
            .expect("one command buffer was requested");

        command.begin()?;

        Ok(Self {
            command,
            staging: Vec::new(),
            _pool: pool,
        })
    }

    /// Copy `bytes` into a fresh host-visible buffer and keep it alive.
    fn stage(&mut self, allocator: &Arc<Allocator>, bytes: &[u8]) -> Result<&Buffer, RenderError> {
        let mut staging = Buffer::new(
            allocator,
            &BufferConfig {
                name: "model staging",
                size: bytes.len() as u64,
                usage: BufferUsage::TRANSFER_SRC,
                location: MemoryLocation::Upload,
            },
        )?;

        staging.mapped_mut()?[..bytes.len()].copy_from_slice(bytes);
        self.staging.push(staging);

        Ok(self
            .staging
            .last()
            .expect("a staging buffer was just pushed"))
    }

    /// Submit everything recorded and block until the GPU has it.
    ///
    /// Consumes the batch, so the staging buffers are freed after the wait and
    /// not before it.
    fn finish(self, device: &Arc<Device>) -> Result<(), RenderError> {
        self.command.end()?;
        slop_rhi::submit_recorded_and_wait(device, &self.command)?;

        Ok(())
    }
}

/// Upload one mesh's vertex and index buffers.
fn upload_mesh(
    allocator: &Arc<Allocator>,
    uploads: &mut Uploads,
    mesh: &Mesh,
    material: u32,
    masked: bool,
) -> Result<GpuMesh, RenderError> {
    let vertices = upload_buffer(
        allocator,
        uploads,
        "model vertices",
        bytemuck::cast_slice(&mesh.vertices),
        BufferUsage::VERTEX,
        BufferState::VERTEX_INPUT,
    )?;
    let indices = upload_buffer(
        allocator,
        uploads,
        "model indices",
        bytemuck::cast_slice(&mesh.indices),
        BufferUsage::INDEX,
        BufferState::INDEX_INPUT,
    )?;

    Ok(GpuMesh {
        vertices,
        indices,
        index_count: mesh.indices.len() as u32,
        material,
        masked,
    })
}

/// Record a staged copy into a device-local buffer.
fn upload_buffer(
    allocator: &Arc<Allocator>,
    uploads: &mut Uploads,
    name: &str,
    bytes: &[u8],
    usage: BufferUsage,
    state: BufferState,
) -> Result<Buffer, RenderError> {
    let buffer = Buffer::new(
        allocator,
        &BufferConfig {
            name,
            size: bytes.len() as u64,
            usage: usage | BufferUsage::TRANSFER_DST,
            location: MemoryLocation::DeviceOnly,
        },
    )?;

    let staging = uploads.stage(allocator, bytes)?.handle();

    uploads
        .command
        .copy_buffer(staging, buffer.handle(), bytes.len() as u64);
    uploads
        .command
        .barrier_buffer(buffer.handle(), BufferState::TRANSFER_DST, state);

    Ok(buffer)
}

/// Record a staged upload of a cooked texture into a sampled image.
fn upload_texture(
    allocator: &Arc<Allocator>,
    uploads: &mut Uploads,
    texture: &slop_asset::Texture,
) -> Result<Image, RenderError> {
    let image = Image::new(
        allocator,
        &ImageConfig {
            name: "model texture",
            extent: Extent2D {
                width: texture.width,
                height: texture.height,
            },
            format: vulkan_format(texture.format),
            usage: ImageUsage::SAMPLED | ImageUsage::TRANSFER_DST,
            mip_levels: texture.mip_levels,
            array_layers: 1,
        },
    )?;

    let staging = uploads.stage(allocator, &texture.pixels)?.handle();

    // Covers every level: `transition_image` names the whole chain, which
    // is what leaves no level behind in UNDEFINED.
    uploads.command.transition_image(
        image.handle(),
        image.aspect(),
        ImageState::UNDEFINED,
        ImageState::TRANSFER_DST,
    );

    // One copy per level, all out of the same staging buffer.
    for (index, level) in texture.levels().enumerate() {
        uploads.command.copy_buffer_to_image_level(
            staging,
            level.offset as u64,
            image.handle(),
            image.aspect(),
            Extent2D {
                width: level.width,
                height: level.height,
            },
            u32::try_from(index).expect("a mip chain is far shorter than u32::MAX"),
        );
    }

    uploads.command.transition_image(
        image.handle(),
        image.aspect(),
        ImageState::TRANSFER_DST,
        ImageState::SHADER_READ,
    );

    Ok(image)
}

/// The Vulkan format a cooked texture's bytes are in.
///
/// UNORM rather than the `_SRGB` variants, matching what the cube does and for
/// the same reason: the shader reads the bytes that were uploaded. Applying the
/// sRGB transfer where a material says to is `TextureSlot::is_srgb`'s job and
/// arrives with real shading at M3 — `docs/PLAN.md` §6.1 records it.
fn vulkan_format(format: slop_asset::Format) -> Format {
    match format {
        slop_asset::Format::Rgba8 => Format::Rgba8Unorm,
        slop_asset::Format::Bc7 => Format::Bc7Unorm,
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
