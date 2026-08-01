//! Loading cooked SPIR-V against real hardware.
//!
//! Uses the artifact `slop-cli cook` actually produced, so this exercises the
//! whole path — Slang source, cook cache, engine load — rather than a synthetic
//! blob. If the cache is missing, the test says how to produce it instead of
//! failing obscurely.

use std::path::PathBuf;
use std::sync::Arc;

use slop_rhi::{Device, DeviceSelection, Instance, InstanceConfig, RhiError, ShaderModule};

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

/// The cooked triangle, or `None` with an explanation if it has not been cooked.
///
/// Dev-only path resolution: the workspace root is two levels above this crate.
/// The asset VFS at M2 replaces this; hard-coding it here is honest about being
/// a placeholder rather than pretending to be a lookup.
fn cooked_triangle() -> Option<Vec<u8>> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let path = workspace
        .join(".slop")
        .join("cache")
        .join("shaders")
        .join("passes")
        .join("triangle.spv");

    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            eprintln!(
                "skipping: {} not found — run `cargo run -p slop-cli -- cook`",
                path.display()
            );
            None
        }
    }
}

#[test]
fn the_cooked_triangle_loads() {
    let Some(device) = device() else { return };
    let Some(bytes) = cooked_triangle() else {
        return;
    };

    let module = ShaderModule::from_bytes(&device, &bytes).expect("cooked SPIR-V must load");

    assert_ne!(module.handle(), slop_rhi::vk::ShaderModule::null());
}

#[test]
fn a_text_file_is_rejected_as_not_spirv() {
    // The realistic corruption: a build step wrote a log or an error message
    // where an artifact should be. Reporting the magic number found makes that
    // obvious instead of handing the driver garbage.
    let Some(device) = device() else { return };

    match ShaderModule::from_bytes(&device, b"this is not a shader at all!") {
        Err(RhiError::NotSpirv { found_magic }) => {
            assert_ne!(found_magic, 0x0723_0203);
        }
        other => panic!("expected NotSpirv, got {other:?}"),
    }
}

#[test]
fn a_truncated_artifact_is_rejected() {
    // SPIR-V is whole 32-bit words; a byte count that is not a multiple of four
    // means the file was cut short.
    let Some(device) = device() else { return };

    assert!(matches!(
        ShaderModule::from_bytes(&device, &[0x03, 0x02, 0x23]),
        Err(RhiError::SpirvNotWordAligned { length: 3 })
    ));
}

#[test]
fn an_empty_artifact_is_rejected() {
    // Word-aligned, but there is no magic number to check — the case a naive
    // `first()` check would let through as `None`.
    let Some(device) = device() else { return };

    assert!(matches!(
        ShaderModule::from_bytes(&device, &[]),
        Err(RhiError::NotSpirv { .. })
    ));
}

#[test]
fn modules_can_be_loaded_and_dropped_repeatedly() {
    let Some(device) = device() else { return };
    let Some(bytes) = cooked_triangle() else {
        return;
    };

    for _ in 0..8 {
        let module = ShaderModule::from_bytes(&device, &bytes).expect("must load");

        assert_ne!(module.handle(), slop_rhi::vk::ShaderModule::null());
    }
}
