//! Buffers: linear GPU memory with a usage declaration.

use std::sync::Arc;

use ash::vk;
use gpu_allocator::vulkan as ga;

use crate::resource::{Allocator, MemoryLocation};
use crate::{BufferHandle, BufferUsage, RhiError};

/// What a buffer is for.
///
/// Every field is required rather than defaulted. A buffer created with the
/// wrong `usage` fails at the point of use with a validation message naming a
/// flag rather than a call site, and a buffer in the wrong `location` works
/// correctly while being slow — the second is the one a default would hide.
#[derive(Debug, Clone)]
pub struct BufferConfig<'a> {
    /// A name for validation messages, allocator reports, and leak reporting.
    /// Not optional: an allocator report full of unnamed blocks cannot be acted
    /// on.
    pub name: &'a str,
    /// Size in bytes. Vulkan rejects zero.
    pub size: u64,
    /// How the buffer will be used.
    pub usage: BufferUsage,
    /// Which memory it should live in.
    pub location: MemoryLocation,
}

/// A GPU buffer and the memory backing it.
pub struct Buffer {
    handle: vk::Buffer,
    // `Option` so `Drop` can move the allocation back to the allocator. Always
    // `Some` between construction and drop.
    allocation: Option<ga::Allocation>,
    allocator: Arc<Allocator>,
    size: u64,
}

impl Buffer {
    /// Allocate a buffer.
    ///
    /// # Errors
    ///
    /// Fails if the driver rejects the buffer, or if no memory heap matching
    /// `config.location` has room.
    pub fn new(allocator: &Arc<Allocator>, config: &BufferConfig<'_>) -> Result<Self, RhiError> {
        let device = allocator.device().raw();

        let create_info = vk::BufferCreateInfo::default()
            .size(config.size)
            .usage(config.usage.to_vk())
            // EXCLUSIVE, always. Sharing across queue families without an
            // ownership transfer costs performance on every access on some
            // hardware, and that transfer is a barrier this crate makes
            // explicit rather than something to buy blanket immunity from.
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        // SAFETY: `create_info` is fully initialized, and the device is alive
        // because the allocator holds an `Arc` to it.
        let handle = unsafe { device.create_buffer(&create_info, None) }?;

        // From here every failure path must destroy `handle`, so each one does
        // so explicitly rather than relying on a `Drop` this value does not yet
        // have.

        // SAFETY: `handle` was just created from this device.
        let requirements = unsafe { device.get_buffer_memory_requirements(handle) };

        // Buffers are linear by definition, so they can never collide with an
        // optimally tiled image under Vulkan's buffer-image granularity rule.
        let allocation = match allocator.allocate(config.name, requirements, config.location, true)
        {
            Ok(allocation) => allocation,
            Err(error) => {
                // SAFETY: created from this device and never used, so no GPU
                // work can reference it.
                unsafe { device.destroy_buffer(handle, None) };
                return Err(error);
            }
        };

        // SAFETY: the allocation satisfies `handle`'s memory requirements — the
        // allocator was handed them directly — the buffer has no memory bound
        // yet, and the allocation outlives the buffer because both are owned by
        // the value returned below.
        let bound =
            unsafe { device.bind_buffer_memory(handle, allocation.memory(), allocation.offset()) };

        if let Err(error) = bound {
            allocator.free(allocation);
            // SAFETY: created from this device and never used.
            unsafe { device.destroy_buffer(handle, None) };
            return Err(error.into());
        }

        Ok(Self {
            handle,
            allocation: Some(allocation),
            allocator: Arc::clone(allocator),
            size: config.size,
        })
    }

    /// The underlying handle.
    pub fn handle(&self) -> BufferHandle {
        BufferHandle(self.handle)
    }

    /// Size in bytes, as requested. The allocation backing it may be larger.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The buffer's contents, for host-visible memory.
    ///
    /// Truncated to the requested size, so alignment padding the allocator added
    /// is never visible.
    ///
    /// # Errors
    ///
    /// Returns [`RhiError::MemoryNotHostVisible`] for a buffer in
    /// [`MemoryLocation::DeviceOnly`], which is not mappable.
    pub fn mapped(&self) -> Result<&[u8], RhiError> {
        self.allocation
            .as_ref()
            .and_then(ga::Allocation::mapped_slice)
            .and_then(|slice| slice.get(..self.size as usize))
            .ok_or(RhiError::MemoryNotHostVisible)
    }

    /// The buffer's contents for writing, for host-visible memory.
    ///
    /// **A write through this needs no barrier before the GPU reads it**, so
    /// long as it happens before the queue submission that reads it. See
    /// [`MemoryLocation::Upload`] for why, and
    /// [`is_host_coherent`](Self::is_host_coherent) for the condition it rests
    /// on.
    ///
    /// # Errors
    ///
    /// Returns [`RhiError::MemoryNotHostVisible`] for a buffer in
    /// [`MemoryLocation::DeviceOnly`], which is not mappable.
    pub fn mapped_mut(&mut self) -> Result<&mut [u8], RhiError> {
        let size = self.size as usize;

        self.allocation
            .as_mut()
            .and_then(ga::Allocation::mapped_slice_mut)
            .and_then(|slice| slice.get_mut(..size))
            .ok_or(RhiError::MemoryNotHostVisible)
    }

    /// Whether this buffer's memory is host-coherent.
    ///
    /// The condition Vulkan's host write ordering guarantee rests on: a host
    /// write to coherent memory is *available to the host memory domain*
    /// immediately, and a queue submission then makes it visible to the device
    /// with no barrier and no flush.
    ///
    /// Exposed so the guarantee can be asserted rather than assumed. Which
    /// memory type an allocator hands back is a runtime decision, and a driver
    /// or an allocator upgrade that produced non-coherent upload memory would
    /// otherwise turn every barrier-free staging copy in the engine into a race
    /// that reproduces on one vendor.
    #[must_use]
    pub fn is_host_coherent(&self) -> bool {
        self.allocation.as_ref().is_some_and(|allocation| {
            allocation
                .memory_properties()
                .contains(vk::MemoryPropertyFlags::HOST_COHERENT)
        })
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if let Some(allocation) = self.allocation.take() {
            self.allocator.free(allocation);
        }

        // SAFETY: created from this device, destroyed exactly once, and the
        // device outlives this because the allocator holds an `Arc` to it. That
        // no GPU work still references the buffer is the caller's obligation,
        // the same one every Vulkan object carries.
        unsafe {
            self.allocator
                .device()
                .raw()
                .destroy_buffer(self.handle, None);
        }
    }
}

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}
