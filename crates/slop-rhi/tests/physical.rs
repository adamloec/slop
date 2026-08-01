//! Physical device enumeration and selection against real hardware.
//!
//! Skips only when the machine has no Vulkan loader; every other failure is
//! reported. See `tests/instance.rs` for why.

use slop_rhi::{DeviceKind, DeviceSelection, Instance, InstanceConfig, RhiError};

/// `None` when the machine genuinely has no Vulkan loader.
fn instance() -> Option<Instance> {
    // Logging is what makes a failing run on someone else's machine
    // diagnosable, and the device-selection decision is logged at `info`.
    //
    // This test binary is an application, so reading the environment here is
    // correct — CONVENTIONS.md §5.1 puts that decision in the caller, which is
    // why `diagnostics` takes a filter rather than looking one up.
    let filter = std::env::var("SLOP_LOG")
        .unwrap_or_else(|_| String::from(slop_core::diagnostics::DEFAULT_FILTER));
    slop_core::diagnostics::try_init(&filter);

    match Instance::new(&InstanceConfig::default()) {
        Ok(instance) => Some(instance),
        Err(RhiError::LoaderUnavailable(_)) => {
            eprintln!("skipping: no Vulkan loader on this machine");
            None
        }
        Err(other) => panic!("instance creation failed: {other}"),
    }
}

#[test]
fn enumerates_the_machines_adapters() {
    let Some(instance) = instance() else { return };

    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");

    assert!(
        !devices.is_empty(),
        "a machine with a Vulkan loader must expose at least one adapter"
    );

    for device in &devices {
        eprintln!(
            "  {} — {:?}, {} MiB, usable: {}{}",
            device.name,
            device.kind,
            device.memory_mib(),
            device.is_usable(),
            device
                .rejection
                .as_ref()
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default()
        );
    }
}

#[test]
fn automatic_selection_picks_a_usable_device() {
    let Some(instance) = instance() else { return };
    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");

    let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic)
        .expect("at least one adapter must be usable");

    assert!(devices[chosen].is_usable());
    assert!(
        devices[chosen].queue_families().is_some(),
        "a usable device must have queue families resolved"
    );
}

#[test]
fn automatic_selection_never_prefers_a_software_rasterizer() {
    // On a machine with real hardware, picking lavapipe would mean rendering
    // thousands of times slower with no indication anything was wrong.
    let Some(instance) = instance() else { return };
    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");

    let has_hardware = devices
        .iter()
        .any(|device| device.is_usable() && device.kind != DeviceKind::Cpu);

    if !has_hardware {
        eprintln!("skipping: this machine exposes only software rasterizers");
        return;
    }

    let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic).expect("one is usable");

    assert_ne!(
        devices[chosen].kind,
        DeviceKind::Cpu,
        "chose a software rasterizer over real hardware"
    );
}

#[test]
fn a_saved_uuid_round_trips_to_the_same_device() {
    // The property a game's graphics settings depend on: persist the UUID of
    // the chosen adapter, and get that same adapter back on next launch.
    let Some(instance) = instance() else { return };
    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");

    let first = slop_rhi::select(&devices, &DeviceSelection::Automatic).expect("one is usable");
    let saved = devices[first].uuid;

    // Re-enumerating models a fresh launch rather than reusing the same list.
    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");
    let second =
        slop_rhi::select(&devices, &DeviceSelection::ByUuid(saved)).expect("the device is present");

    assert_eq!(devices[second].uuid, saved);
    assert_eq!(devices[second].name, devices[first].name);
}

#[test]
fn device_uuids_are_unique_and_nonzero() {
    // A driver reporting a zero or duplicated UUID would silently break saved
    // graphics settings, so it is worth knowing immediately.
    let Some(instance) = instance() else { return };
    let devices = slop_rhi::enumerate(&instance, None).expect("enumeration must succeed");

    for device in &devices {
        assert_ne!(
            device.uuid, [0u8; 16],
            "{} reported an all-zero UUID",
            device.name
        );
    }

    let mut seen: Vec<_> = devices.iter().map(|device| device.uuid).collect();
    let total = seen.len();
    seen.sort_unstable();
    seen.dedup();

    assert_eq!(seen.len(), total, "two adapters reported the same UUID");
}
