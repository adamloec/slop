// The first compute shader in the engine, and deliberately the smallest one
// that proves the path works end to end.
//
// Writes a known pattern into a storage image so a test can read it back and
// check every texel. Not a rendering technique — `docs/PLAN.md` §9.5 E1b needs
// something that dispatches, reaches the bindless heap from a compute stage, and
// produces a result the CPU can verify. The cluster build at E4 is the first
// shader here that does real work.

#include "lib/bindless.slang"

struct PushConstants
{
    // Which storage-image slot to write. Compute reaches the same heap a
    // graphics pass does, through the same binding — proving that is half the
    // point of this shader.
    uint target;

    // Written into the red channel so the test can tell a stale image from a
    // freshly written one. A shader that never ran leaves the cleared contents
    // behind, and a pattern derived only from position cannot distinguish that
    // from a shader that ran correctly.
    uint seed;

    // Image size in texels. The dispatch rounds group counts *up*, so the last
    // group along each axis runs partly outside the image and every invocation
    // must check itself against these before writing.
    uint width;
    uint height;
};

[[vk::push_constant]] PushConstants g_push;

// 8x8, which is the usual starting point for an image-shaped workload: it fills
// a 32- or 64-lane wave with a square tile, so neighbouring invocations touch
// neighbouring texels and the cache lines they pull in are shared. A 64x1 group
// would fill the same wave with a row and waste most of every line.
//
// `slop_rhi::workgroups` on the CPU side must divide by these exact numbers.
// Nothing checks that they agree — the same class of ABI-by-convention this
// file's `#include` warns about for binding numbers.
[shader("compute")]
[numthreads(8, 8, 1)]
void fillMain(uint3 id: SV_DispatchThreadID)
{
    // The bounds check the rounding-up makes mandatory. Without it the last
    // group along each axis writes outside the image, which is undefined rather
    // than merely wrong.
    if (id.x >= g_push.width || id.y >= g_push.height)
    {
        return;
    }

    StorageImageHandle target = { g_push.target };

    // A gradient in green and blue so a transposed write is visible — writing
    // `id.yx` instead of `id.xy` would be invisible in a uniform fill and
    // obvious here. Red carries the seed, which is what separates "ran" from
    // "left over".
    float4 value = float4(
        float(g_push.seed) / 255.0,
        float(id.x) / float(g_push.width),
        float(id.y) / float(g_push.height),
        1.0);

    g_storage_images[NonUniformResourceIndex(target.index)][int2(id.xy)] = value;
}
