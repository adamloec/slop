// The cluster grid, as the GPU sees it.
//
// Shared, and that is the whole point of it being a buffer rather than push
// constants: `passes/cluster_build.slang` puts lights into cells and
// `passes/model.slang` looks its own cell up, and the two disagreeing about
// where a cell is puts a fragment in a cluster whose light list was built for
// somewhere else. Nothing reports that -- it looks like lights in the wrong
// place. One buffer, read twice, is what makes disagreement impossible.
//
// The Rust half is `ClusterGridGpu` in `crates/slop-render/src/cluster.rs`,
// where the same mapping is implemented again for the CPU and tested against
// values worked out by hand.
//
// Include it as:
//
//     #include "lib/cluster.slang"

#ifndef SLOP_CLUSTER_INCLUDED
#define SLOP_CLUSTER_INCLUDED

// How the view frustum is divided, and what the pass that reads it needs.
//
// **The view matrix is four explicit columns, not a `float4x4`.** A matrix in a
// structured buffer has a layout convention on each side and the two
// disagreeing transposes it, which reads as lights in impossible places rather
// than as a layout bug. Four `float4` have one interpretation.
struct ClusterGridGpu
{
    float4 viewColumn0;
    float4 viewColumn1;
    float4 viewColumn2;
    float4 viewColumn3;

    uint tilesX;
    uint tilesY;
    uint slices;
    uint maxPerCluster;

    // The depth-to-slice mapping, precomputed on the CPU:
    //     slice = floor(log(depth) * sliceScale + sliceBias)
    float sliceScale;
    float sliceBias;
    // The range clustering covers. **Not the projection's near and far** --
    // `docs/DESIGN.md` §2.2 uses an infinite far plane, and an exponential slice
    // mapping needs a finite range to divide.
    float zNear;
    float zFar;

    // Pixels, for turning a fragment's screen position into a tile.
    float2 screen;

    float tanHalfFovY;
    float aspect;

    // Heap indices.
    uint lights;
    uint lightCount;
    uint ranges;
    uint indices;
};

// Where one cluster's light indices start, and how many there are.
//
// An offset rather than `cluster * maxPerCluster` computed at the point of use,
// even though that is what it currently equals. Compacting the list later
// changes how this is produced and nothing that reads it.
struct ClusterRange
{
    uint offset;
    uint count;
};

// A point in view space, from a world-space one.
//
// Written out column by column for the reason `ClusterGridGpu` gives.
float3 clusterViewFromWorld(ClusterGridGpu grid, float3 position)
{
    return grid.viewColumn0.xyz * position.x
         + grid.viewColumn1.xyz * position.y
         + grid.viewColumn2.xyz * position.z
         + grid.viewColumn3.xyz;
}

// Which depth slice a view-space *depth* falls in.
//
// `depth` is the positive distance in front of the camera -- `-z` in the
// right-handed view space `slop_math::look_at` produces. Clamped at both ends:
// an index past the end would read another cluster's list, or past the buffer.
uint clusterSliceOf(ClusterGridGpu grid, float depth)
{
    if (depth <= grid.zNear)
    {
        return 0;
    }

    float slice = log(depth) * grid.sliceScale + grid.sliceBias;

    return min(uint(max(slice, 0.0)), grid.slices - 1);
}

// The flat index of a cell, matching how the build pass decomposes one.
uint clusterIndex(ClusterGridGpu grid, uint3 cell)
{
    return cell.z * grid.tilesX * grid.tilesY + cell.y * grid.tilesX + cell.x;
}

// Which cluster a fragment belongs to.
//
// `screenPosition` is `SV_Position.xy` -- pixels, origin top left. `depth` is
// the positive view-space distance, which a fragment gets from
// `1.0 / SV_Position.w`: the projection puts `-z` in `w`, and a fragment
// shader's `SV_Position.w` holds its reciprocal.
uint3 clusterCellOf(ClusterGridGpu grid, float2 screenPosition, float depth)
{
    // Clamped rather than trusted: a fragment exactly on the right or bottom
    // edge divides to the tile count itself, which is one past the end.
    uint x = min(uint(screenPosition.x / grid.screen.x * float(grid.tilesX)), grid.tilesX - 1);
    uint y = min(uint(screenPosition.y / grid.screen.y * float(grid.tilesY)), grid.tilesY - 1);

    return uint3(x, y, clusterSliceOf(grid, depth));
}

// One cell's bounds in view space, as an axis-aligned box.
//
// The twin of `ClusterGrid::bounds` in `crates/slop-render/src/cluster.rs`,
// where the same arithmetic is tested on the CPU. **Two sign conventions meet
// here**: `look_at` is right-handed so in front of the camera is negative `z`,
// and `perspective` flips Y so the top of the screen is positive `y`. Taking a
// min and max over all eight corners means neither needs a case analysis.
void clusterBounds(ClusterGridGpu grid, uint3 cell, out float3 boxMin, out float3 boxMax)
{
    float ndcX0 = float(cell.x) / float(grid.tilesX) * 2.0 - 1.0;
    float ndcX1 = float(cell.x + 1) / float(grid.tilesX) * 2.0 - 1.0;
    float ndcY0 = float(cell.y) / float(grid.tilesY) * 2.0 - 1.0;
    float ndcY1 = float(cell.y + 1) / float(grid.tilesY) * 2.0 - 1.0;

    float ratio = grid.zFar / grid.zNear;
    float near = grid.zNear * pow(ratio, float(cell.z) / float(grid.slices));
    float far = grid.zNear * pow(ratio, float(cell.z + 1) / float(grid.slices));

    float halfHeight = grid.tanHalfFovY;
    float halfWidth = grid.tanHalfFovY * grid.aspect;

    boxMin = float3(1e30, 1e30, 1e30);
    boxMax = float3(-1e30, -1e30, -1e30);

    float depths[2] = { near, far };
    float xs[2] = { ndcX0, ndcX1 };
    float ys[2] = { ndcY0, ndcY1 };

    for (uint d = 0; d < 2; ++d)
    {
        for (uint i = 0; i < 2; ++i)
        {
            for (uint j = 0; j < 2; ++j)
            {
                float3 corner = float3(
                    xs[i] * halfWidth * depths[d],
                    -ys[j] * halfHeight * depths[d],
                    -depths[d]);

                boxMin = min(boxMin, corner);
                boxMax = max(boxMax, corner);
            }
        }
    }
}

// Whether a sphere touches an axis-aligned box.
//
// By its nearest point, not its centre. Testing the centre misses every light
// that reaches into a cell from outside it, which is most of them -- and the
// symptom is lights that only appear when the camera is close enough for their
// centre to enter the cell.
bool sphereTouchesBox(float3 centre, float radius, float3 boxMin, float3 boxMax)
{
    float3 nearest = clamp(centre, boxMin, boxMax);
    float3 offset = centre - nearest;

    return dot(offset, offset) <= radius * radius;
}

#endif // SLOP_CLUSTER_INCLUDED
