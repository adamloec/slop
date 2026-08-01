//! Timeline and binary semaphores against real hardware.
//!
//! Timeline semaphores are testable without rendering anything, because the host
//! can both signal and wait on them. That makes this the last piece of the RHI
//! that can be verified in isolation before frames exist.

use std::sync::Arc;
use std::time::Duration;

use slop_rhi::{
    BinarySemaphore, Device, DeviceSelection, Instance, InstanceConfig, RhiError, TimelineSemaphore,
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
fn a_timeline_starts_at_its_initial_value() {
    let Some(device) = device() else { return };

    let semaphore = TimelineSemaphore::new(&device, 7).expect("creation must succeed");

    assert_eq!(semaphore.value().expect("readable"), 7);
}

#[test]
fn host_signalling_advances_the_counter() {
    let Some(device) = device() else { return };
    let semaphore = TimelineSemaphore::new(&device, 0).expect("creation must succeed");

    semaphore.signal(1).expect("signal must succeed");
    semaphore.signal(5).expect("signal must succeed");

    assert_eq!(semaphore.value().expect("readable"), 5);
}

#[test]
fn waiting_for_an_already_reached_value_returns_immediately() {
    // The property binary semaphores lack: a timeline value can be waited on
    // after it has passed, by any number of waiters.
    let Some(device) = device() else { return };
    let semaphore = TimelineSemaphore::new(&device, 10).expect("creation must succeed");

    assert!(
        semaphore
            .wait(3, Duration::from_millis(0))
            .expect("wait must not fail"),
        "a value already passed must be satisfied instantly"
    );
    assert!(
        semaphore
            .wait(10, Duration::from_millis(0))
            .expect("wait must not fail"),
        "the exact current value must be satisfied"
    );
}

#[test]
fn a_value_that_never_arrives_times_out_rather_than_failing() {
    // Timing out is an expected outcome, not an error. Forcing callers to
    // pattern-match an error variant for it invites treating a normal case as a
    // failure.
    let Some(device) = device() else { return };
    let semaphore = TimelineSemaphore::new(&device, 0).expect("creation must succeed");

    let reached = semaphore
        .wait(99, Duration::from_millis(10))
        .expect("a timeout must not be reported as an error");

    assert!(!reached, "the value was never signalled");
}

#[test]
fn a_waiter_is_released_by_a_later_signal() {
    // Waiting before the signal is exactly what a binary semaphore cannot do.
    let Some(device) = device() else { return };
    let semaphore = Arc::new(TimelineSemaphore::new(&device, 0).expect("creation must succeed"));

    let waiter = {
        let semaphore = Arc::clone(&semaphore);
        std::thread::spawn(move || {
            semaphore
                .wait(1, Duration::from_secs(5))
                .expect("wait must not fail")
        })
    };

    semaphore.signal(1).expect("signal must succeed");

    assert!(
        waiter.join().expect("the waiting thread must not panic"),
        "the waiter should have been released by the signal"
    );
}

#[test]
fn timelines_and_binaries_can_be_created_and_dropped_repeatedly() {
    // With validation active, a leaked or double-destroyed semaphore is
    // reported here rather than at shutdown of something larger.
    let Some(device) = device() else { return };

    for value in 0..8 {
        let timeline = TimelineSemaphore::new(&device, value).expect("creation must succeed");
        let binary = BinarySemaphore::new(&device).expect("creation must succeed");

        assert_eq!(timeline.value().expect("readable"), value);
        drop(binary);
    }
}
