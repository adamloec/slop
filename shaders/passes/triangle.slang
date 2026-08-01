// The first render — M0 task F.
//
// Deliberately allocates nothing: vertex positions come from SV_VertexID rather
// than a vertex buffer, so the first frame on screen does not also depend on the
// memory allocator, buffer uploads, or descriptor sets being correct. Those
// arrive with the cube.
//
// Both entry points live in one file. Slang compiles them into a single SPIR-V
// module with two entry points, which is why the engine loads one module and
// names the stages rather than juggling a pair of files per shader.

struct VertexOutput
{
    float4 position : SV_Position;
    float3 color : COLOR;
};

// Clip space directly — no projection matrix, so none of the depth conventions
// in slop-math apply yet. Vulkan's NDC has +Y pointing down, which is why the
// apex is at negative Y.
static const float2 POSITIONS[3] = {
    float2(0.0, -0.6),
    float2(0.6, 0.6),
    float2(-0.6, 0.6),
};

static const float3 COLORS[3] = {
    float3(1.0, 0.0, 0.0),
    float3(0.0, 1.0, 0.0),
    float3(0.0, 0.0, 1.0),
};

[shader("vertex")]
VertexOutput vertexMain(uint vertexIndex: SV_VertexID)
{
    VertexOutput output;
    output.position = float4(POSITIONS[vertexIndex], 0.0, 1.0);
    output.color = COLORS[vertexIndex];
    return output;
}

[shader("fragment")]
float4 fragmentMain(VertexOutput input) : SV_Target
{
    // Interpolated across the triangle by the rasterizer.
    return float4(input.color, 1.0);
}
