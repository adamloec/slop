# Slop Engine — Implementation Plan & Session Handoff

**Status:** M0 and M1 complete. M2 underway — the content pipeline, the frame
renderer, shader reflection and the debug overlay are in; materials and a
Sponza-scale scene are not.
**Last updated:** 2026-08-02

This document is the working companion to `DESIGN.md`. **`DESIGN.md` is
authoritative for all architectural decisions** — read it first, in full, before
writing any code. This file covers what `DESIGN.md` deliberately does not: who
we are building for, the state of the environment, the immediate task breakdown,
and the invariants that are easy to violate by accident.

`CONVENTIONS.md` is the third document and is authoritative for code-level
conventions — how code is written, rather than what is built or in what order.
The three divide as: **`DESIGN.md` what, `PLAN.md` when, `CONVENTIONS.md` how.**

---

## 1. Context for whoever picks this up

### 1.1 Who the work is for

Adam is a **seasoned software developer with no prior game development
experience.** Both halves matter:

- **Do not dumb down the engineering.** Architecture, concurrency, trade-off
  analysis, and systems design all land fluently. Give recommendations with
  reasoning, not menus of options.
- **Do explain game dev and graphics terminology inline.** ECS, render graph,
  bindless, meshlets, descriptor sets, RHI, cooking, gizmos — define them when
  used. Mapping to general software concepts works well (databases,
  producer/consumer queues, ABI stability, cache locality, demand paging).
  `DESIGN.md` §11 is the running glossary; **add to it as new concepts appear.**

The stated goal is a **best-practice, advanced engine.** Hacky or expedient
options have been explicitly rejected multiple times in favor of the harder,
more scalable choice. Do not propose shortcuts that compromise the architecture
to save time.

### 1.2 How this project got scoped

Worth knowing, because it explains some decisions that look unusual:

- It started as "a Rust engine with AI integration," and the AI angle was
  **deliberately demoted to an accessory** (`DESIGN.md` §9). Do not re-center
  the design on AI tooling.
- The engine is to be **owned, not assembled on top of another engine.** Bevy as
  a foundation was explicitly rejected. Dependencies are fine; being a plugin is
  not. See §3 of `DESIGN.md` for the exact write/take line.
- Fidelity target is **AAA-adjacent**, which is what drove dropping wgpu for an
  owned Vulkan RHI, and dropping macOS.

---

## 2. Environment

### 2.1 Development moves to native Windows

The project was scoped in WSL2, but WSL2 is **not viable for development here.**
Diagnosis run on 2026-07-31:

```
GPU:            NVIDIA RTX 5090, driver 610.47   ✅
Vulkan loader:  1.3.275 present                   ✅
Vulkan ICDs:    lavapipe, nouveau, intel, radeon,
                asahi, virtio, gfxstream          ❌ no NVIDIA ICD
/dev/dri:       missing                           ❌
Rust toolchain: not installed                     —
```

No `nvidia_icd.json` means Vulkan would fall back to **lavapipe, Mesa's CPU
software rasterizer.** Useless for this project. `nvidia-smi` works because WSL
passes through the compute path, not the graphics path.

Even if the WSL Vulkan ICD were installed, **RenderDoc and Nsight Graphics are
degraded across the WSL boundary** — and `DESIGN.md` §5 identifies GPU debugging
as the work that does not compress. Crippling those tools is the wrong trade.

**Decision: develop natively on Windows.** Windows is a first-class target
anyway (`DESIGN.md` §2.1), and §2.13 requires CI on both platforms so Linux
never silently rots.

### 2.2 Windows setup — complete as of 2026-07-31

Installed and verified end to end (hello-world `cargo run` links and executes,
so the MSVC linker is genuinely wired up, not merely present):

| Component | Version |
|---|---|
| Repo location | `C:\Users\adaml\development\slop` — native path, not `/mnt/c` |
| VS Build Tools | MSVC 14.44.35207 |
| Windows SDK | 10.0.26100 |
| Rust | 1.97.1 `stable-x86_64-pc-windows-msvc`, + clippy and rustfmt |
| Vulkan SDK | 1.4.350.0 (LunarG) |
| Slang (`slangc`) | 2026.8 — ships inside the Vulkan SDK |
| RenderDoc | 1.45 |

Not installed: NVIDIA Nsight Graphics (optional).

**Vulkan runtime, confirmed via `vulkaninfo --summary`:**

```
Instance version   1.4.350
RTX 5090           apiVersion 1.4.341, driver 610.47, DISCRETE_GPU
Intel UHD 770      apiVersion 1.4.323, driver 101.7082, INTEGRATED_GPU
```

Two consequences worth carrying into M0:

- **Vulkan 1.4 is available natively**, so timeline semaphores, dynamic
  rendering, and descriptor indexing are all core features rather than
  extensions. `DESIGN.md` §2.2's explicit model needs no extension juggling.
- **Two physical devices enumerate.** Device selection must score on
  `deviceType` and prefer `DISCRETE_GPU`. Taking index 0 is the difference
  between the 5090 and silently rendering on the iGPU.

### 2.3 Repo

`git@github.com:adamloec/slop.git`, branch `main`.

---

## 3. Current state

**M0 and M1 are complete; M2 is about half done.** The lit textured cube renders
with a golden image guarding it, the reflection and ECS foundations are built
through scheduling and serialization, and the content pipeline cooks and hot
reloads meshes, textures and shaders.

What M2 still owes is the half that needs a renderer to be worth anything:
materials, a Sponza-scale scene, and the debug UI. §9 has the order and why
`slop-render` comes out of the examples first.

797 tests. Clippy and rustdoc clean under `-D warnings` in both feature
configurations, Vulkan validation reporting nothing, and every crate containing
`unsafe` passing under Miri — `slop-ecs` under both Stacked and Tree Borrows.

### 3.0 M1 — reflection, the ECS, and serialization

| Area | State |
|---|---|
| `slop-reflect` — `TypeInfo` as data, registry, `#[derive(Reflect)]` | Landed |
| `slop-ecs` — `Column`, `Signature`, `Archetype`, `World`, queries | Landed |
| Command buffers — deferred structural change | Landed |
| Query filters — `With`, `Without`, `Or`, `Option<&T>` | Landed |
| Change detection — `Tick`, `Mut<T>`, `Changed<T>`, `Added<T>` | Landed |
| Work-stealing job pool behind the M0 API | Landed |
| System scheduling from read/write sets — `System`, `WorldCell`, `Schedule` | Landed |
| Resources — data the world holds exactly one of | Landed |
| Layout fingerprints for the §2.3 guest boundary | Landed |
| Serialization — `Value`, the text format, `Value` ↔ component memory | Landed |
| World serialization — the whole world to text and back | Landed |

**M1 is complete.** Its exit condition was that a scene round-trips, and one
does: memory → `Value` → text → `Value` → memory, per component and for a whole
world at once. Save, load and save again produces byte-identical text.

### 3.0.2 M2 — content pipeline and debug UI

`DESIGN.md` §6 sets M2 as "asset pipeline, glTF import + cook, texture
compression, hot reload, **material system**. Load and render a **Sponza-scale
scene**. Plus the **debug UI layer** (§10.2)." **All of it is done**, and §9.3's
exit criteria are met — Sponza loads through the cook pipeline and renders with
its own materials, mipmaps and normal maps, and the debug UI shows frame timing
and inspects a live entity.

What is *not* done is everything §6.1 records as deferred behind a seam. The
milestone is complete; the register is the honest account of what it is standing
on.

| Area | State |
|---|---|
| `slop-asset` — cook cache, content-hash keying, stamps | Landed |
| `slop-asset` — the VFS read path | Landed |
| Shader cooking driven onto the shared cache | Landed |
| The cooked mesh format — binary, versioned, validated on load | Landed |
| glTF import — positions, normals, UVs, indices | Landed |
| Texture cooking — PNG to a versioned artifact | Landed |
| Block compression — BC7, 4:1, with the golden image unchanged | Landed |
| `examples/cube` drawing entirely from cooked assets | Landed |
| `Assets<T>` — the registry, handles, load/reload/unload | Landed |
| Hot reload — `Assets::reload_changed` + `slop-cli cook --watch` | Landed |
| `slop-render` — the frame renderer, all examples driven by it | Landed |
| Safe draw recording in `slop-rhi`, so `slop-render` has no `unsafe` | Landed |
| Shader reflection — layouts read from the cooked shader, not restated | Landed |
| Debug UI (§10.2) — the egui overlay and frame timing | Landed |
| Materials — the cooked format, glTF import, referenced images | Landed |
| Node transforms flattened into a cooked model | Landed |
| Rendering a model's meshes with their materials | Landed — `MeshRenderer`, `examples/model` |
| A Sponza-scale scene loading and drawing | Landed — 103 primitives, 25 materials, 70 textures, clean validation. Fetched by `slop-cli fetch sponza`, not committed. |
| Debug UI — the entity inspector over `slop-reflect` | Landed — `slop_app::inspector`, driven entirely by the type registry. No UI code names a component or a field. |
| Mipmaps | Landed — generated at cook time in RGBA8, then each level compressed to BC7 separately. Payload grows by exactly the geometric-series third. |
| Tangents, so normal maps can be sampled | Landed — read from glTF when present, derived from UVs when not. Sponza's 24 normal maps are sampled. |

Async streaming has **moved to M3**. It was never an M2 exit criterion, and
nothing yet loads enough at once to say what the API needs — Sponza is what will.
`docs/PLAN.md` §6.1 carries the row.

The cache was lifted out of `slop-cli` rather than invented: keying, stamping and
staleness were already working for shaders and are not shader-specific. What was
deliberately *not* lifted is a `Cooker` trait — a shader is one source to one
artifact and a glTF is one source to many, so a trait shaped by the first would
break on the second (§6.1).

**BC7 landed without the golden image moving**, which is a smaller claim than it
looks and worth stating precisely. A two-colour checkerboard is the easy case for
a block codec — two endpoints reproduce it exactly — so the reference approved
before the content pipeline existed still matches bit for bit through a lossy
encoder. That says the pipeline is intact, not that the encoder is good on hard
content; the quality claim rests on Intel's ISPC compressor being the industry
default, not on anything tested here. What *is* tested here is the arithmetic
that silently corrupts: `intel_tex_2` sizes its output as `ceil(w × h / 16) × 16`,
which is the block count only when both dimensions are already multiples of four,
so an unpadded 5×5 surface loses half its blocks. The importer pads first, and
breaking the padding on purpose fails two tests.

**The registry landed before the things that need it, and the ordering is the
point.** A `Handle<Mesh>` is a **seam**, not an implementation, and §1.2
principle 6 says to defer implementations freely and never seams. `slop-render`
arrives at M3 and will be written against whatever the asset API is at that
moment; if that is `Mesh` by value, then streaming and hot reload each become a
refactor of every call site rather than code behind an API nobody has to notice.
So `Assets<T>` went in first and its features follow — the reverse of the order
the feature list suggests. The three properties that are easy to get wrong
(unloading frees the *name* as well as the slot, a reload decodes before it
replaces, a failed load caches nothing) each have a test that was confirmed to
fail when the property was broken on purpose.

**The golden image is the pipeline's oracle, and that was engineered rather than
noticed.** `examples/cube` now draws nothing it holds in code: its shader, its
albedo and its geometry all arrive as cooked bytes through the VFS. Both source
assets were *generated from the code they replaced* — `assets/checker.png` from
the old `checkerboard()`, `assets/cube.gltf` and its buffer from the old
`VERTICES`/`INDICES` — so the reference image from before the change is still the
correct answer after it. An identical render therefore proves the entire path:
parse, cook, key, cache, VFS, decode, upload. A pipeline bug that produced
plausible output would have gone unnoticed against a reference generated *by* the
pipeline; against one that predates it, it cannot. The assertions that used to
sit beside those consts moved to `examples/cube/tests/mesh.rs` and
`tests/texture.rs`, where they now check the artifact rather than a generator
that no longer runs.

**Deferred structural change diverges from the conventional answer, on purpose.**
Bevy and Unity's `EntityCommandBuffer` both return a usable entity id from a
deferred spawn, reserving it from the allocator through an atomic. §2.14 rules
that out: two systems spawning on two threads would receive ids in whatever
order the hardware resolved the contention, making every recorded replay and
every golden image of a scene that spawns anything timing-dependent. `slop-ecs`
returns a `Target` instead — an ordinal within the recording buffer, resolved to
a real `Entity` at the sync point, on one thread, in schedule order. The cost is
recorded in §6.1.

The binding constraint §2.4 named has been honoured: `TypeInfo` is a **value**,
so a component type declared at runtime by a WASM guest is a first-class
component rather than a second tier. A test builds an archetype and allocates a
column for a type with no Rust type behind it.

The two halves §2.10 insisted be designed together were: `Column` is both the
array a query scans linearly and the contiguous run §2.3 hands to a guest, and
`Transfer::Blittable` is what gates the second use.

**Miri is now part of the suite.** Type-erased storage is raw pointer arithmetic
by construction, and its failure modes — misaligned access, aliasing violations,
deallocating with the wrong layout, double frees — are invisible to ordinary
tests and usually invisible on x86. Verified by breaking the code on purpose;
see §3.1.

### 3.0.1 M0, for reference

Verified on the RTX 5090: window, surface, device, swapchain, cooked Slang
shader, graphics pipeline, and a frame loop with two frames in flight, running
with validation active and reporting no errors, and shutting down cleanly. A
one-second run completes roughly 5,400 frames, which is the evidence that the
frames-in-flight pipelining works rather than stalling on the GPU each frame.

Verification landed *before* the cube rather than after it, and brought the
memory work with it — readback needs an allocator, a device-local image and a
host-visible buffer, so task G contained the first half of task D:

- **`gpu-allocator` integration** — `Allocator`, `Buffer`, `Image`, and
  image-to-buffer readback. Every resource suballocates from the start.
- **Headless rendering** — no window, no surface, no swapchain, `present_family`
  genuinely `None`. This is the mode `DESIGN.md` §5 asks for; the golden test
  *is* it, rather than a demo binary that would be the same program written
  twice.
- **`slop-verify`** — the golden-image harness: tolerance model, difference
  reporting, diff images, and an approval mode that never runs by default.

§4.2's exit criteria are met except for CI:

| Exit criterion | State |
|---|---|
| Lit textured cube renders | **Met** on Windows. Linux is untested — see below. |
| Validation layers clean | **Met** |
| One golden-image test passing | **Met** on the hardware tier; lavapipe lands with CI |
| No `#[allow]` hiding real problems | **Met** — one `expect(dead_code)` for an RAII field and one in the shared test-support module, both justified |
| CI green on both platforms | **Outstanding** — deferred by decision, see §2.2 |

What the cube exercises, all at once: staged vertex, index and texture uploads;
the bindless heap; depth testing under reverse-Z; push constants; the projection
conventions; and two draws sharing one pipeline. `slop-rhi` now covers instance,
device, swapchain, sync, commands, memory, resources, descriptors and pipelines.

**The one gap that remains is Linux.** Nothing in this project has ever run
there. That is not just a build question — Wayland's `u32::MAX` extent path
(§4.1-E) has never executed, and §2.14's cross-platform determinism cannot be
tested from one platform by construction. Both are claims the docs already make.

### 3.1 What verification actually bought

Two lists, and the contrast between them is the argument for having pulled
task G ahead of the cube.

**First, a correction to this section's own premise.** For most of M0 and M1 the
golden tests could not fail for the most important reason they exist. Every one
of them was written as:

```rust
let mut renderer = match Headless::new(&device, &allocator) {
    Ok(renderer) => renderer,
    Err(failure) => { eprintln!("skipping: {failure}"); return; }   // ← test passes
};
```

`Headless::new` builds the `Scene`. So a shader that disagreed with its Rust
side, a pipeline that would not build, a missing artifact — **any** setup failure
— printed a line nobody reads and reported the suite green. Found on 2026-08-02
by breaking the cube shader on purpose to check that reflection caught it: the
new reflection tests failed loudly, the demo refused to start, and all five
golden tests passed.

Fixed by separating the one legitimate skip — "nothing has been cooked yet",
checked by name against the VFS before anything is constructed — from every other
failure, which now panics. The lesson generalises past this file: **a test that
skips on setup failure is a test that reports success when the thing it guards is
most broken.** Any `return` in a test body deserves the question "what state does
this silently accept?".

**Before the verification skeleton existed, three bugs reached a running
program:**

1. A missing `shaderDrawParameters` feature: 18 validation errors, zero test
   failures, and correct output on this driver regardless.
2. A backwards triangle winding: invisible geometry, no validation complaint,
   and reasoning about it produced the wrong answer twice.
3. A drop-order crash on shutdown, which only appeared when a human closed the
   window rather than when the process was killed.

None of the three was visible to the type system, clippy, or the test suite.
Validation caught the first; a human caught the other two.

**After it existed, three bugs were caught before any GPU saw them:**

1. **Two cube faces wound inward.** `cross(right, up)` must equal the face
   normal, and for the ±Y faces the obvious axis choice is wrong. Under
   back-face culling both faces would have silently vanished. A unit test on the
   geometry caught it with no GPU involved.
2. **The orthographic depth scale had the wrong sign.** `-1/depth` where
   `+1/depth` was needed. Shadow cascades built on it would have rendered
   inside out.
3. **The cook cache did not key on shader includes.** Editing a shared include
   changed what every dependent compiled to while leaving every stamp matching —
   a cache that was *wrong*, not merely stale.

**And four were found by breaking working code on purpose**, to check the tests
would notice:

- Reversing the triangle's winding — 18.09% of pixels differ.
- Flipping `DEPTH_COMPARE` to `LESS_OR_EQUAL`, which made both cubes vanish
  entirely because every fragment failed against a 0.0 clear.
- Deallocating a column with alignment 1 instead of the element's 16. **Every
  ordinary test still passed** — real allocators do not care in practice. Miri
  named it exactly:

  ```
  error: Undefined Behavior: incorrect layout on deallocation:
  alloc59958 has size 64 and alignment 16, but gave size 64 and alignment 1
    --> crates\slop-ecs\src\column.rs:378:22
  ```

- Making the command buffer's staging area ignore the alignment a component
  asks for — the mistake a `Vec<u8>` staging area makes for free, since its
  allocation is aligned to 1 whatever offsets are computed within it. **All 24
  command-buffer tests still passed**, including one written specifically to
  place a 16-aligned component. Only Miri objected:

  ```
  error: Undefined Behavior: constructing invalid value of type &mut Tracked:
  encountered an unaligned reference (required 8 byte alignment but found 1)
  ```

*A test that has never caught anything is not known to work.* That applies to
golden images and to Miri alike — and Miri's caveat is that it only reports
undefined behaviour on paths a test actually executes, so it is worth precisely
as much as the coverage of the `unsafe` code.

The cube's golden also needed a design fix to be worth anything: a single convex
cube with back-face culling renders identically whether or not depth works, so
the scene draws a **second** cube, near-first, where only a working depth test
keeps the far one behind. Drawing them far-first would have produced a correct
image by draw order alone and proven nothing.

The `Drop`-order crash is covered structurally rather than by a test: every type
owning Vulkan objects waits for idle in its own `Drop`, recorded as invariant 22
in `docs/slop-rhi/README.md`.

1. **Two cube faces wound inward.** `cross(right, up)` must equal the face
   normal, and for the ±Y faces the obvious axis choice is wrong. Under
   back-face culling both faces would have silently vanished. A unit test on the
   geometry caught it with no GPU involved.
2. **The orthographic depth scale had the wrong sign.** `-1/depth` where
   `+1/depth` was needed. Shadow cascades built on it would have rendered
   inside out.
3. **The cook cache did not key on shader includes.** Editing a shared include
   changed what every dependent compiled to while leaving every stamp matching —
   a cache that was *wrong*, not merely stale.

**The determinism tier is settled** — `DESIGN.md` §2.14, decided before M1
rather than before M5. A golden image means nothing unless the frame it captures
is reproducible, and determinism constrains ECS iteration order and job
scheduling, both of which land at M1. Deciding it after they exist would mean
auditing both at once. What landed: `slop_core::Rng` (seeded PCG32),
`slop_core::FxHashMap` (`RandomState` reseeds *per process*, so a plain
`HashMap` iterates differently on every run), `slop_math::scalar` and glam's
`libm` feature, and `clippy.toml` entries so the `std` alternatives cannot come
back by accident.

---

### 3.2 Earlier state

**M0 tasks A and C: workspace scaffolding, and `slop-core` complete.**

`slop-core` ships `Handle<T>` / `RawHandle`, `SlotMap<T>`, `HandleAllocator<T>`,
`FrameArena`, `FixedTimestep` / `Clock`, `JobSystem` / `Scope`, and
`diagnostics`. 68 tests, clippy clean under `-D warnings` in both feature
configurations, rustdoc clean. Two implementations behind it are provisional and
labelled as such — the job system's `std::thread::scope` backing, and
`HandleAllocator`'s `Vec<bool>` liveness — both entirely behind their APIs.

Remaining for M0: `slop-math` (B), `slop-rhi` (D), window and surface (E), first
render (F), verification skeleton (G).

The repository also contains:

- `.gitattributes` — LF normalization (§2.13). Repo-local `core.autocrlf` set to
  `false` so the attributes file is the sole authority; the machine's global
  setting was `true`, which would have defeated §2.8's content-hash cache.
- `rust-toolchain.toml` pinning 1.97.1, `rustfmt.toml`, `clippy.toml`
- Cargo workspace, edition 2024, resolver 3, with `slop-math`, `slop-core`,
  `slop-rhi`, `slop-app` — all libraries, per `DESIGN.md` §1.2 principle 4. The
  M0 cube lands as an example, not a binary.
- Workspace lints centralized in the root manifest. Notably
  `clippy::undocumented_unsafe_blocks` is on, which turns §7's "every `unsafe`
  block carries a `// SAFETY:` comment" convention into a machine-checked rule
  rather than a review responsibility.
- `.github/workflows/ci.yml` — Windows + Linux matrix running fmt, clippy,
  build, and test with `-D warnings` and `fail-fast: false`. **Currently paused
  to `workflow_dispatch` only** — see below.
- `LICENSE-APACHE` and `LICENSE-MIT`, backing the `MIT OR Apache-2.0` every
  manifest declares.

All four crates pass fmt, clippy, build, and test locally on Windows.

> **CI must come back before task E.** It was paused while the workspace was
> empty, on the stated condition that it returns before the first
> platform-conditional code lands. Task E — the `winit` window and Vulkan
> surface — is exactly that point, and it is now also the first code with
> dependencies that must build under both MSVC and GCC. `DESIGN.md` §2.13 exists
> to stop "we'll port it later" from becoming the default, and this is the
> commit where that starts being possible.

**Design decisions locked** (`DESIGN.md` §2): target platforms, owned Vulkan RHI,
WASM gameplay ABI, reflection-first, job-system-first, handles everywhere, fixed
timestep, cooked assets, render snapshot, archetype ECS, Slang, editor-as-host,
dual-platform CI.

**Still open** (`DESIGN.md` §8), none blocking M0: scene text format, Slang Rust
binding choice (M3), whether to enable pipelining (M3), runtime UI (M5), debug UI
library (M2), naming.

---

## 4. M0 — Foundation

**Goal:** prove the entire stack connects end to end. A lit, textured, rotating
cube in a window, on both platforms, with CI green.

The cube is deliberately unambitious. Its job is integration, not looks.

### 4.1 Task breakdown, in dependency order

**A. Workspace scaffolding**
- Cargo workspace with the crate layout from `DESIGN.md` §4 (create only the
  crates M0 needs: `slop-math`, `slop-core`, `slop-rhi`, `slop-app`)
- `rust-toolchain.toml` pinning the toolchain
- `.gitattributes` normalizing line endings — required by §2.13, and it must
  exist *before* content hashing lands in M2
- `rustfmt.toml`, `clippy.toml`
- **CI matrix: Windows + Linux, build + clippy + test.** Add now; retrofitting is
  annoying and it is the whole enforcement mechanism for §2.13.

**B. `slop-math`**
- Re-export `glam`; do not wrap what does not need wrapping
- `Transform` (translation / rotation / scale, and to-matrix conversion)
- `Aabb`, `Frustum`, plane types
- Only what M0 needs; grow it on demand

**C. `slop-core`**
- Generational-index slotmap and handle types — `DESIGN.md` §2.6, used by
  everything downstream, so get the ergonomics right

> **Handle API — decided 2026-07-31.** Four calls, each hard to reverse once
> entities, assets, GPU resources, and scene nodes all depend on them.
>
> 1. **Typed.** `Handle<T>` carrying `PhantomData<fn() -> T>`. Passing a
>    `Handle<Texture>` where a `Handle<Buffer>` belongs becomes a compile error
>    rather than a garbage read. The `fn() -> T` phantom rather than a bare
>    `PhantomData<T>` is deliberate: it keeps `Handle<T>` unconditionally `Copy`,
>    `Send`, and `Sync` no matter what `T` is. At the §2.3 WASM boundary handles
>    erase to opaque integers through an explicit `to_raw` / `from_raw` pair.
>
> 2. **64-bit: `u32` index + `NonZeroU32` generation.** Packing to 32 bits
>    (24 index / 8 generation) was considered to halve bandwidth in the arrays
>    culling and transform propagation sweep every frame, and rejected: 8 bits of
>    generation wraps after 256 reuses of a slot, and for high-churn entities —
>    bullets, particles, decals — 256 is trivially reachable. A wrapped
>    generation means a stale handle silently validates against the wrong object.
>    Correctness over four bytes. `NonZeroU32` additionally makes
>    `Option<Handle<T>>` the same size as `Handle<T>`; fresh slots start at
>    generation 1.
>
> 3. **Checked access returning `Option`** — not panic, not debug-only. The check
>    is one comparison against a generation already being loaded into cache.
>    Panicking is wrong for an engine where deleting an object something still
>    references is routine in the editor and during hot reload. Debug-only
>    checking manufactures precisely the bugs that surface only in shipping
>    builds, which inverts the point of `DESIGN.md` §5. An `unsafe` unchecked
>    accessor may exist for *measured* hot spots under §7's policy. Note the perf
>    objection is largely misplaced: handle lookup is not the ECS hot path —
>    iteration over archetype columns is, and that never touches a handle.
>
> 4. **Two primitives, not one.** This is the call that matters most:
>    - `SlotMap<T>` owns its values — for GPU resources, assets, and scene
>      nodes, where lookup by handle is the access pattern.
>    - `HandleAllocator` tracks generations with no payload — for ECS entities,
>      whose component data lives in §2.10's archetype columns and *not* in one
>      array.
>
>    Both hand out the same `Handle<T>`. Building only the owning variant and
>    discovering at M1 that the ECS cannot use it is exactly the rework §8 warns
>    about when it says archetype storage and the columnar boundary must be
>    designed together.
- Arena / bump allocator for per-frame scratch
- Time and frame pacing primitives
- `tracing` setup for structured logging
- Job system: **a work-stealing scheduler is §2.5 and foundational**, but
  **decided 2026-07-31: M0 lands the API shape only, backed by a plain thread
  pool; the work-stealing implementation follows in M1.** M0 has nothing to
  schedule, so writing the scheduler now means designing against imagined
  workloads — ECS system scheduling at M1 is what supplies real requirements.
  The API shape must not assume single-threaded execution, because that
  assumption is the part that becomes unfixable later. The implementation
  behind it can be replaced freely.

**D. `slop-rhi` — the bulk of M0**
- `ash` instance creation, validation layers wired in debug builds
- Physical device selection with explicit feature requirements
- Logical device, queue family selection (graphics / compute / transfer)
- `gpu-allocator` integration
- Swapchain creation and recreation on resize
- Command pool and buffer management
- Synchronization primitives — **design against timeline semaphores from the
  start**, per §2.2
- Minimal pipeline creation path

> **Decided 2026-07-31 — M0 ships RHI primitives, not RHI abstraction.**
>
> An earlier draft of this section held that the RHI's consumer-facing API shape
> was M0's central design problem. That is now considered a trap. An abstraction
> designed with zero consumers is designed against imagined requirements; the
> render graph and frame renderer at M3 are what actually determine what the API
> must be, and a shape guessed now gets rebuilt then anyway. Building it twice is
> fine. Building it once, early, and then living with it is worse.
>
> So M0 sits close to `ash` and defers the extraction to M3.
>
> What M0 *must* get right is the **feature model**, because that is the part
> which cannot be retrofitted (`DESIGN.md` §2.2):
>
> - Timeline semaphores, not fences plus binary semaphores
> - Explicit barriers, never implicit synchronization
> - A bindless descriptor heap allocated from the start, even though the cube
>   uses one texture
> - Graphics, compute, and transfer queues acquired up front
> - Physical device selection scoring on `deviceType` — see §2.2, two devices
>   enumerate on this machine
>
> Get those right and the M3 extraction is a refactor. Get them wrong and it is a
> rewrite. Expect roughly a thousand lines before the first triangle regardless —
> that is the tax §2.2 knowingly accepted.

**E. Window + surface**
- `winit` window
- `ash-window` / `raw-window-handle` for surface creation — **never hand-roll
  platform surface code**, per §2.13

**F. First render**
- Triangle, then a lit textured cube
- Shaders in Slang compiled to SPIR-V. `slangc` CLI is acceptable *for M0 only*;
  §2.11 requires library integration once reflection is needed (M2/M3)

**G. Verification skeleton** — *done, see §3*
- Headless mode that renders N frames without a window
- One golden-image test wired into CI
- Establishes the §5 pattern early, while it is trivial

> **Note on ordering — moved ahead of the cube.** Task G was scoped last and ran
> sixth, because three bugs had already reached a running program that review did
> not catch (§3). Building the cube first would have meant debugging a silent
> visual regression with no reference to compare against.
>
> Task G also turned out to *contain* the first half of task D's memory work:
> readback needs an allocator, a device-local image, and a host-visible buffer.
> Doing it in this order meant the allocator arrived with a test that exercises
> it, rather than arriving alongside the cube and being debugged at the same
> time as vertex buffers, descriptor sets, and depth.
>
> Still outstanding from the original scope: **wired into CI**. The test exists
> and passes locally; CI itself is deferred (§2.2), and the lavapipe tier lands
> with it.

> **Decided 2026-07-31 — golden images run on lavapipe in CI.**
>
> GitHub-hosted runners have no GPU, so the §4.2 exit criterion "one
> golden-image test passing on both platforms" cannot be met by hosted CI as
> written. `DESIGN.md` §2.13's expectation that images match across operating
> systems also quietly assumed self-hosted runners with identical hardware — a
> standing cost this project should not take on yet.
>
> Resolution is two tiers:
>
> 1. **Hosted CI, both platforms, lavapipe** (Mesa's CPU rasterizer). Being a
>    software rasterizer it is bit-deterministic with no vendor divergence, so
>    comparison is **exact match, not tolerance**, and a Windows/Linux diff
>    becomes a real signal instead of driver noise. This catches the class of bug
>    golden images actually catch: state, ordering, and logic errors.
> 2. **Real-GPU goldens on the 5090**, in a separate opt-in lane, run locally.
>    Covers what lavapipe cannot — driver behavior and actual hardware features.
>
> Note this uses lavapipe for exactly the purpose §2.1 rejected it for: it is
> useless for *development* and well suited to *deterministic verification*.

### 4.2 Definition of done

- Lit textured cube renders on Windows and Linux
- CI green on both
- Validation layers clean — **met**
- One golden-image test passing — **met on the hardware tier**; the lavapipe tier
  needs CI
- No `#[allow]` suppressions hiding real problems

---

## 5. Invariants — easy to violate, expensive to fix

These are the ones where a small local convenience creates a large structural
problem later. Check work against this list.

1. **The renderer never reads live simulation state.** It consumes an immutable
   snapshot (§2.9). The moment one subsystem reaches across, the boundary is
   gone and pipelining, replay, and interpolation all become unavailable.
2. **The WASM guest API is columnar and bulk, never per-entity.** (§2.3) A
   convenient `get_transform(entity)` accessor is the failure mode that makes the
   whole ABI unusably slow. Hand slices of component columns.
3. **Handles, never pointers,** for engine-owned resources (§2.6). No `Rc<RefCell<>>`
   in engine data structures.
4. **Structural ECS changes go through command buffers** applied at explicit sync
   points, never immediately (§2.10). Required for both archetype churn and safe
   parallel systems.
5. **Asset paths are lowercase, always.** (§2.13) Enforced at cook time. This is
   the single most common Windows→Linux breakage.
6. **`PathBuf` / `Path::join` only.** No separator literals, no string concatenation.
7. **Nothing a shader declares is restated in Rust.** Vertex layouts, attribute
   offsets and push constant sizes come from cooked reflection, never from a
   parallel table someone maintains by hand. *(This invariant used to read
   "Slang integrates as a library, because reflection is unavailable from
   CLI-compiled shaders" — which was false. `slangc -reflection-json` produces
   it; see `DESIGN.md` §2.11's correction. The library is still wanted, for
   link-time specialization, and is no longer blocking.)*
8. **Nothing parses source assets at runtime in shipping builds.** (§2.8)
9. **Both platforms stay green.** (§2.13) Do not defer a Linux fix.

## 6. Explicitly rejected — do not reintroduce

- **wgpu** as the graphics layer (§2.2) — including "temporarily, to move faster"
- **macOS support** (§2.1)
- **Native dynamic library plugins** for gameplay (§2.3) — Rust has no stable ABI
- **Sparse-set as the default ECS storage** (§2.10)
- **Editor as a privileged engine mode** (§2.12)
- **AI-first design** (§9)
- **Building on Bevy or as anyone's plugin** (§1.1)

---

## 6.1 Provisional implementations — the register

Everything currently standing in for something else, in one place, so that
"temporary" stays a decision rather than becoming an accident.

**"Moved" does not mean copied.** Several rows below say a piece of example code
moves into an engine crate. Read that as *the requirement moves* — the example is
evidence that the problem is real and evidence of what shape a solution has to
be, not a donor implementation to lift. Example code was written to get a picture
on screen: it returns `String` errors where a library owes typed ones
(`CONVENTIONS.md` §6), it reaches for `CARGO_MANIFEST_DIR`, it submits uploads
and waits, and it hard-codes constants an engine has to expose. Lifting it would
import all of that under a better crate name. **Each of these is a rewrite against
two working references, and the golden images are what say the rewrite is
equivalent.**

**The distinction this table enforces:** a *hack* is a shortcut that makes the
right thing harder later, and there are none here. Everything below is either a
requirement currently living in the wrong crate (which gets **rebuilt** where it
belongs), or a simple implementation behind a final seam (which gets **replaced**
with no caller changing). `DESIGN.md` §1.2 principle 6 is the rule: defer implementations
freely, never seams.

| What | Where | Standing in for | Fate | When |
|---|---|---|---|---|
| Every renderer must be told whether it is the last to draw | `slop-render` | The render graph deriving barriers from declared reads and writes | **Replaced.** Only the last writer may transition the target to its final state, and no renderer can know whether it is last — so each stops at `COLOR_ATTACHMENT` and the caller ends with `Frame::finish`. That is a convention held by comment, and it already failed once: `MeshRenderer` transitioned to `PRESENT_SRC`, and adding an overlay put a pass on an image the presentation engine already owned, once per frame. Item E is what makes it derived rather than remembered. | M3 |
| The tangent is transformed by the normal matrix, not the model matrix | `shaders/passes/model.slang` | Per-instance data in a storage buffer, carrying the model matrix | **Replaced.** A normal transforms by the inverse transpose; a tangent lies *along* the surface and transforms by the model matrix. They agree exactly after normalisation under rotation and uniform scale, and diverge under non-uniform scale, which skews the frame. The model matrix is not available to use: the push constant block is 120 of the 128 bytes Vulkan guarantees and another `float3x3` needs 48. Per-instance data is where a model matrix belongs anyway. | M3 |
| Every shader drawing cooked geometry must declare all four vertex attributes | `slop-asset/src/mesh.rs`, every pass | Per-mesh vertex layouts, with pipeline variants to match | **Extended.** The layout is derived from shader reflection, so a shader omitting a field computes a stride shorter than the buffer's and reads every vertex after the first from the middle of its predecessor. `cube.slang` declares a tangent it never samples for exactly this reason, and a test asserts the reflected stride equals `VERTEX_SIZE`. Real per-mesh layouts arrive with skinning, which needs joints and weights on some meshes and not others. | M5 |
| `slop-cook` uses `anyhow`, and `CONVENTIONS.md` §6 says libraries use `thiserror` | `slop-cook` | Typed errors, once something branches on the kind | **Replaced.** Argued from the rule's own reason — "a caller must be able to match and respond" — which no caller does: the CLI and the editor both report the failure and mark the asset uncooked. What a cook failure is *for* is the context chain, and "reading primitive 3 of mesh 'Body' in sponza.gltf: index 5 names a vertex the primitive does not have" is the whole diagnosis. A flat enum discards it. **The trigger is an editor that shows a missing-texture failure differently from a malformed-file one.** | M4 |
| Mip levels are averaged in whatever space the texture is stored in | `slop-cli/src/texture_import.rs` | Filtering in linear light for colour textures, and in raw values for data ones | **Replaced.** A box filter over sRGB-encoded bytes is not the mean of the light they represent — it biases dark, so distant surfaces darken slightly. Doing it right needs to know which textures are colour and which are data (normal, roughness, occlusion), and that is per-asset import settings, which do not exist. The material already records `TextureSlot::is_srgb`, so the information exists at *import* time and does not reach the texture cooker — closing that gap is the work. | M3 |
| Mip generation is a box filter | `slop-cli/src/texture_import.rs` | A Kaiser or Mitchell kernel, per asset | **Replaced.** What hardware would do and what every pipeline starts with. Better kernels trade sharpness against ringing, which is a per-asset judgement — the same missing import settings. | M3 |
| `Gpu` ties one window to one device | `slop-app/src/gpu.rs` | A device shared by several surfaces | **Extended.** Right for a game, which has one window; wrong for the editor (`DESIGN.md` §2.12), where a detached viewport is a second surface on the *same* device — re-running bring-up would create a second device and make sharing a texture between panels impossible. The split is `Gpu` keeping instance/device/allocator and handing out surfaces, and it is additive: `Gpu::new` stays the one-window path. Not done now because the editor does not exist and a two-window API designed without one is a guess. | M6 |
| `FrameRenderer` has no automated test | `slop-render` | A smoke test that drives a real window, or a headless path that fakes a swapchain | **Extended.** Everything it does needs a surface, a surface needs a window, and a test harness has no event loop — the cube's golden renders headlessly and so covers `Scene`, not this. The check today is running both examples under `SLOP_FRAMES` with validation on, which is a command someone has to type. **The resize path has no coverage at all**, automated or otherwise, because `SLOP_FRAMES` never resizes the window. | M3 |
| Scene setup — uploads, pipeline, draw recording | `examples/cube/src/scene.rs` | `slop-render` + `slop-asset` | **Rebuilt.** It proves the pieces fit together; it is not the shape an engine wants. One hard-coded pipeline, a raw sampler freed by hand, `CARGO_MANIFEST_DIR` in the load path, and push constants restated from the shader — all of it example-grade on purpose, none of it moves. | M3 |
| `VertexBinding` cannot express a buffer format that differs from the shader's type | `slop-render/src/vertex.rs` | A per-location format override | **Extended.** Reflection is a fact about the shader; the buffer format is a decision about memory. They coincide for every float attribute and diverge for a packed one — egui's four-byte colour read as a `float4`. The overlay states its layout and uses reflection to check the shader, which is correct and is not derivation. | M3 |
| A glTF-referenced image is cooked separately from the same file under `assets/` | `slop-cli` | One artifact per distinct source image | **Replaced.** `assets/checker.png` cooks to `textures/checker.tex` *and*, because `cube.gltf` references it, to `textures/cube.0.tex`. Correct and wasteful. Deduplicating means keying artifacts by content rather than by name, which is a cache change rather than an importer one. | M2/M3 |
| A cooked model is a flat list, not a hierarchy | `slop-asset/src/model.rs` | `slop-scene`'s runtime tree, once something articulates | **Joined by.** Right for a static level, which is drawn rather than posed, and wrong the moment a parent joint animates. The tree is a *runtime* structure `slop-scene` owns; this format records where things ended up. | M5 |
| Materials carry no occlusion or HDR emissive | `slop-asset/src/material.rs` | More slots and a float texture format | **Extended.** Occlusion is a baked term a real-time renderer computes or ignores; float images are refused by name rather than silently narrowed, and arrive with IBL. | M3 |
| Frame timing is CPU wall-clock, not GPU time | `examples/cube/src/main.rs` | GPU timestamp queries written into the command buffer | **Joined by.** Wall-clock between frames is the honest measure of how fast frames arrive and a poor one for attributing cost to a pass — it includes waiting for the GPU. Attribution needs timestamps, and the render graph is what will know which pass one belongs to. | M3 |
| The overlay assumes one scale factor for the whole frame | `slop-render/src/overlay.rs` | Per-viewport scale, once a window can span two monitors at different scalings | **Extended.** `pixels_per_point` arrives per frame and applies to every draw in it, which is right until a window straddles a 100% and a 150% display. | M3 |
| A partial texture update re-uploads the whole image | `slop-render/src/overlay.rs` | `vkCmdCopyBufferToImage` into the sub-region | **Replaced.** Wasteful and correct. Font atlases settle within a few frames of startup, so this runs a handful of times and then never again. | M3 |
| `PushConstants` field *order* is not checked against the shader | `examples/cube/src/scene.rs` | A generic material parameter writer driven by reflected field offsets | **Replaced.** Reflection gives every field's name, offset and size; only the block *size* is compared today. Swapping two same-sized fields would still pass. The writer that fixes it arrives with materials. | M2 |
| Synchronous upload — submit and wait | `examples/cube/src/scene.rs` | Async transfer queue + staging ring | **Replaced.** Correct for startup, wrong for streaming. | M2 |
| `slangc` invoked as a CLI | `slop-cli/src/cook.rs` | The Slang library, for link-time specialization | **Replaced**, and no longer urgent. This was recorded as blocking reflection; that premise was false (`DESIGN.md` §2.11, corrected) and `-reflection-json` now feeds the cooker. What the library still buys is composing modules with specialization constants, and not spawning a process per shader. The cache layout, keying and read path all survive either way. | M3+ |
| The asset VFS reads synchronously | `slop-asset` | Async streaming alongside it | **Joined by, not replaced.** A blocking read stays correct for startup, for tools, and for the cooker itself; §2.8's streaming is an additional entry point rather than a different one. Recorded because "the VFS is sync" reads like a shortcut and is not.  Moved to M3: nothing yet loads enough at once to notice, and a Sponza-scale scene is what will say what the streaming API needs. | M3 |
| An asset is unloaded by hand, never by refcount | `slop-asset` | Reference counting, once something holds handles long enough to outlive its need for them | **Extended.** `unload` is explicit and correct; what is missing is *who decides*, and nothing holds a handle past a frame yet. Counting references now would count them from one place. | M2/M3 |
| `Assets<T>` is single-threaded — every mutation takes `&mut` | `slop-asset` | Interior mutability or a job-system-owned loader, once streaming decodes off the main thread | **Extended.** `Asset: Send + Sync` is already required so the bound does not have to be added later; only the *ownership* is provisional, and no public signature changes when the loader moves. | M2 |
| Every texture is cooked to BC7 with one fixed encoder setting | `slop-cli/src/texture_import.rs` | Per-asset import settings — format, sRGB, alpha mode, mip policy | **Extended.** BC7 is right for colour and wrong for a normal map (BC5) or HDR (BC6H), and the alpha modes differ in whether they preserve alpha at all. Nothing yet knows what a texture *is for*; the material system is what will. | M2/M3 |
| No mipmaps | `slop-cli/src/texture_import.rs` | A mip chain generated at cook time, compressed per level | **Extended.** Block compression without mips aliases badly at distance, and the cube never gets far enough away to show it. Generating them is another pass over the same pixels and changes no format — the header would gain a level count. | M2 |
| `cook --watch` polls the source tree on a timer | `slop-cli/src/main.rs` | An event-driven watcher (`notify` or the platform API) | **Replaced.** A tree walk four times a second is nothing here and wrong for a large project. Correctness does not depend on the watcher being right about what changed — the cache decides that, on the same code path a one-shot cook uses — so the loop is all that is replaced. | M2/M3 |
| The runtime polls cooked mtimes rather than subscribing | `slop-asset/src/vfs.rs` | Watching the cache directory for events | **Replaced.** One `stat` per loaded asset, throttled; fine for tens, wasteful for thousands. `Version` is already opaque, so where the answer comes from can change without a caller noticing. | M2/M3 |
| Hot reload waits for the device to go idle before swapping | `examples/cube/src/scene.rs` | Deferred deletion — free after the last frame that could reference it retires | **Replaced.** Correct, and the blunt instrument: a stall nobody perceives when a human saves a file. A renderer streaming assets per frame cannot do this, and that is what forces the queue. | M3 |
| A cooked mesh has one fixed vertex layout — position, normal, UV | `slop-asset` | Flexible attributes, once a material needs tangents or a second UV set | **Replaced.** Cheap to change *because* it is cooked: bump `COOKER_VERSION` and every artifact regenerates from source, which is exactly what that constant is for. A format that guessed at flexibility now would be guessing. | M2 |
| A cooked mesh is decoded field by field rather than cast | `slop-asset` | A zero-copy read over an aligned or memory-mapped buffer | **Replaced.** `fs::read` returns a `Vec<u8>` aligned to 1, so casting it to `[Vertex]` is undefined; decoding explicitly is also what makes the format little-endian by construction rather than by accident. Zero-copy arrives with the streaming loader, which is what will own an aligned buffer. | M2 |
| glTF import covers positions, normals, UVs and indices | `slop-cli` | Materials, tangents, skinning, animation, scene hierarchy | **Extended.** Enough that `examples/cube` draws from a file and its golden image still matches, which is the consumer that exists. Each addition is another attribute in the same pipeline, not a different one. | M2/M3 |
| No `Cooker` trait — each asset kind drives the cache itself | `slop-asset`, `slop-cli` | An asset-kind abstraction, once two kinds disagree usefully | **Extended.** Deliberately not designed against one real implementor: a shader is one source to one artifact, a glTF is one source to many, and a trait shaped by the first would break on the second. The **cache** is what is shared and is what was factored out. | M2 |
| Coarse include digest — any include recooks everything | `slop-cli/src/cook.rs` | Per-shader dependency lists via `slangc -depfile` | **Replaced.** Correct but pessimistic; wrong would be a cache that lies. | M2 |
| `JobSystem` backed by `std::thread::scope` | `slop-core/src/jobs.rs` | Work-stealing pool | **Replaced.** API shape is final; do not build on the cost model. | M1 |
| `HandleAllocator` liveness in a `Vec<bool>` | `slop-core/src/alloc.rs` | A bitset | **Replaced.** Entirely behind the API. | M1 |
| Every sampler is created fresh at its use site | `examples/cube/src/scene.rs`, `slop-render/src/{mesh,overlay}.rs` | A sampler cache in the material system | **Replaced.** No longer a raw `vk::Sampler` freed by hand — `slop_rhi::TextureSampler` owns its own `Drop` as of 2026-08-02 — but there is still no sharing. Samplers are a small, highly repeated set of states, and a device permits only a few thousand; one per material is how that limit is reached. | M2 |
| Whole-frame golden comparison only | `slop-verify` | Region assertions, intermediate captures | **Extended**, not replaced (`DESIGN.md` §8 item 8). | M3 |
| Hardware-tier golden references | `examples/cube/tests/golden/` | The lavapipe exact-match tier | **Joined by**, not replaced (§4.1-G). | M1 |
| A deferred spawn's `Target` cannot be stored inside a component | `slop-ecs` | Nothing — this is permanent, and §2.14 is why | **Kept.** Wiring a freshly spawned child into a parent's component takes the direct `&mut World` path or a second frame. | — |
| A deferred spawn plus *n* inserts performs *n* archetype moves | `slop-ecs` | Recording the full component set and spawning straight into the final archetype | **Replaced.** Correct today, and pure throughput — no caller changes when it lands. | M1 |
| `CommandBuffer::apply` reports only the first error | `slop-ecs` | Nothing planned | **Kept.** The alternative is stopping half way with no way to describe which half; an unregistered type is a wiring bug, not a condition to recover from. | — |
| A stamp older than `Tick::MAX_AGE` reads as recently changed | `slop-ecs` | A periodic pass clamping stamps that old | **Replaced.** The comparison is by age and already correct; what is missing is the scan that stops ages growing without bound. Reachable only after ~2<sup>31</sup> ticks. | M2 |
| `last_run` is supplied by hand through `Query::since` | `slop-ecs` | The scheduler supplying each system's own last run | **Replaced.** The filters do not change; only who fills in the window does. | M1 |
| `World::get_mut` stamps eagerly rather than on write | `slop-ecs` | Nothing planned | **Kept.** A point lookup is a caller who already named the single component they intend to write, so the cost is one false positive per call — and the alternative is `Mut<T>` leaking into every single-entity access path. | — |
| Batches, not a dependency graph | `slop-ecs` | Run a system the moment its predecessors finish | **Replaced.** Derived from the same access sets, so it is a scheduling policy change rather than a data model one. What it additionally needs is a deterministic tie-break, so buffers still apply in schedule order rather than completion order — which batching gets for free. | M3 |
| A system cannot *create* a resource, only mutate one | `slop-ecs` | `CommandBuffer` recording resource insertion | **Extended.** Resources are installed at setup with `&mut World`; a system computing a new one is the rare case, and deferring it needs the same staging the buffer already does for components. | M2 |
| `WorldCell::query` allocates a small `Vec` per call to check the declaration | `slop-ecs` | The access set precomputed per system | **Replaced.** A handful of elements, once per query rather than per row — but it is in the frame loop, which `CONVENTIONS.md` §8 says should allocate nothing. | M2 |
| The layout fingerprint has no consumer | `slop-reflect` | A module loader comparing the guest's against the host's | **Joined by.** Built now because `TypeInfo` is the contract a guest is compiled against, and the check is a pure function of data already there. | M4 |
| Type identity is a path, so renaming a type breaks saves | `slop-reflect` | An alias table mapping old paths to current ids | **Extended.** `#[reflect(path)]` already covers a type *moving modules*. What is missing is renaming with old saves in existence — and nothing is serialized yet, so the alias table wants designing against a real format rather than an imagined one. | M2 |
| No parent/child hierarchy | `slop-ecs` | A relationship component, plus cascade-despawn and a topological transform pass | **Joined by.** Deliberately not M1: it changes none of the scheduler's conflict rules, since parent-before-child is ordering *within* a system rather than between systems. It is a subsystem rather than a feature — Bevy reworked theirs more than once — and wants designing when transform propagation is a real consumer. | M2 |
| `TypeKind` models structs, primitives and opaque types only | `slop-reflect` | Enums, tuples, lists, maps | **Extended**, one variant each. A consumer's `match` fails to compile when one lands rather than silently ignoring it. | M2 |
| `Reflect` rejects generic types | `slop-reflect-derive` | A path encoding the type arguments | **Replaced.** Rejected loudly today rather than silently giving every instantiation one id. | M2 |
| `World::remove` drops the component rather than returning it | `slop-ecs` | A typed take that hands the value back | **Extended.** Needs a path that can name the type's Rust identity, which the erased core deliberately cannot. | M1 |

**The duplication that was on this table, and is now resolved.** Two copies of
roughly 150 lines of frame-loop plumbing were allowed to accumulate across
`examples/triangle` and `examples/cube`, and the decision recorded here on
2026-08-01 was to leave them until M3: `slop-render` is what determines the frame
loop's real shape, and extracting an abstraction from two toy examples is
designing against imagined requirements — §4.1-D's position applied one layer up.
The stated trigger was a third copy, per `CONVENTIONS.md` §2.3.

The third copy arrived (`examples/model`), and the trigger fired. Both halves are
now extracted:

- **The frame loop** — acquire, submit, present, frames in flight — is
  `slop_render::FrameRenderer`.
- **Device bring-up** — window, instance, surface, adapter selection, device,
  allocator — is `slop_app::gpu::Gpu`, whose field order discharges the safety
  condition `window::create_surface` states and cannot enforce. **That removed
  the last `unsafe` from every example.**

Waiting was the right call and is worth recording as such: the shape both
abstractions took was decided by `slop-render` and by the third example's needs,
not by the first two. Neither was guessed.

---

## 7. Conventions

**Moved to `CONVENTIONS.md`, which is now authoritative for code-level
conventions.** It covers crate and module layout, naming, the data-oriented
rules, API design, errors and panics, `unsafe`, allocation and performance,
concurrency, portability, documentation, testing, logging, dependencies, lints,
and commits — each rule with its reason and a reference to the decision it
protects.

Settled here and not repeated there:

- **MSRV:** pinned via `rust-toolchain.toml` (currently 1.97.1), bumped
  deliberately rather than incidentally.

---

## 8. After M0

`DESIGN.md` §6 has the full milestone list, and **§6.1 above is the register of
what each milestone takes back** from the provisional implementations standing
in for it today. Immediate outlook:

**M1 — ECS + reflection.** *Complete; see §3.0.* Slower than M0 despite less
code, as expected — Vulkan bring-up is high-volume but well-trodden, while the
ECS and reflection design carried real judgment.

The critical constraint was that **§2.10's archetype storage and §2.3's columnar
WASM boundary be designed together, not sequentially.** That has been honoured:
`Column` is simultaneously the array a query scans and the contiguous run handed
to a guest, and `Transfer::Blittable` gates the second use. Nothing about the
storage would change if the boundary were built tomorrow.

The scheduling half landed on top of it: systems declaring read/write sets so the
job system can parallelize them. Everything it rested on was already there —
command buffers so a system can change structure without `&mut World`, `Access`
so a scheduler can tell two systems apart, and change detection so a system can
decline work it does not need to do. That is why `slop-core`'s work-stealing pool
was an M1 item rather than an M0 one: §4.1-C deferred it precisely so ECS
scheduling would supply the real requirements, and it did.

The data model is settled. Nothing since has reshaped storage, which was the
point of taking the three storage-shaping decisions — archetype tables, deferred
structural change, and per-component ticks — before anything was built on top of
them.

M1 also landed the §5 verification infrastructure properly. Do not defer it: code
can be produced faster than it can be reviewed line by line, and automated truth
is the only thing preventing large volumes of subtly wrong architecture. Miri
joined that suite with the ECS storage layer and belongs to it permanently.

**M2 — Content + debug UI.** *Complete; see §3.0.2 and §9.3.* Cook pipeline,
shader reflection, materials, mipmaps, tangents, a Sponza-scale scene, and the
debug UI with a reflection-driven entity inspector.

The debug UI is pulled forward deliberately, and it stays pulled forward. Renderer
bring-up without inspection tooling is the largest avoidable time sink in the
plan (`DESIGN.md` §6), and the argument for deferring it — that an overlay needs a
renderer to live in — does not survive contact with what is already built. An
overlay records draw commands into a supplied command buffer against a supplied
target, which is exactly the shape `Scene` already has on `slop-rhi` alone. The
one part of §10.2 that genuinely needs M3 is the **render pass visualizer**,
because there is no graph to visualize; it lands with the graph.

**M3 — Renderer, Stage A.** Clustered forward+, shadows, IBL, HDR/tonemap, post
stack, render graph. See §9 for the ordering and why the frame loop comes out
first.

---

## 9. M2 remaining and M3 — task breakdown

In dependency order. Each item says what it unblocks, because the ordering is the
decision — the list itself is not controversial.

### 9.1 Why `slop-render` starts before the rest of M2

Three outstanding things all need the same missing piece:

```mermaid
flowchart TD
    loop["A — frame loop in slop-render"] --> ui["C — debug UI overlay"]
    loop --> mat["D — materials + Sponza"]
    refl["B — shader reflection"] --> mat
    loop --> graph["E — render graph, M3"]
    ui --> graph
    mat --> graph
```

The frame loop existed twice, in `examples/cube` and `examples/triangle`, and
§6.1 recorded a **third copy as the signal to extract it early**. The debug UI was
that third copy, so the loop came out first — not because M3 had started, but
because M2's remaining items forced it. **Item A landed** as
`slop_render::FrameRenderer`, followed by safe draw recording in `slop-rhi`
(`Pass`) and device bring-up in `slop-app` (`Gpu`), which between them removed
`unsafe` from `slop-render` and from every example.

**This was a rewrite, not a move, and the same applies to what is left.** See the
note above §6.1's table. The examples say what a frame loop must handle —
swapchain recreation on resize and suboptimal acquire, a command pool reset per
in-flight slot, timeline waits before touching one, semaphores per swapchain image
rather than per frame. That knowledge transferred. The `String` errors, the
hard-coded `FRAMES_IN_FLIGHT`, the `CARGO_MANIFEST_DIR` asset lookup and the
panic-on-failure did not; they are example-grade on purpose, and lifting the files
would have imported all of it under a better crate name.

**What is still in `examples/cube/src/scene.rs` comes out the same way**, and
mostly with the material system: the hard-coded pipeline, push constants restated
from the shader by hand, synchronous submit-and-wait uploads, and per-use-site
samplers. Each has its own row in §6.1.

The golden images are what make a rewrite checkable: every example must render
identically afterwards, and no reference moves.

### 9.2 The order

| | Item | Unblocks | Milestone |
|---|---|---|---|
| **A** | `slop-render` — frame renderer: acquire, submit, present, frames in flight, swapchain recreation. Typed errors, configurable frame count. Both examples rewritten onto it and both goldens unchanged. **Landed.** | C, D, E | M2 |
| **B** | Shader reflection — vertex layouts and push constant sizes read from the cooked shader instead of restated in Rust. **Landed**, via `slangc -reflection-json`; §2.11's claim that this needed the Slang library was false and has been corrected. | D | M2 |
| **C** | Debug UI (§10.2) — immediate mode, egui. **Overlay landed**: it draws, and a headless test proves it changes the image. The entity inspector over `slop-reflect` is what remains; the pass visualizer waits for E. | E | M2 |
| **D** | Materials — glTF materials, multiple meshes per file, scene hierarchy, mipmaps, then a Sponza-scale scene that loads and draws. **M2's exit criterion.** | E | M2 |
| **E** | Render graph — passes declaring reads and writes, barriers derived rather than hand-written. Then Stage A proper: clustered forward+, shadows, IBL, HDR/tonemap. | — | M3 |

Async streaming sits beside D and E rather than before them. Sponza is the first
thing that loads enough at once to say what the streaming API needs, and building
it earlier would be designing against an imagined consumer — the mistake §4.1-C
avoided for the job system.

### 9.3 Definition of done — M2

**Met, 2026-08-03.**

- ✅ A Sponza-scale glTF loads through the cook pipeline and renders — 103
  primitives, 25 materials, 70 BC7 textures with mip chains, normal-mapped
- ✅ Materials come from the file, not from code
- ✅ Vertex layouts and push constants are reflected out of cooked shaders, not
  restated in Rust
- ✅ A debug overlay shows frame timing and lets an entity be inspected live
- ✅ Both existing goldens still pass, unchanged, through `slop-render`
- ✅ Nothing in `examples/` is doing a job an engine crate should be doing —
  `FrameRenderer`, `Pass`, `Gpu`, `DebugUi`, `FrameTimes` and `inspector` all
  came out of examples during M2, and every example is `unsafe`-free

**Two gaps that are not exit criteria and are worth naming rather than leaving
implicit:**

- **`examples/model` has no golden image.** `MeshRenderer`, materials, mipmaps
  and tangents have no image-level regression; the cube's five goldens cover a
  path none of them use. A Sponza golden must skip when the asset is not fetched,
  and skips are what once let the whole suite report green while the demo refused
  to start — so the skip has to be checked by name, as `harness` already does.
- **Linux has never been run.** Every portability claim in this repository is
  currently untested. Standing since M0.
