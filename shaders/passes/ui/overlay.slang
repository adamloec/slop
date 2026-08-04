// The debug overlay — DESIGN.md 10.2.
//
// Draws what egui tessellated: textured, vertex-coloured triangles in screen
// space, clipped by scissor rectangles the renderer sets per draw. There is no
// lighting, no depth, and no projection matrix — egui works in logical pixels
// with the origin at the top left, so the vertex shader only has to map that
// range onto clip space.

#include "lib/bindless.slang"

// Per-draw data. The screen size is what turns egui's pixel coordinates into
// clip space; the texture index selects the font atlas or a user image out of
// the bindless heap.
//
// Not restated in Rust: `Overlay` sizes its pipeline layout from this shader's
// cooked reflection (PLAN.md 9.2 item B).
struct PushConstants
{
    float2 screenSize;
    uint texture;
    uint sampler;
};

[[vk::push_constant]] PushConstants g_push;

struct VertexIn
{
    // Logical pixels, origin top left.
    float2 position : POSITION;
    float2 uv : TEXCOORD0;
    // sRGB with premultiplied alpha. Supplied as four normalized bytes and read
    // as a float4 — the buffer format is R8G8B8A8_UNORM, which reflection cannot
    // see. See `slop-render`'s `vertex` module on why that is the caller's
    // choice rather than something derived.
    float4 color : COLOR;
};

struct VertexOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
    float4 color : COLOR;
};

[shader("vertex")]
VertexOut vertexMain(VertexIn input)
{
    VertexOut output;

    // Pixels to normalized device coordinates. Vulkan's Y axis points down in
    // NDC, which is the same direction egui's does, so Y is *not* flipped —
    // getting this wrong renders the whole interface upside down while every
    // individual widget looks correct.
    output.position = float4(
        2.0 * input.position.x / g_push.screenSize.x - 1.0,
        2.0 * input.position.y / g_push.screenSize.y - 1.0,
        0.0,
        1.0);
    output.uv = input.uv;
    output.color = input.color;

    return output;
}

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target
{
    float4 texel = g_textures[g_push.texture].Sample(g_samplers[g_push.sampler], input.uv);

    // Both sides are premultiplied-alpha sRGB, so a plain multiply is the
    // correct composite. egui does its own blending maths in that space, and
    // converting here would double-apply the transfer function.
    return input.color * texel;
}
