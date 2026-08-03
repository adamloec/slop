// The M0 exit criterion: a lit, textured cube.
//
// Deliberately unambitious. Its job is integration, not looks — it exercises
// vertex and index buffers, a staged texture upload, the bindless heap, depth
// testing under reverse-Z, and a per-draw push constant, all at once. Every
// piece of that is replaced by the material system at M2; none of the shading
// here is meant to survive.

#include "lib/bindless.slang"

// Per-draw data. Push constants rather than a uniform buffer: no descriptor, no
// allocation, no synchronization, and the 128 bytes Vulkan guarantees are more
// than this needs.
//
// The layout must match `PushConstants` in the Rust side exactly. A field added
// on one side and not the other reads garbage rather than failing to compile —
// shader reflection at M2 (DESIGN.md 2.11) is what removes the duplication.
struct PushConstants
{
    float4x4 modelViewProjection;
    float4x4 model;
    uint texture;
    uint sampler;
};

[[vk::push_constant]] PushConstants g_push;

struct VertexIn
{
    float3 position : POSITION;
    float3 normal : NORMAL;
    float2 uv : TEXCOORD0;
    // Declared but unused, and it must stay declared. The vertex layout is
    // derived from this struct by reflection, so leaving the tangent out would
    // compute a 32-byte stride for a buffer whose vertices are 48 — every
    // vertex after the first would be read from the middle of its predecessor.
    //
    // The cube has no normal map. It carries tangents because the mesh format
    // has one layout, which `docs/PLAN.md` §6.1 records as deliberate.
    float4 tangent : TANGENT;
};

struct VertexOut
{
    float4 clipPosition : SV_Position;
    float3 worldNormal : NORMAL;
    float2 uv : TEXCOORD0;
};

[shader("vertex")]
VertexOut vertexMain(VertexIn input)
{
    VertexOut output;

    output.clipPosition = mul(g_push.modelViewProjection, float4(input.position, 1.0));

    // The normal matrix would be the inverse transpose in general. This cube is
    // rotated and uniformly scaled only, and for that the model matrix is
    // already correct -- taking the upper 3x3 directly is exact here and wrong
    // the moment non-uniform scale appears, which is a thing the material
    // system will have to handle properly.
    output.worldNormal = normalize(mul((float3x3)g_push.model, input.normal));
    output.uv = input.uv;

    return output;
}

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target
{
    TextureHandle texture = { g_push.texture };
    SamplerHandle sampler = { g_push.sampler };

    float3 albedo = sampleTexture(texture, sampler, input.uv).rgb;

    // A single directional light plus a flat ambient term. Not PBR, not even
    // close -- DESIGN.md 4.2 stage A is where lighting actually gets built. This
    // exists so the cube's faces are distinguishable, which is what makes a
    // wrong normal or a wrong winding visible.
    const float3 lightDirection = normalize(float3(0.4, 0.8, 0.45));
    const float3 lightColour = float3(1.0, 0.97, 0.92);
    const float3 ambient = float3(0.10, 0.11, 0.14);

    float3 normal = normalize(input.worldNormal);
    float diffuse = max(dot(normal, lightDirection), 0.0);

    float3 lit = albedo * (ambient + lightColour * diffuse);

    // No tonemapping and no gamma. The target is UNORM, so what is written is
    // what is read back -- which is what keeps the golden image comparing bytes
    // the shader produced rather than bytes a colour space conversion produced.
    return float4(lit, 1.0);
}
