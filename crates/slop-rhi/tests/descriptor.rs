//! The bindless descriptor heap against real hardware.
//!
//! Validation is doing most of the work here. A descriptor heap that is
//! structurally wrong — a pool flag not matching its layout, a write past a
//! binding's capacity, a set used before it is written — produces either a
//! validation error or undefined behaviour the driver happens to tolerate, and
//! almost never a wrong value a plain assertion would catch. So these tests
//! build the heap, fill it, and bind it, and the assertion that matters is the
//! absence of validation output.

mod support;

use std::sync::Arc;

use slop_rhi::{
    BindlessHeap, BindlessHeapConfig, Extent2D, Format, Image, ImageConfig, ImageState, ImageUsage,
    PipelineLayout, PipelineLayoutConfig, vk,
};

#[test]
fn the_required_feature_tier_is_still_satisfiable() {
    // The first thing to break when features are added to `device/features.rs`,
    // and it breaks by rejecting every adapter rather than by failing to
    // compile. Worth its own test precisely because the bindless work added
    // three requirements.
    let Some(device) = support::device() else {
        return;
    };

    assert_ne!(device.queue_families().graphics, u32::MAX);
}

#[test]
fn a_heap_is_created_with_a_capacity_no_larger_than_requested() {
    let Some(device) = support::device() else {
        return;
    };

    let requested = BindlessHeapConfig::default();
    let heap = BindlessHeap::new(&device, &requested).expect("heap creation must succeed");
    let granted = heap.capacity();

    assert!(granted.sampled_images <= requested.sampled_images);
    assert!(granted.samplers <= requested.samplers);
    assert!(granted.storage_images <= requested.storage_images);

    // A device granting nothing would pass the bounds above while being
    // useless, so the floor matters as much as the ceiling.
    assert!(
        granted.sampled_images >= 1024,
        "any desktop GPU should manage 1024 sampled images, got {}",
        granted.sampled_images
    );

    assert_eq!(heap.occupancy().sampled_images, 0);
}

#[test]
fn an_absurd_request_is_clamped_rather_than_rejected() {
    // Clamping is the whole reason `capacity()` exists. Failing instead would
    // be the capability-tier branching `DESIGN.md` §2.1 exists to avoid, in its
    // harshest form — a device rejected for supporting "only" a hundred
    // thousand textures.
    let Some(device) = support::device() else {
        return;
    };

    let heap = BindlessHeap::new(
        &device,
        &BindlessHeapConfig {
            sampled_images: u32::MAX,
            samplers: u32::MAX,
            storage_images: u32::MAX,
            storage_buffers: u32::MAX,
        },
    )
    .expect("an oversized request must clamp, not fail");

    assert!(heap.capacity().sampled_images < u32::MAX);
}

#[test]
fn inserting_a_texture_returns_a_slot_a_shader_could_index() {
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };

    let mut heap =
        BindlessHeap::new(&device, &BindlessHeapConfig::default()).expect("heap creation");
    let texture = texture(&allocator);

    let handle = heap
        .insert_sampled_image(texture.view(), ImageState::SHADER_READ)
        .expect("an empty heap must have room");

    // The index is what reaches the GPU; the generation stays on the CPU.
    assert_eq!(handle.index(), 0, "the first slot should be index 0");
    assert!(heap.is_live_sampled_image(handle));
    assert_eq!(heap.occupancy().sampled_images, 1);

    device.wait_idle().expect("the device must go idle");
}

#[test]
fn slots_are_reused_after_removal_and_stale_handles_stop_resolving() {
    // The reason handles carry a generation at all. A texture unloaded and
    // another loaded into its slot must not leave the first handle silently
    // pointing at the second.
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };

    let mut heap =
        BindlessHeap::new(&device, &BindlessHeapConfig::default()).expect("heap creation");
    let texture = texture(&allocator);

    let first = heap
        .insert_sampled_image(texture.view(), ImageState::SHADER_READ)
        .expect("room");

    assert!(heap.remove_sampled_image(first));
    assert!(
        !heap.is_live_sampled_image(first),
        "the handle is now stale"
    );
    assert!(
        !heap.remove_sampled_image(first),
        "removing twice must not succeed"
    );

    let second = heap
        .insert_sampled_image(texture.view(), ImageState::SHADER_READ)
        .expect("room");

    assert_eq!(second.index(), first.index(), "the slot should be reused");
    assert_ne!(
        second.generation(),
        first.generation(),
        "but the generation must have moved"
    );
    assert!(!heap.is_live_sampled_image(first));
    assert!(heap.is_live_sampled_image(second));

    device.wait_idle().expect("the device must go idle");
}

#[test]
fn a_full_heap_returns_none_rather_than_panicking() {
    // Running out of texture slots is a content problem a game may want to
    // report. A crash is the least useful response available.
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };

    let mut heap = BindlessHeap::new(
        &device,
        &BindlessHeapConfig {
            sampled_images: 4,
            samplers: 1,
            storage_images: 1,
            storage_buffers: 1,
        },
    )
    .expect("heap creation");

    let texture = texture(&allocator);
    let capacity = heap.capacity().sampled_images;

    for slot in 0..capacity {
        assert!(
            heap.insert_sampled_image(texture.view(), ImageState::SHADER_READ)
                .is_some(),
            "slot {slot} should fit"
        );
    }

    assert!(
        heap.insert_sampled_image(texture.view(), ImageState::SHADER_READ)
            .is_none(),
        "the heap is full and must say so"
    );

    device.wait_idle().expect("the device must go idle");
}

#[test]
fn many_textures_can_be_written_while_the_set_is_live() {
    // What `UPDATE_AFTER_BIND` and `PARTIALLY_BOUND` buy: a heap sized for
    // thousands, holding a handful, written into repeatedly. Without
    // `PARTIALLY_BOUND` the set would be invalid until every one of its
    // descriptors had been written.
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };

    let mut heap =
        BindlessHeap::new(&device, &BindlessHeapConfig::default()).expect("heap creation");
    let texture = texture(&allocator);

    for _ in 0..512 {
        heap.insert_sampled_image(texture.view(), ImageState::SHADER_READ)
            .expect("room");
    }

    assert_eq!(heap.occupancy().sampled_images, 512);

    device.wait_idle().expect("the device must go idle");
}

#[test]
fn a_pipeline_layout_accepts_the_heap_and_push_constants() {
    // The join between the heap and a pipeline. A layout the driver rejects
    // here would otherwise fail at pipeline creation, one step removed from the
    // cause.
    let Some(device) = support::device() else {
        return;
    };

    let heap = BindlessHeap::new(&device, &BindlessHeapConfig::default()).expect("heap creation");

    let layout = PipelineLayout::new(
        &device,
        &PipelineLayoutConfig {
            heap: Some(&heap),
            // The guaranteed minimum. Anything a draw needs beyond this belongs
            // in a buffer reached by address.
            push_constant_bytes: 128,
        },
    )
    .expect("layout creation must succeed");

    assert_ne!(layout.handle(), vk::PipelineLayout::null());
}

#[test]
fn an_empty_layout_is_still_valid() {
    // `empty()` routes through `new()` now, and a zero-sized push-constant
    // range is invalid Vulkan — so this is the test that the zero case is
    // special-cased rather than passed through.
    let Some(device) = support::device() else {
        return;
    };

    let layout = PipelineLayout::empty(&device).expect("an empty layout must be valid");

    assert_ne!(layout.handle(), vk::PipelineLayout::null());
}

#[test]
fn heaps_can_be_created_and_dropped_repeatedly() {
    // Descriptor pools are the easiest Vulkan object to leak, because
    // destroying one implicitly frees its sets and it is tempting to free them
    // explicitly as well. Validation reports either mistake here.
    let Some(device) = support::device() else {
        return;
    };

    for _ in 0..8 {
        let heap =
            BindlessHeap::new(&device, &BindlessHeapConfig::default()).expect("heap creation");

        assert_ne!(heap.set(), vk::DescriptorSet::null());
    }
}

/// A small sampled image to put in the heap.
fn texture(allocator: &Arc<slop_rhi::Allocator>) -> Image {
    Image::new(
        allocator,
        &ImageConfig {
            name: "descriptor test texture",
            extent: Extent2D {
                width: 4,
                height: 4,
            },
            format: Format::Rgba8Unorm,
            usage: ImageUsage::SAMPLED | ImageUsage::TRANSFER_DST,
            mip_levels: 1,
            array_layers: 1,
        },
    )
    .expect("the texture must be creatable")
}
