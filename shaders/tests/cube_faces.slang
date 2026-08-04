// Sample a cube map along each of the six axes and record what came back.
//
// `docs/PLAN.md` §9.7 E6c. The one thing that can check that the CPU's face
// table and the hardware's cube sampler agree about which layer a direction
// lands on — and nothing else can, because a mismatch renders as an environment
// with two sides swapped, which is entirely plausible to look at.
//
// `slop-cook`'s `cube.rs` asserts its own table is self-consistent: `face_of`
// and `direction_of` are inverses over all six faces. That says nothing about
// whether the ordering matches the API's, and getting it wrong is the single
// most likely mistake in the feature. The layer order is fixed — `+X, -X, +Y,
// -Y, +Z, -Z` — and this is what says the engine writes it that way.
//
// Six invocations, one direction each, each writing what it sampled. That the
// result lands where the CPU expects is the point; what it samples is a colour
// per face chosen by the test.

#include "lib/bindless.slang"

struct PushConstants
{
    // The cube to sample, and the sampler to read it with.
    uint cube;
    uint sampler;

    // Where to record the six results, one `uint` each.
    uint target;
};

[[vk::push_constant]] PushConstants g_push;

// One thread per face, in layer order. Written out rather than derived so this
// file states the order it is checking rather than importing it from the thing
// under test.
static const float3 AXES[6] = {
    float3(1.0, 0.0, 0.0),
    float3(-1.0, 0.0, 0.0),
    float3(0.0, 1.0, 0.0),
    float3(0.0, -1.0, 0.0),
    float3(0.0, 0.0, 1.0),
    float3(0.0, 0.0, -1.0),
};

[shader("compute")]
[numthreads(6, 1, 1)]
void cubeFacesMain(uint3 id: SV_DispatchThreadID)
{
    if (id.x >= 6)
    {
        return;
    }

    TextureHandle cube = { g_push.cube };
    SamplerHandle sampler = { g_push.sampler };
    BufferHandle target = { g_push.target };

    // Level zero, and the centre of a face — where filtering has nothing to
    // blend with, so the value read is the value written rather than a mixture
    // of two faces.
    float4 read = sampleCubeLevel(cube, sampler, AXES[id.x], 0.0);

    // The red channel, scaled back to the 0..255 the test wrote. Comparing
    // integers rather than floats keeps the assertion exact.
    storeToBuffer<uint>(target, id.x, uint(read.r * 255.0 + 0.5));
}
