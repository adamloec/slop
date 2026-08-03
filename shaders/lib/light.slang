// Lights, as the GPU sees them.
//
// Shared because two shaders need it: the forward pass shades with these, and
// `passes/cluster_build.slang` decides which cells each one reaches. A copy in
// each would be two declarations of one ABI, and the Rust side
// (`crates/slop-render/src/light.rs`) would have to match both.
//
// Include it as:
//
//     #include "lib/light.slang"

#ifndef SLOP_LIGHT_INCLUDED
#define SLOP_LIGHT_INCLUDED

// One point light, matching `PointLightGpu` in `crates/slop-render/src/light.rs`.
//
// The radius sits beside the position rather than beside the intensity so that
// the cluster build reads the sphere it is testing -- centre and radius -- out
// of one sixteen-byte row.
struct PointLightGpu
{
    float3 position;
    float radius;

    float3 color;
    float intensity;
};

// How much of a light reaches a point at `distanceSquared` from it.
//
// Inverse square, windowed so it reaches exactly zero at the radius. **The
// window is not cosmetic.** Unwindowed inverse square never reaches zero, so a
// light simply cut off at its radius leaves a visible edge -- and the radius is
// what the cluster build tests, so the shading has to agree with it or lights
// appear and vanish as the camera crosses a cell boundary.
//
// Returns zero at and beyond the radius, so a caller may either test first or
// just add the result.
float pointLightAttenuation(float distanceSquared, float radius)
{
    float radiusSquared = radius * radius;

    if (distanceSquared >= radiusSquared)
    {
        return 0.0;
    }

    float window = saturate(1.0 - (distanceSquared * distanceSquared)
                                / (radiusSquared * radiusSquared));

    // Guarded against the singularity at zero distance, which is otherwise an
    // infinity that spreads through the sum and shows as a white pixel.
    return window * window / max(distanceSquared, 1e-4);
}

#endif // SLOP_LIGHT_INCLUDED
