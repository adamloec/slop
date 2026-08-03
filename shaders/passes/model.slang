// Drawing a cooked model: many meshes, each with its own material.
//
// The cube's shader (`cube.slang`) is a fixed surface with one hardcoded
// texture. This one reads its parameters from the bindless heap, so a scene of
// several hundred materials is one pipeline and one draw loop rather than a
// pipeline per surface.

#include "lib/bindless.slang"
#include "lib/cluster.slang"
#include "lib/environment.slang"
#include "lib/light.slang"
#include "lib/shadow.slang"

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

// Where one placement of one mesh sits, matching `InstanceGpu` in
// `crates/slop-render/src/mesh.rs`.
//
// **The model matrix is stored as its four columns, not as a `float4x4`.** That
// is not stylistic: a matrix in a structured buffer has a layout convention, and
// getting it backwards transposes every transform in the scene — which looks
// like broken geometry rather than like a layout bug. Four `float4` have exactly
// one interpretation, and the multiply below is written out, so there is no
// convention to get wrong.
struct InstanceGpu
{
    float4 modelColumn0;
    float4 modelColumn1;
    float4 modelColumn2;
    float4 modelColumn3;

    // Rows of the normal matrix — transpose(inverse(mat3(model))). Three float4
    // because a float3x3 is padded to the same size anyway and the packing is
    // then implicit rather than stated.
    float4 normalRow0;
    float4 normalRow1;
    float4 normalRow2;
};

// Per-draw data. 84 of the 128 bytes Vulkan guarantees.
//
// It used to be 120, carrying the model-view-projection and the normal matrix
// per draw, and it was full — which is why the model matrix itself could not be
// here and why the tangent was transformed by the wrong matrix. Both moved to
// `InstanceGpu`, where `docs/DESIGN.md` §4.2 stage B needs them anyway: a draw
// list built on the GPU has nothing per-draw to push.
struct PushConstants
{
    // The camera, not the object. Per frame rather than per draw, and still
    // pushed per draw because it is 64 bytes either way and a per-frame buffer
    // for one value would be machinery for nothing.
    float4x4 viewProjection;

    uint instances;
    uint instance;

    uint materials;
    uint material;

    // The cluster grid, which is also where this frame's lights are. One index
    // rather than several, so that this shader and `cluster_build.slang` cannot
    // hold different opinions about the grid — see `lib/cluster.slang`.
    uint grid;
    // The directional light and the ambient term. A second index rather than
    // fields in the grid buffer: that one describes where the *cells* are, and
    // the shadow passes read this without caring about clustering.
    uint environment;
    // The cascades, or `NO_SHADOWS` while a pass has none — which is every
    // shadow render itself, since a cascade does not shadow itself.
    uint shadows;
    uint _pad;
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
    // Where this fragment is in the world, which is what a point light needs to
    // measure a distance from. Interpolated, so it is the surface point rather
    // than the vertex.
    float3 worldPosition : POSITION0;
    float3 normal : NORMAL;
    float2 uv : TEXCOORD0;
    // World-space tangent, with handedness carried through untouched in `w`.
    //
    // Interpolated across the triangle like the normal, and renormalised in the
    // fragment shader for the same reason: interpolating two unit vectors gives
    // something shorter than unit length between them.
    float4 tangent : TANGENT;
};

// A point in object space, moved into the world by an instance's model matrix.
//
// Written out column by column rather than as a matrix multiply, for the reason
// `InstanceGpu` gives: four `float4` have one interpretation and a `float4x4` in
// a buffer has two.
float3 toWorld(InstanceGpu instance, float3 position)
{
    return instance.modelColumn0.xyz * position.x
         + instance.modelColumn1.xyz * position.y
         + instance.modelColumn2.xyz * position.z
         + instance.modelColumn3.xyz;
}

// A *direction* moved into the world — the same thing without the translation.
float3 directionToWorld(InstanceGpu instance, float3 direction)
{
    return instance.modelColumn0.xyz * direction.x
         + instance.modelColumn1.xyz * direction.y
         + instance.modelColumn2.xyz * direction.z;
}

[shader("vertex")]
VertexOut vertexMain(VertexIn input)
{
    InstanceGpu instance =
        loadFromBuffer<InstanceGpu>(BufferHandle(g_push.instances), g_push.instance);

    VertexOut output;

    float3 world = toWorld(instance, input.position);

    output.worldPosition = world;
    output.position = mul(g_push.viewProjection, float4(world, 1.0));

    // The normal matrix rather than the model matrix. They differ under
    // non-uniform scale, and using the model matrix there tilts every normal —
    // which looks like a lighting bug and is a transform one.
    float3x3 normalMatrix = float3x3(
        instance.normalRow0.xyz,
        instance.normalRow1.xyz,
        instance.normalRow2.xyz);

    output.normal = normalize(mul(normalMatrix, input.normal));
    output.uv = input.uv;

    // **The model matrix, on the tangent, which is now the correct one.** A
    // normal is perpendicular to the surface and transforms by the inverse
    // transpose; a tangent lies *along* the surface and transforms like any
    // other direction. This used to use the normal matrix because the model
    // matrix would not fit in the push block, which skewed tangents under
    // non-uniform scale; the instance buffer is what fixed it.
    //
    // `w` passes through untouched: handedness is a property of the UV mapping,
    // not of where the model sits.
    output.tangent =
        float4(normalize(directionToWorld(instance, input.tangent.xyz)), input.tangent.w);

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

    // The directional light and the ambient term. Both were constants compiled
    // into this file until E5 needed the direction on the CPU — cascaded shadow
    // maps are built along it, so it has to be something a caller chooses.
    EnvironmentGpu environment =
        loadFromBuffer<EnvironmentGpu>(BufferHandle(g_push.environment), 0);

    // The view-space depth this fragment sits at, which both the cluster lookup
    // and the cascade choice need. The projection puts `-z` in `w`, and a
    // fragment shader's `SV_Position.w` holds its reciprocal.
    float viewDepth = 1.0 / input.position.w;

    // How much of the sun reaches here. One when there are no cascades, which
    // is what a shadow render itself uses — a cascade cannot shadow itself, and
    // reading the map it is currently writing would be a hazard as well as
    // nonsense.
    float sunlight = 1.0;

    if (g_push.shadows != 0xFFFFFFFF)
    {
        ShadowsGpu shadows = loadFromBuffer<ShadowsGpu>(BufferHandle(g_push.shadows), 0);

        sunlight = shadowFactor(
            shadows,
            input.worldPosition,
            normal,
            environment.sunDirection,
            viewDepth);
    }

    float3 lit = environment.ambient
               + environment.sunColor * environment.sunIntensity * sunlight
                     * saturate(dot(normal, environment.sunDirection));

    // Matching `slop_render::NO_CLUSTERS`. Not zero, because zero is a real heap
    // slot and a view without a grid would read whatever landed there.
    if (g_push.grid == 0xFFFFFFFF)
    {
        return float4(albedo.rgb * lit, albedo.a);
    }

    ClusterGridGpu grid = loadFromBuffer<ClusterGridGpu>(BufferHandle(g_push.grid), 0);

    // Which cell this fragment is in, from the depth computed above.
    uint3 cell = clusterCellOf(grid, input.position.xy, viewDepth);
    uint cluster = clusterIndex(grid, cell);

    ClusterRange range = loadFromBuffer<ClusterRange>(BufferHandle(grid.ranges), cluster);

    // **Only this cell's lights.** That is the whole of clustered forward+: the
    // cost stops being fragments times lights and becomes fragments times the
    // lights that actually reach them.
    for (uint slot = 0; slot < range.count; ++slot)
    {
        uint index = loadFromBuffer<uint>(BufferHandle(grid.indices), range.offset + slot);
        PointLightGpu light = loadFromBuffer<PointLightGpu>(BufferHandle(grid.lights), index);

        float3 toLight = light.position - input.worldPosition;
        float distanceSquared = dot(toLight, toLight);

        // The same falloff the cluster build's radius test assumes, from the
        // same place, so the two cannot drift — see `lib/light.slang`.
        float attenuation = pointLightAttenuation(distanceSquared, light.radius);

        if (attenuation <= 0.0)
        {
            continue;
        }

        float lambert = saturate(dot(normal, normalize(toLight)));

        lit += light.color * light.intensity * lambert * attenuation;
    }

    return float4(albedo.rgb * lit, albedo.a);
}
