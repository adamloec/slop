# Considerations

Two kinds of note, kept together because both are things that would otherwise be
re-discovered from scratch:

- **Future ideas and tech worth revisiting.** Not commitments, not scheduled.
- **Debt found by review.** Problems in code that already exists, recorded
  with enough specificity to act on. Distinct from `PLAN.md` §6.1, which is the
  register of things deliberately deferred behind a seam — everything here was
  found rather than chosen.

## Neural Texture Compression (NVIDIA NTC)

Shown at GTC 2026. Trains a small neural network to reconstruct texture detail
at sample time instead of storing it directly. Demoed ~85% VRAM reduction
(6.5GB → 970MB) at comparable quality.

- **Not NVIDIA-exclusive.** Baseline decode runs over standard Vulkan/DX12
  compute and is validated on NVIDIA GTX 1000+, AMD RX 6000+, and Intel Arc A.
  A faster NVIDIA-only path ("Cooperative Vector") exists but needs an
  experimental DX12 SDK and Developer Mode — not shippable yet.
- **Unproven.** SDK public since early 2026; no shipped games use it yet.
- **Real integration cost**, not a drop-in BC7 swap:
  - New cooked texture format (neural weights, not blocks)
  - A decode compute pass in `slop-rhi` — sampling becomes GPU inference, not
    a texel read
  - VRAM savings trade against added per-sample GPU compute — unmeasured
  - Vendor/capability branching for the fast path, which this project has
    otherwise avoided (see BC7's fixed feature-tier decision)

**Verdict:** watch, don't build. Revisit once shipped games validate it and
the fast path is out of preview.

---

# Codebase review — 2026-08-03

Read of the tree at `e9fe35f`, at the M2/M3 boundary. Ordered by what gets more
expensive to fix the longer it waits, not by severity.

**Calibration first, because it changes how the rest should be read.** The Rust
is strong. `slop-ecs/src/column.rs` is disciplined type-erased storage — numbered
invariants on the type, every `unsafe` block justified against one of them, ZSTs
and over-alignment handled deliberately rather than discovered. The note at
`column.rs:85-93`, rejecting a `Vec<Tick>` that *passes* Miri because it rests on
an unguaranteed `Vec` implementation detail, is a higher standard than most
shipped engines hold. The `slop-cook` / `slop-asset` split genuinely makes "no
glTF parser in a shipping build" a property of the dependency graph. The crate
layering is acyclic and the layers are the right ones.

None of what follows is sloppiness. It is one recurring pattern: **a decision
argued carefully once, then not enforced at the boundary where it would have
had to hold.**

One framing to retire. `PLAN.md` §6.1 opens with *"a hack is a shortcut that
makes the right thing harder later, and there are none here."* Items 1 through 4
below are each exactly that. The register's vocabulary — **Replaced** /
**Extended** / **Rebuilt** — makes every row read as scheduling, which is the
one thing that can turn a debt register into a narrative about not having debt.

---

## 1. The RHI is not an abstraction — Vulkan leaks through every layer above it

`slop-rhi/src/lib.rs:75` re-exports `ash::vk` publicly, and callers took it up:

```rust
// slop-render/src/frame.rs:60-64 — a public struct in the renderer
pub struct Target {
    pub image:  vk::Image,
    pub view:   vk::ImageView,
    pub extent: vk::Extent2D,
}
// slop-render/src/mesh.rs:140 — public constructor
pub fn new(..., color_format: vk::Format, depth_format: vk::Format)
// slop-app/src/gpu.rs:164
pub fn extent(&self) -> vk::Extent2D
```

45 `vk::` references in `slop-render`, 21 in `slop-editor`, 37 across `examples/`.

**Why this is the top item.** `DESIGN.md` §2.2 bought the owned RHI — at an
explicitly accepted cost of 8–15k lines before a first triangle — on the promise
that *"a DX12 backend then slots in cleanly."* It cannot. Every consumer above
`slop-rhi`, including all four examples, names Vulkan types in its own public
signatures. Swapping or adding a backend today is a rewrite of every call site,
which is the §1.2 principle 6 test — *refactor or rewrite?* — coming out on the
wrong side.

The conflation to name: **explicit** and **leaking the backend's type system**
are not the same property. §2.2 wanted the first and got the second.

**Why it is cheap now.** `slop-rhi` already owns `ImageState`, `BufferState`,
`Load`, `Blend`, `Filter`, `Wrap`, `PresentMode`. The pattern exists and is
correct; it simply was not applied to `Format`, `Extent2D`, and the raw
image/view handles, which are the ones that escaped. Closing it is mechanical
while there is one renderer and four examples. After M3's render graph, shadow
atlas, and post stack it is every pass.

**Verdict:** fix before M3 starts. Highest leverage item in the tree.

## 2. `MeshRenderer` is a god object, and it is the shape M3 has to demolish

`slop-render/src/mesh.rs` — one type owns the pipeline, the GPU meshes, the
material storage buffer, the bindless texture slots, the sampler, the depth
image and the push-constant size; **also** performs file I/O through the VFS;
**also** opens and records its own render pass with a hardcoded clear colour
(`mesh.rs:484`).

The tell is in the manifest: `slop-render` depends on `slop-asset`. The renderer
reads files. Two locked decisions say it should not — §2.9's snapshot boundary
(the renderer consumes an immutable packet, it does not go fetch state) and the
render graph owning passes and barriers. Today `MeshRenderer` *is* the scene
database, the loader and the pass.

Three concrete defects fall out of that shape rather than being independent bugs:

- **Two-phase init through `Option`.** `materials: Option<Buffer>`,
  `depth: Option<Image>`, and `record()` at `mesh.rs:462` silently returns when
  either is unset. Forgetting `resize()` yields a black screen with no log, no
  error and no panic — the exact "reports success when the thing it guards is
  most broken" failure mode `PLAN.md` §3.1 already learned once from the golden
  tests' skip-on-setup-failure.
- **`load()` is not safe to call twice.** The `materials` vec is local and
  restarts at row 0, while `self.meshes` accumulates across calls holding the
  *previous* call's row indices; `upload_materials` (`mesh.rs:417-418`) then
  replaces `self.materials` and `self.materials_slot` outright. A second model
  silently re-points the first model's meshes at the wrong material rows, or
  past the end of the buffer. The superseded heap slot is never removed, so it
  leaks. Nothing in the signature or the docs says "once."
- **Silent data loss.** `mesh.rs:285-287`: an instance naming a mesh absent from
  `index_of` hits a bare `continue` — no `warn!`, nothing. Missing *textures* at
  least log (`mesh.rs:357`). For a cooked artifact a dangling mesh reference is a
  cooker bug and should be loud.

**Verdict:** the three defects are worth fixing on their own terms now. The
decomposition is M3 work, but note that the material system landing *into* this
type rather than beside it is how it gets worse.

## 3. `examples/cube/src/scene.rs` is a second renderer, not an example

834 lines carrying its own `upload_buffer` and `upload_texture`
(`scene.rs:620,672`) duplicating `mesh.rs:608,647`, its own `vulkan_format`, its
own pipeline creation, bindless heap ownership, hot reload, resize and draw
recording.

Both paths are golden-tested, so both are load-bearing and will drift
independently until M3 absorbs one. `PLAN.md` §6.1 describes this as
"example-grade on purpose, none of it moves" — that is accurate about intent and
does not address the consequence, which is a parallel implementation of the crate
that was extracted specifically to stop parallel implementations.

**Verdict:** resolves itself when the material system lands, *provided* the
material system absorbs it rather than becoming a third copy. Worth stating as
an explicit exit condition on that work.

## 4. The application shell is duplicated four times, and the project's own rule already fired

`window`, `triangle`, `cube` and `model` each carry an `App`/`Renderer` struct
pair, an `impl ApplicationHandler` with `resumed` / `window_event` /
`about_to_wait`, and hand-rolled `SLOP_FRAMES` environment parsing.

`CONVENTIONS.md` §2.3's "third copy is the trigger to extract" is the rule that
correctly produced `FrameRenderer` and `Gpu`. It fired on the frame loop and the
extraction stopped halfway: `slop-app` now owns `gpu`, `window`, `timing` and
`logging` — everything *except* the loop, which is the part actually being
copied. It is at four copies, one more than the trigger.

**Verdict:** small, and it gets larger with every example. `slop-app` is the
right home and already depends on `winit`.

## 5. `slop-editor` is misnamed and mis-scoped

The crate holds the debug UI — overlay, frame timing, inspector — which is
`DESIGN.md` §10.2 and M2. The *editor* is §10.1 and M6, and per §2.12 it is a
host **application** that embeds the engine as a library. §10.2 and §10.1 are
explicitly described as sharing "almost nothing architecturally", and are then
shipped in one crate under the editor's name.

Two costs:

- **Today.** `examples/triangle` — a triangle — links `egui`, `egui-winit`,
  `slop-ecs` and `slop-reflect`. That is precisely the complaint
  `slop-editor/src/lib.rs:7-18` records as the reason for splitting this out of
  `slop-app`, reproduced one layer up.
- **Later.** When the real editor arrives as a binary, its name is taken by a
  library that games link.

**Verdict:** rename to `slop-debug` while it is cheap, leaving `slop-editor`
free for what §2.12 says it is. Optionally gate it behind a feature so a game
opts in.

## 6. `Reflect` has no `Send + Sync` bound, but `Column` asserts both unconditionally

`column.rs:103,110` carry `unsafe impl Send for Column {}` and
`unsafe impl Sync for Column {}`, justified by a comment deferring the claim to
"the level above". No level above enforces it: `Reflect` is bounded only by
`'static` (`slop-reflect/src/lib.rs`), and `World::insert<T: Reflect>` adds no
bound either.

**Verified, not inferred.** A hand-written `Reflect` impl over a type holding an
`Rc` — which is exactly what `column.rs`'s own `Witness` test helper is, and
exactly what a runtime-registered guest type will be — inserted into a `World`
and read by two systems in one batch races the non-atomic refcount across rayon
workers. A probe against `e9fe35f` ended with `strong_count == 5` where 2 was
correct, from entirely safe caller code.

The derive is what masks it today: every `Reflect` type reachable through
`#[derive(Reflect)]` is currently `Send + Sync`, so nothing in-tree trips it. But
`Reflect`'s stated safety contract says nothing about thread safety, so an
implementor who satisfies the contract exactly can still produce UB — which is
the definition of an unsound contract rather than a missing check.

**Verdict:** a supertrait bound on `Reflect`, or a bound at the `Column`/`World`
boundary, is a small change now and an audit of every registration path later.
Worth deciding before the §2.3 guest path makes runtime registration the common
case.

## 7. Smaller, Rust-level

- **`Result<_, String>` across all four examples**, 22 sites.
  `scene.rs:784`'s `cook_first(error: AssetError) -> String` takes a typed error
  and flattens it to prose. `PLAN.md` §6.1 calls this example-grade on purpose,
  which is fair; the counter-argument is that examples are the thing people copy,
  and these are workspace members with 800-line modules.
- **`WorldCell::query` allocates a `Vec` per call** (`system.rs:281-284`) to
  check the access declaration — in the frame loop, which `CONVENTIONS.md` §8
  says allocates nothing. Already on §6.1's register; noting only that the access
  set is a pure function of the system and could be computed once in
  `System::new` rather than per query.
- **Test placement is inconsistent.** Four of thirteen crates have a `tests/`
  directory (`slop-ecs`, `slop-reflect`, `slop-asset`, `slop-rhi`). `slop-render`
  — the newest and most structural crate — has no integration tests and inline
  tests only in `vertex.rs` and `mesh.rs`. `slop-core`, `slop-math`, `slop-cook`
  and `slop-editor` have none. 482 inline tests against 353 in `tests/`.
- **`slop-core` is the misc crate.** Handles, slotmap, arena, jobs, time, rng,
  hash, diagnostics — four or five unrelated reasons to change. Fine at eleven
  files and not worth splitting today; it is the one to watch, and worth
  resisting additions to.
- **`Cargo.toml` carries ~150 lines of prose** for ~30 dependencies. The
  reasoning is good and mostly correct; a manifest is a poor place to keep it,
  because nothing re-reads it and nothing checks it against the code it
  describes.

## 8. The documentation has begun reviewing itself

Roughly 120 KB of design prose against 45k lines of Rust, and it has drifted
from the tree it describes. `PLAN.md` §6.1 — the artifact whose entire job is
keeping "temporary" honest — cites `slop-cli/src/texture_import.rs`,
`slop-cli/src/cook.rs` and `slop-render/src/overlay.rs`. All three moved crates.
`DESIGN.md` §2.11 cites `slop-cli/src/reflection.rs`, now
`slop-cook/src/reflection.rs`.

A register that cannot name the file has stopped being a control.

**Verdict:** the docs are an asset and the reasoning in them is the most valuable
thing in the repository — this is not an argument for fewer of them. It is an
argument that path references in `PLAN.md` §6.1 and `DESIGN.md` are load-bearing
and want checking whenever a crate boundary moves.

---

## Suggested order

1. **The `vk::` leak (item 1).** Cheap today, structural after M3.
2. **`MeshRenderer`'s three defects (item 2).** Small fixes; each is also a
   signal about the decomposition M3 needs.
3. **`Reflect`'s missing bound (item 6).** One line now, an audit later.
4. **App-shell extraction and the `slop-editor` rename (items 4, 5).** An
   afternoon each, and both cheaper the sooner they happen.
5. **`scene.rs` (item 3)** resolves with the material system, if that work takes
   absorbing it as an exit condition.
