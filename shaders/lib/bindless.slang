// The bindless descriptor heap, as shaders see it.
//
// This file is the shader half of an ABI whose other half is
// `crates/slop-rhi/src/descriptor.rs`. The set and binding numbers below MUST
// match the constants there:
//
//     HEAP_SET                 = 0
//     SAMPLED_IMAGE_BINDING    = 0
//     SAMPLER_BINDING          = 1
//     STORAGE_IMAGE_BINDING    = 2
//
// Nothing checks that they agree. A mismatch is not a compile error and not a
// validation error — it is a shader reading a different array than the one the
// engine wrote to, which looks like a content bug. Shader reflection at M2
// (DESIGN.md 2.11) is what will let one side be derived from the other.
//
// Files in `shaders/lib/` are includes and are never cooked on their own: they
// declare no entry points, so `slangc` would have nothing to compile. The cook
// step skips this directory and folds its contents into every shader's cache
// key instead (see crates/slop-cli/src/cook.rs).
//
// Include it as:
//
//     #include "lib/bindless.slang"
//
// which resolves because the cook step puts `shaders/` on the include path.

#ifndef SLOP_BINDLESS_INCLUDED
#define SLOP_BINDLESS_INCLUDED

// Unbounded arrays, matching the runtime-sized descriptor bindings the heap
// declares. The engine decides the real capacity at device creation and clamps
// it to the hardware limit; shaders never need to know the number.
//
// Images and samplers are separate — never `Sampler2D` — so N textures and M
// samplers cost N + M descriptors rather than N * M. Combine them at the point
// of use with `SampledImage`.
[[vk::binding(0, 0)]] Texture2D<float4> g_textures[];
[[vk::binding(1, 0)]] SamplerState g_samplers[];
[[vk::binding(2, 0)]] RWTexture2D<float4> g_storage_images[];

// A texture slot. Mirrors `Handle<SampledImage>::index()` on the CPU side; the
// generation stays there and never crosses to the GPU.
//
// A struct rather than a bare uint so that a material index, a mesh index, and
// a texture index cannot be passed to each other's slots — the same reasoning
// as `Handle<T>` being typed on the CPU (DESIGN.md 2.6).
struct TextureHandle
{
    uint index;
};

struct SamplerHandle
{
    uint index;
};

struct StorageImageHandle
{
    uint index;
};

// Sample a texture from the heap.
//
// `NonUniformResourceIndex` is not optional and not a hint. Without it, the
// compiler may assume every lane in a wave uses the same index and read one
// lane's texture for all of them. That is correct for a whole draw using one
// material and silently wrong the moment a GPU-driven pass puts different
// materials in one wave, which is the entire point of the bindless heap.
float4 sampleTexture(TextureHandle texture, SamplerHandle sampler, float2 uv)
{
    return g_textures[NonUniformResourceIndex(texture.index)]
        .Sample(g_samplers[NonUniformResourceIndex(sampler.index)], uv);
}

// Sample with an explicit mip level, for shaders outside a pixel stage where
// there are no derivatives to compute one from.
float4 sampleTextureLevel(TextureHandle texture, SamplerHandle sampler, float2 uv, float level)
{
    return g_textures[NonUniformResourceIndex(texture.index)]
        .SampleLevel(g_samplers[NonUniformResourceIndex(sampler.index)], uv, level);
}

// Read a texel directly, with no filtering.
float4 loadTexel(TextureHandle texture, int2 coordinate, int level = 0)
{
    return g_textures[NonUniformResourceIndex(texture.index)].Load(int3(coordinate, level));
}

#endif // SLOP_BINDLESS_INCLUDED
