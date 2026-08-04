// The environment itself, drawn where nothing else was — `docs/PLAN.md` §9.7 E6e.
//
// # This is not for looks
//
// Without it the frame shows the environment only through its *effect* on
// surfaces, and "lit by the right environment" and "lit by an environment" stop
// being distinguishable — the gap §6.1 records for the shadow cascades, arriving
// through a second door. With the sky visible beside the surfaces it lights, a
// reflection pointing the wrong way is something a human can see in one frame
// rather than something that needs a test to suspect.
//
// # Where it sits in the frame, and why that is the whole design
//
// After the forward pass, into the same colour and depth attachments, at the far
// plane. Under reverse-Z the far plane is zero and `DEPTH_COMPARE` is
// `GREATER_OR_EQUAL`, so a fragment here passes exactly where the depth buffer
// is still at its cleared value — which is exactly where no geometry was drawn.
// That is one comparison doing the work a stencil mask or a second target would
// otherwise be invented for, and it needs no new pipeline state: the depth
// configuration `slop-rhi` already has is the configuration this wants.
//
// The consequence is that the pass shades only background pixels. Drawing the
// sky *first* and letting the scene overdraw it would be the same picture for
// more work, and would move the colour clear into a pass that only exists when
// an environment has been fetched.
//
// # The sky and a mirror are one lookup
//
// The fragment calls `specularFrom` at roughness zero rather than sampling the
// cube itself. A perfectly smooth surface reflects the environment, and the sky
// *is* the environment seen directly, so the two must agree by construction: a
// separate lookup here could pick a different level, a different sampler, or a
// different cube, and the failure would read as "the reflections are subtly
// wrong" rather than as two code paths disagreeing.
//
// It also means the degenerate case costs nothing to state — `specularFrom`
// returns black when the frame has no environment, and the pass is not declared
// then.

#include "lib/bindless.slang"
#include "lib/lighting/environment.slang"

struct PushConstants
{
    // Clip space back to world space. The camera position is **not** here and is
    // not needed: a ray direction is the difference of two points on the ray,
    // and both come out of this matrix.
    float4x4 inverseViewProjection;

    // The environment buffer's slot — the same index the forward pass reads, so
    // the sky drawn here and the sky reflected there cannot be two different
    // environments.
    uint environment;
};

[[vk::push_constant]] PushConstants g_push;

// The second depth the ray is unprojected at, in NDC.
//
// One over this is how many near-plane distances down the ray the far point
// sits, under the reverse-Z infinite projection `slop_math::perspective` builds.
// The only thing it has to buy is separation: the *near* point is a fraction of
// a unit from the eye, so a difference taken against something equally close
// would cancel most of the significant digits it started with.
static const float RAY_DEPTH = 1.0 / 1024.0;

struct VertexOutput
{
    float4 position : SV_Position;

    // The world-space ray through this pixel, unnormalised.
    //
    // **Interpolating this is exact, not an approximation.** A point unprojected
    // at a fixed NDC depth lies on a plane of constant view-space depth, which is
    // parallel to the image plane — so it is an affine function of screen
    // position, and so is the difference of two of them. Normalising per vertex
    // and interpolating *that* would not be: unit vectors do not stay unit under
    // a lerp, and the sky would bend towards the triangle's corners.
    float3 direction : POSITION0;
};

// One triangle, not two — see `passes/post/tonemap.slang` for why.
[shader("vertex")]
VertexOutput vertexMain(uint id: SV_VertexID)
{
    VertexOutput output;

    // 0 -> (0,0), 1 -> (2,0), 2 -> (0,2), then into clip space. Vulkan's NDC has
    // +Y down and its framebuffer origin is top-left, so this is the same sense
    // the projection matrix already flipped into.
    float2 ndc = float2((id << 1) & 2, id & 2) * 2.0 - 1.0;

    // Zero is the far plane under reverse-Z. The depth test does the rest.
    output.position = float4(ndc, 0.0, 1.0);

    float4 near = mul(g_push.inverseViewProjection, float4(ndc, 1.0, 1.0));
    float4 far = mul(g_push.inverseViewProjection, float4(ndc, RAY_DEPTH, 1.0));

    output.direction = far.xyz / far.w - near.xyz / near.w;

    return output;
}

[shader("fragment")]
float4 fragmentMain(VertexOutput input) : SV_Target
{
    EnvironmentGpu environment =
        loadFromBuffer<EnvironmentGpu>(BufferHandle(g_push.environment), 0);

    // Roughness zero: the sky is what a mirror would reflect, which is level
    // zero of the prefiltered chain — the environment untouched.
    float3 sky = specularFrom(environment, normalize(input.direction), 0.0);

    // Opaque, like `tonemap`: the alpha of a background is not coverage
    // information anything downstream reads, and carrying a low one to the
    // swapchain makes some compositors treat the window as translucent.
    return float4(sky, 1.0);
}
