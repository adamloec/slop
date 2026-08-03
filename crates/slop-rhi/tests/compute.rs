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
    PipelineLayoutConfig, ShaderModule, ShaderStage, workgroups,
};

/// Matches `[numthreads(8, 8, 1)]` in `shaders/passes/fill.slang`.
///
/// Nothing checks that these agree — the same ABI-by-convention the shader's own
/// comment names. Dividing by the wrong number here dispatches too few groups
/// and leaves part of the image unwritten, which the per-texel assertions below
/// would catch.
const GROUP: u32 = 8;

/// Deliberately not a multiple of [`GROUP`].
///
/// 20 is two full groups plus four texels, so the last group along each axis
/// runs half outside the image. That exercises the shader's bounds check and the
/// round-up in [`workgroups`] together; a 16 or 32 would let both bugs pass.
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

/// The cooked shader, or `None` when nothing has been cooked yet.
fn module(device: &Arc<slop_rhi::Device>) -> Option<ShaderModule> {
    let vfs = match Vfs::discover(&std::env::current_dir().expect("a working directory")) {
        Ok(vfs) => vfs,
        Err(error) => {
            eprintln!("skipping: no cooked assets ({error}); run `cargo run -p slop-cli -- cook`");
            return None;
        }
    };

    let bytes = vfs
        .read("shaders/passes/fill.spv")
        .expect("fill.spv must be cooked; run `cargo run -p slop-cli -- cook`");

    Some(ShaderModule::from_bytes(device, &bytes).expect("the cooked module must be valid SPIR-V"))
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
    let Some(module) = module(&device) else {
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
        compute.dispatch(workgroups(SIZE, GROUP), workgroups(SIZE, GROUP), 1);
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
