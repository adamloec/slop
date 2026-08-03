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
//     STORAGE_BUFFER_BINDING   = 3
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

// The sampled-image binding again, as arrays.
//
// Not a second descriptor and not a second slot: binding 0 holds image *views*,
// and a view created with `VK_IMAGE_VIEW_TYPE_2D_ARRAY` is what this reads. The
// same aliasing the storage buffers use above, and for the same reason -- the
// shadow cascades at `docs/PLAN.md` §9.4 are one image with four layers, and a
// shader sampling them needs a third coordinate that `Texture2D` has no room
// for.
//
// A slot holds one or the other. Reading a plain 2D texture through this view,
// or an array through `g_textures`, is a type mismatch nothing reports -- so a
// handle's *kind* is tracked on the CPU side by `Handle<SampledImage>` and by
// which accessor a shader reaches for.
//
// Slang warns "explicit binding overlap" on this pair, as it does for the
// buffers. The cooker disables that warning by id.
[[vk::binding(0, 0)]] Texture2DArray<float4> g_texture_arrays[];

// Structured data a shader indexes: per-material parameters, per-instance
// transforms, and the draw lists a GPU-driven pass builds (DESIGN.md 4.2).
//
// Untyped, and that is what lets one binding serve all of them. A
// `StructuredBuffer<T>` fixes T for every shader sharing the binding, so
// materials and transforms would need a binding each — and a binding added to
// this layout later invalidates every pipeline built against it. Reading through
// `Load<T>` moves the type to the point of use, where it belongs.
//
// Offsets are in bytes and must be a multiple of four.
[[vk::binding(3, 0)]] ByteAddressBuffer g_buffers[];

// The same binding again, writable, for compute.
//
// Not a second descriptor and not a second slot: binding 3 is already
// VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, which permits writes -- only the read-only
// declaration above decides otherwise. Aliasing one binding with two views is
// legal, and is how a compute pass writes the buffer a fragment shader later
// reads through the same heap index.
//
// **The split is a hard requirement, not tidiness.** Declaring one writable view
// for everyone was tried, and it makes every fragment shader that touches a
// storage buffer declare write access -- which Vulkan only permits under the
// `fragmentStoresAndAtomics` device feature. The model shader's pipeline was
// rejected outright: "uses descriptor [Set 0, Binding 3] which is not marked
// with NonWritable, but fragmentStoresAndAtomics was not enabled". Reading
// through the read-only view is what keeps that feature unnecessary.
//
// Slang warns "explicit binding overlap" on this pair. The cooker disables that
// one warning by id, deliberately and narrowly -- see `shader_import.rs`.
[[vk::binding(3, 0)]] RWByteAddressBuffer g_rw_buffers[];

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

struct BufferHandle
{
    uint index;
};

// Read one element out of a heap buffer.
//
// `NonUniformResourceIndex` for the same reason sampling needs it: without it
// the compiler may assume every lane in a wave reads the same buffer, which is
// correct for one draw and wrong the moment a GPU-driven pass puts several in
// one wave.
T loadFromBuffer<T>(BufferHandle buffer, uint index)
{
    return g_buffers[NonUniformResourceIndex(buffer.index)].Load<T>(index * sizeof(T));
}

// Write one element into a heap buffer.
//
// The counterpart of `loadFromBuffer`, through the writable view. `T` must be
// blittable and its size a multiple of four, as the load side requires.
void storeToBuffer<T>(BufferHandle buffer, uint index, T value)
{
    g_rw_buffers[NonUniformResourceIndex(buffer.index)].Store<T>(index * sizeof(T), value);
}

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

// Sample one layer of an array texture, at the base mip level.
//
// `SampleLevel` rather than `Sample`: a shadow lookup happens inside branches
// that differ between neighbouring fragments, where implicit derivatives are
// undefined. The shadow map has one level anyway, so nothing is lost.
float4 sampleTextureArrayLevel(TextureHandle texture, SamplerHandle sampler, float2 uv, uint layer)
{
    return g_texture_arrays[NonUniformResourceIndex(texture.index)]
        .SampleLevel(g_samplers[NonUniformResourceIndex(sampler.index)], float3(uv, float(layer)), 0.0);
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
