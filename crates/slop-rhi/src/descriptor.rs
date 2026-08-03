//! The bindless descriptor heap — `docs/DESIGN.md` §2.2.
//!
//! # What bindless replaces
//!
//! The conventional Vulkan model binds a descriptor set per material: the CPU
//! knows, for every draw, exactly which textures that draw reads, and rebinds
//! between draws. That model cannot express `docs/DESIGN.md` §4.2 stage B at
//! all — a GPU-driven pipeline decides on the GPU which draws happen and which
//! materials they use, and there is no CPU in the loop to rebind anything.
//!
//! So: one descriptor set, allocated once, containing every texture in the
//! engine. Shaders index it with an integer that can come from a buffer the GPU
//! filled in. Binding happens once per frame and never per draw.
//!
//! # Why it exists at M0, with one texture
//!
//! `docs/DESIGN.md` §2.2 lists this alongside timeline semaphores and explicit
//! barriers as something that cannot be retrofitted, and the reason is the
//! same: it is not a feature, it is the shape every shader is written against.
//! A material system built on per-draw descriptor sets does not gain bindless
//! later — it is replaced by one that has it. The cube uses one texture and
//! goes through the heap anyway.
//!
//! # What is deliberately not in the heap
//!
//! **Buffers.** `buffer_device_address` is in the required feature tier, so a
//! shader reaches a buffer through a 64-bit pointer rather than a descriptor.
//! That is strictly more capable — pointers can be stored in other buffers,
//! chased, and offset — and it removes a whole descriptor type and its capacity
//! limit from this file. Vertex, index, uniform and storage data all take that
//! route.
//!
//! **Combined image samplers.** Images and samplers are separate bindings, so
//! N textures and M samplers cost N + M descriptors rather than N × M. Every
//! bindless design converges on this; taking the combined type would cap the
//! texture count at the point where the two multiply.
//!
//! # The binding numbers are a shader ABI
//!
//! [`SAMPLED_IMAGE_BINDING`] and its neighbours are duplicated in
//! `shaders/lib/bindless.slang`. There is no mechanism keeping the two in step;
//! shader reflection at M2 (`docs/DESIGN.md` §2.11) is what will let one be
//! derived from the other. Until then, changing a number here means changing it
//! there, and the tests in this module exist partly to make the pairing visible.

use std::sync::Arc;

use ash::vk;
use slop_core::diagnostics::tracing::{debug, info};
use slop_core::{Handle, HandleAllocator};

use crate::{BufferHandle, Device, ImageState, ImageViewHandle, RhiError, SamplerHandle};

/// Set index the heap is bound at. Set 0, so that any per-pass or per-frame set
/// added later takes a higher number and does not disturb it.
pub const HEAP_SET: u32 = 0;

/// Binding index for sampled images — textures a shader reads.
pub const SAMPLED_IMAGE_BINDING: u32 = 0;

/// Binding index for samplers, kept separate from the images they filter.
pub const SAMPLER_BINDING: u32 = 1;

/// Binding index for storage images — targets a compute shader writes.
pub const STORAGE_IMAGE_BINDING: u32 = 2;

/// Type tag for a texture slot. Never constructed.
#[derive(Debug)]
pub enum SampledImage {}

/// Type tag for a sampler slot. Never constructed.
#[derive(Debug)]
pub enum Sampler {}

/// Binding index for storage buffers — arrays of structured data a shader
/// indexes, such as per-material parameters or per-instance transforms.
///
/// Added here rather than when the first consumer appeared, for the reason the
/// module docs give: a binding introduced into the global set layout later
/// invalidates every pipeline built against it, because the layout is a shader
/// ABI. `shader_storage_buffer_array_dynamic_indexing` has been in the required
/// feature tier since M0; this is the binding that makes it reachable.
pub const STORAGE_BUFFER_BINDING: u32 = 3;

/// Type tag for a storage image slot. Never constructed.
#[derive(Debug)]
pub enum StorageImage {}

/// Type tag for a storage buffer slot. Never constructed.
#[derive(Debug)]
pub enum StorageBuffer {}

/// How many of each kind of descriptor the heap can hold.
///
/// Requests, not guarantees: each is clamped to what the device reports, and
/// the result is readable through [`BindlessHeap::capacity`]. Clamping rather
/// than failing is deliberate — a device that supports 8192 textures instead of
/// 16384 is perfectly usable, and rejecting it would be the capability-tier
/// branching `docs/DESIGN.md` §2.1 exists to avoid, in its harshest form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindlessHeapConfig {
    /// Textures readable by shaders.
    pub sampled_images: u32,
    /// Samplers. Far fewer than textures: a sampler is a filtering mode, and an
    /// engine has a handful of those no matter how many textures it loads.
    pub samplers: u32,
    /// Images compute shaders write.
    pub storage_images: u32,
    /// Buffers of structured data a shader indexes — material parameters,
    /// instance transforms, and the draw lists §4.2 stage B builds on the GPU.
    pub storage_buffers: u32,
}

impl Default for BindlessHeapConfig {
    /// Sized for a real scene, not for the cube.
    ///
    /// Descriptors are cheap — sixteen thousand of them is well under a
    /// megabyte — and the count cannot grow after creation without rebuilding
    /// the set and every pipeline bound against its layout. Starting small and
    /// discovering the ceiling mid-project is the expensive direction.
    fn default() -> Self {
        Self {
            sampled_images: 16_384,
            samplers: 128,
            storage_images: 1_024,
            storage_buffers: 1_024,
        }
    }
}

/// One descriptor set holding every texture, sampler, and storage image.
///
/// Slots are handed out by [`HandleAllocator`], so a [`Handle`] carries a
/// generation the heap checks on removal. The GPU only ever sees
/// [`Handle::index`]; the generation is CPU-side protection against writing
/// through a slot that was freed and reused.
pub struct BindlessHeap {
    // Drop order: the pool frees its sets, and the layout must outlive the
    // pipelines built from it — which the caller's `Arc` handles.
    pool: vk::DescriptorPool,
    layout: vk::DescriptorSetLayout,
    set: vk::DescriptorSet,
    capacity: BindlessHeapConfig,
    sampled_images: HandleAllocator<SampledImage>,
    samplers: HandleAllocator<Sampler>,
    storage_images: HandleAllocator<StorageImage>,
    storage_buffers: HandleAllocator<StorageBuffer>,
    device: Arc<Device>,
}

impl BindlessHeap {
    /// Create the heap, clamping `config` to what the device supports.
    ///
    /// # Errors
    ///
    /// Fails if the driver rejects the layout, the pool, or the set — all of
    /// which mean the requested capacity exceeded a limit this did not clamp
    /// against, and are therefore bugs here rather than in a caller.
    pub fn new(device: &Arc<Device>, config: &BindlessHeapConfig) -> Result<Self, RhiError> {
        let capacity = clamp_to_limits(device, config);

        if capacity != *config {
            info!(
                requested = ?config,
                granted = ?capacity,
                "bindless heap clamped to device limits"
            );
        }

        let counts = [
            (
                SAMPLED_IMAGE_BINDING,
                vk::DescriptorType::SAMPLED_IMAGE,
                capacity.sampled_images,
            ),
            (
                SAMPLER_BINDING,
                vk::DescriptorType::SAMPLER,
                capacity.samplers,
            ),
            (
                STORAGE_IMAGE_BINDING,
                vk::DescriptorType::STORAGE_IMAGE,
                capacity.storage_images,
            ),
            (
                STORAGE_BUFFER_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
                capacity.storage_buffers,
            ),
        ];

        let bindings: Vec<vk::DescriptorSetLayoutBinding<'_>> = counts
            .iter()
            .map(|&(binding, kind, count)| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(binding)
                    .descriptor_type(kind)
                    .descriptor_count(count)
                    // Visible everywhere. A bindless heap whose visibility had
                    // to be declared per stage would need a different layout
                    // per pipeline kind, which is the thing it replaces.
                    .stage_flags(vk::ShaderStageFlags::ALL)
            })
            .collect();

        // PARTIALLY_BOUND is what makes a mostly-empty heap legal: without it,
        // every descriptor in the array must be written before the set is used,
        // so a heap sized for sixteen thousand textures would need sixteen
        // thousand real textures.
        //
        // UPDATE_AFTER_BIND and UPDATE_UNUSED_WHILE_PENDING together are what
        // make streaming possible — a texture can be written into a slot while
        // frames that do not use that slot are still in flight.
        //
        // Deliberately NOT VARIABLE_DESCRIPTOR_COUNT: the capacity is fixed at
        // creation and the set is allocated once, so a variable count would add
        // a parameter to every allocation while changing nothing. The feature
        // stays in the required tier for the streaming work at M2.
        let flags = vk::DescriptorBindingFlags::PARTIALLY_BOUND
            | vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
            | vk::DescriptorBindingFlags::UPDATE_UNUSED_WHILE_PENDING;
        let binding_flags = [flags; 4];

        let mut flags_info =
            vk::DescriptorSetLayoutBindingFlagsCreateInfo::default().binding_flags(&binding_flags);

        let layout_info = vk::DescriptorSetLayoutCreateInfo::default()
            .bindings(&bindings)
            .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
            .push_next(&mut flags_info);

        // SAFETY: `layout_info` is fully initialized, every borrowed array
        // outlives the call, and the descriptor-indexing features it relies on
        // are in the required feature tier.
        let layout = unsafe {
            device
                .raw()
                .create_descriptor_set_layout(&layout_info, None)
        }?;

        let pool_sizes: Vec<vk::DescriptorPoolSize> = counts
            .iter()
            .map(|&(_, kind, count)| {
                vk::DescriptorPoolSize::default()
                    .ty(kind)
                    .descriptor_count(count)
            })
            .collect();

        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .pool_sizes(&pool_sizes)
            .max_sets(1)
            // Must match the layout's UPDATE_AFTER_BIND_POOL, or set allocation
            // fails with a message that names neither.
            .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND);

        // SAFETY: `pool_info` is fully initialized and `pool_sizes` outlives
        // the call.
        let pool = match unsafe { device.raw().create_descriptor_pool(&pool_info, None) } {
            Ok(pool) => pool,
            Err(error) => {
                // SAFETY: created from this device just above and never used.
                unsafe { device.raw().destroy_descriptor_set_layout(layout, None) };
                return Err(error.into());
            }
        };

        let layouts = [layout];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);

        // SAFETY: the pool has room for one set of this layout, both were
        // created from this device, and `layouts` outlives the call.
        let allocated = unsafe { device.raw().allocate_descriptor_sets(&allocate_info) };

        let set = match allocated {
            Ok(mut sets) => sets.pop().expect("one set was requested"),
            Err(error) => {
                // SAFETY: both created from this device and neither used;
                // destroying the pool also frees any set allocated from it.
                unsafe {
                    device.raw().destroy_descriptor_pool(pool, None);
                    device.raw().destroy_descriptor_set_layout(layout, None);
                }
                return Err(error.into());
            }
        };

        debug!(
            sampled_images = capacity.sampled_images,
            samplers = capacity.samplers,
            storage_images = capacity.storage_images,
            "created bindless descriptor heap"
        );

        Ok(Self {
            pool,
            layout,
            set,
            capacity,
            sampled_images: HandleAllocator::new(),
            samplers: HandleAllocator::new(),
            storage_images: HandleAllocator::new(),
            storage_buffers: HandleAllocator::new(),
            device: Arc::clone(device),
        })
    }

    /// The set layout, for building pipeline layouts against.
    pub fn layout(&self) -> vk::DescriptorSetLayout {
        self.layout
    }

    /// The set itself, bound once per frame at [`HEAP_SET`].
    pub fn set(&self) -> vk::DescriptorSet {
        self.set
    }

    /// The capacity actually granted, after clamping to device limits.
    pub fn capacity(&self) -> BindlessHeapConfig {
        self.capacity
    }

    /// How many slots of each kind are currently occupied.
    pub fn occupancy(&self) -> BindlessHeapConfig {
        BindlessHeapConfig {
            sampled_images: u32::try_from(self.sampled_images.len()).unwrap_or(u32::MAX),
            samplers: u32::try_from(self.samplers.len()).unwrap_or(u32::MAX),
            storage_images: u32::try_from(self.storage_images.len()).unwrap_or(u32::MAX),
            storage_buffers: u32::try_from(self.storage_buffers.len()).unwrap_or(u32::MAX),
        }
    }

    /// Place a texture in the heap and return the slot a shader indexes.
    ///
    /// `view` must remain alive and unchanged until the handle is removed. That
    /// is not expressible in the type system here — the heap deliberately does
    /// not own the images it points at, because their lifetimes belong to the
    /// asset system at M2.
    ///
    /// Returns `None` when the heap is full, rather than panicking: running out
    /// of texture slots is a content problem a game may want to report, and it
    /// is exactly the situation where a crash is least helpful.
    pub fn insert_sampled_image(
        &mut self,
        view: ImageViewHandle,
        state: ImageState,
    ) -> Option<Handle<SampledImage>> {
        if self.sampled_images.len() >= self.capacity.sampled_images as usize {
            return None;
        }

        let handle = self.sampled_images.allocate();
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(view.0)
            .image_layout(state.layout)];

        self.write(SAMPLED_IMAGE_BINDING, handle.index(), |write| {
            write
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(&image_info)
        });

        Some(handle)
    }

    /// Place a sampler in the heap.
    ///
    /// Returns `None` when the heap is full.
    pub fn insert_sampler(&mut self, sampler: SamplerHandle) -> Option<Handle<Sampler>> {
        if self.samplers.len() >= self.capacity.samplers as usize {
            return None;
        }

        let handle = self.samplers.allocate();
        let image_info = [vk::DescriptorImageInfo::default().sampler(sampler.0)];

        self.write(SAMPLER_BINDING, handle.index(), |write| {
            write
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(&image_info)
        });

        Some(handle)
    }

    /// Place a storage image in the heap, for compute shaders to write.
    ///
    /// Returns `None` when the heap is full.
    pub fn insert_storage_image(&mut self, view: ImageViewHandle) -> Option<Handle<StorageImage>> {
        if self.storage_images.len() >= self.capacity.storage_images as usize {
            return None;
        }

        let handle = self.storage_images.allocate();
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(view.0)
            // The only layout a storage image may be written through.
            .image_layout(vk::ImageLayout::GENERAL)];

        self.write(STORAGE_IMAGE_BINDING, handle.index(), |write| {
            write
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&image_info)
        });

        Some(handle)
    }

    /// Place a buffer of structured data in the heap.
    ///
    /// `buffer` must remain alive and unchanged until the handle is removed,
    /// for the same reason a sampled image must: the descriptor holds the
    /// handle, not a reference the borrow checker can see.
    ///
    /// The whole buffer is bound. A shader reads it as an array of whatever its
    /// own declaration says, so the element type is the shader's business and
    /// this only has to know where the bytes are — which is what makes one
    /// binding serve materials, transforms and draw lists alike.
    pub fn insert_storage_buffer(&mut self, buffer: BufferHandle) -> Option<Handle<StorageBuffer>> {
        if self.storage_buffers.len() >= self.capacity.storage_buffers as usize {
            return None;
        }

        let handle = self.storage_buffers.allocate();
        let buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(buffer.0)
            .offset(0)
            .range(vk::WHOLE_SIZE)];

        self.write(STORAGE_BUFFER_BINDING, handle.index(), |write| {
            write
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&buffer_info)
        });

        Some(handle)
    }

    /// Release a storage buffer slot. Returns whether the handle was live.
    pub fn remove_storage_buffer(&mut self, handle: Handle<StorageBuffer>) -> bool {
        self.storage_buffers.free(handle)
    }

    /// Release a texture slot. Returns whether the handle was live.
    ///
    /// The descriptor is left as it was. That is safe because
    /// `PARTIALLY_BOUND` permits a shader to hold an index it never reads, and
    /// because the slot cannot be handed out again without being overwritten.
    /// Writing a null descriptor instead would need a placeholder image the
    /// heap does not own.
    pub fn remove_sampled_image(&mut self, handle: Handle<SampledImage>) -> bool {
        self.sampled_images.free(handle)
    }

    /// Release a sampler slot. Returns whether the handle was live.
    pub fn remove_sampler(&mut self, handle: Handle<Sampler>) -> bool {
        self.samplers.free(handle)
    }

    /// Release a storage image slot. Returns whether the handle was live.
    pub fn remove_storage_image(&mut self, handle: Handle<StorageImage>) -> bool {
        self.storage_images.free(handle)
    }

    /// Whether a texture handle still refers to the slot it was issued for.
    pub fn is_live_sampled_image(&self, handle: Handle<SampledImage>) -> bool {
        self.sampled_images.is_live(handle)
    }

    /// Bind the heap for subsequent draws or dispatches.
    ///
    /// Once per frame per bind point, not once per draw — which is the point of
    /// the whole design.
    pub fn bind(
        &self,
        command: vk::CommandBuffer,
        bind_point: vk::PipelineBindPoint,
        layout: vk::PipelineLayout,
    ) {
        let sets = [self.set];

        // SAFETY: the command buffer is recording, the set and layout belong to
        // this device, and `sets` outlives the call.
        unsafe {
            self.device.raw().cmd_bind_descriptor_sets(
                command,
                bind_point,
                layout,
                HEAP_SET,
                &sets,
                &[],
            );
        }
    }

    /// Write one descriptor into `binding` at `index`.
    ///
    /// The closure supplies the type and payload; everything structural is
    /// filled in here so no caller can write to the wrong set or forget the
    /// array element.
    fn write<'a>(
        &self,
        binding: u32,
        index: u32,
        describe: impl FnOnce(vk::WriteDescriptorSet<'a>) -> vk::WriteDescriptorSet<'a>,
    ) {
        let write = describe(
            vk::WriteDescriptorSet::default()
                .dst_set(self.set)
                .dst_binding(binding)
                .dst_array_element(index),
        );
        let writes = [write];

        // SAFETY: the set belongs to this device, `binding` is one of the three
        // this layout declares, `index` came from a handle allocator bounded by
        // the binding's capacity, and the borrowed image info outlives the call.
        // UPDATE_AFTER_BIND makes writing while the set is bound legal.
        unsafe { self.device.raw().update_descriptor_sets(&writes, &[]) };
    }
}

impl Drop for BindlessHeap {
    fn drop(&mut self) {
        debug!("destroying bindless descriptor heap");

        // SAFETY: both were created from this device and are destroyed exactly
        // once. Destroying the pool frees the set allocated from it, so the set
        // needs no separate cleanup. That no GPU work still references them is
        // the caller's obligation, as for every Vulkan object here.
        unsafe {
            self.device.raw().destroy_descriptor_pool(self.pool, None);
            self.device
                .raw()
                .destroy_descriptor_set_layout(self.layout, None);
        }
    }
}

impl std::fmt::Debug for BindlessHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BindlessHeap")
            .field("capacity", &self.capacity)
            .field("occupancy", &self.occupancy())
            .finish_non_exhaustive()
    }
}

/// Reduce a request to what the device actually permits.
///
/// Two limits apply to each kind and both matter: the per-set limit, and the
/// per-stage limit. A set within the per-set limit whose bindings exceed the
/// per-stage limit is rejected at pipeline creation rather than at set
/// creation, which puts the error a long way from its cause.
fn clamp_to_limits(device: &Arc<Device>, config: &BindlessHeapConfig) -> BindlessHeapConfig {
    let mut indexing = vk::PhysicalDeviceDescriptorIndexingProperties::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut indexing);

    // SAFETY: the physical device came from this instance's enumeration, and
    // `properties` is fully initialized with a valid pNext chain whose members
    // outlive the call.
    unsafe {
        device
            .instance()
            .raw()
            .get_physical_device_properties2(device.physical_device(), &mut properties);
    }

    BindlessHeapConfig {
        sampled_images: config
            .sampled_images
            .min(indexing.max_descriptor_set_update_after_bind_sampled_images)
            .min(indexing.max_per_stage_descriptor_update_after_bind_sampled_images),
        samplers: config
            .samplers
            .min(indexing.max_descriptor_set_update_after_bind_samplers)
            .min(indexing.max_per_stage_descriptor_update_after_bind_samplers),
        storage_images: config
            .storage_images
            .min(indexing.max_descriptor_set_update_after_bind_storage_images)
            .min(indexing.max_per_stage_descriptor_update_after_bind_storage_images),
        storage_buffers: config
            .storage_buffers
            .min(indexing.max_descriptor_set_update_after_bind_storage_buffers)
            .min(indexing.max_per_stage_descriptor_update_after_bind_storage_buffers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binding_numbers_are_distinct_and_start_at_zero() {
        // These are a shader ABI duplicated in `shaders/lib/bindless.slang`.
        // A collision would be diagnosed by the driver as a layout error naming
        // neither file.
        let bindings = [
            SAMPLED_IMAGE_BINDING,
            SAMPLER_BINDING,
            STORAGE_IMAGE_BINDING,
            STORAGE_BUFFER_BINDING,
        ];
        let mut sorted = bindings;
        sorted.sort_unstable();

        assert_eq!(sorted, [0, 1, 2, 3], "bindings must be 0..4 with no gaps");
        assert_eq!(HEAP_SET, 0);
    }

    #[test]
    fn the_default_capacity_is_sized_for_a_scene_not_a_demo() {
        // Guards against someone shrinking these to "what M0 needs". The
        // capacity cannot grow after creation without rebuilding every pipeline
        // bound against the layout.
        let config = BindlessHeapConfig::default();

        assert!(config.sampled_images >= 4096);
        assert!(config.storage_images >= 256);
        assert!(config.samplers >= 16);
        assert!(config.storage_buffers >= 256);
    }

    #[test]
    fn the_shader_side_declares_the_same_bindings() {
        // The ABI's other half is a text file nothing compiles against, and its
        // own comments say a mismatch is neither a compile error nor a
        // validation one — it is a shader reading a different array than the
        // engine wrote to, which looks like a content bug.
        //
        // Crude, and better than nothing until reflection carries descriptor
        // bindings (`docs/PLAN.md` §6.1). It catches the case that actually
        // happens: a binding added on one side and not the other.
        let shader = include_str!("../../../shaders/lib/bindless.slang");

        for binding in [
            SAMPLED_IMAGE_BINDING,
            SAMPLER_BINDING,
            STORAGE_IMAGE_BINDING,
            STORAGE_BUFFER_BINDING,
        ] {
            let declaration = format!("[[vk::binding({binding}, {HEAP_SET})]]");

            assert!(
                shader.contains(&declaration),
                "shaders/lib/bindless.slang does not declare `{declaration}`"
            );
        }
    }
}
