// Cascaded shadow maps, as the GPU sees them.
//
// The Rust half is `ShadowsGpu` in `crates/slop-render/src/shadow.rs`, where the
// cascade fitting is implemented and tested on the CPU.
//
// Include it as:
//
//     #include "lib/lighting/shadow.slang"

#ifndef SLOP_SHADOW_INCLUDED
#define SLOP_SHADOW_INCLUDED

// Matching `CASCADES` on the Rust side.
static const uint SHADOW_CASCADES = 4;

struct CascadeGpu
{
    // World to the light's clip space. Four explicit columns, because a matrix
    // in a structured buffer has a layout convention on each side and the two
    // disagreeing transposes it -- which here would sample the shadow map at a
    // completely unrelated place and read as shadows in the wrong shape.
    float4 lightColumn0;
    float4 lightColumn1;
    float4 lightColumn2;
    float4 lightColumn3;

    // Where this cascade stops, in view-space depth.
    float far;
    // How big one texel is in world units, for scaling the normal offset.
    float texelWorldSize;
    float _pad0;
    float _pad1;
};

struct ShadowsGpu
{
    CascadeGpu cascades[SHADOW_CASCADES];

    uint map;
    uint sampler;
    float depthBias;
    float slopeBias;

    // One texel, in UV. Shared across cascades because they share a resolution.
    float texelUv;
    float _pad0;
    float _pad1;
    float _pad2;
};

// Which cascade a fragment at `depth` belongs to.
//
// The first whose far edge it is within. Beyond the last one there is no shadow
// data, and the caller checks for that rather than clamping -- clamping would
// sample the last cascade's map far outside its box, which after edge clamping
// reads as a hard shadow boundary at a fixed distance.
uint shadowCascadeOf(ShadowsGpu shadows, float depth)
{
    for (uint index = 0; index < SHADOW_CASCADES; ++index)
    {
        if (depth < shadows.cascades[index].far)
        {
            return index;
        }
    }

    return SHADOW_CASCADES;
}

// A world position in one cascade's clip space.
float4 shadowClipOf(CascadeGpu cascade, float3 world)
{
    return cascade.lightColumn0 * world.x
         + cascade.lightColumn1 * world.y
         + cascade.lightColumn2 * world.z
         + cascade.lightColumn3;
}

// How much of the light reaches `world`: 1 for fully lit, 0 for fully shadowed.
//
// # The bias, and why there are two
//
// A shadow map records one depth per texel, and a texel covers an area. A
// surface at a glancing angle to the light spans a range of depths inside one
// texel, so comparing its own depth against the single recorded value makes
// roughly half of it shadow itself -- the striped artefact called **acne**.
//
// A constant bias large enough to clear that on a glancing surface is far too
// large on one facing the light, where it detaches the shadow from its caster
// (**peter-panning**). So the bias scales with the slope: `1/dot(N, L)` grows
// exactly as the depth span per texel does.
//
// The **normal offset** does the rest, and does it where bias cannot: moving the
// sample point along the surface normal by about a texel's world size sidesteps
// self-shadowing geometrically rather than by fudging the comparison, and it
// does not detach contact shadows the way a large depth bias does.
//
// # Reverse-Z
//
// `slop_math::orthographic` reverses depth, so **larger is nearer** and the
// shadow map holds the maximum. A fragment is occluded when its own depth is
// *less* than what was recorded, and the bias is therefore added to the
// fragment rather than subtracted. Getting this backwards shadows everything
// except what should be shadowed, which is the sort of wrong that looks
// deliberate.
float shadowFactor(
    ShadowsGpu shadows,
    float3 world,
    float3 normal,
    float3 toLight,
    float viewDepth)
{
    uint index = shadowCascadeOf(shadows, viewDepth);

    // Past the last cascade: nothing is known, so nothing is shadowed. Better
    // than clamping, which would paint a hard edge at the range's end.
    if (index >= SHADOW_CASCADES)
    {
        return 1.0;
    }

    CascadeGpu cascade = shadows.cascades[index];

    float incidence = saturate(dot(normal, toLight));

    // A surface facing away from the light is not lit regardless of what the
    // shadow map says, and its shadow lookup is meaningless -- the normal offset
    // below would push the sample through the surface.
    if (incidence <= 0.0)
    {
        return 0.0;
    }

    // Along the normal, scaled by how glancing the light is. One texel at
    // normal incidence, more as the surface turns away.
    float offsetScale = cascade.texelWorldSize * (2.0 - incidence);
    float4 clip = shadowClipOf(cascade, world + normal * offsetScale);

    // Orthographic, so `w` is one and the divide is a formality -- kept because
    // the matrix is general and a projective one would need it.
    float3 ndc = clip.xyz / clip.w;

    // Outside the cascade's box. Its own far edge should have caught this, but
    // the sphere fit is generous and a fragment near a corner can fall outside
    // the box while still being inside the depth range.
    if (any(abs(ndc.xy) > 1.0) || ndc.z <= 0.0 || ndc.z >= 1.0)
    {
        return 1.0;
    }

    float2 uv = ndc.xy * 0.5 + 0.5;

    // Slope-scaled, and clamped: at grazing incidence `1/incidence` runs away,
    // and an unbounded bias detaches every shadow near a silhouette.
    float bias = shadows.depthBias + shadows.slopeBias * min(1.0 / incidence - 1.0, 4.0);

    // Reverse-Z: nearer is larger, so a fragment is lit when its own depth --
    // biased *towards* the light -- is at least what the map recorded.
    float reference = ndc.z + bias;

    // A 3x3 box of taps, one texel apart, averaged. Not a weighted kernel: at
    // this radius the difference from a box average is invisible, and the point
    // is to turn a binary edge into a few steps rather than to be smooth.
    TextureHandle map = { shadows.map };
    SamplerHandle sampler = { shadows.sampler };

    float lit = 0.0;

    for (int y = -1; y <= 1; ++y)
    {
        for (int x = -1; x <= 1; ++x)
        {
            float2 tap = uv + float2(float(x), float(y)) * shadows.texelUv;

            // `.r` is the depth: this is a `D32Float` image sampled as a
            // texture, so the one channel it has is what was written.
            float recorded = sampleTextureArrayLevel(map, sampler, tap, index).r;

            lit += reference >= recorded ? 1.0 : 0.0;
        }
    }

    return lit / 9.0;
}

#endif // SLOP_SHADOW_INCLUDED
