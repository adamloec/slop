//! A cube map uploaded face by face, and sampled by direction.
//!
//! `docs/PLAN.md` §9.7 E6c. **The only thing that can check that the engine's
//! face ordering matches the hardware's**, and the reason it exists at all.
//!
//! `slop-cook`'s `cube.rs` proves its own table is self-consistent — `face_of`
//! and `direction_of` are inverses over all six faces, asserted exhaustively.
//! That says nothing about whether the ordering agrees with the API's, and
//! nothing on the CPU can: both sides of a self-consistent table can be rotated
//! together and every test still passes. The disagreement only appears when the
//! hardware samples what the CPU wrote, and it appears as an environment with
//! two sides swapped — which reads as an odd source panorama rather than a bug,
//! and which no reference image distinguishes.
//!
//! So: write a different colour into each of the six layers, sample along each
//! of the six axes on the GPU, and check the pairs.
//!
//! # What each piece of this covers
//!
//! | | |
//! |---|---|
//! | `ImageKind::Cube` | `CUBE_COMPATIBLE` at creation and a `TYPE_CUBE` view — an image made without the flag cannot be viewed as a cube, and the view type is what makes a lookup directional |
//! | `Subresource::layer` | Six copies into six layers. Before E6c the copy could name a mip and not a layer, so every face would have landed on layer zero |
//! | `TextureCube` on binding 0 | The third alias of the sampled-image binding. If the descriptor and the declaration disagreed about view type, this reads rubbish and nothing reports it |

mod support;

use std::sync::Arc;

use slop_asset::Vfs;
use slop_rhi::{
    BindlessHeap, BindlessHeapConfig, Buffer, BufferConfig, BufferState, ComputePipeline, Extent2D,
    Format, Image, ImageConfig, ImageKind, ImageState, ImageUsage, MemoryLocation, PipelineLayout,
    PipelineLayoutConfig, SamplerConfig, ShaderModule, ShaderStage, Subresource, TextureSampler,
};

/// Texels along each edge of a face.
///
/// Four rather than one, so the centre of a face is well away from its edges:
/// at one texel per face a linear sample would sit exactly on every boundary and
/// seamless filtering would blend all six, which would make this test pass or
/// fail for reasons that have nothing to do with layer ordering.
const SIZE: u32 = 4;

/// How many faces a cube has, and the layer count that implies.
const FACES: usize = 6;

/// What each face is filled with, in layer order.
///
/// Distinct, spread out, and none of them zero — an unwritten layer reads as
/// zero, and a face colour of zero would make "not uploaded" and "uploaded
/// correctly" the same observation. Spread out so that a neighbouring face
/// bleeding in through filtering would show as a value between two of these
/// rather than as one of them.
const COLOURS: [u8; FACES] = [20, 60, 100, 140, 180, 220];

/// Push constants, matching `PushConstants` in `cube_faces.slang`.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PushConstants {
    cube: u32,
    sampler: u32,
    target: u32,
}

/// The cooked shader and its reflection, or `None` when nothing is cooked.
fn module(device: &Arc<slop_rhi::Device>) -> Option<(ShaderModule, slop_asset::Reflection)> {
    let vfs = match Vfs::discover(&std::env::current_dir().expect("a working directory")) {
        Ok(vfs) => vfs,
        Err(error) => {
            eprintln!("skipping: no cooked assets ({error}); run `cargo run -p slop-cli -- cook`");
            return None;
        }
    };

    let bytes = vfs
        .read("shaders/tests/cube_faces.spv")
        .expect("cube_faces must be cooked; run `cargo run -p slop-cli -- cook`");
    let reflection = vfs
        .read("shaders/tests/cube_faces.refl")
        .expect("cooked reflection must sit beside the module");

    Some((
        ShaderModule::from_bytes(device, &bytes).expect("the cooked module must be valid SPIR-V"),
        slop_asset::Reflection::read(&reflection).expect("the cooked reflection must be valid"),
    ))
}

#[test]
fn each_cube_face_is_sampled_by_the_direction_it_faces() {
    let Some((device, allocator)) = support::device_and_allocator() else {
        return;
    };
    let Some((module, _reflection)) = module(&device) else {
        return;
    };

    let mut heap = BindlessHeap::new(&device, &BindlessHeapConfig::default())
        .expect("a heap must be creatable");

    let cube = Image::new(
        &allocator,
        &ImageConfig {
            name: "cube faces",
            extent: Extent2D {
                width: SIZE,
                height: SIZE,
            },
            format: Format::Rgba8Unorm,
            usage: ImageUsage::TRANSFER_DST | ImageUsage::SAMPLED,
            mip_levels: 1,
            kind: ImageKind::Cube,
        },
    )
    .expect("a cube image must be creatable");

    // One staging buffer holding all six faces back to back, which is the shape
    // a cooked environment already has on disk.
    let face_bytes = (SIZE * SIZE * 4) as usize;
    let mut staging = Buffer::new(
        &allocator,
        &BufferConfig {
            name: "cube staging",
            size: (face_bytes * FACES) as u64,
            usage: slop_rhi::BufferUsage::TRANSFER_SRC,
            location: MemoryLocation::Upload,
        },
    )
    .expect("a staging buffer must be creatable");

    {
        let mapped = staging.mapped_mut().expect("staging must be mappable");

        for (layer, colour) in COLOURS.iter().enumerate() {
            for texel in 0..(SIZE * SIZE) as usize {
                let at = layer * face_bytes + texel * 4;

                mapped[at] = *colour;
                mapped[at + 1] = 0;
                mapped[at + 2] = 0;
                mapped[at + 3] = 255;
            }
        }
    }

    let sampler = TextureSampler::new(
        &device,
        &SamplerConfig {
            filter: slop_rhi::Filter::Linear,
            wrap: slop_rhi::Wrap::ClampToEdge,
            ..SamplerConfig::default()
        },
    )
    .expect("a sampler must be creatable");

    let cube_slot = heap
        .insert_sampled_image(cube.view(), ImageState::SHADER_READ)
        .expect("the heap must have room");
    let sampler_slot = heap.insert_sampler(sampler.handle()).expect("room");

    let target = Buffer::new(
        &allocator,
        &BufferConfig {
            name: "cube results",
            size: (FACES * 4) as u64,
            usage: slop_rhi::BufferUsage::STORAGE | slop_rhi::BufferUsage::TRANSFER_SRC,
            location: MemoryLocation::DeviceOnly,
        },
    )
    .expect("a storage buffer must be creatable");

    let target_slot = heap
        .insert_storage_buffer(target.handle())
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
            entry: c"cubeFacesMain",
        },
    )
    .expect("the compute pipeline must compile");

    let readback = Buffer::new(
        &allocator,
        &BufferConfig {
            name: "cube readback",
            size: (FACES * 4) as u64,
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

    command.transition_image(
        cube.handle(),
        cube.aspect(),
        ImageState::UNDEFINED,
        ImageState::TRANSFER_DST,
    );

    // **One copy per face.** This is what `Subresource::layer` exists for: with
    // the layer fixed at zero every face would land on `+X` and the five others
    // would stay undefined, which reads back as five zeros.
    for layer in 0..FACES {
        command.copy_buffer_to_image_part(
            staging.handle(),
            (layer * face_bytes) as u64,
            cube.handle(),
            cube.aspect(),
            Extent2D {
                width: SIZE,
                height: SIZE,
            },
            Subresource {
                level: 0,
                layer: u32::try_from(layer).expect("six fits in a u32"),
            },
        );
    }

    command.transition_image(
        cube.handle(),
        cube.aspect(),
        ImageState::TRANSFER_DST,
        ImageState::SHADER_READ,
    );

    {
        let compute = command.bind_compute(&pipeline);
        compute.bind_heap(&heap);
        compute.push_constants(bytemuck::bytes_of(&PushConstants {
            cube: cube_slot.index(),
            sampler: sampler_slot.index(),
            target: target_slot.index(),
        }));
        // One group: the shader declares six threads and there are six faces.
        compute.dispatch(1, 1, 1);
    }

    command.barrier_buffer(
        target.handle(),
        BufferState::storage_write(slop_rhi::Stage::Compute),
        BufferState::TRANSFER_SRC,
    );
    command.copy_buffer(target.handle(), readback.handle(), (FACES * 4) as u64);
    command.barrier_buffer(
        readback.handle(),
        BufferState::TRANSFER_DST,
        BufferState::HOST_READ,
    );
    command.make_visible_to_host(readback.handle());
    command.end().expect("recording must end");

    support::submit_and_wait(&device, &command);

    let mut readback = readback;
    let bytes = readback.mapped_mut().expect("readback must be mappable");

    let read: Vec<u32> = (0..FACES)
        .map(|face| {
            u32::from_le_bytes(
                bytes[face * 4..face * 4 + 4]
                    .try_into()
                    .expect("four bytes"),
            )
        })
        .collect();

    let names = ["+X", "-X", "+Y", "-Y", "+Z", "-Z"];

    for (face, expected) in COLOURS.iter().enumerate() {
        assert_eq!(
            read[face],
            u32::from(*expected),
            "sampling along {} read {} but layer {face} was filled with {expected} \
             — the engine's face order and the hardware's disagree. Read back: {read:?}",
            names[face],
            read[face]
        );
    }
}
