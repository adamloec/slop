// Assigning lights to cells of the cluster grid.
//
// `docs/PLAN.md` §9.5 E4, designed against §9.4. One thread per cluster: it
// works out its own cell's bounds in view space, tests every light's sphere
// against them, and writes the indices that survive.
//
// # Why one thread per cluster rather than one per light
//
// A thread per light would have to append to whichever cells that light touches,
// from many threads at once, which needs an atomic per cell. A thread per
// cluster owns its own list exclusively, so nothing is contended and nothing is
// ordered by chance — the same reasoning `docs/DESIGN.md` §2.5 gives for
// partitioning over locking, and it also makes the output deterministic: a
// cluster's light indices come out in ascending order every run, on every
// machine.
//
// The cost is that every thread reads every light. At §9.4's 3456 clusters that
// is fine, and the version that stops being fine — a coarse cull per tile column
// first — is an implementation behind this same declaration.

#include "lib/bindless.slang"
#include "lib/lighting/cluster.slang"
#include "lib/lighting/light.slang"

// Matching `WORKGROUP` in `crates/slop-render/src/cluster.rs`. The cooked
// reflection carries this number, and the Rust side reads it from there rather
// than restating it — see `Reflection::workgroups`.
static const uint WORKGROUP_SIZE = 64;

struct PushConstants
{
    // The whole configuration is one heap index, deliberately. Everything the
    // grid needs is in the buffer the forward pass also reads, which is what
    // makes the two unable to disagree about where a cell is.
    uint grid;
};

[[vk::push_constant]] PushConstants g_push;

[shader("compute")]
[numthreads(64, 1, 1)]
void clusterBuildMain(uint3 id: SV_DispatchThreadID)
{
    ClusterGridGpu grid = loadFromBuffer<ClusterGridGpu>(BufferHandle(g_push.grid), 0);

    uint cluster = id.x;
    uint total = grid.tilesX * grid.tilesY * grid.slices;

    // The dispatch is rounded up to whole workgroups, so the last one runs past
    // the end. Without this the surplus threads write past both output buffers.
    if (cluster >= total)
    {
        return;
    }

    uint3 cell = uint3(
        cluster % grid.tilesX,
        (cluster / grid.tilesX) % grid.tilesY,
        cluster / (grid.tilesX * grid.tilesY));

    float3 boxMin;
    float3 boxMax;
    clusterBounds(grid, cell, boxMin, boxMax);

    uint offset = cluster * grid.maxPerCluster;
    uint count = 0;

    for (uint index = 0; index < grid.lightCount; ++index)
    {
        // Stop reading once the list is full rather than testing lights that
        // cannot be recorded. **The overflow is silent, and that is a real
        // limitation**: a cell reached by more than `maxPerCluster` lights drops
        // the rest, and what it drops depends on their order in the buffer. See
        // `docs/PLAN.md` §6.1.
        if (count >= grid.maxPerCluster)
        {
            break;
        }

        PointLightGpu light = loadFromBuffer<PointLightGpu>(BufferHandle(grid.lights), index);

        // World to view. The light buffer holds world positions because the
        // forward pass shades in world space and because a buffer holding view
        // space would have to be rewritten for every view that reads it.
        float3 centre = clusterViewFromWorld(grid, light.position);

        if (sphereTouchesBox(centre, light.radius, boxMin, boxMax))
        {
            storeToBuffer<uint>(BufferHandle(grid.indices), offset + count, index);
            count += 1;
        }
    }

    ClusterRange range;
    range.offset = offset;
    range.count = count;

    storeToBuffer<ClusterRange>(BufferHandle(grid.ranges), cluster, range);
}
