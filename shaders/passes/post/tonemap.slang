// Resolving the HDR target to the swapchain — `docs/PLAN.md` §9.5 E2.
//
// The scene renders into a floating-point target because radiance has no upper
// bound: sunlit stone and a shadowed arch differ by orders of magnitude, and an
// eight-bit target clips the top of that before anything gets to decide what to
// do about it. This pass is what decides.
//
// # It decides nothing yet, deliberately
//
// The curve is identity. §9.4 puts the choice at E7 and says why: a tonemap
// curve is a *look* decision, it is one function, and picking it before there is
// lit content to judge it on would be picking it blind.
//
// What the identity buys now is a verification that nothing else does. The
// existing golden images were approved against geometry drawn straight into the
// swapchain; routing the same colours through a float target and back out must
// reproduce them. A curve here would change every pixel and the plumbing would
// have to be trusted instead of checked.
//
// # Where the sRGB encode happens, and why this pass does not do it
//
// The swapchain is an `_SRGB` format, so the hardware applies the transfer
// function on write. The HDR target is float and linear, and sampling it applies
// nothing. So linear values pass through untouched and get encoded exactly once,
// at the same place they were encoded before this pass existed.
//
// Doing the encode here as well would apply it twice, which washes the image out
// in a way that looks like a lighting bug.

#include "lib/bindless.slang"

struct PushConstants
{
    // The HDR target's slot in the bindless heap, and the sampler to read it
    // with.
    uint source;
    uint sampler;
};

[[vk::push_constant]] PushConstants g_push;

struct VertexOutput
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

// **One triangle, not two.** A fullscreen quad is the obvious shape and the
// wrong one: two triangles meet along the diagonal, and every pixel on that seam
// is rasterized by both, which costs a second shading pass over a line across the
// middle of the screen. One oversized triangle covering the viewport has no seam,
// and the hardware clips what falls outside for free.
//
// The vertices are (-1,-1), (3,-1), (-1,3) in clip space, so the [0,1] UV range
// lands exactly on the viewport.
[shader("vertex")]
VertexOutput vertexMain(uint id: SV_VertexID)
{
    VertexOutput output;

    // 0 -> (0,0), 1 -> (2,0), 2 -> (0,2).
    output.uv = float2((id << 1) & 2, id & 2);

    // UV to clip space. Vulkan's NDC has +Y down and its framebuffer origin is
    // top-left, which is the same sense the UV runs in — so no flip.
    output.position = float4(output.uv * 2.0 - 1.0, 0.0, 1.0);

    return output;
}

[shader("fragment")]
float4 fragmentMain(VertexOutput input) : SV_Target
{
    TextureHandle source = { g_push.source };
    SamplerHandle sampler = { g_push.sampler };

    // Point-sampled and one-to-one: the HDR target is the size of the swapchain,
    // so every fragment reads exactly one texel. Filtering would only blur it.
    float4 hdr = sampleTexture(source, sampler, input.uv);

    // The identity, and the one line E7 replaces. Alpha is forced opaque: the
    // scene's alpha is coverage information the blend already consumed, and
    // carrying it to the swapchain would make the compositor treat the window as
    // translucent on some platforms.
    return float4(hdr.rgb, 1.0);
}
