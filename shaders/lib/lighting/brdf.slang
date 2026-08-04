// The reflectance model: how much light a surface sends towards the eye.
//
// `docs/PLAN.md` §9.7 E6d. Shared between the direct lights and the environment
// because they are the same BRDF asked two different questions — a light asks
// "how much of *this* direction", an environment asks "how much of all of them"
// — and two copies would be two places for the roughness convention to drift.
//
// Include it as:
//
//     #include "lib/lighting/brdf.slang"
//
// # Cook-Torrance, and what each term is for
//
//     f = D · Vis · F
//
// `D` is how many microfacets face the half vector — the lobe's shape, and what
// roughness controls. `Vis` is how many of them are shadowed or masked by their
// neighbours, which is what keeps a rough surface from getting brighter than a
// smooth one at grazing angles. `F` is Fresnel: every surface becomes a mirror
// edge-on, and forgetting it is why hand-rolled shading looks like plastic.
//
// # Roughness is squared, once, here
//
// A material's roughness is *perceptual* — linear to an artist's eye — and the
// GGX distribution wants its square. Squaring in one place and not another makes
// low-roughness surfaces far blurrier than the material says, and the mistake is
// invisible except by comparison. `slop-cook`'s prefilter squares it in the same
// way, which is what lets a roughness select a chain level and get the lobe the
// direct light would have given.

#ifndef SLOP_BRDF_INCLUDED
#define SLOP_BRDF_INCLUDED

static const float PI = 3.14159265;

// Reflectance at normal incidence for a dielectric.
//
// 0.04 is the industry's constant for "not a metal" — glass, plastic, stone and
// skin all sit within a few percent of it — and the reason a metallic workflow
// needs only one number per material instead of a spectrum.
static const float3 DIELECTRIC_F0 = float3(0.04);

// The GGX normal distribution: what fraction of microfacets face `H`.
//
// Long-tailed, which is why it replaced Blinn-Phong everywhere: real surfaces
// have a bright core and a wide dim halo, and a Gaussian-ish lobe has only the
// core.
float distributionGGX(float NoH, float roughness)
{
    float alpha = roughness * roughness;
    float alphaSquared = alpha * alpha;

    float denominator = NoH * NoH * (alphaSquared - 1.0) + 1.0;

    return alphaSquared / max(PI * denominator * denominator, 1e-7);
}

// Smith's height-correlated visibility term, with the BRDF's denominator folded
// in.
//
// **Visibility, not geometry.** The Cook-Torrance form is `D·G·F / (4·NoL·NoV)`,
// and that denominator cancels most of `G`. Folding them together is both faster
// and better behaved: computing `G` and then dividing loses precision at grazing
// angles, where the two are each near zero and their ratio is not.
float visibilitySmith(float NoV, float NoL, float roughness)
{
    float alpha = roughness * roughness;
    float alphaSquared = alpha * alpha;

    float lambdaV = NoL * sqrt(NoV * NoV * (1.0 - alphaSquared) + alphaSquared);
    float lambdaL = NoV * sqrt(NoL * NoL * (1.0 - alphaSquared) + alphaSquared);

    return 0.5 / max(lambdaV + lambdaL, 1e-5);
}

// Schlick's approximation to Fresnel.
float3 fresnelSchlick(float cosine, float3 f0)
{
    float flipped = clamp(1.0 - cosine, 0.0, 1.0);
    float squared = flipped * flipped;

    // `pow(x, 5)` written as squares: two multiplies instead of a transcendental,
    // and exact rather than approximate.
    return f0 + (float3(1.0) - f0) * (squared * squared * flipped);
}

// The environment BRDF: the split-sum approximation's second factor.
//
// # Why this is arithmetic and not a lookup table
//
// `docs/PLAN.md` §9.7 argues it. The function depends on **roughness and N·V
// alone** — nothing about the environment — so a cooked table would be one
// artifact shared by every scene, which is a second asset kind and a second
// upload path for what fits in ten instructions. The original formulation uses a
// 128² RG16F texture; this is Lazarov's fit to it, accurate to a fraction of a
// percent over the whole domain and used by shipped titles.
//
// `docs/PLAN.md` §6.1 carries the row: if a measurement ever shows the fit
// costing something visible, the table replaces this function and nothing that
// calls it changes.
//
// Returns the scale and bias to apply to `F0`.
float2 environmentBrdf(float NoV, float roughness)
{
    const float4 c0 = float4(-1.0, -0.0275, -0.572, 0.022);
    const float4 c1 = float4(1.0, 0.0425, 1.04, -0.04);

    float4 r = roughness * c0 + c1;
    float a004 = min(r.x * r.x, exp2(-9.28 * NoV)) * r.x + r.y;

    return float2(-1.04, 1.04) * a004 + r.zw;
}

// Fresnel for an environment, where there is no single half vector.
//
// The roughness term is not decoration: with plain Schlick, a rough metal picks
// up a bright rim it should not have, because the environment's grazing
// reflection is already spread out by the prefilter. Clamping against
// `1 - roughness` is Sébastien Lagarde's fix and is what every engine uses here.
float3 fresnelSchlickRoughness(float cosine, float3 f0, float roughness)
{
    float3 ceiling = max(float3(1.0 - roughness), f0);
    float flipped = clamp(1.0 - cosine, 0.0, 1.0);
    float squared = flipped * flipped;

    return f0 + (ceiling - f0) * (squared * squared * flipped);
}

// What a surface sends towards `view` from light arriving along `light`.
//
// The whole BRDF, for one direction, as a multiplier on the incoming radiance
// times `NoL`. Both the sun and every point light go through this — the shape of
// a highlight does not depend on what is casting it, and a second copy for point
// lights is how the two would drift.
//
// **Energy is conserved between the two lobes**: what Fresnel reflects
// specularly is not available to scatter diffusely, which is the `1 - F`. Metals
// carry no diffuse at all, and that is handled by the caller passing a
// `diffuseColor` already scaled by `1 - metallic` — it belongs to the material
// rather than to the reflectance model.
float3 surfaceResponse(
    float3 diffuseColor,
    float3 f0,
    float roughness,
    float3 normal,
    float3 view,
    float3 light,
    float NoV,
    float NoL)
{
    float3 halfway = normalize(view + light);

    float NoH = saturate(dot(normal, halfway));
    float VoH = saturate(dot(view, halfway));

    float3 F = fresnelSchlick(VoH, f0);
    float D = distributionGGX(NoH, roughness);
    float Vis = visibilitySmith(NoV, NoL, roughness);

    // **The `1/PI` is not optional and was missing before E6d.** Lambert's law
    // is `albedo/π · E`, and shading here used `albedo · E` — three times too
    // bright. That was invisible while it was the only term: it reads as a
    // choice of light intensity. It stops being invisible the moment a specular
    // lobe with the correct normalisation sits beside it, because then the two
    // are wrong relative to each other and no exposure setting fixes both.
    float3 diffuse = (float3(1.0) - F) * diffuseColor / PI;
    float3 specular = D * Vis * F;

    return diffuse + specular;
}

#endif // SLOP_BRDF_INCLUDED
