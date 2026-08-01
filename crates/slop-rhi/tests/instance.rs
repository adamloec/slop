//! Instance creation against the real Vulkan loader.
//!
//! These need a working driver, so they live in `tests/` rather than beside the
//! code. A machine with no Vulkan loader at all skips them; every other failure
//! is reported, which keeps "this machine has no GPU" distinguishable from "our
//! code is wrong". Silently passing on any error would make the whole file
//! worthless the first time it mattered.

use std::ffi::CString;

use slop_rhi::{Instance, InstanceConfig, RhiError, Validation};

/// Returns `None` when the machine genuinely has no Vulkan loader, so the test
/// can skip. Any other error propagates.
fn try_create(config: &InstanceConfig) -> Result<Option<Instance>, RhiError> {
    match Instance::new(config) {
        Ok(instance) => Ok(Some(instance)),
        Err(RhiError::LoaderUnavailable(_)) => {
            eprintln!("skipping: no Vulkan loader on this machine");
            Ok(None)
        }
        Err(other) => Err(other),
    }
}

#[test]
fn creates_a_headless_instance() {
    // No surface extensions requested — this is the configuration DESIGN.md §5's
    // headless mode depends on, so it must work without a window.
    let config = InstanceConfig {
        application_name: String::from("slop-test"),
        ..Default::default()
    };

    let created = try_create(&config).expect("instance creation must not fail on a Vulkan machine");

    if let Some(instance) = created {
        // Touching the raw handle proves it is live rather than merely returned.
        assert_ne!(
            instance.raw().handle(),
            ash::vk::Instance::null(),
            "instance handle must not be null"
        );
    }
}

#[test]
fn validation_can_be_disabled_explicitly() {
    let config = InstanceConfig {
        validation: Validation::Disabled,
        ..Default::default()
    };

    if let Some(instance) = try_create(&config).expect("creation must succeed") {
        assert!(
            !instance.validation_enabled(),
            "validation must stay off when explicitly disabled"
        );
    }
}

#[test]
fn a_missing_extension_is_reported_by_name() {
    // The failure mode that matters: a driver lacking something we need should
    // say which thing, not return a bare error code.
    let config = InstanceConfig {
        required_extensions: vec![CString::new("VK_EXT_this_does_not_exist").expect("no NUL")],
        ..Default::default()
    };

    match Instance::new(&config) {
        Err(RhiError::MissingInstanceExtension(name)) => {
            assert_eq!(name, "VK_EXT_this_does_not_exist");
        }
        Err(RhiError::LoaderUnavailable(_)) => {
            eprintln!("skipping: no Vulkan loader on this machine");
        }
        Err(other) => panic!("expected MissingInstanceExtension, got {other}"),
        Ok(_) => panic!("a nonexistent extension must not succeed"),
    }
}

#[test]
fn instances_can_be_created_and_dropped_repeatedly() {
    // Guards the Drop order: destroying the debug messenger after its instance,
    // or double-destroying, shows up here as a validation error or a crash
    // rather than in whatever renders next.
    for _ in 0..4 {
        let created = try_create(&InstanceConfig::default()).expect("creation must succeed");

        if created.is_none() {
            return;
        }
    }
}
