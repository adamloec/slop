# slop-math

**Last updated:** 2026-08-01

## 1. Purpose

Linear algebra and geometry. Re-exports `glam` for vector, matrix, and
quaternion types, and owns the engine-specific geometry layer on top of it.

**Why `glam` rather than our own vectors.** Vocabulary types are contagious. A
`SlopVec3` would not just cost the week to write — it would cost a conversion at
every boundary, permanently: glTF import, GPU buffer layout, `egui`, and
anything third parties write against `slop-abi`. Marshalling code at every seam
is where sign and handedness bugs breed. Linear algebra is also the most solved
thing in the stack, and no design decision in this engine flows from it
(`DESIGN.md` §3.2).

The interesting part of this crate is therefore everything `glam` does *not*
provide, which is most of its eventual mass.

## 2. Status

Stub. Types land when a consumer needs them, not before.

| Area | State | Milestone |
|---|---|---|
| `glam` re-export | Planned | M0 |
| `Transform` — TRS, composition, hierarchy semantics | Planned | M0 |
| `Aabb`, `Sphere`, `Plane`, `Ray` and intersections | Planned | M0 |
| `Frustum` extraction and culling tests | Planned | M0 |
| Packing — octahedral normals, quaternion compression, half floats | Planned | M2 |
| Morton codes, spatial hashing | Planned | M7 |
| Curves and splines | Planned | M5 |

## 3. Key types

None yet. The table lands with the types.

## 4. Decisions

| Decision | Where |
|---|---|
| Take `glam`; do not write our own linear algebra | `DESIGN.md` §3.2 |
| Which `glam` feature set — tied to the determinism tier | `DESIGN.md` §8 item 8 |

## 5. Invariants

1. **`glam` types are the vocabulary.** Do not wrap `Vec3`, `Mat4`, or `Quat` in
   newtypes. Wrapping reintroduces exactly the conversion tax the decision to
   take `glam` exists to avoid.
2. **Handedness and coordinate conventions are stated once, here, and never
   assumed elsewhere.** This is where the bugs live in every engine that got it
   wrong.
3. **The `glam` feature set is a determinism decision, not a performance one.**
   SIMD paths can differ across CPU feature levels; `scalar-math` and `libm`
   exist to trade throughput for reproducibility. Settle it with `DESIGN.md` §8
   item 8, not independently.
