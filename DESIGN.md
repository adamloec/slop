# Slop Engine — Design Document

**Status:** Draft — pre-implementation
**Last updated:** 2026-07-31

---

## 1. What we are building

Slop is a 3D game engine written in Rust, built from scratch, targeting Windows
and Linux desktop with modern (AAA-adjacent) visual fidelity.

It is an **engine and a platform**, not a game and not a plugin on top of
someone else's engine. The distinction that matters: a platform has a stable
extension surface, a content pipeline, and tooling that third parties can build
against without our toolchain.

### 1.1 Non-goals

Explicitly out of scope, to keep the target honest:

- **Web/browser support.** Deleted from scope. It constrains the threading model
  and caps the renderer at WebGPU's feature set.
- **macOS.** Dropped. Vulkan on macOS runs through MoltenVK, which lags on
  exactly the features the fidelity target requires. See §2.1.
- **Mobile and console.** Not targeted. The architecture should not actively
  prevent a future port, but we will not pay abstraction costs for it now.
- **Being a Bevy/Godot plugin.** We own the engine.
- **Rewriting solved science.** We do not write our own linear algebra library,
  physics solver, or WASM runtime. See §3.
- **AI-first design.** AI integration is a downstream consequence of good
  architecture, not a driver of it. See §9.

### 1.2 Guiding principles

1. **Own everything that defines engine behavior.** Take dependencies only for
   hardware abstraction and specialized solved problems.
2. **Data-oriented by default.** Structure of arrays where it counts, handles
   instead of pointers, no reference-counted object graphs in hot paths.
3. **Explicit over implicit.** The GPU layer, the scheduler, and the asset
   pipeline all expose their costs rather than hiding them.
4. **The engine is a library.** The game owns `main()` and can drive the loop.
   We do not impose a framework.
5. **Verification scales with generation.** Code can be produced faster than it
   can be eyeballed. Automated truth — determinism, golden images, round-trip
   tests, perf budgets — is infrastructure, not polish.

---

## 2. Locked decisions

These are decided. Changing any of them is a re-architecture, not a refactor, so
they are listed with rationale.

### 2.1 Desktop only — Windows and Linux

Removes the portability constraint on the GPU layer, permits unrestricted
multithreading, and allows targeting current-generation GPU features directly.

**macOS is excluded deliberately.** Vulkan there runs through MoltenVK, which
lags on precisely the features the fidelity target depends on — mesh shaders,
sparse residency, full bindless descriptor indexing. Supporting it would mean
either a permanently reduced feature tier or a separate Metal RHI backend.
Neither is worth paying for now.

The payoff is significant: Windows and Linux both expose Vulkan natively and
share effectively the same driver feature set, so there is **one GPU feature
tier**, no capability-tier branching in the renderer, and no translation layer
between us and the hardware. If a second backend is ever wanted it is DX12 on
Windows, which the RHI design in §2.2 already accommodates.

### 2.2 We own the RHI; Vulkan backend first, via `ash`

The render hardware interface is ours. The first and only initial backend is
Vulkan through `ash`.

**Why not `wgpu`:** wgpu's two core value propositions are portability and
safety. Desktop-only deletes the first. The second matters less when the
features we want are precisely the ones wgpu does not expose well:

- Mesh shaders
- Full bindless descriptor indexing
- Sparse / virtual texture residency
- Explicit barrier control and transient resource aliasing
- Timeline semaphores
- Async compute and transfer queues
- Ray tracing

That list is effectively a definition of AAA-adjacent rendering. Building on
wgpu means fighting the abstraction rather than using it.

**Critical detail:** an RHI designed against wgpu's model bakes in wgpu's
assumptions — render-pass-centric, implicit synchronization, bound descriptor
sets. A Vulkan backend under that abstraction is permanently awkward. Therefore
the RHI is **designed against the modern explicit model from day one**: explicit
barriers, timeline semaphores, bindless descriptor heaps, transient aliasing,
multiple queues. A DX12 backend then slots in cleanly; wgpu becomes unnecessary
rather than load-bearing.

**Cost:** roughly 8–15k lines of Vulkan infrastructure — allocator, swapchain,
descriptor management, synchronization, pipeline caching, shader reflection —
before a well-architected first triangle. Accepted.

### 2.3 Gameplay and extensions run as WASM modules

The engine core is native compiled Rust. Gameplay logic, mods, and third-party
extensions are WebAssembly modules loaded by `wasmtime`, with interfaces defined
in WIT (Component Model).

**Why:** every successful engine converged on native core + separate gameplay
layer — Unreal/Blueprint, Unity/C#, Godot/GDScript. Gameplay code is ~90% of a
game's source and ~5% of its CPU time, and it changes constantly. It should be
isolated, hot-swappable, and safe to load from untrusted sources.

Specific to Rust: **Rust has no stable ABI.** This kills the obvious
alternative — native dylib plugins via `libloading` — where any compiler version
or feature-flag mismatch is undefined behavior. WASM provides a genuinely
stable, versioned ABI.

Additional properties we get for free:

- **Sandboxing.** A platform hosting third-party content needs a security
  boundary. It cannot be retrofitted.
- **Language independence.** WIT generates bindings for Rust, C, C++, Zig,
  AssemblyScript, and later C#.
- **Determinism**, which we want anyway for netcode, replay, and rollback.
- **Hot reload** without process restart, because module state is confined to
  its own linear memory.

**The constraint this imposes:** WASM's real cost is not compute throughput
(Cranelift AOT runs at roughly 1.1–1.5× native) — it is **call-boundary
chatter**. A guest calling `get_transform(entity)` a million times per frame is
fatal.

Therefore, non-negotiable from day one: **the guest API is columnar and bulk,
never per-entity and chatty.** The host hands a system a slice of component
columns in shared linear memory; the guest iterates natively over it. Boundary
crossings happen tens of times per frame, not millions. Designed in from the
start this overhead is negligible; bolted on later it is unfixable.

A scripting language (Lua, or similar) may later be layered *above* this ABI. It
is not a competitor to it.

### 2.4 Reflection is a first-class, early subsystem

A runtime type system with a derive macro, built in the first milestone.

**Why:** serialization, the scene file format, editor property panels, WASM
binding generation, network replication, undo/redo diffs, save games, and debug
inspectors are all *derived* from one reflection system. Building it early makes
every later subsystem cheaper. Skipping it is the single most common fatal
mistake in from-scratch engines — it produces five incompatible hand-written
serializers and a rewrite around month 18.

**Derived constraint: types must be registrable at runtime, not only at compile
time.** This follows necessarily from §2.3 and §2.12 taken together. A WASM
guest module declares its own components — `struct Inventory { ... }` — that the
host was never compiled against, and §2.12's editor is expected to render a
property panel for exactly those components. So reflection cannot be a derive
macro populating a static registry of host-known types. It needs a runtime
registration path where size, alignment, field offsets, and drop behavior arrive
as *data* at module load time, with the derive macro being merely the
convenient front end for host-native types.

The same constraint propagates into §2.10: archetype columns must support
component types whose layout is known only at runtime. This is a materially
different design from "every component is a Rust type known at compile time,"
and it is cheap to design in and expensive to retrofit. It is therefore an M1
concern, alongside the ECS itself — see `PLAN.md` §8.

### 2.5 Job-system-first threading

A work-stealing task scheduler is foundational, not additive. Systems declare
read/write dependencies so the scheduler can auto-parallelize. Bolting
parallelism onto a single-threaded engine is a rewrite, not an optimization.

### 2.6 Handles everywhere, never pointers

Generational indices into slotmaps for all engine-owned resources — entities,
assets, GPU resources, scene nodes. This is the single most important
Rust-specific engine idiom: it is how graph-shaped engine data avoids fighting
the borrow checker without descending into `Rc<RefCell<>>`.

### 2.7 Fixed-timestep simulation, interpolated rendering

Simulation advances at a fixed rate; rendering interpolates between the two most
recent simulation states. Physics stability, determinism, netcode, and replay
all depend on this. Retrofitting it touches everything.

### 2.8 Cooked asset pipeline from day one

Shipping builds never parse a PNG or a glTF file at runtime.

```
source asset  →  import  →  cook  →  runtime format  →  mmap
 (.gltf/.png)             (BCn/ASTC textures,
                           optimized index buffers,
                           meshlet-ready geometry)
```

Cache keyed on content hash + importer version. Retrofitting this means redoing
every asset type, so it lands with the first asset.

### 2.9 The renderer consumes an immutable snapshot, never live world state

Each simulation tick produces a self-contained packet — a *render snapshot* —
holding everything the renderer needs for that frame. The renderer never reads
the live simulation world.

**Background.** Each frame the engine does two large jobs: *simulation* (input,
gameplay logic, physics, transform updates) and *rendering* (visibility
determination, command list construction, GPU submission). Running them
sequentially is simple and gives the lowest input latency. Running them
overlapped — the simulation computing frame N+1 while the renderer draws frame N
— is *pipelined rendering*, and it substantially raises throughput by keeping
both stages busy on separate cores. The cost is one frame of added latency and a
shared-mutable-state hazard, since the simulation would be writing data the
renderer is reading. It is the classic synchronous-handler versus
producer/consumer-queue trade.

**Why snapshot rather than deciding pipelining now:** the snapshot is the
architectural commitment; pipelining is then a scheduling policy that can be
toggled and measured against real content rather than guessed at today. The
snapshot is independently required by three things already locked in:
interpolated rendering (§2.7), deterministic replay (§5), and the columnar WASM
boundary (§2.3). It also eliminates the sim/render data race by construction
instead of by discipline.

Every subsystem that spans both sides must respect the boundary from the start —
this is the constraint that is expensive to add later.

### 2.10 Archetype (table) ECS storage

**Background.** The ECS — Entity Component System — is the engine's data model,
structurally an in-memory database. An *entity* is an ID, nothing more. A
*component* is a plain data struct attached to an entity (`Position`, `Mesh`,
`Health`) with no methods and no inheritance. A *system* is a function that runs
over every entity possessing a given set of components. Entities are rows,
components are columns, systems are queries. The reason every modern engine uses
some form of it is memory layout: contiguous component arrays turn iteration
into a linear scan that the CPU cache and prefetcher handle optimally, where an
object-graph approach costs a cache miss per entity.

The storage question is how the columns are physically laid out:

- **Archetype / table storage** groups entities by their exact component set.
  All entities with exactly `{Position, Velocity, Mesh}` share one table of
  parallel arrays. Queries walk contiguous memory. Adding or removing a component
  changes an entity's archetype and requires physically moving its data between
  tables.
- **Sparse-set storage** gives each component type its own array plus an
  entity→slot index. Add/remove is a cheap O(1) operation on one array, but
  multi-component queries hop between arrays through an indirection and lose
  cache locality.

**Decision: archetype.** Rationale:

1. The fidelity target makes iteration-heavy workloads dominant — culling,
   transform propagation, and render extraction all sweep large entity counts
   every frame.
2. **Forced by §2.3.** The WASM boundary requires handing guest modules
   contiguous columns of component data. Archetype storage produces this
   natively; sparse-set would require gathering scattered data into a temporary
   buffer every frame — exactly the per-frame cost the columnar ABI exists to
   avoid.
3. Disjoint archetypes parallelize with no coordination — separate tables to
   separate threads, no locking.

**Mitigating the churn weakness:** structural changes (adding/removing
components, spawning/despawning) are queued into command buffers and applied at
an explicit sync point rather than immediately. This is required for safe
parallel system execution regardless, so it is not a cost specific to this
choice.

Opt-in per-component sparse-set storage may be added later for pathologically
high-churn component types without disturbing the archetype default.

### 2.11 Slang as the shading language, integrated as a library

Shaders are authored in [Slang](https://shader-slang.org/) and compiled to
SPIR-V. The Slang compiler is linked into our toolchain **as a library**, not
invoked as the `slangc` command-line binary.

**Background.** A *shading language* is what GPU programs are written in. Vulkan
consumes SPIR-V, a binary intermediate representation, so the real choice is
which source language compiles down to it. The traditional options are GLSL and
HLSL, both of which are effectively C with preprocessor macros and no module
system.

**Why Slang:**

- **Khronos-governed.** Slang is developed as open source under Khronos, the
  same body that owns Vulkan and SPIR-V, and has shipped inside the Vulkan SDK
  since 1.3.296.0. It is part of the standard Vulkan toolchain rather than a
  third-party dependency that can be abandoned.
- **Real language features.** Modules, generics, and interfaces, rather than
  textual `#include` and macro expansion. This is what makes a large shader
  codebase maintainable.
- **Link-time specialization.** Modules can be composed and linked with
  specialization constants at build time. This is the modern answer to shader
  permutation explosion — the `#ifdef` combinatorics that generate thousands of
  preprocessor variants in mature engines.
- **Multi-target.** Compiles to SPIR-V, HLSL/DXIL, Metal, WGSL, and CUDA from
  one source. If a DX12 backend is ever added per §2.2, shaders do not fork.
- **Proven at scale.** The Khronos Vulkan Samples repository carries Slang
  versions of nearly a hundred samples.

**Why library integration is mandatory, not a preference:** reflection
information cannot be queried from shaders compiled with the `slangc`
command-line tool — it is available only through the compilation API. Reflection
is how `slop-rhi` derives descriptor set layouts and pipeline layouts directly
from shader source instead of hand-maintaining parallel definitions in Rust and
relying on discipline to keep them synchronized. Shelling out to a binary
forecloses that.

**Integration point.** Shaders are a cooked asset type (§2.8). The pipeline
compiles Slang → SPIR-V and cooks the reflection metadata alongside the binary.
Development builds compile on demand to support hot reload; shipping builds
consume only precompiled SPIR-V.

**Known risk — the Rust binding layer, not Slang itself.** Slang is mature; the
Rust path to it is thin. The `shader-slang` crate covers the modern compilation
and reflection API but is early (0.1.x, low adoption), and several competing
binding forks exist. Mitigation: wrap it behind a narrow internal interface in
`slop-rhi` so that swapping bindings, vendoring, or generating our own `bindgen`
layer stays contained to one module. **Revisit at M3.**

### 2.12 The editor is a host application; the game loads into it as a WASM module

The editor is not a privileged mode inside the engine. It is a separate
application that embeds the engine as a library — the same way a shipping game
does — and loads the game's gameplay modules at runtime.

**Background.** Engines take one of two approaches. A *separate editor
application* writes data files that a distinct game process later consumes:
clean separation, but the renderer and scene code get duplicated and iteration is
slow. *Editor-as-mode* (Unity, Unreal, Godot) runs the game inside the editor's
own viewport, giving instant iteration at the cost of having to save and restore
world state across every play/stop cycle — a notorious and permanent source of
bugs where stopping play mode silently discards edits or leaks state.

**Why the hybrid works here:** §2.3 already puts gameplay in WASM modules with
isolated linear memory. "Play" is loading a module; "stop" is discarding it.
Module state physically cannot leak into the editor's world, so the state
restoration problem that plagues editor-as-mode is solved by construction rather
than by bookkeeping. We get play-in-editor iteration speed without its usual
defect class.

This is also the direct consequence of §1.2's "the engine is a library, not a
framework" — the editor is just another consumer of the engine, no more
privileged than the game runtime.

### 2.13 Windows and Linux are both first-class from M0, enforced by CI

Both target platforms (§2.1) are built and tested continuously from the first
milestone. Neither is a "port."

**Rationale.** Portability breakage caught within hours of the causing commit is
a three-line fix. The same breakage discovered months later, accumulated across a
large codebase, is a multi-week project. It is the same work distributed
differently over time, and one distribution is dramatically worse. "We'll port it
later" is the failure mode this decision exists to prevent.

The stack is portable by construction — Rust, Vulkan, winit, Slang, wasmtime,
glam, and rapier are all cross-platform, and Vulkan in particular varies more
across GPU vendors than across operating systems. §2.3 also sidesteps native
dynamic library loading entirely by shipping gameplay as WASM rather than `.dll`
/ `.so`. The risk is therefore not architectural; it is a small set of specific
traps that silently pass on one platform and fail on the other:

| Trap | Consequence | Mitigation |
|---|---|---|
| **Filesystem case sensitivity** — Windows insensitive, Linux sensitive | `Textures/Rock.png` vs `textures/rock.png` works on Windows, fails on Linux. Accumulates invisibly. **The most common real-world breakage.** | Asset cooker enforces and validates lowercase paths at cook time |
| **Line endings** (git autocrlf) | Identical shaders hash differently across platforms, silently breaking §2.8's content-hash cache and CI artifact reuse | `.gitattributes` normalizing line endings |
| **Path construction** | Backslash literals and string concatenation break on Linux | `PathBuf` / `Path::join` exclusively; lint against separator literals |
| **Native build dependencies** — MSVC vs GCC/Clang | Any C/C++-wrapping crate must build on both. **Most likely to actually bite us:** the Slang bindings (§2.11) wrap a C++ library | CI matrix catches it immediately; contained behind the §2.11 wrapper |
| **Vulkan surface extensions** — `VK_KHR_win32_surface` vs xlib/wayland | Hand-rolled surface creation does not port; Linux adds a Wayland/X11 axis | `ash-window` + `raw-window-handle` |
| **File watching** for hot reload | Different OS mechanisms (ReadDirectoryChangesW vs inotify) | `notify` crate |

**Implementation:** a CI matrix building both platforms, running the §5
verification suite on each. Because both use the same GPU vendor and driver
generation, golden images should match within tolerance across operating
systems — cross-platform divergence therefore becomes an actionable signal rather
than an unexplained difference.

Primary development currently happens on Windows. That is a workflow detail, not
a design position; the CI requirement is what keeps it from becoming one.

---

## 3. Dependency policy

The line: **write everything that defines engine behavior; take the
hardware-abstraction and solved-science layers.**

### 3.1 We write

| Subsystem | Why it must be ours |
|---|---|
| ECS / data model | The core of the engine's identity and performance |
| Reflection | Everything else derives from it (§2.4) |
| Asset pipeline | Content pipeline is where engines live or die |
| RHI + render graph | The fidelity target requires explicit control (§2.2) |
| Frame renderer | The visual identity |
| Scene, transforms, culling, streaming | Core simulation structure |
| Job system / scheduler | Determines the whole threading model |
| Module ABI + app runtime | The platform surface |

### 3.2 We take

| Crate | Role | Rationale |
|---|---|---|
| `glam` | Linear algebra | SIMD-optimized, battle-tested; rewriting is ego cost |
| `ash` | Vulkan bindings | Thin FFI layer, not an abstraction |
| `gpu-allocator` | GPU memory allocation | Solved, subtle, well-tested |
| `shader-slang` (or vendored bindings) | Slang compiler API | Khronos-governed compiler; only the Rust binding is ours to worry about (§2.11) |
| `winit` | Windowing / OS input | Pure platform drudgery |
| `rapier3d` | Physics | A competitive solver is a multi-year specialty |
| `wasmtime` | WASM runtime | Enormous, security-critical, actively maintained |
| `image`, `gltf`, `basis`/`texpresso` | Import-side format parsing | Import-time only, never in the runtime |
| `cpal` | Audio device I/O | Platform layer; our mixer/spatializer sits above |
| `egui` | Editor UI (initially) | Tooling, not runtime; replaceable later |
| `tracy-client` or similar | Profiling | Instrumentation, not engine logic |

Using PhysX and Ogg Vorbis does not make Unreal not Epic's engine. The same
reasoning applies here.

---

## 4. Architecture

Layered crates, each depending only downward.

```
slop-math       glam wrappers, Transform, AABB, frustum, curves, packing
slop-core       arenas, slotmaps/handles, string interning, job system,
                time, tracing, profiling markers
slop-reflect    runtime type info, derive macro, serialization primitives
slop-ecs        archetype storage, queries, change detection, relationships,
                system scheduling, command buffers
slop-asset      VFS, async loading, dependency graph, hot reload,
                source→cooked pipeline, content-hash cache
slop-rhi        Vulkan backend, explicit sync, bindless descriptor heaps,
                queue management, pipeline cache, shader reflection
slop-render     render graph, material/shader system, frame renderer,
                lighting, shadows, post stack
slop-scene      hierarchy, transform propagation, BVH culling, LOD, streaming
slop-physics    rapier integration, character controllers, queries
slop-audio      mixer, spatialization, DSP graph on cpal
slop-abi        WIT interface definitions — the platform contract
slop-host       wasmtime host, module lifecycle, bulk data marshalling
slop-app        main loop, module/plugin wiring, configuration
slop-editor     egui-based tooling
slop-cli        build, cook, run, inspect, test
```

### 4.1 Frame structure

```
[ sim thread ]   fixed timestep N Hz
                 input → gameplay systems (native + WASM) → physics
                 → scene graph update → produce render-ready snapshot
                        │
                        ▼  (double-buffered state, interpolation alpha)
[ render thread ] variable rate
                 culling → render graph compile → command recording (parallel)
                 → submit
```

Whether rendering runs a full frame behind simulation (pipelined) is an open
question — see §8.

### 4.2 Renderer strategy

Staged, because the fidelity target is a destination and not a starting point.

**Stage A — Clustered forward+.** PBR metallic-roughness, cascaded shadow maps,
IBL from HDR environment, HDR pipeline with proper tonemapping, TAA, bloom,
SSAO. Handles transparency and MSAA correctly, scales to thousands of lights.
This is a credible modern look on a well-trodden path.

**Stage B — GPU-driven.** Bindless resources, GPU culling, indirect draw,
meshlet geometry pipeline, virtual shadow maps. This is where the explicit RHI
decision pays for itself.

**Stage C — Frontier.** Visibility buffer with deferred material shading,
real-time GI, virtual texturing, ray-traced effects.

The render graph must be designed so a visibility-buffer path can be added
without restructuring — pass declaration and resource lifetimes stay independent
of the shading strategy.

---

## 5. Verification infrastructure

Treated as a first-class subsystem, landing at M1 rather than at the end.

Rationale: when code is produced faster than it can be reviewed line by line,
automated truth is the only thing preventing large volumes of subtly wrong
architecture. The bottleneck in this project is verification, not authoring.

- **Deterministic headless mode.** Run N simulation ticks with no window, seeded
  RNG, stable iteration order. Reproducible bug reports and CI.
- **Golden-image regression tests.** Render fixed scenes at fixed frames,
  compare against approved references. Two tiers: hosted CI on both platforms
  renders through **lavapipe**, a CPU rasterizer, which is bit-deterministic and
  vendor-independent and so compares by **exact match**; a separate opt-in lane
  renders on real hardware to cover driver behavior. See `PLAN.md` §4.1-G.
- **Serialization round-trip tests.** Every reflected type: serialize →
  deserialize → compare. Runs automatically for all registered types.
- **Frame-budget harness.** Per-subsystem timing budgets asserted in CI against
  reference scenes. Performance regressions fail the build.
- **Validation layers on in debug.** Vulkan validation, plus our own RHI-level
  assertions on barrier and lifetime correctness.
- **Dual-platform CI matrix.** Every check above runs on both Windows and Linux
  from M0, per §2.13.

---

## 6. Milestones

Ordered by risk and by what unblocks what — not by what is visually impressive.

**M0 — Foundation.** `slop-core` (job system, handles, arenas), `slop-math`,
window, Vulkan device/swapchain bring-up, a lit textured cube.
*Exit: the stack is proven end to end.*

**M1 — Data model.** `slop-ecs` and `slop-reflect` with derive macro. Scene
serialization to text and back. Verification infrastructure (§5) online.
*Exit: a scene round-trips and CI can assert correctness. The hardest and
least glamorous milestone; everything downstream depends on it.*

**M2 — Content + debug UI.** Asset pipeline, glTF import + cook, texture
compression, hot reload, material system. Load and render a Sponza-scale scene.
Plus the debug UI layer (§10.2): frame timing overlay, live entity inspector,
render pass visualizer.
*Exit: real content flows through the engine, and we can see inside it.*

The debug UI is pulled this far forward deliberately. GPU bring-up and renderer
debugging are the work that does not compress (§5), and doing M3 blind would be
the single largest avoidable time sink in the plan. The components are
individually small and derive mostly from reflection.

**M3 — Renderer, Stage A.** Clustered forward+, shadows, IBL, HDR/tonemap, post
stack. Render graph fully in place.
*Exit: it looks good.*

**M4 — Extension ABI.** WIT interfaces, wasmtime host, bulk columnar system
dispatch, module hot reload. First gameplay written as a WASM module.
*Exit: the platform contract exists and something plays.*

**M5 — Simulation.** Physics integration, character controller, audio, input
mapping, animation (skinning, blend trees).
*Exit: a game is buildable.*

**M6 — Tooling.** Full editor (§10.1), integrated profiler, CLI maturity,
documentation.
*Exit: someone other than us can use it.*

**M7+ — Renderer Stages B/C.** GPU-driven pipeline, then frontier features.

---

## 7. What "platform" requires

Tracked explicitly, because it is the difference between an engine and a tech
demo:

- A **stable, versioned extension ABI** (§2.3) with a deprecation policy
- A **content pipeline** third parties can target
- **Tooling** — editor, profiler, CLI
- **Documentation** generated from reflection where possible
- **Semantic versioning** on `slop-abi` independent of engine internals

---

## 8. Open questions

1. **Scene format.** Text format for diffability during development, binary for
   shipping — both derived from reflection. Which text format (RON, TOML,
   custom) is undecided.
2. **Slang Rust bindings.** Which binding crate to use, or whether to vendor and
   maintain our own. Contained behind a narrow interface either way (§2.11).
   **Revisit at M3.**
3. **Enabling pipelining.** §2.9 makes the renderer snapshot-driven, so running
   it a frame behind simulation is now a scheduling toggle rather than an
   architectural question. Whether to enable it is a measurement decision, taken
   against real content around M3.
4. **Runtime UI.** Not scoped (§10.3). Needs a retained-mode or custom design
   and a WASM-facing interface, and is a substantial subsystem in its own right.
   No decision needed before M5.
5. **Debug UI library.** `egui` versus Dear ImGui via FFI. **Decide at M2.**
6. **Networking.** Not scoped. Replication would derive from reflection, which
   is why reflection matters even though networking is far out.
7. **Naming.** "Slop" is a joke that becomes load-bearing if the project gets
   serious. Worth revisiting before any public surface exists.
8. **Which tier of determinism we are buying.** §2.3, §2.7, and §5 each cite
   determinism, for different purposes, and the tiers differ enormously in cost.
   *Same-build, same-machine* determinism is nearly free and is sufficient for
   replay, CI, and regression testing. *Cross-platform lockstep* — the tier
   netcode would need — is not free and is not something `rapier` provides:
   its `enhanced-determinism` feature is scoped to identical platform and build.
   Provisional position: commit to same-build determinism only, which still
   justifies every decision currently citing it, and treat cross-platform
   lockstep as out of scope until networking is actually scoped (item 6).
   **Decide before M5.**

   This decision also selects `glam`'s feature set rather than merely being
   affected by it: glam's SIMD paths can yield differing results across CPU
   feature levels, and it ships `scalar-math` and `libm` features precisely to
   trade throughput for reproducibility. So the question is never "glam or our
   own math" (§3.2) — it is which glam configuration. Settle both together.

**Resolved:**
- macOS support — dropped, see §2.1.
- Sim/render coupling — immutable render snapshot, see §2.9.
- ECS storage strategy — archetype, see §2.10.
- Shader authoring — Slang, library-integrated, see §2.11.
- Editor architecture — host app embedding the engine, see §2.12.

---

## 9. AI integration

Deliberately downstream. It is an accessory, not a design driver.

The relationship is that **good engine architecture makes AI integration nearly
free, while building for AI first would have made the engine worse.**
Specifically, everything an AI agent would need already exists for other
reasons:

- **Reflection** provides a machine-readable schema of every type — built for
  serialization and tooling (§2.4).
- **The WASM ABI** is already a sandbox for third-party code — built for the mod
  and extension story (§2.3).
- **Deterministic headless mode** with state introspection and frame capture —
  built for CI and regression testing (§5).
- **The CLI** exposes all of the above — built for developer workflow.

Given those, an MCP server or equivalent integration is a thin shim over
existing surfaces. Realistically a weekend of work, appropriate around M4. No
architectural commitment is made for it now.

---

## 10. Editor and user interface

Three distinct problems that are routinely conflated. They share almost nothing
architecturally, and building one system to serve all three produces something
clumsy as a tool and bloated as a runtime.

### 10.1 The editor — M6

The tool used to build games: scene hierarchy tree, property inspector, asset
browser, 3D viewport with manipulation gizmos, material editor, profiler.
Architecture is settled in §2.12.

Most of its content is *derived* rather than built, which is a direct payoff from
reflection (§2.4):

| Editor feature | Derived from |
|---|---|
| Property inspector | Walk a reflected type, emit a widget per field — works for every component ever written, including third-party WASM ones |
| Scene hierarchy | The ECS hierarchy, rendered as a tree |
| Undo / redo | Reflection diffs |
| Save / load | Scene serialization from M1 |

What remains genuinely bespoke: viewport gizmos, asset browser, material editor,
profiler visualization.

**Scope warning:** editors are enormous — Unity's is plausibly larger than its
runtime. This is where most from-scratch engines stall. It stays at M6, after the
engine can already produce a game.

### 10.2 Debug / developer UI — M2

Overlays on the running engine: frame timing graphs, live-tweakable values,
entity inspectors, render pass visualizers. Small, and disproportionately
valuable early — see the M2 rationale in §6.

**Immediate mode is the right model here.** Retained-mode UI (Qt, the browser
DOM) keeps a persistent widget tree that you mutate and must keep synchronized
with your data. Immediate mode re-declares the entire UI every frame from current
state, so desynchronization is structurally impossible. It costs CPU and makes
complex layout and animation harder, which is an excellent trade for tools and a
poor one for shipping UI. This is why effectively every in-house engine uses Dear
ImGui or egui for tooling.

Library choice — `egui` (pure Rust, simpler integration) versus Dear ImGui via
FFI (more mature docking and node graphs) — is an M2 decision worth researching
at the time rather than guessing now.

### 10.3 Runtime UI — not scoped

The HUD, menus, and inventory screens the *player* sees. Ships inside the game,
must be styleable by whoever builds on the engine, and must be drivable from
WASM gameplay code (§2.3).

Deliberately unscoped. Immediate mode is a poor fit for shipping UI, so this will
not reuse the debug UI layer. See §8.

---

## 11. Glossary

Engine and graphics terminology used in this document, with the nearest
general-software-engineering analogue where one exists.

**ABI (Application Binary Interface)** — the binary-level contract between
compiled modules: struct layouts, calling conventions, symbol names. Rust has no
stable ABI, which is why native plugin hot-loading is unsafe there and why §2.3
uses WASM instead.

**Archetype** — a group of entities sharing the exact same set of component
types, stored together as parallel arrays. See §2.10.

**Bindless** — a GPU resource-access model where shaders index into one large
global array of textures/buffers rather than having resources individually bound
before each draw. Roughly: a global handle table instead of per-call parameter
passing. Essential for GPU-driven rendering.

**BVH (Bounding Volume Hierarchy)** — a spatial tree used to answer "what is
inside this region" quickly. A spatial index, structurally analogous to a B-tree
over 3D space. Used for culling and physics queries.

**Cooking** — converting a source asset (a `.gltf` file, a `.png`) into an
optimized runtime binary format ahead of time. A build step for content. See
§2.8.

**Culling** — discarding objects before drawing them because they cannot be
seen: outside the camera's view (frustum culling) or hidden behind other
geometry (occlusion culling).

**Deferred / forward shading** — two strategies for lighting. *Forward* shades
each object as it is drawn. *Deferred* first writes surface properties into
buffers, then lights the whole screen in one pass. *Clustered forward+* (§4.2,
Stage A) is a modern forward variant that divides the view into 3D cells and
assigns lights to cells, so it scales to thousands of lights while keeping
forward's advantages with transparency and anti-aliasing.

**Descriptor set / pipeline layout** — the declaration of which GPU resources
(textures, buffers, samplers) a shader can access and at which slots. In Vulkan
this must be defined explicitly and must match the shader exactly, or you get
undefined behavior. Deriving it automatically via shader reflection (§2.11)
removes an entire category of desynchronization bug.

**ECS (Entity Component System)** — the engine's data model. Entities are IDs,
components are plain data structs, systems are functions that query over them.
Structurally an in-memory database optimized for cache-friendly iteration. See
§2.10.

**Frustum** — the truncated pyramid of space the camera can see.

**Gizmo** — the interactive handles drawn in an editor viewport for moving,
rotating, and scaling objects with the mouse.

**GI (Global Illumination)** — simulating light that bounces off surfaces rather
than only arriving directly from light sources. Expensive; the main thing that
separates "realistic" from "video game" lighting.

**GPU-driven rendering** — the GPU decides what to draw (culling, LOD selection,
draw-call generation) instead of the CPU building a draw list each frame.
Removes the CPU as the bottleneck at high object counts.

**Immediate mode / retained mode (UI)** — *retained* UI keeps a persistent widget
tree you mutate and must keep in sync with your data (Qt, the browser DOM).
*Immediate* UI rebuilds the entire interface every frame from current state, so
it cannot desynchronize. See §10.2.

**IBL (Image-Based Lighting)** — lighting a scene using a captured environment
image, so objects pick up the color and directionality of their surroundings.

**Interpolated rendering** — because simulation runs at a fixed rate (§2.7) and
rendering runs at whatever the display allows, the renderer draws a blend between
the two most recent simulation states rather than the latest one. Prevents
stuttering.

**LOD (Level of Detail)** — swapping in simpler geometry for distant objects.

**Meshlet** — a small cluster of triangles (typically 64–128) treated as an
atomic unit for culling and rendering. The granularity that makes GPU-driven
geometry pipelines (and Nanite-style virtualized geometry) work.

**PBR (Physically Based Rendering)** — a material model where surfaces are
described by physical parameters (metallic, roughness, base color) rather than
ad-hoc artistic ones, so they respond correctly under any lighting. The industry
standard.

**Render graph** — a declarative description of the frame's passes and the
resources flowing between them, from which the engine automatically derives
execution order, GPU synchronization barriers, and memory reuse. Conceptually a
DAG-based build system for a frame.

**RHI (Render Hardware Interface)** — the engine's own abstraction over the
graphics API (Vulkan, DX12, Metal). Ours is thin and explicit by design (§2.2).

**Shader** — a program that runs on the GPU. Vertex shaders position geometry,
fragment (pixel) shaders compute surface color, compute shaders do general
parallel work.

**Shader permutation** — the combinatorial explosion of variants produced when
shaders are configured by preprocessor flags (with/without normal maps, N light
types, 4 quality levels…). A classic engine maintenance disaster; §2.11 avoids it
via Slang's link-time specialization.

**Shader reflection** — inspecting a compiled shader to discover what resources
it expects (which textures, buffers, constants, at which binding slots), so the
engine can construct matching GPU state automatically instead of duplicating the
declarations by hand. §2.11 depends on this.

**Slang** — the shading language this engine uses. A Khronos-governed language
with modules, generics, and interfaces that compiles to SPIR-V and other targets.
See §2.11.

**Sparse set** — an alternative ECS storage layout favoring cheap component
add/remove over iteration speed. Rejected as the default in §2.10.

**SPIR-V** — the binary intermediate representation Vulkan consumes. Shaders are
compiled from a source language (Slang, here) into SPIR-V, which the GPU driver
then compiles to its own machine code. Roughly analogous to LLVM IR or JVM
bytecode.

**Swapchain** — the rotating set of images the GPU renders into and the OS
presents to the display. A double/triple buffer between engine and monitor.

**TAA (Temporal Anti-Aliasing)** — reduces jagged edges by blending information
from previous frames, using motion vectors to know where each pixel was.

**Tonemapping** — converting the renderer's high-dynamic-range output into the
limited range a monitor can display, analogous to exposure and film response in
photography.

**Virtual shadow maps / virtual texturing** — techniques that allocate GPU memory
only for the parts of a very large shadow map or texture actually visible this
frame. Demand paging, applied to GPU resources.

**WIT (WebAssembly Interface Types)** — the interface definition language of the
WASM Component Model. Interfaces are declared once and bindings are generated for
each guest language. Comparable in role to a `.proto` file. See §2.3.
