//! Device memory suballocation.
//!
//! Backed by `gpu-allocator`, which is not named in this module's public API.
//! That is deliberate: the allocator is the one component here most likely to be
//! replaced — by a hand-written one once `docs/DESIGN.md` §2.2's transient
//! aliasing needs lifetime information a general-purpose allocator does not
//! have — and a replacement should not be a breaking change to every caller.

use std::sync::{Arc, Mutex};

use ash::vk;
use gpu_allocator::vulkan as ga;
use slop_core::diagnostics::tracing::debug;

use crate::{Device, RhiError};

/// Where an allocation should live, stated by what the memory is *for* rather
/// than by heap flags.
///
/// Naming the intent rather than the property bits is what lets the allocator
/// pick differently on discrete and integrated hardware without every call site
/// growing a branch — on a system with resizable BAR, [`Upload`](Self::Upload)
/// is device-local *and* host-visible, and on one without it is neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryLocation {
    /// Only the GPU ever touches it: vertex and index buffers, textures, render
    /// targets. The fastest memory, and not mappable.
    DeviceOnly,
    /// Written by the CPU, read by the GPU — staging buffers and per-frame
    /// uniform data.
    ///
    /// Host-coherent, and that is load-bearing rather than incidental: **a host
    /// write to this memory needs no barrier before the GPU reads it**, as long
    /// as the write happens before the queue submission.
    ///
    /// Vulkan's host write ordering guarantee is what provides it. A queue
    /// submission's first synchronization scope includes execution of
    /// `vkQueueSubmit` on the host, and its first access scope includes every
    /// host write already *available to the host memory domain* — which for
    /// coherent memory means every host write, with no flush and no barrier.
    /// Non-coherent memory would need `vkFlushMappedMemoryRanges` first, which
    /// is why the coherence is asserted by
    /// [`Buffer::is_host_coherent`](crate::Buffer::is_host_coherent) and tested
    /// rather than assumed.
    ///
    /// Worth stating because the alternative is a `HOST_WRITE` → `TRANSFER_SRC`
    /// barrier on every staging buffer, which is correct, free of charge, and
    /// indistinguishable from a barrier that is actually required. Two upload
    /// paths in this repository disagreed about it, and nothing recorded which
    /// was right — see `CONSIDERATIONS.md` item 3.
    Upload,
    /// Written by the GPU, read by the CPU — screenshots, golden-image
    /// readback, GPU-side query results.
    ///
    /// Always host-coherent, so a read after the copy completes needs no
    /// explicit cache invalidation.
    Readback,
}

impl MemoryLocation {
    fn to_gpu_allocator(self) -> gpu_allocator::MemoryLocation {
        match self {
            Self::DeviceOnly => gpu_allocator::MemoryLocation::GpuOnly,
            Self::Upload => gpu_allocator::MemoryLocation::CpuToGpu,
            Self::Readback => gpu_allocator::MemoryLocation::GpuToCpu,
        }
    }
}

/// What the allocator is currently holding.
///
/// Mostly for tests and the debug UI: `live` returning to zero after a frame's
/// resources drop is a leak check that needs no external tooling, and
/// `reserved` minus `used` is the fragmentation the suballocator is carrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocatorStats {
    /// Live suballocations.
    pub live: usize,
    /// Bytes handed out across those suballocations.
    pub used: u64,
    /// Bytes the allocator has taken from the driver, including unused space
    /// inside its blocks.
    pub reserved: u64,
}

/// Suballocates device memory for buffers and images.
///
/// One per [`Device`] is the intended shape. Creating several is not an error
/// but gives up the sharing that makes suballocation worth having.
///
/// Cloneable handles are `Arc<Allocator>` rather than `Allocator: Clone`, so
/// that the interior `Mutex` is shared rather than duplicated.
pub struct Allocator {
    // `Option` so `Drop` can take the allocator out and destroy it explicitly.
    // gpu-allocator reports leaks when dropped, and it must be dropped before
    // the device — which the `Arc<Device>` below guarantees.
    inner: Mutex<Option<ga::Allocator>>,
    device: Arc<Device>,
}

impl Allocator {
    /// Create an allocator for `device`.
    ///
    /// # Errors
    ///
    /// Fails if the driver reports memory properties the allocator cannot make
    /// sense of, which in practice means a broken driver.
    pub fn new(device: &Arc<Device>) -> Result<Arc<Self>, RhiError> {
        let inner = ga::Allocator::new(&ga::AllocatorCreateDesc {
            instance: device.instance().raw().clone(),
            device: device.raw().clone(),
            physical_device: device.physical_device(),
            debug_settings: gpu_allocator::AllocatorDebugSettings::default(),
            // Must match the device: `buffer_device_address` is in the required
            // feature tier, and memory backing a buffer whose address is taken
            // has to be allocated with the matching flag. Hard-coded `true`
            // rather than queried, because `device/features.rs` requires it
            // unconditionally — a device without it never got this far.
            buffer_device_address: true,
            allocation_sizes: gpu_allocator::AllocationSizes::default(),
        })
        .map_err(|source| RhiError::AllocatorUnavailable {
            reason: source.to_string(),
        })?;

        debug!("created GPU memory allocator");

        Ok(Arc::new(Self {
            inner: Mutex::new(Some(inner)),
            device: Arc::clone(device),
        }))
    }

    /// The device whose memory this allocates.
    pub fn device(&self) -> &Arc<Device> {
        &self.device
    }

    /// A snapshot of what is currently allocated.
    ///
    /// # Panics
    ///
    /// Panics if a previous allocation panicked while holding the internal lock.
    pub fn stats(&self) -> AllocatorStats {
        let guard = self.lock();
        let Some(allocator) = guard.as_ref() else {
            return AllocatorStats {
                live: 0,
                used: 0,
                reserved: 0,
            };
        };

        let report = allocator.generate_report();

        AllocatorStats {
            live: report.allocations.len(),
            used: report.total_allocated_bytes,
            reserved: report.total_capacity_bytes,
        }
    }

    /// Suballocate memory satisfying `requirements`.
    ///
    /// `linear` distinguishes buffers and linear-tiled images from optimally
    /// tiled ones. Getting it wrong is not a validation error but a correctness
    /// one — Vulkan's buffer-image granularity rule means the two kinds may not
    /// share a page, and the allocator can only honour that if it is told which
    /// it is dealing with.
    pub(crate) fn allocate(
        &self,
        name: &str,
        requirements: vk::MemoryRequirements,
        location: MemoryLocation,
        linear: bool,
    ) -> Result<ga::Allocation, RhiError> {
        let mut guard = self.lock();
        let allocator = guard.as_mut().ok_or(RhiError::AllocatorShutDown)?;

        allocator
            .allocate(&ga::AllocationCreateDesc {
                name,
                requirements,
                location: location.to_gpu_allocator(),
                linear,
                // Suballocate. Dedicated allocations are what the driver wants
                // for very large render targets, and choosing between them is a
                // policy this crate will need eventually — but guessing at the
                // threshold with no measurements would be inventing a number.
                allocation_scheme: ga::AllocationScheme::GpuAllocatorManaged,
            })
            .map_err(|source| RhiError::Allocation {
                name: name.to_owned(),
                size: requirements.size,
                reason: source.to_string(),
            })
    }

    /// Return an allocation to the pool.
    ///
    /// Called from resource `Drop` impls, where failure cannot be propagated —
    /// hence the boolean rather than a `Result`. A `false` return means the
    /// allocator's bookkeeping is inconsistent, which is a bug in this crate.
    pub(crate) fn free(&self, allocation: ga::Allocation) -> bool {
        let mut guard = self.lock();
        let Some(allocator) = guard.as_mut() else {
            // The allocator was already shut down. Only reachable if a resource
            // outlived it, which `Arc<Allocator>` makes impossible.
            return false;
        };

        allocator.free(allocation).is_ok()
    }

    /// The internal lock, recovering the guard if a previous holder panicked.
    ///
    /// Poison recovery rather than propagation: the allocator's state is
    /// consistent regardless, because every path that holds this lock either
    /// completes its `gpu-allocator` call or fails before mutating anything.
    /// Propagating poison would turn one unrelated panic into a permanently
    /// unusable renderer.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<ga::Allocator>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for Allocator {
    fn drop(&mut self) {
        debug!(stats = ?self.stats(), "destroying GPU memory allocator");

        // Explicit rather than implicit so it happens *here*, while the device
        // is still alive — gpu-allocator's own `Drop` frees its device memory,
        // and the `Arc<Device>` field is only dropped after this function
        // returns. Also reports leaks through `log`, which the subscriber
        // captures.
        let mut guard = self.lock();
        drop(guard.take());
    }
}

impl std::fmt::Debug for Allocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Allocator")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}
