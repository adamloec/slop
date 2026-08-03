// The lighting that has no position, as the GPU sees it.
//
// A directional light and a constant ambient term. Shared for the same reason
// `lib/light.slang` is: the shadow passes at `docs/PLAN.md` §9.5 E5 build their
// cascades along this direction, and a copy per shader would be two declarations
// of one ABI.
//
// The Rust half is `EnvironmentGpu` in `crates/slop-render/src/environment.rs`.
//
// Include it as:
//
//     #include "lib/environment.slang"

#ifndef SLOP_ENVIRONMENT_INCLUDED
#define SLOP_ENVIRONMENT_INCLUDED

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

    // Flat, and a placeholder. Real ambient light arrives from different
    // directions with different colours, which is what E6's image-based
    // lighting captures and a constant cannot.
    float3 ambient;
    float _pad2;
};

#endif // SLOP_ENVIRONMENT_INCLUDED
