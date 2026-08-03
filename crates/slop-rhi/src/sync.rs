//! Synchronization primitives.
//!
//! `docs/DESIGN.md` §2.2 commits to timeline semaphores rather than fences plus
//! binary semaphores. A timeline semaphore is a monotonically increasing 64-bit
//! counter that both the host and the device can wait on and signal, which
//! subsumes three older primitives at once:
//!
//! | Older primitive | Replaced by |
//! |---|---|
//! | Fence — device signals, host waits | Host waiting on a timeline value |
//! | Binary semaphore — device to device | Device waiting on a timeline value |
//! | Event — fine-grained ordering | Timeline values within a queue |
//!
//! The practical difference is that a timeline value can be waited on *before*
//! it is signalled, and waited on repeatedly by any number of waiters. Binary
//! semaphores can be waited exactly once and must be signalled first, which is
//! what makes frame-in-flight bookkeeping with them so error-prone.
//!
//! # The one exception the spec forces
//!
//! [`BinarySemaphore`] exists because `vkAcquireNextImageKHR` and
//! `vkQueuePresentKHR` **do not accept timeline semaphores**. That is a Vulkan
//! limitation, not a design choice, and it is the only place binary semaphores
//! are permitted in this engine. Everything else — queue-to-queue ordering,
//! frame pacing, host readback — uses [`TimelineSemaphore`].

use std::sync::Arc;
use std::time::Duration;

use ash::vk;

use crate::{Device, RhiError, SemaphoreHandle};

/// Wait forever. `u64::MAX` nanoseconds is roughly 584 years, which Vulkan
/// treats as "no timeout".
const NO_TIMEOUT: u64 = u64::MAX;

/// A monotonically increasing counter both the host and device can wait on.
///
/// The engine's default synchronization primitive. Values must only ever
/// increase; signalling a value lower than the current one is undefined
/// behaviour, which is why [`signal`](Self::signal) is checked in debug builds.
pub struct TimelineSemaphore {
    handle: vk::Semaphore,
    device: Arc<Device>,
}

impl TimelineSemaphore {
    /// Create a timeline semaphore starting at `initial`.
    ///
    /// # Errors
    ///
    /// Fails if the driver rejects creation.
    pub fn new(device: &Arc<Device>, initial: u64) -> Result<Self, RhiError> {
        let mut type_info = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(initial);
        let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut type_info);

        // SAFETY: `create_info` borrows `type_info`, which outlives the call,
        // and the timeline feature was verified present during device selection.
        let handle = unsafe { device.raw().create_semaphore(&create_info, None) }?;

        Ok(Self {
            handle,
            device: Arc::clone(device),
        })
    }

    /// The underlying handle, for submission structures.
    pub fn handle(&self) -> SemaphoreHandle {
        SemaphoreHandle(self.handle)
    }

    /// The current counter value.
    ///
    /// # Errors
    ///
    /// Fails if the device was lost.
    pub fn value(&self) -> Result<u64, RhiError> {
        // SAFETY: the semaphore belongs to this device and is alive.
        let value = unsafe { self.device.raw().get_semaphore_counter_value(self.handle) }?;

        Ok(value)
    }

    /// Block until the counter reaches `value`, or the timeout elapses.
    ///
    /// Returns `true` if the value was reached, `false` on timeout. A timeout is
    /// deliberately not an error: waiting with a deadline and not reaching it is
    /// an expected outcome, and forcing callers to pattern-match an error
    /// variant for it invites treating a normal case as a failure.
    ///
    /// # Errors
    ///
    /// Fails if the device was lost.
    pub fn wait(&self, value: u64, timeout: Duration) -> Result<bool, RhiError> {
        let handles = [self.handle];
        let values = [value];
        let info = vk::SemaphoreWaitInfo::default()
            .semaphores(&handles)
            .values(&values);

        let nanos = u64::try_from(timeout.as_nanos()).unwrap_or(NO_TIMEOUT);

        // SAFETY: `info` borrows arrays that outlive the call, and the semaphore
        // belongs to this device.
        match unsafe { self.device.raw().wait_semaphores(&info, nanos) } {
            Ok(()) => Ok(true),
            Err(vk::Result::TIMEOUT) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Block until the counter reaches `value`, however long that takes.
    ///
    /// # Errors
    ///
    /// Fails if the device was lost.
    pub fn wait_forever(&self, value: u64) -> Result<(), RhiError> {
        self.wait(value, Duration::from_nanos(NO_TIMEOUT))?;

        Ok(())
    }

    /// Signal the counter from the host.
    ///
    /// Device-side signalling happens through submission instead; this is for
    /// the host to unblock waiters directly.
    ///
    /// # Panics
    ///
    /// In debug builds, if `value` is not greater than the current value.
    /// Timeline counters must increase monotonically, and going backwards is
    /// undefined behaviour that Vulkan will not report.
    ///
    /// # Errors
    ///
    /// Fails if the device was lost.
    pub fn signal(&self, value: u64) -> Result<(), RhiError> {
        debug_assert!(
            self.value().is_ok_and(|current| value > current),
            "timeline semaphore values must strictly increase"
        );

        let info = vk::SemaphoreSignalInfo::default()
            .semaphore(self.handle)
            .value(value);

        // SAFETY: the semaphore belongs to this device and is alive.
        unsafe { self.device.raw().signal_semaphore(&info) }?;

        Ok(())
    }
}

impl Drop for TimelineSemaphore {
    fn drop(&mut self) {
        // SAFETY: created from this device, destroyed exactly once, and the
        // device outlives this because we hold an `Arc` to it.
        unsafe { self.device.raw().destroy_semaphore(self.handle, None) };
    }
}

impl std::fmt::Debug for TimelineSemaphore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimelineSemaphore")
            .field("value", &self.value().ok())
            .finish_non_exhaustive()
    }
}

/// A single-use device-to-device signal.
///
/// **Only for swapchain acquire and present.** `vkAcquireNextImageKHR` and
/// `vkQueuePresentKHR` do not accept timeline semaphores, so this type exists to
/// satisfy that spec limitation and nothing else. Reach for
/// [`TimelineSemaphore`] everywhere else — see the module documentation for why.
pub struct BinarySemaphore {
    handle: vk::Semaphore,
    device: Arc<Device>,
}

impl BinarySemaphore {
    /// Create an unsignalled binary semaphore.
    ///
    /// # Errors
    ///
    /// Fails if the driver rejects creation.
    pub fn new(device: &Arc<Device>) -> Result<Self, RhiError> {
        let create_info = vk::SemaphoreCreateInfo::default();

        // SAFETY: `create_info` is a fully initialized default with no pNext
        // chain to outlive.
        let handle = unsafe { device.raw().create_semaphore(&create_info, None) }?;

        Ok(Self {
            handle,
            device: Arc::clone(device),
        })
    }

    /// The underlying handle, for acquire and present structures.
    pub fn handle(&self) -> SemaphoreHandle {
        SemaphoreHandle(self.handle)
    }
}

impl Drop for BinarySemaphore {
    fn drop(&mut self) {
        // SAFETY: created from this device, destroyed exactly once, and the
        // device outlives this because we hold an `Arc` to it.
        unsafe { self.device.raw().destroy_semaphore(self.handle, None) };
    }
}

impl std::fmt::Debug for BinarySemaphore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinarySemaphore").finish_non_exhaustive()
    }
}
