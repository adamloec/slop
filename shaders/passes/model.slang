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
};

struct VertexOut
{
    float4 position : SV_Position;
    float3 normal : NORMAL;
    float2 uv : TEXCOORD0;
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

    return output;
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
    float lambert = saturate(dot(normal, LIGHT_DIRECTION));

    return float4(albedo.rgb * (AMBIENT + lambert), albedo.a);
}
