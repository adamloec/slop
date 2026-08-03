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

use slop_rhi::{
    Allocator, CommandBuffer, Device, DeviceSelection, Instance, InstanceConfig, RhiError,
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
/// Thin wrapper over [`slop_rhi::submit_recorded_and_wait`] that panics instead
/// of returning, because a test cannot report anything meaningful past a failed
/// submission. The blocking itself is only correct here: a frame loop waits on
/// the timeline value for a *previous* frame so the CPU can run ahead, and
/// waiting on the submission just made discards the pipelining entirely.
///
/// # Panics
///
/// Panics if submission fails or the GPU does not finish in time.
pub(crate) fn submit_and_wait(device: &Arc<Device>, command: &CommandBuffer) {
    slop_rhi::submit_recorded_and_wait(device, command)
        .expect("the submission must complete within the one-shot timeout");
}
