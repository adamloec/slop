# Slop Engine — Implementation Plan & Session Handoff

**Status:** Pre-implementation. No code written yet.
**Last updated:** 2026-07-31

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

**M0 tasks A, B, C, E and F are done. The triangle renders.**

Verified on the RTX 5090: window, surface, device, swapchain, cooked Slang
shader, graphics pipeline, and a frame loop with two frames in flight, running
with validation active and reporting no errors, and shutting down cleanly. A
one-second run completes roughly 5,400 frames, which is the evidence that the
frames-in-flight pipelining works rather than stalling on the GPU each frame.

Remaining for M0:

| Task | State |
|---|---|
| D — `slop-rhi` | Mostly done. `gpu-allocator`, buffers, images and descriptors remain, and are needed for the cube rather than the triangle. |
| G — verification skeleton | Not started. Headless mode, one golden image. |
| §4.2 exit criteria | The cube, dual-platform CI, and a golden image are all outstanding. |

The triangle deliberately allocates nothing — positions come from `SV_VertexID`
— so the first render did not also depend on the allocator, buffer uploads, or
descriptor sets being correct. Those arrive together with the cube.

**Three bugs reached a running program that review did not catch**, which is the
argument for pulling task G forward rather than leaving it last:

1. A missing `shaderDrawParameters` feature: 18 validation errors, zero test
   failures, and correct output on this driver regardless.
2. A backwards triangle winding: invisible geometry, no validation complaint,
   and reasoning about it produced the wrong answer twice.
3. A drop-order crash on shutdown, which only appeared when a human closed the
   window rather than when the process was killed.

None of the three was visible to the type system, clippy, or the test suite.

---

### 3.1 Earlier state

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

**G. Verification skeleton**
- Headless mode that renders N frames without a window
- One golden-image test wired into CI
- Establishes the §5 pattern early, while it is trivial

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
- Validation layers clean
- One golden-image test passing
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
7. **Slang integrates as a library, not the `slangc` binary,** once reflection is
   needed (§2.11) — reflection is unavailable from CLI-compiled shaders.
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

`DESIGN.md` §6 has the full milestone list. Immediate outlook:

**M1 — ECS + reflection.** Expect this to be *slower* than M0 despite less code.
Vulkan bring-up is high-volume but well-trodden; the ECS and reflection design
carries real judgment. Critically, **§2.10's archetype storage and §2.3's
columnar WASM boundary must be designed together, not sequentially** — they want
the same memory layout, and discovering that after the fact means rework.

M1 also lands the §5 verification infrastructure properly. Do not defer it: code
can be produced faster than it can be reviewed line by line, and automated truth
is the only thing preventing large volumes of subtly wrong architecture.

**M2 — Content + debug UI.** The debug UI is pulled forward deliberately;
renderer bring-up without inspection tooling is the largest avoidable time sink
in the plan.
