//! Logical device creation against real hardware.
//!
//! Validation layers are active in these runs, so a wrong drop order, a leaked
//! child object, or a malformed create-info surfaces here as a validation error
//! rather than in whatever renders next.

use std::sync::Arc;

use ash::vk;
use slop_rhi::{Device, DeviceSelection, Instance, InstanceConfig, QueueHandle, RhiError};

/// `None` when the machine genuinely has no Vulkan loader.
fn instance() -> Option<Arc<Instance>> {
    // A test binary is an application, so reading the environment here is
    // correct — see CONVENTIONS.md §5.1.
    let filter = std::env::var("SLOP_LOG")
        .unwrap_or_else(|_| String::from(slop_core::diagnostics::DEFAULT_FILTER));
    slop_core::diagnostics::try_init(&filter);

    match Instance::new(&InstanceConfig::default()) {
        Ok(instance) => Some(Arc::new(instance)),
        Err(RhiError::LoaderUnavailable(_)) => {
            eprintln!("skipping: no Vulkan loader on this machine");
            None
        }
        Err(other) => panic!("instance creation failed: {other}"),
    }
}

/// The automatically selected device, or `None` to skip.
fn device() -> Option<Device> {
    let instance = instance()?;
    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");
    let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic)
        .expect("one adapter must be usable");

    Some(Device::new(&instance, &devices[chosen]).expect("device creation must succeed"))
}

#[test]
fn creates_a_logical_device_on_the_selected_adapter() {
    let Some(device) = device() else { return };

    assert_ne!(
        device.raw().handle(),
        vk::Device::null(),
        "device handle must not be null"
    );
}

#[test]
fn every_queue_is_live() {
    let Some(device) = device() else { return };
    let queues = device.queues();

    for (name, queue) in [
        ("graphics", queues.graphics),
        ("compute", queues.compute),
        ("transfer", queues.transfer),
    ] {
        assert_ne!(
            queue,
            QueueHandle::default(),
            "{name} queue must not be null"
        );
    }

    // Headless, so nothing was asked to present.
    assert!(
        queues.present.is_none(),
        "no surface was supplied, so there must be no present queue"
    );
}

#[test]
fn queues_from_coinciding_families_are_the_same_handle() {
    // Not a defect — it is what "no async compute on this device" looks like.
    // The engine must behave correctly either way, so the relationship between
    // family indices and queue handles is worth pinning.
    let Some(device) = device() else { return };
    let families = device.queue_families();
    let queues = device.queues();

    if families.compute == families.graphics {
        assert_eq!(queues.compute, queues.graphics);
    }

    if families.transfer == families.graphics {
        assert_eq!(queues.transfer, queues.graphics);
    }
}

#[test]
fn waiting_for_idle_succeeds_on_a_fresh_device() {
    let Some(device) = device() else { return };

    device
        .wait_idle()
        .expect("a device with no work must go idle");
}

#[test]
fn devices_can_be_created_and_dropped_repeatedly() {
    // Guards the drop order and the wait-idle in `Drop`. With validation active,
    // destroying a device while children are live, or leaking one, is reported.
    let Some(instance) = instance() else { return };
    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");
    let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic).expect("one is usable");

    for _ in 0..4 {
        let device = Device::new(&instance, &devices[chosen]).expect("creation must succeed");
        device.wait_idle().expect("must go idle");
    }
}

#[test]
fn the_instance_outlives_devices_made_from_it() {
    // The Arc is what makes this true by construction rather than by discipline:
    // dropping the caller's handle must not destroy the instance underneath a
    // live device.
    let Some(instance) = instance() else { return };
    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");
    let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic).expect("one is usable");
    let device = Device::new(&instance, &devices[chosen]).expect("creation must succeed");

    drop(devices);
    drop(instance);

    // Still usable, and still safe to destroy, with the instance held only by
    // the device itself.
    device.wait_idle().expect("device must still be valid");
}

#[test]
fn adapters_rejected_for_features_say_which_ones() {
    // Diagnostic rather than assertion: on hardware meeting the tier nothing is
    // rejected, and printing the reasons is what makes a failure on someone
    // else's machine actionable.
    let Some(instance) = instance() else { return };
    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");

    for device in &devices {
        match &device.rejection {
            Some(reason) => eprintln!("  {} rejected: {reason}", device.name),
            None => eprintln!("  {} meets the required feature tier", device.name),
        }
    }
}
