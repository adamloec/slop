//! Command pools and buffers against real hardware.
//!
//! Recording is verified by actually submitting to a queue and waiting on a
//! timeline semaphore — a command buffer that records without error but never
//! executes would pass a weaker test while being useless.

use std::sync::Arc;
use std::time::Duration;

use slop_rhi::{
    CommandPool, Device, DeviceSelection, Instance, InstanceConfig, RhiError, TimelineSemaphore, vk,
};

/// `None` when the machine genuinely has no Vulkan loader.
fn device() -> Option<Arc<Device>> {
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

#[test]
fn a_pool_allocates_the_requested_number_of_buffers() {
    let Some(device) = device() else { return };
    let pool = CommandPool::new(&device, device.queue_families().graphics)
        .expect("pool creation must succeed");

    let buffers = pool.allocate(3).expect("allocation must succeed");

    assert_eq!(buffers.len(), 3);
    for buffer in &buffers {
        assert_ne!(buffer.handle(), vk::CommandBuffer::null());
    }
}

#[test]
fn an_empty_buffer_records_and_ends() {
    let Some(device) = device() else { return };
    let pool = CommandPool::new(&device, device.queue_families().graphics).expect("pool");
    let buffers = pool.allocate(1).expect("allocation");

    buffers[0].begin().expect("begin must succeed");
    buffers[0].end().expect("end must succeed");
}

#[test]
fn a_submitted_buffer_signals_its_timeline_value() {
    // The real test: recorded work reaches the GPU, executes, and the timeline
    // advances. Recording without submitting would pass a weaker test while
    // proving nothing.
    let Some(device) = device() else { return };
    let pool = CommandPool::new(&device, device.queue_families().graphics).expect("pool");
    let buffers = pool.allocate(1).expect("allocation");
    let timeline = TimelineSemaphore::new(&device, 0).expect("semaphore");

    buffers[0].begin().expect("begin");
    buffers[0].end().expect("end");

    submit(&device, buffers[0].handle(), &timeline, 1);

    assert!(
        timeline
            .wait(1, Duration::from_secs(5))
            .expect("wait must not fail"),
        "the GPU should have signalled the timeline"
    );
    assert_eq!(timeline.value().expect("readable"), 1);
}

#[test]
fn a_pool_can_be_reset_and_its_buffers_reused() {
    // The frame-loop pattern: wait on the timeline value the frame signalled,
    // then reset the whole pool rather than individual buffers.
    let Some(device) = device() else { return };
    let pool = CommandPool::new(&device, device.queue_families().graphics).expect("pool");
    let buffers = pool.allocate(1).expect("allocation");
    let timeline = TimelineSemaphore::new(&device, 0).expect("semaphore");

    for frame in 1..=4 {
        buffers[0].begin().expect("begin");
        buffers[0].end().expect("end");

        submit(&device, buffers[0].handle(), &timeline, frame);

        assert!(
            timeline
                .wait(frame, Duration::from_secs(5))
                .expect("wait must not fail"),
            "frame {frame} should have completed"
        );

        // Safe only because the wait above proved nothing from this pool is
        // still executing.
        pool.reset().expect("reset must succeed");
    }

    assert_eq!(timeline.value().expect("readable"), 4);
}

#[test]
fn pools_can_be_created_and_dropped_repeatedly() {
    // Destroying a pool frees its buffers; with validation active, leaking one
    // or double-freeing is reported here.
    let Some(device) = device() else { return };

    for _ in 0..8 {
        let pool = CommandPool::new(&device, device.queue_families().graphics).expect("pool");
        let buffers = pool.allocate(2).expect("allocation");

        assert_eq!(buffers.len(), 2);
    }
}

/// Submit one command buffer, signalling `value` on `timeline` when it
/// completes.
fn submit(
    device: &Arc<Device>,
    buffer: vk::CommandBuffer,
    timeline: &TimelineSemaphore,
    value: u64,
) {
    let command_infos = [vk::CommandBufferSubmitInfo::default().command_buffer(buffer)];
    let signal_infos = [vk::SemaphoreSubmitInfo::default()
        .semaphore(timeline.handle().raw())
        .value(value)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];

    let submit_info = vk::SubmitInfo2::default()
        .command_buffer_infos(&command_infos)
        .signal_semaphore_infos(&signal_infos);
    let submits = [submit_info];

    // SAFETY: the buffer is recorded and not pending, the timeline belongs to
    // this device, and every borrowed array outlives the call. `synchronization2`
    // is part of the required feature tier, so `queue_submit2` is available.
    unsafe {
        device
            .raw()
            .queue_submit2(device.queues().graphics.raw(), &submits, vk::Fence::null())
    }
    .expect("submission must succeed");
}
