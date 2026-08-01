# Slop Engine — Implementation Plan & Session Handoff

**Status:** Pre-implementation. No code written yet.
**Last updated:** 2026-07-31

This document is the working companion to `DESIGN.md`. **`DESIGN.md` is
authoritative for all architectural decisions** — read it first, in full, before
writing any code. This file covers what `DESIGN.md` deliberately does not: who
we are building for, the state of the environment, the immediate task breakdown,
and the invariants that are easy to violate by accident.

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

### 2.2 Windows setup checklist

- [ ] Clone to a **native Windows path** (e.g. `C:\dev\slop`). **Not** under
      `/mnt/c` or a WSL path — cross-boundary Rust builds are slow and hot-reload
      file watching is unreliable.
- [ ] Visual Studio Build Tools (for the MSVC linker)
- [ ] `rustup` with the `x86_64-pc-windows-msvc` toolchain
- [ ] LunarG Vulkan SDK — provides validation layers and `slangc`
- [ ] RenderDoc — install now, not when already stuck
- [ ] Optional: NVIDIA Nsight Graphics

### 2.3 Repo

`git@github.com:adamloec/slop.git`, branch `main`.

---

## 3. Current state

Nothing implemented. The repository contains `README.md`, `DESIGN.md`, and this
file.

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
- Arena / bump allocator for per-frame scratch
- Time and frame pacing primitives
- `tracing` setup for structured logging
- Job system: **a work-stealing scheduler is §2.5 and foundational.** It is
  acceptable to land a minimal but *correctly shaped* API in M0 and deepen it in
  M1 — but the API shape must not assume single-threaded execution, because
  that assumption is what becomes unfixable later.

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

> **This is where M0's real design judgment lives.** Everything else in M0 is
> mechanical. The RHI's API shape determines whether §2.2's explicit model
> actually holds up, so it deserves genuine thought rather than transcription
> from a tutorial. Expect roughly a thousand lines before the first triangle —
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

## 7. Conventions to settle at M0

Proposals, to confirm when starting rather than debate mid-implementation:

- **Errors:** `thiserror` for library crates, `anyhow` only at application
  boundaries
- **Logging:** `tracing`, structured, with spans around subsystem work
- **`unsafe`:** confined to `slop-rhi` and the allocator; every block carries a
  `// SAFETY:` comment stating the invariant
- **Testing:** unit tests colocated; integration and golden-image tests in
  `tests/`
- **MSRV:** pin via `rust-toolchain.toml`, bump deliberately

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
