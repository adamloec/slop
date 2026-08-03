//! Staging uploads and depth attachments against real hardware.
//!
//! These are the two pieces the cube needs that the triangle did not: getting
//! CPU data into device-local memory, and having something to occlude with.
//!
//! Depth is tested by round-tripping the buffer back to the CPU rather than by
//! looking at colour, because reverse-Z fails *silently*. A pipeline with the
//! conventional `LESS` comparison and a 1.0 clear renders a plausible image in
//! which the furthest surface wins at every pixel — no validation error, no
//! crash, and nothing obviously wrong in a screenshot of convex geometry.

mod support;

use std::sync::Arc;

use slop_rhi::{
    Allocator, Buffer, BufferConfig, BufferState, CommandPool, DEPTH_CLEAR, Image, ImageConfig,
    ImageState, MemoryLocation, vk,
};

/// Upload a byte pattern to device-local memory and read it back.
#[test]
fn data_survives_a_round_trip_through_device_local_memory() {
    // The whole staging path: host-visible source, device-local middle,
    // host-visible destination. The middle is the part that cannot be mapped,
    // which is why the two copies exist.
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };

    let payload: Vec<u8> = (0..1024_u32).map(|value| (value % 251) as u8).collect();

    let mut staging = buffer(
        &allocator,
        "staging upload",
        payload.len() as u64,
        vk::BufferUsageFlags::TRANSFER_SRC,
        MemoryLocation::Upload,
    );
    staging
        .mapped_mut()
        .expect("upload memory must be mappable")
        .copy_from_slice(&payload);

    let device_local = buffer(
        &allocator,
        "device local",
        payload.len() as u64,
        vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::TRANSFER_SRC,
        MemoryLocation::DeviceOnly,
    );

    let readback = buffer(
        &allocator,
        "readback",
        payload.len() as u64,
        vk::BufferUsageFlags::TRANSFER_DST,
        MemoryLocation::Readback,
    );

    let pool = CommandPool::new(&device, device.queue_families().graphics).expect("pool");
    let command = pool.allocate(1).expect("allocation").pop().expect("one");

    command.begin().expect("begin");
    command.barrier_buffer(
        staging.handle(),
        BufferState::HOST_WRITE,
        BufferState::TRANSFER_SRC,
    );
    command.copy_buffer(
        staging.handle(),
        device_local.handle(),
        payload.len() as u64,
    );
    command.barrier_buffer(
        device_local.handle(),
        BufferState::TRANSFER_DST,
        BufferState::TRANSFER_SRC,
    );
    command.copy_buffer(
        device_local.handle(),
        readback.handle(),
        payload.len() as u64,
    );
    command.make_visible_to_host(readback.handle());
    command.end().expect("end");

    support::submit_and_wait(&device, &command);

    assert_eq!(
        readback.mapped().expect("readback is mappable"),
        payload.as_slice(),
        "what came back must be what went in"
    );
}

#[test]
fn device_local_memory_cannot_be_mapped() {
    // The reason staging exists at all. A caller reaching for `mapped()` on a
    // device-local buffer gets an error naming the fix rather than a pointer
    // into nothing.
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };

    let buffer = buffer(
        &allocator,
        "device local",
        256,
        vk::BufferUsageFlags::TRANSFER_DST,
        MemoryLocation::DeviceOnly,
    );

    assert!(matches!(
        buffer.mapped(),
        Err(slop_rhi::RhiError::MemoryNotHostVisible)
    ));

    device.wait_idle().expect("idle");
}

#[test]
fn a_texture_uploads_and_reads_back_unchanged() {
    // Buffer to image and back. Catches a row-padding mistake, which is the
    // classic buffer-image copy bug: a stride the driver pads and the caller
    // does not produces an image sheared by one pixel per row.
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };

    const SIZE: u32 = 16;
    let bytes = (SIZE * SIZE * 4) as u64;

    // A gradient, not a solid colour: a solid fill survives almost any copy
    // mistake, including a completely transposed one.
    let payload: Vec<u8> = (0..SIZE * SIZE)
        .flat_map(|texel| {
            let x = (texel % SIZE) as u8;
            let y = (texel / SIZE) as u8;
            [x * 16, y * 16, x.wrapping_add(y) * 8, 255]
        })
        .collect();

    let mut staging = buffer(
        &allocator,
        "texture upload",
        bytes,
        vk::BufferUsageFlags::TRANSFER_SRC,
        MemoryLocation::Upload,
    );
    staging
        .mapped_mut()
        .expect("mappable")
        .copy_from_slice(&payload);

    let texture = Image::new(
        &allocator,
        &ImageConfig {
            name: "uploaded texture",
            extent: vk::Extent2D {
                width: SIZE,
                height: SIZE,
            },
            format: vk::Format::R8G8B8A8_UNORM,
            usage: vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC,
            mip_levels: 1,
        },
    )
    .expect("texture creation");

    let readback = buffer(
        &allocator,
        "texture readback",
        bytes,
        vk::BufferUsageFlags::TRANSFER_DST,
        MemoryLocation::Readback,
    );

    let pool = CommandPool::new(&device, device.queue_families().graphics).expect("pool");
    let command = pool.allocate(1).expect("allocation").pop().expect("one");

    command.begin().expect("begin");
    command.transition_image(
        texture.handle(),
        texture.aspect(),
        ImageState::UNDEFINED,
        ImageState::TRANSFER_DST,
    );
    command.copy_buffer_to_image(
        staging.handle(),
        texture.handle(),
        texture.aspect(),
        texture.extent(),
    );
    command.transition_image(
        texture.handle(),
        texture.aspect(),
        ImageState::TRANSFER_DST,
        ImageState::TRANSFER_SRC,
    );
    command.copy_image_to_buffer(texture.handle(), readback.handle(), texture.extent());
    command.make_visible_to_host(readback.handle());
    command.end().expect("end");

    support::submit_and_wait(&device, &command);

    assert_eq!(
        readback.mapped().expect("mappable"),
        payload.as_slice(),
        "the texture must survive the trip through optimal tiling"
    );
}

#[test]
fn the_device_offers_a_float_depth_format() {
    // Reverse-Z buys its precision from the floating-point exponent, so a
    // fixed-point depth format would pay the complexity and collect none of the
    // benefit. Worth asserting rather than assuming: the fallback chain would
    // silently hand back D24_UNORM_S8_UINT on a device without D32.
    let Some(device) = support::device() else {
        return;
    };

    let format = slop_rhi::preferred_depth_format(&device);

    assert_eq!(
        format,
        vk::Format::D32_SFLOAT,
        "any desktop GPU should offer D32_SFLOAT"
    );
    assert_eq!(
        slop_rhi::aspect_of(format),
        vk::ImageAspectFlags::DEPTH,
        "a pure depth format must not claim a stencil aspect"
    );
}

#[test]
fn the_depth_aspect_follows_the_format() {
    // A barrier naming the wrong aspect transitions nothing and reports
    // nothing, so deriving it from the format is the only safe option — and
    // depth-stencil formats need *both* bits or the transition is incomplete.
    assert_eq!(
        slop_rhi::aspect_of(vk::Format::R8G8B8A8_UNORM),
        vk::ImageAspectFlags::COLOR
    );
    assert_eq!(
        slop_rhi::aspect_of(vk::Format::D32_SFLOAT),
        vk::ImageAspectFlags::DEPTH
    );
    assert_eq!(
        slop_rhi::aspect_of(vk::Format::D32_SFLOAT_S8_UINT),
        vk::ImageAspectFlags::DEPTH | vk::ImageAspectFlags::STENCIL
    );
    assert_eq!(
        slop_rhi::aspect_of(vk::Format::D24_UNORM_S8_UINT),
        vk::ImageAspectFlags::DEPTH | vk::ImageAspectFlags::STENCIL
    );
}

#[test]
fn a_depth_image_clears_to_the_far_plane_at_zero() {
    // The assertion that catches reverse-Z being wired backwards. Under the
    // reversed convention the far plane is 0.0, so a cleared depth buffer reads
    // as zeros. A codebase that cleared to the conventional 1.0 would fail
    // here — and would otherwise fail nowhere, while quietly rejecting every
    // fragment in the scene.
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };

    const SIZE: u32 = 8;
    let format = slop_rhi::preferred_depth_format(&device);

    let depth = Image::new(
        &allocator,
        &ImageConfig {
            name: "depth clear probe",
            extent: vk::Extent2D {
                width: SIZE,
                height: SIZE,
            },
            format,
            usage: vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT
                | vk::ImageUsageFlags::TRANSFER_SRC,
            mip_levels: 1,
        },
    )
    .expect("depth image creation");

    let readback = buffer(
        &allocator,
        "depth readback",
        u64::from(SIZE * SIZE * 4),
        vk::BufferUsageFlags::TRANSFER_DST,
        MemoryLocation::Readback,
    );

    let pool = CommandPool::new(&device, device.queue_families().graphics).expect("pool");
    let command = pool.allocate(1).expect("allocation").pop().expect("one");

    command.begin().expect("begin");
    command.transition_image(
        depth.handle(),
        depth.aspect(),
        ImageState::UNDEFINED,
        ImageState::DEPTH_ATTACHMENT,
    );

    // An empty render pass whose only job is the clear.
    let attachment = vk::RenderingAttachmentInfo::default()
        .image_view(depth.view())
        .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(vk::ClearValue {
            depth_stencil: vk::ClearDepthStencilValue {
                depth: DEPTH_CLEAR,
                stencil: 0,
            },
        });

    let rendering = vk::RenderingInfo::default()
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: depth.extent(),
        })
        .layer_count(1)
        .depth_attachment(&attachment);

    // SAFETY: the command buffer is recording, `rendering` borrows `attachment`
    // which outlives the call, and `dynamic_rendering` is in the required
    // feature tier.
    unsafe {
        device
            .raw()
            .cmd_begin_rendering(command.handle(), &rendering);
        device.raw().cmd_end_rendering(command.handle());
    }

    command.transition_image(
        depth.handle(),
        depth.aspect(),
        ImageState::DEPTH_ATTACHMENT,
        ImageState::TRANSFER_SRC,
    );
    command.copy_image_to_buffer(depth.handle(), readback.handle(), depth.extent());
    command.make_visible_to_host(readback.handle());
    command.end().expect("end");

    support::submit_and_wait(&device, &command);

    let bytes = readback.mapped().expect("mappable");
    let depths: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|word| f32::from_le_bytes(word.try_into().expect("four bytes")))
        .collect();

    assert_eq!(depths.len(), (SIZE * SIZE) as usize);
    assert!(
        depths.iter().all(|&value| value == DEPTH_CLEAR),
        "every texel should hold the far plane; got {:?}",
        &depths[..4]
    );
    assert_eq!(
        DEPTH_CLEAR, 0.0,
        "reverse-Z puts the far plane at zero, not one"
    );
}

#[test]
fn the_depth_comparison_matches_the_reversed_convention() {
    // Three things have to agree and they live in three files: this comparison,
    // the clear value, and `slop-math`'s projection. Two out of three renders a
    // plausible image that is wrong.
    assert_eq!(
        slop_rhi::DEPTH_COMPARE,
        vk::CompareOp::GREATER_OR_EQUAL,
        "closer means larger under reverse-Z, so the test is GREATER"
    );
}

/// A buffer with the given usage and location.
fn buffer(
    allocator: &Arc<Allocator>,
    name: &str,
    size: u64,
    usage: vk::BufferUsageFlags,
    location: MemoryLocation,
) -> Buffer {
    Buffer::new(
        allocator,
        &BufferConfig {
            name,
            size,
            usage,
            location,
        },
    )
    .expect("buffer creation must succeed")
}
