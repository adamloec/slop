# slop-rhi

**Last updated:** 2026-08-01

## 1. Purpose

The render hardware interface — the engine's own abstraction over the graphics
API. Vulkan via `ash` is the first and only initial backend.

The RHI is ours rather than `wgpu`'s because desktop-only removes the
portability argument, and the features the fidelity target needs are precisely
the ones `wgpu` does not expose well: mesh shaders, full bindless descriptor
indexing, sparse residency, explicit barriers and transient aliasing, timeline
semaphores, async compute, and ray tracing (`DESIGN.md` §2.2).

## 2. Status

Started. This is the bulk of M0 and the largest single body of work in it.

| Area | State | Milestone |
|---|---|---|
| Instance, validation layers, debug messenger | Landed | M0 |
| Physical device enumeration, scoring, selection | Landed | M0 |
| Queue family discovery | Landed | M0 |
| Required feature tier, checked at selection | Landed | M0 |
| Logical device, queue creation | Landed | M0 |
| Surface, and surface capability queries | Landed | M0 |
| `gpu-allocator` integration | Planned | M0 |
| Swapchain and recreation | Planned | M0 |
| Command pools and buffers | Planned | M0 |
| Timeline semaphores, explicit barriers | Planned | M0 |
| Bindless descriptor heap | Planned | M0 |
| Minimal pipeline path | Planned | M0 |
| Shader reflection, pipeline layout derivation | Planned | M2–M3 |
| Consumer-facing RHI API extraction | Planned | M3 |

## 3. Scope at M0 — primitives, not abstraction

M0 sits close to `ash` and defers the consumer-facing API to M3.

An abstraction designed with no consumers is designed against imagined
requirements. The render graph and frame renderer at M3 are what determine what
the API must be, and a shape guessed now gets rebuilt then anyway. Building it
twice is fine; building it once, early, and living with it is worse
(`PLAN.md` §4.1-D).

What M0 must get right is the **feature model**, because that is the part which
cannot be retrofitted:

```mermaid
flowchart TD
    subgraph fixed ["Fixed at M0 — unfixable later"]
        ts["timeline semaphores, not fences plus binary semaphores"]
        bar["explicit barriers, never implicit sync"]
        bind["bindless descriptor heap allocated from the start"]
        queues["graphics, compute and transfer queues acquired up front"]
        dev["device selection scores on type — discrete over integrated"]
    end

    subgraph later ["Deferred to M3 — a refactor either way"]
        api["pass and resource abstraction"]
        rg["render graph integration"]
    end

    fixed --> later
```

Get the left side right and the M3 extraction is a refactor. Get it wrong and it
is a rewrite.

## 4. Vulkan 1.3 is the required API version

Not 1.4, despite the development machine reporting 1.4.341. Everything §2.2
commits to is core in 1.3:

| Feature | Core since |
|---|---|
| Timeline semaphores | 1.2 |
| Descriptor indexing (bindless) | 1.2 |
| Dynamic rendering | 1.3 |
| `synchronization2` — explicit barriers | 1.3 |

Requiring 1.4 would narrow supported hardware without buying anything the design
needs. The version is checked at instance creation and reported as a typed error
naming both the required and found versions, since "update your driver" is only
actionable with numbers.

## 5. Validation

Enabled automatically in debug builds, off in release — validation costs
substantial CPU per call and has no place in a shipping frame loop.

Requesting it explicitly and not getting it is an **error, not a downgrade**. A
developer who asked for validation and silently did not receive it would be
debugging undefined behaviour with the one tool that reports it switched off.
`Validation::Automatic` does fall back with a warning, so a machine without the
SDK can still run a debug build.

Validation output is routed into `tracing` rather than stdout, so it obeys the
same filtering as everything else and appears in captured logs. Vulkan's `INFO`
severity maps to `debug` here, keeping `CONVENTIONS.md` §13's rule that `info`
stays meaningful.

## 6. Device selection is player-facing

A game built on the engine will expose a GPU picker in its graphics settings, so
enumeration is public API rather than an internal step (`DESIGN.md` §7). Three
consequences shape the design:

**Selection keys on `deviceUUID`, not an enumeration index.** A player's saved
choice must survive adding a GPU, removing one, or a driver update reordering
them. Indices do not survive any of those; a saved index silently points at a
different card.

**A saved choice degrades rather than fails.** If the named device is gone or has
become unusable, selection falls back to automatic with a warning. Swapping a
graphics card must not prevent a game from launching. `ByIndex` deliberately does
*not* fall back — it exists for test harnesses, which want to fail loudly rather
than silently measure a different device.

**Unusable devices are listed with a reason**, not hidden, so a settings UI can
grey an entry out and say why.

```mermaid
flowchart TD
    all["every adapter the driver reports"] --> filter{"meets requirements?"}
    filter -->|"no"| reject["listed with a Rejection reason"]
    filter -->|"yes"| usable["usable"]
    usable --> pick{"selection mode"}
    pick -->|"ByUuid, found"| chosen["chosen"]
    pick -->|"ByUuid, missing"| fallback["warn, fall back to automatic"]
    pick -->|"Automatic"| score["score: kind, then memory"]
    fallback --> score
    score --> chosen
    reject -.->|"never selectable"| chosen
```

Filtering happens **before** scoring, so scoring can never pick a device that
cannot do the job.

Kind outranks memory in the score, which is not arbitrary: an integrated GPU
reports shared system RAM as device-local memory. On this development machine
the UHD 770 claims 16 GiB against the 5090's 32 GiB, and on a machine with an
8 GiB card and 32 GiB of RAM a memory-only score would pick the iGPU.

A software rasterizer ranks last by a wide margin. lavapipe is what CI golden
images render on (`PLAN.md` §4.1-G), and selecting it by accident on real
hardware would mean rendering thousands of times slower with nothing reporting
it.

## 7. One feature tier, declared once

`DESIGN.md` §2.1 buys "one GPU feature tier, no capability-tier branching in the
renderer" by targeting desktop only. That guarantee is worth nothing unless the
tier is stated somewhere singular and checked *before* a device is accepted —
otherwise it decays into scattered runtime checks, which is exactly the
branching the decision exists to avoid.

`features.rs` is that single place. A device either supports all of it and is
usable, or it is rejected by name. There is no partial support and no fallback
path.

| Requirement | Why |
|---|---|
| `timeline_semaphore` | §2.2 — not fences plus binary semaphores |
| `synchronization2` | §2.2 — explicit barriers, modern API |
| `dynamic_rendering` | Removes render pass and framebuffer objects the render graph would have to cache |
| `descriptor_indexing` + 5 related | §2.2 — what "bindless" actually means in Vulkan terms |
| `buffer_device_address` | Shaders holding pointers, for GPU-driven passes |
| `multi_draw_indirect`, `draw_indirect_count` | §4.2 stage B's GPU-driven pipeline |
| `sampler_anisotropy` | Table stakes for the fidelity target |
| `fill_mode_non_solid` | Wireframe, for the §10.2 debug UI |

Rejections carry the spec's own feature names, so a report can be looked up
directly rather than translated.

**Device extensions are conditional on need.** `VK_KHR_swapchain` is enabled if
and only if a present queue family exists — which happens exactly when a surface
was supplied. It depends on the instance-level `VK_KHR_surface`, so requesting it
on a headless instance is a spec violation that permissive drivers accept and
strict ones reject. Validation caught this during bring-up; NVIDIA had been
creating the device anyway.

## 8. Decisions

| Decision | Where |
|---|---|
| Own the RHI; Vulkan via `ash`; not `wgpu` | `DESIGN.md` §2.2 |
| Require Vulkan 1.3, not 1.4 | §4 above |
| Device selection by UUID, degrading to automatic | §6 above |
| One feature tier, checked at selection, no fallbacks | §7 above |
| `Device` holds an `Arc<Instance>` to encode lifetime ordering | `device.rs` module docs |
| M0 ships primitives, not abstraction | `PLAN.md` §4.1-D |
| Slang as the shading language, library-integrated | `DESIGN.md` §2.11 |
| Which Slang Rust binding | `DESIGN.md` §8 item 2 — revisit at M3 |
| Desktop only; one GPU feature tier | `DESIGN.md` §2.1 |

## 9. Invariants

1. **This crate and the allocator are the only sanctioned homes for `unsafe`.**
   `unsafe` anywhere else is a design discussion, not a review comment.
2. **Every `unsafe` block carries a `// SAFETY:` comment** stating the invariant
   that makes it sound. Enforced by `clippy::undocumented_unsafe_blocks`.
3. **Physical device selection scores on `deviceType`.** The development machine
   exposes both a discrete 5090 and an integrated UHD 770; taking index 0 is the
   difference between the two.
4. **Never hand-roll platform surface code.** `ash-window` and
   `raw-window-handle` exist to absorb the Win32/Wayland/X11 split.
5. **Validation layers on in debug builds**, plus our own assertions on barrier
   and resource-lifetime correctness.
6. **The FFI seam stays in one place.** Wrapping `ash` and the Slang bindings
   behind a narrow internal interface is what keeps swapping or vendoring them
   contained (`DESIGN.md` §2.11).
7. **Struct field order is drop order, and it is load-bearing.** Vulkan objects
   must be destroyed before whatever created them — the debug messenger before
   its instance, the instance before the entry that loaded the library.
   Reordering fields to look tidier is a use-after-free.
8. **The instance knows nothing about windows.** Surface extensions are supplied
   by the caller, so one code path serves both a windowed application and the
   headless mode `DESIGN.md` §5 requires.
9. **GPU-dependent tests live in `tests/` and skip only on a missing loader.**
   Any other failure is reported. Skipping on every error would make the suite
   worthless the first time it mattered.
10. **Filter before scoring.** A device that cannot meet requirements is never
    scored, so it can never be selected — including by an explicit request,
    which errors with the reason instead.
11. **Never select by index in player-facing paths.** Indices are not stable
    across hardware or driver changes; `deviceUUID` is.
12. **Required features are declared only in `features.rs`.** A capability check
    anywhere else is the capability-tier branching §2.1 exists to prevent.
13. **This crate never depends on a windowing library.** `Surface` takes raw
    handles via `raw-window-handle`, so the RHI stays agnostic, the headless
    path pulls in no window code, and an embedder can supply a surface from
    windows it already owns. Joining `winit` to Vulkan is `slop-app`'s job.
14. **Swapchain extents come from the surface, in physical pixels.** A window
    requested at 1280×720 logical pixels reports a 1920×1080 surface on a
    display at 150% scaling. Sizing a swapchain from the logical request
    produces blurry output or validation errors.
13. **Enable an extension only when it is needed and its dependencies are
    present.** A permissive driver accepting an invalid create-info is not
    evidence of correctness.
14. **`Device::drop` waits for idle first.** Destroying objects with GPU work
    still referencing them is the shutdown crash that only reproduces under
    load.
