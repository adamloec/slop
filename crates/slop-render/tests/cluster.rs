//! The cluster build, checked against the CPU twin of the same arithmetic.
//!
//! `docs/PLAN.md` §9.5 E4.
//!
//! # Why this test exists rather than a golden image
//!
//! **A correct light assignment and one that lists every light in every cell
//! produce the same picture.** The forward pass sums the lights its cell names,
//! and a falloff that reaches zero at the radius means a light listed in a cell
//! it does not reach contributes nothing. So an assignment that did no work at
//! all — or a grid whose cells are in the wrong place, as long as the union
//! still covers the lights — renders identically to a correct one.
//!
//! `examples/model`'s references confirmed exactly that: adding clustering
//! changed **0 of 65536 pixels**, on both models. That is the right answer and
//! it is not evidence of anything, which is why the assignment is compared
//! element by element here instead.
//!
//! The oracle is `slop_render::ClusterGrid::bounds` and
//! `slop_render::sphere_touches_box` — the same arithmetic as
//! `shaders/lib/cluster.slang`, written twice on purpose, with the CPU copy
//! tested against slice boundaries worked out by hand.

use std::sync::Arc;

use slop_asset::Vfs;
use slop_math::{Mat4, Vec3};
use slop_render::{ClusterCamera, ClusterGrid, Clusters, Lights, PointLight, sphere_touches_box};
use slop_rhi::{
    BindlessHeap, BindlessHeapConfig, Buffer, BufferConfig, BufferState, BufferUsage, CommandPool,
    Device, DeviceSelection, Instance, InstanceConfig, MemoryLocation, RhiError, ShaderModule,
};

/// Deliberately small, so every cluster can be checked without the test taking
/// noticeable time — and small enough that a hand-placed light lands in a
/// knowable handful of cells rather than all of them.
const GRID: ClusterGrid = ClusterGrid {
    tiles_x: 8,
    tiles_y: 4,
    slices: 6,
    near: 1.0,
    far: 64.0,
    max_per_cluster: 16,
};

/// A headless device, or `None` when the machine has no Vulkan loader.
fn device() -> Option<(Arc<Device>, Arc<slop_rhi::Allocator>)> {
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
    let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic).expect("a usable adapter");
    let device =
        Arc::new(Device::new(&instance, &devices[chosen]).expect("device creation must succeed"));
    let allocator = slop_rhi::Allocator::new(&device).expect("an allocator");

    Some((device, allocator))
}

/// Lights spread through the view, at a range of depths and radii.
///
/// Placed by hand rather than randomly: `docs/DESIGN.md` §2.14 keeps unseeded
/// randomness out of anything reproducible, and a fixed set means a failure is
/// the same failure on every machine. The spread matters — lights all at one
/// depth would leave most slices empty and the test would pass with the depth
/// mapping broken.
fn lights() -> Vec<PointLight> {
    let mut lights = Vec::new();

    for (index, depth) in [2.0_f32, 5.0, 11.0, 23.0, 47.0].into_iter().enumerate() {
        let sway = (index as f32).mul_add(0.37, -0.6);

        lights.push(PointLight {
            // Negative Z: right-handed view space, and the identity view matrix
            // below means these are already view-space positions.
            position: Vec3::new(sway * depth * 0.5, sway * depth * 0.3, -depth),
            color: Vec3::new(1.0, 1.0, 1.0),
            intensity: 1.0,
            // Deliberately small relative to the depth, so a light reaches a few
            // cells rather than all of them. A radius that covered everything
            // would make the test pass against an assignment that did nothing.
            radius: depth * 0.35,
        });
    }

    lights
}

/// Every cluster's light list is exactly what the CPU says it should be.
#[test]
fn the_build_pass_assigns_each_light_to_the_cells_it_reaches() {
    let Some((device, allocator)) = device() else {
        return;
    };

    let Ok(vfs) = Vfs::discover(&std::env::current_dir().expect("a working directory")) else {
        eprintln!("skipping: nothing cooked; run `cargo run -p slop-cli -- cook`");
        return;
    };

    let module = ShaderModule::from_bytes(
        &device,
        &vfs.read("shaders/passes/cluster_build.spv")
            .expect("cluster_build.spv must be cooked"),
    )
    .expect("valid SPIR-V");

    let reflection = slop_asset::Reflection::read(
        &vfs.read("shaders/passes/cluster_build.refl")
            .expect("its reflection must sit beside it"),
    )
    .expect("valid reflection");

    let mut heap =
        BindlessHeap::new(&device, &BindlessHeapConfig::default()).expect("a bindless heap");

    let placed = lights();

    let mut light_buffer = Lights::new(&allocator, &mut heap, 1, 64).expect("a light buffer");
    light_buffer.write(0, &placed).expect("room for the lights");

    let mut clusters = Clusters::new(
        &device,
        &allocator,
        &mut heap,
        &module,
        &reflection,
        GRID,
        1,
    )
    .expect("the cluster build must compile");

    // The identity view, so the lights' world positions *are* their view-space
    // positions and the expected answer can be computed without a second
    // transform to get wrong.
    let tan_half_fov_y = 0.5_f32;
    let aspect = 2.0;

    clusters
        .write(
            0,
            &ClusterCamera {
                view: Mat4::IDENTITY,
                tan_half_fov_y,
                aspect,
                screen: (256.0, 128.0),
            },
            &light_buffer,
        )
        .expect("the grid must be writable");

    let (ranges, indices) = readback(&device, &allocator, &clusters, &heap);

    assert_eq!(
        device.instance().validation_errors(),
        0,
        "the validation layer reported something while the cluster build ran"
    );

    let mut assigned = 0_usize;
    let mut discriminating = false;

    for z in 0..GRID.slices {
        for y in 0..GRID.tiles_y {
            for x in 0..GRID.tiles_x {
                let cluster = (z * GRID.tiles_y * GRID.tiles_x + y * GRID.tiles_x + x) as usize;

                let (min, max) = GRID.bounds((x, y, z), tan_half_fov_y, aspect);

                let expected: Vec<u32> = placed
                    .iter()
                    .enumerate()
                    .filter(|(_, light)| sphere_touches_box(light.position, light.radius, min, max))
                    .map(|(index, _)| index as u32)
                    .collect();

                let offset = ranges[cluster * 2] as usize;
                let count = ranges[cluster * 2 + 1] as usize;

                assert_eq!(
                    offset,
                    cluster * GRID.max_per_cluster as usize,
                    "cluster ({x}, {y}, {z}) has the wrong list offset"
                );

                let found = &indices[offset..offset + count];

                assert_eq!(
                    found,
                    expected.as_slice(),
                    "cluster ({x}, {y}, {z}) at {min:?}..{max:?} disagrees with the CPU"
                );

                assigned += count;

                if count > 0 && count < placed.len() {
                    discriminating = true;
                }
            }
        }
    }

    // Without these, the assertions above are satisfied by an assignment that
    // finds nothing at all — every expected list would be empty too, if the
    // bounds were wrong in the same way on both sides.
    assert!(
        assigned > 0,
        "no light was assigned to any cluster; the grid is empty"
    );
    assert!(
        discriminating,
        "every non-empty cluster listed every light, so this would pass against \
         an assignment that did no work"
    );
}

/// A light behind the camera reaches nothing.
///
/// The sign convention, tested where it is cheap to see: view space is
/// right-handed, so positive Z is *behind*. Getting it backwards puts every
/// cluster behind the camera, which renders as a scene lit by the ambient term
/// alone rather than as a failure.
#[test]
fn a_light_behind_the_camera_is_in_no_cluster() {
    let Some((device, allocator)) = device() else {
        return;
    };

    let Ok(vfs) = Vfs::discover(&std::env::current_dir().expect("a working directory")) else {
        eprintln!("skipping: nothing cooked; run `cargo run -p slop-cli -- cook`");
        return;
    };

    let module = ShaderModule::from_bytes(
        &device,
        &vfs.read("shaders/passes/cluster_build.spv")
            .expect("cluster_build.spv must be cooked"),
    )
    .expect("valid SPIR-V");
    let reflection = slop_asset::Reflection::read(
        &vfs.read("shaders/passes/cluster_build.refl")
            .expect("its reflection must sit beside it"),
    )
    .expect("valid reflection");

    let mut heap =
        BindlessHeap::new(&device, &BindlessHeapConfig::default()).expect("a bindless heap");

    let behind = vec![PointLight {
        // Positive Z, and a radius far too small to reach forward past the
        // camera.
        position: Vec3::new(0.0, 0.0, 20.0),
        color: Vec3::ONE,
        intensity: 1.0,
        radius: 5.0,
    }];

    let mut light_buffer = Lights::new(&allocator, &mut heap, 1, 8).expect("a light buffer");
    light_buffer.write(0, &behind).expect("room");

    let mut clusters = Clusters::new(
        &device,
        &allocator,
        &mut heap,
        &module,
        &reflection,
        GRID,
        1,
    )
    .expect("the cluster build must compile");

    clusters
        .write(
            0,
            &ClusterCamera {
                view: Mat4::IDENTITY,
                tan_half_fov_y: 0.5,
                aspect: 2.0,
                screen: (256.0, 128.0),
            },
            &light_buffer,
        )
        .expect("writable");

    let (ranges, _) = readback(&device, &allocator, &clusters, &heap);

    let total: u32 = (0..GRID.count() as usize)
        .map(|cluster| ranges[cluster * 2 + 1])
        .sum();

    assert_eq!(
        total, 0,
        "a light behind the camera was assigned to {total} cluster slots"
    );
}

/// Dispatch the build and copy both outputs back.
fn readback(
    device: &Arc<Device>,
    allocator: &Arc<slop_rhi::Allocator>,
    clusters: &Clusters,
    heap: &BindlessHeap,
) -> (Vec<u32>, Vec<u32>) {
    let grid = clusters.grid();
    let range_bytes = u64::from(grid.count()) * 8;
    let index_bytes = u64::from(grid.index_capacity()) * 4;

    let mut range_readback = staging(allocator, "cluster range readback", range_bytes);
    let mut index_readback = staging(allocator, "cluster index readback", index_bytes);

    let pool = CommandPool::new(device, device.queue_families().graphics).expect("a pool");
    let command = pool
        .allocate(1)
        .expect("one buffer")
        .pop()
        .expect("one was requested");

    command.begin().expect("recording begins");

    clusters.build(&command, heap, 0);

    let [ranges, indices] = clusters.buffers(0);

    // Hand-written, because this is not the frame graph's frame — the test is
    // the caller here, and it is the one that knows a copy follows the dispatch.
    for buffer in [ranges, indices] {
        command.barrier_buffer(
            buffer,
            BufferState::storage_write(slop_rhi::Stage::Compute),
            BufferState::TRANSFER_SRC,
        );
    }

    command.copy_buffer(ranges, range_readback.handle(), range_bytes);
    command.copy_buffer(indices, index_readback.handle(), index_bytes);

    for buffer in [range_readback.handle(), index_readback.handle()] {
        command.barrier_buffer(buffer, BufferState::TRANSFER_DST, BufferState::HOST_READ);
        command.make_visible_to_host(buffer);
    }

    command.end().expect("recording ends");
    slop_rhi::submit_recorded_and_wait(device, &command).expect("the submission must complete");

    let ranges =
        bytemuck::cast_slice::<u8, u32>(range_readback.mapped_mut().expect("mappable")).to_vec();
    let indices =
        bytemuck::cast_slice::<u8, u32>(index_readback.mapped_mut().expect("mappable")).to_vec();

    (ranges, indices)
}

fn staging(allocator: &Arc<slop_rhi::Allocator>, name: &'static str, size: u64) -> Buffer {
    Buffer::new(
        allocator,
        &BufferConfig {
            name,
            size,
            usage: BufferUsage::TRANSFER_DST,
            location: MemoryLocation::Readback,
        },
    )
    .expect("a readback buffer")
}
