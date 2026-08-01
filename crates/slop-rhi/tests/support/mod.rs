//! Shared setup for the integration tests in this directory.
//!
//! A `mod.rs` inside a subdirectory rather than a `tests/support.rs`, because
//! Cargo compiles every top-level file in `tests/` as its own test binary and a
//! subdirectory is the one thing it does not.
//!
//! Each test file writes `mod support;` and uses what it needs.

// Each test binary compiles this module separately and uses only part of it, so
// the unused half warns in every one of them. There is no `pub(crate)` that
// helps here — the crates are genuinely separate. This is the standard cost of
// shared test helpers in Cargo, and it is the only place in the repository this
// attribute should appear.
#![allow(dead_code, reason = "each test binary uses a different subset")]

use std::sync::Arc;
use std::time::Duration;

use slop_rhi::{
    Allocator, CommandBuffer, Device, DeviceSelection, Instance, InstanceConfig, RhiError,
    TimelineSemaphore, vk,
};

/// A headless device, or `None` when the machine genuinely has no Vulkan loader.
///
/// Headless in the real sense: no surface is passed to enumeration, so no
/// present queue is found and `VK_KHR_swapchain` is never enabled. That is the
/// configuration CI runs in, and it exercises a different branch of device
/// creation from the windowed examples.
///
/// Returning `None` rather than panicking means a machine without a GPU reports
/// skipped tests instead of a wall of failures. The trade is that a silently
/// broken loader looks like a pass, which is why the reason is printed.
pub(crate) fn device() -> Option<Arc<Device>> {
    let filter = std::env::var("SLOP_LOG")
        .unwrap_or_else(|_| String::from(slop_core::diagnostics::DEFAULT_FILTER));
    slop_core::diagnostics::try_init(&filter);

    let instance = match Instance::new(&InstanceConfig::default()) {
        Ok(instance) => Arc::new(instance),
        Err(RhiError::LoaderUnavailable(_)) => {
            eprintln!("skipping: no Vulkan loader on this machine");
            return None;
        }
        Err(other) => panic!("instance creation failed: {other}"),
    };

    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");
    let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic)
        .expect("one adapter must be usable");

    Some(Arc::new(
        Device::new(&instance, &devices[chosen]).expect("device creation must succeed"),
    ))
}

/// A headless device and an allocator for it.
pub(crate) fn device_and_allocator() -> Option<(Arc<Device>, Arc<Allocator>)> {
    let device = device()?;
    let allocator = Allocator::new(&device).expect("allocator creation must succeed");

    Some((device, allocator))
}

/// Submit one recorded command buffer and block until the GPU finishes it.
///
/// Only correct in tests. A frame loop waits on the timeline value for a
/// *previous* frame so the CPU can run ahead; waiting on the submission just
/// made discards the pipelining entirely.
///
/// # Panics
///
/// Panics if submission fails or the GPU does not finish within five seconds,
/// since either means the test cannot report anything meaningful.
pub(crate) fn submit_and_wait(device: &Arc<Device>, command: &CommandBuffer) {
    let timeline = TimelineSemaphore::new(device, 0).expect("semaphore creation must succeed");

    let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(command.handle())];
    let signals = [vk::SemaphoreSubmitInfo::default()
        .semaphore(timeline.handle())
        .value(1)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];

    let submits = [vk::SubmitInfo2::default()
        .command_buffer_infos(&commands)
        .signal_semaphore_infos(&signals)];

    // SAFETY: the buffer is recorded and not pending, the timeline belongs to
    // this device, and every borrowed array outlives the call. `synchronization2`
    // is in the required feature tier, so `queue_submit2` is available.
    unsafe {
        device
            .raw()
            .queue_submit2(device.queues().graphics, &submits, vk::Fence::null())
    }
    .expect("submission must succeed");

    assert!(
        timeline
            .wait(1, Duration::from_secs(5))
            .expect("waiting must not fail"),
        "the GPU did not finish within five seconds"
    );
}
