//! A compute pass through the graph, against a real device.
//!
//! `docs/PLAN.md` §9.5 E4's prerequisite. E3 shipped a graph that could only
//! express rendering passes and said so; §9.4's cluster build is a dispatch that
//! writes a buffer the forward pass then reads, so that gap is what E4 starts
//! from.
//!
//! # What this verifies, and what it does not
//!
//! It verifies that a pass declared to the graph is dispatched, reaches the
//! bindless heap, and writes the values expected — every element, including the
//! ones in the workgroup that runs past the end.
//!
//! **It does not verify the barrier.** Removing `final_state` from the import,
//! so no transition to `TRANSFER_SRC` is emitted at all, leaves this passing and
//! the validation layer silent. Measured, twice — the same gap E1c found on the
//! buffer side. Synchronization validation catches image hazards and
//! transfer-to-transfer buffer hazards on this driver; a compute write followed
//! by a transfer read is not among them.
//!
//! Worth stating rather than implying, because the graph's whole claim is that
//! it derives barriers correctly, and this is a place where nothing independent
//! is checking that claim. The image side *is* checked — `examples/model`'s
//! goldens run through the graph under sync validation with the HDR
//! write-then-sample dependency in them.

use std::sync::Arc;

use slop_asset::Vfs;
use slop_render::{ComputePass, Graph, ImportedBuffer};
use slop_rhi::{
    BindlessHeap, BindlessHeapConfig, Buffer, BufferConfig, BufferState, BufferUsage, CommandPool,
    ComputePipeline, Device, DeviceSelection, Instance, InstanceConfig, MemoryLocation,
    PipelineLayout, PipelineLayoutConfig, RhiError, ShaderModule, ShaderStage,
};

/// Deliberately not a multiple of `accumulate.slang`'s 64-wide workgroup, so the
/// last group runs partly past the end and the shader's bounds check matters.
const COUNT: u32 = 100;

/// Push constants, matching `PushConstants` in `shaders/tests/accumulate.slang`.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Push {
    target: u32,
    count: u32,
}

/// A headless device, or `None` when the machine has no Vulkan loader.
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
    let chosen = slop_rhi::select(&devices, &DeviceSelection::Automatic).expect("a usable adapter");

    Some(Arc::new(
        Device::new(&instance, &devices[chosen]).expect("device creation must succeed"),
    ))
}

/// A compute pass declared to the graph dispatches, and writes what it should.
///
/// The declaration is the whole point: `writes: &[cluster]` on the pass and
/// `final_state: Some(TRANSFER_SRC)` on the import. Nothing here names a
/// barrier.
///
/// See this file's header for what the barrier half of that is *not* verified
/// by.
#[test]
fn a_compute_pass_writes_a_buffer_the_graph_makes_readable() {
    let Some(device) = device() else {
        return;
    };
    let allocator = slop_rhi::Allocator::new(&device).expect("an allocator");

    let Ok(vfs) = Vfs::discover(&std::env::current_dir().expect("a working directory")) else {
        eprintln!("skipping: nothing cooked; run `cargo run -p slop-cli -- cook`");
        return;
    };

    let module = ShaderModule::from_bytes(
        &device,
        &vfs.read("shaders/tests/accumulate.spv")
            .expect("accumulate.spv must be cooked"),
    )
    .expect("valid SPIR-V");

    let reflection = slop_asset::Reflection::read(
        &vfs.read("shaders/tests/accumulate.refl")
            .expect("its reflection must sit beside it"),
    )
    .expect("valid reflection");

    let mut heap =
        BindlessHeap::new(&device, &BindlessHeapConfig::default()).expect("a bindless heap");

    let bytes = u64::from(COUNT) * 4;

    let target = Buffer::new(
        &allocator,
        &BufferConfig {
            name: "graph compute target",
            size: bytes,
            usage: BufferUsage::STORAGE | BufferUsage::TRANSFER_SRC,
            location: MemoryLocation::DeviceOnly,
        },
    )
    .expect("a storage buffer");

    let slot = heap
        .insert_storage_buffer(target.handle())
        .expect("heap room");

    let layout = Arc::new(
        PipelineLayout::new(
            &device,
            &PipelineLayoutConfig {
                heap: Some(&heap),
                push_constant_bytes: size_of::<Push>() as u32,
            },
        )
        .expect("a layout"),
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

    let mut readback = Buffer::new(
        &allocator,
        &BufferConfig {
            name: "graph compute readback",
            size: bytes,
            usage: BufferUsage::TRANSFER_DST,
            location: MemoryLocation::Readback,
        },
    )
    .expect("a readback buffer");

    let pool = CommandPool::new(&device, device.queue_families().graphics).expect("a pool");
    let command = pool
        .allocate(1)
        .expect("one buffer")
        .pop()
        .expect("one was requested");

    command.begin().expect("recording begins");

    let mut graph = Graph::new();

    let cluster = graph.import_buffer(&ImportedBuffer {
        name: "cluster indices",
        buffer: target.handle(),
        // Nothing has touched it, and the pass that writes it writes every
        // element, so there is nothing to make available first.
        state: BufferState::storage_write(slop_rhi::Stage::Compute),
        // What the copy below needs. The graph emits this because it ran the
        // passes and knows which one touched the buffer last.
        final_state: Some(BufferState::TRANSFER_SRC),
    });

    let heap_ref = &heap;
    let groups = reflection
        .workgroups(0, COUNT)
        .expect("accumulate is a compute shader");

    graph.add_compute(
        &ComputePass {
            name: "cluster build",
            writes: &[cluster],
            ..ComputePass::default()
        },
        |command| {
            let compute = command.bind_compute(&pipeline);
            compute.bind_heap(heap_ref);
            compute.push_constants(bytemuck::bytes_of(&Push {
                target: slot.index(),
                count: COUNT,
            }));
            compute.dispatch(groups, 1, 1);
        },
    );

    assert_eq!(graph.pass_count(), 1);
    assert_eq!(
        graph.pass_names().collect::<Vec<_>>(),
        vec!["cluster build"]
    );

    graph.execute(&command);

    command.copy_buffer(target.handle(), readback.handle(), bytes);
    command.barrier_buffer(
        readback.handle(),
        BufferState::TRANSFER_DST,
        BufferState::HOST_READ,
    );
    command.make_visible_to_host(readback.handle());
    command.end().expect("recording ends");

    slop_rhi::submit_recorded_and_wait(&device, &command).expect("the submission must complete");

    let written: &[u32] = bytemuck::cast_slice(readback.mapped_mut().expect("mappable"));

    for index in 0..COUNT {
        assert_eq!(
            written[index as usize],
            index * 3 + 1,
            "element {index} is wrong; the dispatch did not land where the graph said it would"
        );
    }

    assert_eq!(
        device.instance().validation_errors(),
        0,
        "the validation layer reported something while the graph drove a compute pass"
    );
}
