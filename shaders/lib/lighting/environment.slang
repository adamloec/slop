// The lighting that has no position, as the GPU sees it.
//
// A directional light and the sky, the latter as nine spherical-harmonic
// coefficients. Shared for the same reason `lib/lighting/light.slang` is: the shadow
// passes at `docs/PLAN.md` §9.5 E5 build their cascades along this direction,
// and a copy per shader would be two declarations of one ABI.
//
// The Rust half is `EnvironmentGpu` in `crates/slop-render/src/environment.rs`.
//
// Include it as:
//
//     #include "lib/lighting/environment.slang"

#ifndef SLOP_ENVIRONMENT_INCLUDED
#define SLOP_ENVIRONMENT_INCLUDED

// For the cube lookup below. Guarded, so including both is free.
#include "lib/bindless.slang"

// How many coefficients an order-3 expansion has. Matches
// `slop_math::COEFFICIENTS` and `slop_asset::environment::COEFFICIENTS`.
static const uint SH_COEFFICIENTS = 9;

struct EnvironmentGpu
{
    // **Towards the light, not away from it.** So a sun overhead points up, and
    // `dot(normal, sunDirection)` is the cosine of the incidence angle with no
    // negation. The opposite convention is equally defensible, which is exactly
    // why picking one silently is how a scene ends up lit from underneath.
    float3 sunDirection;
    float sunIntensity;

    float3 sunColor;
    float _pad;

    // The sky, projected onto spherical harmonics — `docs/PLAN.md` §9.7 E6b.
    // This replaced a single `float3 ambient`, which could not express that real
    // ambient light arrives from different directions with different colours.
    //
    // `float4` because an array of `float3` has a sixteen-byte stride anyway;
    // `w` is padding that exists either way and is never read.
    //
    // **Raw coefficients**, not pre-convolved: the cosine weighting happens in
    // `irradianceFrom` below, so these are the same numbers the cooked artifact
    // holds and there is exactly one place the convolution lives.
    // The prefiltered cube, or NO_SKY when the frame has none. See
    // `slop_render::NO_SKY` — not zero, because zero is a real heap slot and a
    // frame without an environment would sample whatever landed there.
    uint specular;
    uint specularSampler;
    // How many roughness levels the chain has. Written by the CPU rather than
    // assumed: the cooker decides the length from the cube's size, and a shader
    // that guessed would map roughness onto the wrong level — a reflection that
    // never quite blurs out, which reads as a material authoring problem.
    uint specularLevels;
    uint _pad2;

    float4 irradiance[SH_COEFFICIENTS];
};

// Matching `slop_render::NO_SKY`.
static const uint NO_SKY = 0xFFFFFFFF;

// What a Lambertian albedo facing `normal` multiplies.
//
// The irradiance arriving there **divided by π**, because a Lambertian surface
// reflects `albedo / π · E` and returning `E` would leave every caller to
// remember the division.
//
// # The two sets of constants
//
// `K*` are the normalisations of the real spherical harmonic basis. `BANDS` are
// the cosine lobe's own expansion `Â_l` divided by π — `Â_0 = π`, `Â_1 = 2π/3`,
// `Â_2 = π/4`. The second set is the entire reason nine coefficients are enough:
// the lobe has almost no energy above the third band, so there is nothing there
// worth storing.
//
// The polar axis of the basis is Z and the world's up axis is Y. That is a
// rotation of the basis and nothing more — it is complete either way, and the
// projection in `slop-cook` uses the same assignment. Stated because it looks
// like a bug on first reading.
//
// `Sh9::diffuse` in `slop-math` is the CPU twin of this function, in the sense
// `slop-render/tests/cluster.rs` established: the same formula written twice, so
// that a wrong band weight — invisible in a rendered image — is caught by a test
// that needs no GPU.
float3 irradianceFrom(EnvironmentGpu environment, float3 normal)
{
    const float K0 = 0.28209479;  // 1/2 · √(1/π)
    const float K1 = 0.48860251;  // √(3/4π)
    const float K2 = 1.09254840;  // 1/2 · √(15/π)
    const float K20 = 0.31539157; // 1/4 · √(5/π)
    const float K22 = 0.54627420; // 1/4 · √(15/π)

    float x = normal.x;
    float y = normal.y;
    float z = normal.z;

    float basis[SH_COEFFICIENTS] = {
        K0,
        K1 * y,
        K1 * z,
        K1 * x,
        K2 * x * y,
        K2 * y * z,
        K20 * (3.0 * z * z - 1.0),
        K2 * x * z,
        K22 * (x * x - y * y),
    };

    // Â_l / π, per band: one coefficient in band 0, three in band 1, five in
    // band 2.
    float bands[SH_COEFFICIENTS] = {
        1.0,
        2.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0,
        0.25, 0.25, 0.25, 0.25, 0.25,
    };

    float3 total = float3(0.0);

    for (uint index = 0; index < SH_COEFFICIENTS; ++index)
    {
        total += environment.irradiance[index].xyz * (basis[index] * bands[index]);
    }

    // Clamped at zero. A second-order reconstruction of a high-contrast sky can
    // ring below zero — the classic spherical-harmonic artefact — and a negative
    // ambient term subtracts light, which shows as black patches on surfaces
    // facing away from the sun rather than as anything resembling its cause.
    return max(total, float3(0.0));
}

// The environment reflected off a surface of `roughness`, along `direction`.
//
// The prefiltered half of the split-sum approximation — `slop-cook`'s `specular`
// module did the integral, and this is the lookup. Multiply by
// `environmentBrdf` to get the reflected radiance.
//
// # The roughness-to-level mapping is an ABI
//
// Linear from level zero at roughness zero to the last level at roughness one,
// which is exactly what `specular::roughness_of` inverts on the cooker's side.
// The two are stated separately because a file format cannot carry a function,
// and `specularLevels` is written into the buffer so at least the *length* is
// not guessed twice.
//
// Returns black when the frame has no environment, so a caller adds nothing
// rather than branching around this.
float3 specularFrom(EnvironmentGpu environment, float3 direction, float roughness)
{
    if (environment.specular == NO_SKY || environment.specularLevels == 0)
    {
        return float3(0.0);
    }

    TextureHandle cube = { environment.specular };
    SamplerHandle sampler = { environment.specularSampler };

    float level = saturate(roughness) * float(environment.specularLevels - 1);

    return sampleCubeLevel(cube, sampler, direction, level).rgb;
}

#endif // SLOP_ENVIRONMENT_INCLUDED
