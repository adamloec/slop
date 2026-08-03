// Drawing a cooked model: many meshes, each with its own material.
//
// The cube's shader (`cube.slang`) is a fixed surface with one hardcoded
// texture. This one reads its parameters from the bindless heap, so a scene of
// several hundred materials is one pipeline and one draw loop rather than a
// pipeline per surface.

#include "lib/bindless.slang"

// Per-material parameters, matching `MaterialGpu` in
// `crates/slop-render/src/mesh.rs`.
//
// Laid out so that std430 and Rust's `#[repr(C)]` agree without padding either
// side: the `float4` comes first because it needs sixteen-byte alignment, and
// the scalars that follow pack into three more sixteen-byte rows exactly.
struct MaterialGpu
{
    float4 baseColor;

    float metallic;
    float roughness;
    float alphaCutoff;
    uint flags;

    // `NO_TEXTURE` when the material has none, so the shader falls back to the
    // factors above rather than sampling whatever happens to be in slot zero.
    uint baseColorTexture;
    uint normalTexture;
    uint metallicRoughnessTexture;
    uint sampler;
};

// Matches `NO_TEXTURE` on the Rust side. Not zero: zero is a perfectly good
// slot, and a material with no albedo would sample somebody else's.
static const uint NO_TEXTURE = 0xFFFFFFFF;

// Alpha modes, matching `slop_asset::AlphaMode`'s discriminants.
static const uint ALPHA_MODE_MASK_BITS = 3;
static const uint ALPHA_MODE_MASK = 1;

// Per-draw data. Under the 128 bytes Vulkan guarantees, which is why the
// material lives in a buffer rather than here.
struct PushConstants
{
    float4x4 modelViewProjection;
    // Rows of the normal matrix — transpose(inverse(mat3(model))) — as three
    // float4 because a float3x3 in a push constant block is padded to the same
    // size anyway and the packing is then implicit rather than stated.
    float4 normalRow0;
    float4 normalRow1;
    float4 normalRow2;

    uint materials;
    uint material;
};

[[vk::push_constant]] PushConstants g_push;

struct VertexIn
{
    float3 position : POSITION;
    float3 normal : NORMAL;
    float2 uv : TEXCOORD0;
    // Object-space tangent, handedness in `w`. See `slop_asset::Vertex`.
    float4 tangent : TANGENT;
};

struct VertexOut
{
    float4 position : SV_Position;
    float3 normal : NORMAL;
    float2 uv : TEXCOORD0;
    // World-space tangent, with handedness carried through untouched in `w`.
    //
    // Interpolated across the triangle like the normal, and renormalised in the
    // fragment shader for the same reason: interpolating two unit vectors gives
    // something shorter than unit length between them.
    float4 tangent : TANGENT;
};

[shader("vertex")]
VertexOut vertexMain(VertexIn input)
{
    VertexOut output;

    output.position = mul(g_push.modelViewProjection, float4(input.position, 1.0));

    // The normal matrix rather than the model matrix. They differ under
    // non-uniform scale, and using the model matrix there tilts every normal —
    // which looks like a lighting bug and is a transform one.
    float3x3 normalMatrix = float3x3(
        g_push.normalRow0.xyz,
        g_push.normalRow1.xyz,
        g_push.normalRow2.xyz);

    output.normal = normalize(mul(normalMatrix, input.normal));
    output.uv = input.uv;

    // **The normal matrix, used on a tangent, which is not strictly correct.**
    //
    // A normal is perpendicular to the surface and transforms by the inverse
    // transpose; a tangent lies *along* the surface and transforms like any
    // other direction, by the model matrix. The two agree up to scale under
    // rotation and uniform scale — and after the normalize below, agree exactly.
    // They diverge only under non-uniform scale, where this skews the tangent.
    //
    // The model matrix is not here to use: this block is 120 of the 128 bytes
    // Vulkan guarantees, and another float3x3 needs 48 more. The fix is the
    // per-instance data that a storage buffer will carry, which is where the
    // model matrix belongs anyway. `docs/PLAN.md` §6.1 has the row.
    //
    // `w` passes through untouched: handedness is a property of the UV mapping,
    // not of where the model sits.
    output.tangent = float4(normalize(mul(normalMatrix, input.tangent.xyz)), input.tangent.w);

    return output;
}

// The depth prepass's fragment stage — cutout, and nothing else.
//
// Opaque geometry runs the prepass with **no fragment shader at all**, which is
// the entire point of a prepass: rasterization writes depth and no shading runs.
// Alpha-masked geometry cannot do that. The `discard` below is what decides
// whether the fragment exists, so a prepass that skipped it would write depth
// across each leaf card's full rectangle and occlude whatever stood behind the
// transparent part — Sponza's foliage and chains are entirely alpha-masked, so
// this is most of the scene rather than a corner case (`docs/PLAN.md` §9.4).
//
// The condition is deliberately the same expression as `fragmentMain`'s. If the
// two ever disagree the prepass cuts away a fragment the forward pass keeps, or
// keeps one it cuts, and the result is geometry that vanishes rather than an
// error anyone gets told about.
[shader("fragment")]
void prepassMain(VertexOut input)
{
    MaterialGpu material =
        loadFromBuffer<MaterialGpu>(BufferHandle(g_push.materials), g_push.material);

    float alpha = material.baseColor.a;

    if (material.baseColorTexture != NO_TEXTURE)
    {
        TextureHandle texture = { material.baseColorTexture };
        SamplerHandle sampler = { material.sampler };

        alpha *= sampleTexture(texture, sampler, input.uv).a;
    }

    if ((material.flags & ALPHA_MODE_MASK_BITS) == ALPHA_MODE_MASK && alpha < material.alphaCutoff)
    {
        discard;
    }
}

// A single directional light, deliberately.
//
// This is a bring-up shader: its job is to prove that geometry, materials and
// textures reach the GPU intact, and shading that anyone would ship arrives with
// M3's Stage A. A flat unlit surface would hide a wrong normal, so there is just
// enough lighting to make one visible.
static const float3 LIGHT_DIRECTION = normalize(float3(0.4, 0.8, 0.45));
static const float3 AMBIENT = float3(0.18, 0.19, 0.22);

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target
{
    MaterialGpu material =
        loadFromBuffer<MaterialGpu>(BufferHandle(g_push.materials), g_push.material);

    float4 albedo = material.baseColor;

    if (material.baseColorTexture != NO_TEXTURE)
    {
        TextureHandle texture = { material.baseColorTexture };
        SamplerHandle sampler = { material.sampler };

        albedo *= sampleTexture(texture, sampler, input.uv);
    }

    // Cutout, before anything else is computed for a fragment about to be
    // thrown away. Sponza's foliage and chain are entirely this, and without it
    // every leaf card is an opaque rectangle.
    if ((material.flags & ALPHA_MODE_MASK_BITS) == ALPHA_MODE_MASK && albedo.a < material.alphaCutoff)
    {
        discard;
    }

    float3 normal = normalize(input.normal);

    // Normal mapping. The map stores a direction in **tangent space** — a frame
    // aligned to the texture's axes — so applying it means building that frame
    // and rotating the sampled direction into world space.
    //
    // Guarded on the tangent being non-zero as well as on the texture existing.
    // A mesh whose source had no tangents and whose UVs are degenerate gets a
    // zero tangent (see `slop_asset::Vertex::has_tangent`), and normalizing that
    // gives NaN — which spreads into the lit colour and shows as black or white
    // pixels rather than as an obviously wrong normal.
    if (material.normalTexture != NO_TEXTURE && dot(input.tangent.xyz, input.tangent.xyz) > 0.0)
    {
        TextureHandle texture = { material.normalTexture };
        SamplerHandle sampler = { material.sampler };

        // Renormalised after interpolation, then made perpendicular to the
        // normal again by Gram-Schmidt: interpolating across a triangle does not
        // preserve either unit length or the right angle between them.
        float3 tangent = normalize(input.tangent.xyz);
        tangent = normalize(tangent - normal * dot(normal, tangent));

        // `w` is where mirrored UVs are recorded. Dropping it flips the
        // bitangent on every mirrored surface — extremely common, since artists
        // mirror half a symmetrical model to halve the texture budget — and
        // lights those surfaces as though from the opposite side.
        float3 bitangent = cross(normal, tangent) * input.tangent.w;

        // Stored 0..1, used -1..1: a normal map's texels are directions biased
        // into an unsigned texture, and skipping this leaves every normal in the
        // positive octant, which lights the whole surface as though it faced up
        // and to the right.
        float3 sampled = sampleTexture(texture, sampler, input.uv).xyz * 2.0 - 1.0;

        normal = normalize(tangent * sampled.x + bitangent * sampled.y + normal * sampled.z);
    }

    float lambert = saturate(dot(normal, LIGHT_DIRECTION));

    return float4(albedo.rgb * (AMBIENT + lambert), albedo.a);
}
