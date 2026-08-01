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
| Physical device selection and scoring | Planned | M0 |
| Logical device, queue families | Planned | M0 |
| `gpu-allocator` integration | Planned | M0 |
| Surface, swapchain and recreation | Planned | M0 |
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

## 6. Decisions

| Decision | Where |
|---|---|
| Own the RHI; Vulkan via `ash`; not `wgpu` | `DESIGN.md` §2.2 |
| Require Vulkan 1.3, not 1.4 | §4 above |
| M0 ships primitives, not abstraction | `PLAN.md` §4.1-D |
| Slang as the shading language, library-integrated | `DESIGN.md` §2.11 |
| Which Slang Rust binding | `DESIGN.md` §8 item 2 — revisit at M3 |
| Desktop only; one GPU feature tier | `DESIGN.md` §2.1 |

## 7. Invariants

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
