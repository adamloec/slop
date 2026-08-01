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

Types land when a consumer needs them, not before.

| Area | State | Milestone |
|---|---|---|
| `glam` re-export, coordinate conventions | Landed | M0 |
| `Transform` — TRS, composition, interpolation | Landed | M0 |
| `scalar` — platform-independent transcendentals | Landed | M0 |
| Projection matrices, camera | Planned — binds the depth conventions | M0 |
| `Aabb`, `Sphere`, `Plane`, `Ray` and intersections | Planned — when culling needs them | M2 |
| `Frustum` extraction and culling tests | Planned | M2 |
| Packing — octahedral normals, quaternion compression, half floats | Planned | M2 |
| Curves and splines | Planned | M5 |
| Morton codes, spatial hashing | Planned | M7 |

Bounding volumes and frusta are deliberately not built yet. Frustum extraction
depends on the projection convention, and building it before projection exists
risks encoding the wrong one — see §5.

## 3. Coordinate conventions

Stated once here and never assumed elsewhere. This is where the bugs live in
every engine that left it implicit.

| | |
|---|---|
| World space | Right-handed, **Y up**, **−Z forward**, +X right |
| Rotation | Quaternions; counter-clockwise about the axis, viewed from its positive end |
| Matrices | Column-major, column-vector — `M * v`, and `parent * child` composes |
| Depth range | `[0, 1]`, not OpenGL's `[-1, 1]` |
| Depth direction | **Reversed** — near maps to 1.0, far to 0.0 |
| Framebuffer origin | Vulkan's, Y down; the projection matrix absorbs the flip |

**Right-handed Y-up because glTF is.** glTF is the import format
(`DESIGN.md` §2.8), so matching it means mesh import applies no basis change —
an entire class of mirrored-model and inside-out-normal bug that never occurs.
It is also `glam`'s `_rh` default.

**Reversed depth** because floating-point precision clusters near zero while a
conventional projection spends its range near the far plane. The two are exactly
mismatched, which is what produces z-fighting on distant geometry. Reversing
aligns them and buys orders of magnitude of precision for free.

## 4. Key types

| Type | Role | Decision |
|---|---|---|
| `Transform` | TRS, kept decomposed | `DESIGN.md` §2.7 |
| `UP`, `FORWARD`, `RIGHT` | World axis constants | §3 above |
| `scalar` | `sin`, `cos`, `exp`, `powf` … via `libm`, identical on every platform | `DESIGN.md` §2.14 |
| `glam` re-export | Vector, matrix, quaternion vocabulary | `DESIGN.md` §3.2 |

## 5. Decisions

| Decision | Where |
|---|---|
| Take `glam`; do not write our own linear algebra | `DESIGN.md` §3.2 |
| Right-handed Y-up world space, matching glTF | §3 above |
| Reversed depth, `[0, 1]` range | §3 above — decided at M0, see below |
| `glam` with `libm`, without `scalar-math` | `DESIGN.md` §2.14 |

**Reverse-Z was decided at M0 rather than deferred to the renderer,** which is
`DESIGN.md` §1.2 principle 6's "refactor or rewrite?" test coming out on the
rewrite side. It is not a renderer tweak: it changes every projection matrix,
the depth compare operation, the depth clear value, and the sense of every depth
test in the engine. Retrofitting means auditing all of them simultaneously.

Nothing consumes the depth conventions yet — they bind when projection matrices
land with the camera in M0 task F.

## 6. Invariants

1. **`glam` types are the vocabulary.** Do not wrap `Vec3`, `Mat4`, or `Quat` in
   newtypes. Wrapping reintroduces exactly the conversion tax the decision to
   take `glam` exists to avoid.
2. **Coordinate conventions are stated once, in §3, and never assumed
   elsewhere.** Code that needs one cites it; code that contradicts it is a bug
   regardless of whether it looks right on screen.
3. **The `glam` feature set is a determinism decision, not a performance one**
   (`DESIGN.md` §2.14). `libm` is on, so transcendentals do not call the
   platform C library and Windows agrees with Linux. `scalar-math` is off, and
   deliberately: glam picks its SIMD path at compile time rather than by runtime
   CPU detection, so one build is already one code path, and scalar maths would
   only buy the cross-architecture tier §2.14 puts out of scope.
4. **Loose `f32` transcendentals go through `scalar`, never `std`.** `f32::sin`
   and its neighbours reach the platform C library, which defeats the point of
   the previous invariant for every call that is not on a `glam` type.
   `clippy.toml` disallows them. `sqrt`, `abs`, `floor`, `ceil`, `round`,
   `trunc` and `mul_add` are exactly specified by IEEE-754, are already
   identical everywhere, and must **not** be wrapped.
5. **`Transform` is not a matrix and must not become one.** It stays decomposed
   because the scene graph, the editor, serialization, and §2.7's interpolation
   all need the parts individually — and because blending matrices is not the
   same as blending the transforms they represent.
6. **No `Transform::inverse` returning a `Transform`.** The inverse of a TRS with
   non-uniform scale is not a TRS, so such a method would be silently wrong in
   exactly the cases that matter. `inverse_matrix` returns a `Mat4` and is
   correct in every case.
7. **`transform_vector` is not for normals.** Under non-uniform scale, normals
   need the inverse transpose or they stop being perpendicular to the surface.
