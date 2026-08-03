//! A compute shader that runs, reaches the bindless heap, and produces a result
//! the CPU can check.
//!
//! `docs/PLAN.md` §9.5 E1b. The queues have been acquired since M0 and the heap
//! has had a storage-image binding just as long; until now nothing could build a
//! compute pipeline or dispatch one, so none of that was ever executed.
//!
//! Uses the artifact `slop-cli cook` produced rather than a synthetic SPIR-V
//! blob, so a Slang source that stops compiling — or a cooker that stops
//! handling compute entry points — fails here rather than at E4.

mod support;

use std::sync::Arc;

use slop_asset::Vfs;
use slop_rhi::{
    BindlessHeap, BindlessHeapConfig, Buffer, BufferConfig, BufferState, ComputePipeline, Extent2D,
    Format, Image, ImageConfig, ImageState, ImageUsage, MemoryLocation, PipelineLayout,
    PipelineLayoutConfig, ShaderModule, ShaderStage,
};

/// Deliberately not a multiple of `fill.slang`'s 8×8 workgroup.
///
/// 20 is two full groups plus four texels, so the last group along each axis
/// runs half outside the image. That exercises the shader's bounds check and the
/// round-up in `Reflection::workgroups` together; a 16 or 32 would let both bugs
/// pass.
const SIZE: u32 = 20;

/// Written into the red channel, so a shader that never ran is distinguishable
/// from one that ran correctly. An all-zero image is what a cleared allocation
/// looks like.
const SEED: u32 = 200;

/// Push constants, matching `PushConstants` in `fill.slang`.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PushConstants {
    target: u32,
    seed: u32,
    width: u32,
    height: u32,
}

/// One cooked shader and what it says about itself, or `None` when nothing has
/// been cooked yet.
///
/// The reflection comes back alongside the module because the dispatch needs it:
/// the workgroup size is the shader's own, not a number restated here.
fn module(
    device: &Arc<slop_rhi::Device>,
    name: &str,
) -> Option<(ShaderModule, slop_asset::Reflection)> {
    let vfs = match Vfs::discover(&std::env::current_dir().expect("a working directory")) {
        Ok(vfs) => vfs,
        Err(error) => {
            eprintln!("skipping: no cooked assets ({error}); run `cargo run -p slop-cli -- cook`");
            return None;
        }
    };

    let spirv = format!("shaders/tests/{name}.spv");
    let bytes = vfs
        .read(&spirv)
        .unwrap_or_else(|_| panic!("{spirv} must be cooked; run `cargo run -p slop-cli -- cook`"));

    let reflection = vfs
        .read(&format!("shaders/tests/{name}.refl"))
        .expect("cooked reflection must sit beside the module");
    let reflection =
        slop_asset::Reflection::read(&reflection).expect("the cooked reflection must be valid");

    Some((
        ShaderModule::from_bytes(device, &bytes).expect("the cooked module must be valid SPIR-V"),
        reflection,
    ))
}

/// The whole point: dispatch, then check every texel the shader was supposed to
/// write.
///
/// Not "did it crash" and not "is it non-zero". A compute pass that writes the
/// wrong texel, transposes its coordinates, or skips the partial groups at the
/// edges produces an image that is present and wrong, which is the failure mode
/// `docs/PLAN.md` §3.1 keeps relearning.
#[test]
fn a_compute_shader_writes_every_texel_it_was_dispatched_for() {
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };
    let Some((module, reflection)) = module(&device, "fill") else {
        return;
    };

    let mut heap = BindlessHeap::new(&device, &BindlessHeapConfig::default())
        .expect("a heap must be creatable");

    let image = Image::new(
        &allocator,
        &ImageConfig {
            name: "compute target",
            extent: Extent2D {
                width: SIZE,
                height: SIZE,
            },
            format: Format::Rgba8Unorm,
            // STORAGE to be written by the shader, TRANSFER_SRC to be copied
            // back out. Neither implies the other.
            usage: ImageUsage::STORAGE | ImageUsage::TRANSFER_SRC,
            mip_levels: 1,
            array_layers: 1,
        },
    )
    .expect("a storage image must be creatable");

    let slot = heap
        .insert_storage_image(image.view())
        .expect("the heap must have room");

    let layout = Arc::new(
        PipelineLayout::new(
            &device,
            &PipelineLayoutConfig {
                heap: Some(&heap),
                push_constant_bytes: size_of::<PushConstants>() as u32,
            },
        )
        .expect("a layout must be creatable"),
    );

    let pipeline = ComputePipeline::new(
        &device,
        &layout,
        ShaderStage {
            module: &module,
            entry: c"fillMain",
        },
    )
    .expect("the compute pipeline must compile");

    let readback = Buffer::new(
        &allocator,
        &BufferConfig {
            name: "compute readback",
            size: u64::from(SIZE * SIZE * 4),
            usage: slop_rhi::BufferUsage::TRANSFER_DST,
            location: MemoryLocation::Readback,
        },
    )
    .expect("a readback buffer must be creatable");

    let pool = slop_rhi::CommandPool::new(&device, device.queue_families().graphics)
        .expect("a pool must be creatable");
    let command = pool
        .allocate(1)
        .expect("one buffer must be allocatable")
        .pop()
        .expect("one was requested");

    command.begin().expect("recording must begin");

    // GENERAL before the shader writes it, which is the only layout a storage
    // image may be written through.
    command.transition_image(
        image.handle(),
        image.aspect(),
        ImageState::UNDEFINED,
        ImageState::STORAGE_WRITE,
    );

    {
        let compute = command.bind_compute(&pipeline);
        compute.bind_heap(&heap);
        compute.push_constants(bytemuck::bytes_of(&PushConstants {
            target: slot.index(),
            seed: SEED,
            width: SIZE,
            height: SIZE,
        }));
        // Divided by what the shader declared, read out of its cooked
        // reflection. Nothing here restates `[numthreads(..)]`.
        compute.dispatch(
            reflection
                .workgroups(0, SIZE)
                .expect("fill is a compute shader"),
            reflection
                .workgroups(1, SIZE)
                .expect("fill is a compute shader"),
            1,
        );
    }

    command.transition_image(
        image.handle(),
        image.aspect(),
        ImageState::STORAGE_WRITE,
        ImageState::TRANSFER_SRC,
    );
    command.copy_image_to_buffer(
        image.handle(),
        readback.handle(),
        Extent2D {
            width: SIZE,
            height: SIZE,
        },
    );
    command.barrier_buffer(
        readback.handle(),
        BufferState::TRANSFER_DST,
        BufferState::HOST_READ,
    );
    command.make_visible_to_host(readback.handle());
    command.end().expect("recording must end");

    support::submit_and_wait(&device, &command);

    let mut readback = readback;
    let pixels = readback.mapped_mut().expect("readback must be mappable");

    for y in 0..SIZE {
        for x in 0..SIZE {
            let at = ((y * SIZE + x) * 4) as usize;
            let texel = &pixels[at..at + 4];

            assert_eq!(
                texel[0], SEED as u8,
                "texel ({x}, {y}) carries no seed; the shader did not write here"
            );

            // The gradient, checked per axis so a transposed write fails rather
            // than passing on the diagonal. Rounded the same way the shader's
            // float-to-unorm conversion does, with a tolerance of one step.
            let expected_g = ((x as f32 / SIZE as f32) * 255.0).round() as u8;
            let expected_b = ((y as f32 / SIZE as f32) * 255.0).round() as u8;

            assert!(
                texel[1].abs_diff(expected_g) <= 1,
                "texel ({x}, {y}) green is {} not {expected_g}; x and y may be swapped",
                texel[1]
            );
            assert!(
                texel[2].abs_diff(expected_b) <= 1,
                "texel ({x}, {y}) blue is {} not {expected_b}",
                texel[2]
            );
        }
    }
}

/// A format the device cannot store to is refused, rather than producing an
/// image a compute shader silently cannot write.
///
/// `Bc7Unorm` is block-compressed, so it can never be a storage image. Guards
/// `ImageUsage::STORAGE` having been added to `to_vk` but not to
/// `required_format_features`, which would leave the check passing everything.
#[test]
fn a_format_that_cannot_be_stored_to_is_refused() {
    let Some((_device, allocator)) = support::device_and_allocator() else {
        return;
    };

    let failure = Image::new(
        &allocator,
        &ImageConfig {
            name: "impossible storage image",
            extent: Extent2D {
                width: 16,
                height: 16,
            },
            format: Format::Bc7Unorm,
            usage: ImageUsage::STORAGE,
            mip_levels: 1,
            array_layers: 1,
        },
    )
    .expect_err("a block-compressed storage image must be refused");

    match failure {
        slop_rhi::RhiError::FormatUnsupported { missing, .. } => {
            assert_eq!(missing, "storage image");
        }
        other => panic!("expected FormatUnsupported, got {other}"),
    }
}

/// Deliberately not a multiple of `accumulate.slang`'s 64-wide workgroup, for
/// the reason [`SIZE`] is not a multiple of `fill.slang`'s.
const COUNT: u32 = 100;

/// Push constants, matching `PushConstants` in `accumulate.slang`.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct AccumulatePush {
    target: u32,
    count: u32,
}

/// A compute shader writes a storage **buffer**, and the barrier that orders
/// that write against the CPU's read actually orders it.
///
/// The image test above proves dispatch works. This one exists because
/// [`BufferState::storage_write`] is a barrier constant, and a barrier constant
/// with no consumer is untested by construction — asserting its access mask in a
/// unit test proves only that it was typed as typed.
///
/// It also exercises the writable buffer view added to `lib/bindless.slang`,
/// which is what `docs/PLAN.md` §9.4's cluster build needs and what the storage
/// **image** path let E1b avoid noticing.
#[test]
fn a_compute_shader_writes_a_storage_buffer_the_cpu_can_then_read() {
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };
    let Some((module, reflection)) = module(&device, "accumulate") else {
        return;
    };

    let mut heap = BindlessHeap::new(&device, &BindlessHeapConfig::default())
        .expect("a heap must be creatable");

    let bytes = u64::from(COUNT) * 4;

    // Device-local, so the readback below genuinely crosses a barrier rather
    // than reading host memory the shader happened to write coherently.
    let target = Buffer::new(
        &allocator,
        &BufferConfig {
            name: "compute target",
            size: bytes,
            usage: slop_rhi::BufferUsage::STORAGE | slop_rhi::BufferUsage::TRANSFER_SRC,
            location: MemoryLocation::DeviceOnly,
        },
    )
    .expect("a storage buffer must be creatable");

    let slot = heap
        .insert_storage_buffer(target.handle())
        .expect("the heap must have room");

    let layout = Arc::new(
        PipelineLayout::new(
            &device,
            &PipelineLayoutConfig {
                heap: Some(&heap),
                push_constant_bytes: size_of::<AccumulatePush>() as u32,
            },
        )
        .expect("a layout must be creatable"),
    );

    let pipeline = ComputePipeline::new(
        &device,
        &layout,
        ShaderStage {
            module: &module,
            entry: c"accumulateMain",
        },
    )
    .expect("the compute pipeline must compile");

    let readback = Buffer::new(
        &allocator,
        &BufferConfig {
            name: "buffer readback",
            size: bytes,
            usage: slop_rhi::BufferUsage::TRANSFER_DST,
            location: MemoryLocation::Readback,
        },
    )
    .expect("a readback buffer must be creatable");

    let pool = slop_rhi::CommandPool::new(&device, device.queue_families().graphics)
        .expect("a pool must be creatable");
    let command = pool
        .allocate(1)
        .expect("one buffer must be allocatable")
        .pop()
        .expect("one was requested");

    command.begin().expect("recording must begin");

    {
        let compute = command.bind_compute(&pipeline);
        compute.bind_heap(&heap);
        compute.push_constants(bytemuck::bytes_of(&AccumulatePush {
            target: slot.index(),
            count: COUNT,
        }));
        compute.dispatch(
            reflection
                .workgroups(0, COUNT)
                .expect("accumulate is a compute shader"),
            1,
            1,
        );
    }

    // The barrier this test exists for: the compute write must be visible to
    // the transfer that follows.
    command.barrier_buffer(
        target.handle(),
        BufferState::storage_write(slop_rhi::Stage::Compute),
        BufferState::TRANSFER_SRC,
    );
    command.copy_buffer(target.handle(), readback.handle(), bytes);
    command.barrier_buffer(
        readback.handle(),
        BufferState::TRANSFER_DST,
        BufferState::HOST_READ,
    );
    command.make_visible_to_host(readback.handle());
    command.end().expect("recording must end");

    support::submit_and_wait(&device, &command);

    let mut readback = readback;
    let written: &[u32] = bytemuck::cast_slice(readback.mapped_mut().expect("mappable"));

    for index in 0..COUNT {
        assert_eq!(
            written[index as usize],
            index * 3 + 1,
            "element {index} is wrong; the shader wrote elsewhere or not at all"
        );
    }
}

/// The cooked reflection reports the size the shader source declares.
///
/// This is the whole point of carrying it: a dispatch divides by these numbers,
/// and if the cooker reported the wrong ones the division would be wrong in a
/// way no amount of care at the call site could catch. Asserted against the
/// literals in the two `.slang` files, which is the one place the pair can be
/// compared without the reflection being both sides of the comparison.
#[test]
fn the_cooked_thread_group_matches_what_the_shader_declares() {
    let Some(device) = support::device() else {
        return;
    };

    let Some((_, fill)) = module(&device, "fill") else {
        return;
    };
    let Some((_, accumulate)) = module(&device, "accumulate") else {
        return;
    };

    assert_eq!(
        fill.thread_group,
        Some([8, 8, 1]),
        "shaders/tests/fill.slang declares [numthreads(8, 8, 1)]"
    );
    assert_eq!(
        accumulate.thread_group,
        Some([64, 1, 1]),
        "shaders/tests/accumulate.slang declares [numthreads(64, 1, 1)]"
    );

    // The round-up, against a size the shader owns. 20 over 8 is three groups,
    // and rounding down would leave the last four texels of each axis unwritten.
    assert_eq!(fill.workgroups(0, 20), Some(3));
    assert_eq!(fill.workgroups(0, 16), Some(2));
    assert_eq!(accumulate.workgroups(0, 100), Some(2));

    // A graphics shader has no compute stage and says so, rather than reporting
    // a group of one and letting a dispatch look reasonable.
    let Some((_, model)) = graphics_reflection(&device) else {
        return;
    };
    assert_eq!(model.thread_group, None);
    assert_eq!(model.workgroups(0, 100), None);
}

/// A cooked graphics shader, for the negative half of the test above.
fn graphics_reflection(
    device: &Arc<slop_rhi::Device>,
) -> Option<(ShaderModule, slop_asset::Reflection)> {
    let vfs = Vfs::discover(&std::env::current_dir().expect("a working directory")).ok()?;
    let bytes = vfs.read("shaders/passes/model.spv").ok()?;
    let reflection = vfs.read("shaders/passes/model.refl").ok()?;

    Some((
        ShaderModule::from_bytes(device, &bytes).expect("valid SPIR-V"),
        slop_asset::Reflection::read(&reflection).expect("valid reflection"),
    ))
}
