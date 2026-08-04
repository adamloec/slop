# Slop Engine — Code Conventions

**Status:** In force. Nine crates are written against these rules; a diff that
breaks one is a bug in the diff.
**Last updated:** 2026-08-01

**`DESIGN.md` what, `PLAN.md` when, this file how.** Check diffs against it.

Rules state their reason in one line and then show it. Where a rule could be a
lint and is not yet, it is marked **[gap]** — a rule enforced by prose is a rule
that decays.

**Overriding rule (`DESIGN.md` §1.2 principle 6): no shortcut that has to be
unpicked later.** Defer implementations freely; never defer a seam. If a choice
turns out wrong, a refactor is acceptable and a rewrite is not — so design for
the finished engine, not for this week. Where a rule below is inconvenient, that
is the rule working.

---

## 1. Repository layout

```
slop/
├── crates/                 workspace members — the engine (DESIGN.md §4)
│   ├── slop-math/
│   ├── slop-core/
│   ├── slop-rhi/
│   └── ...                 one directory per crate, added as milestones need them
│
├── shaders/                Slang source — one tree, not per-crate (see below)
│   ├── lib/                shared includes — NEVER cooked standalone
│   │   └── lighting/       grouped by subject, per §2.7
│   ├── passes/             engine entry points, grouped by frame stage
│   │   ├── scene/          everything that produces the lit HDR image
│   │   ├── post/           everything that operates on it afterwards
│   │   └── ui/             what draws over the resolved frame
│   ├── examples/           entry points owned by examples/, not by the engine
│   └── tests/              entry points that exist only to test the RHI
│
├── assets/                 SOURCE assets only, committed. Never cooked output.
│   ├── meshes/
│   ├── textures/
│   └── env/
│
├── examples/               runnable native demos, each its own workspace member
│   └── cube/
│
├── guests/                 WASM guest modules — SEPARATE workspace (see below)
│
├── tests/                  workspace-level integration tests, when one spans crates
│                           (per-crate tests live in crates/<name>/tests/, and a
│                           crate's golden images live in its own tests/golden/)
│
├── tools/                  dev scripts, CI helpers, cook utilities
│
├── docs/                   all documentation except the root README
│   ├── README.md           index, and the conventions these docs follow
│   ├── DESIGN.md           what — architectural decisions
│   ├── PLAN.md             when — milestones and task breakdown
│   ├── CONVENTIONS.md      how — this file
│   ├── architecture.md     cross-crate diagrams
│   └── slop-<name>/        one directory per crate, named exactly as the crate
│       └── README.md       purpose, status, module map, diagrams, invariants
│
├── .slop/                  tool-owned local state — gitignored
│   └── cache/              cooked assets, keyed by content hash (§2.8)
│
├── target/                 cargo output — gitignored
│
├── README.md               landing page only; points at docs/
├── Cargo.toml              workspace root
├── rust-toolchain.toml  rustfmt.toml  clippy.toml  .gitattributes
└── .github/workflows/
```

**Documentation lives under `docs/`, one directory per crate.** The root README
is a landing page and nothing more. Per-crate docs carry what rustdoc cannot:
why a crate is shaped as it is, how its parts relate, and the diagrams. See
[docs/README.md](README.md) for the template every crate document follows and
the diagram conventions — Mermaid only, one diagram per question, consistent
shapes across the docset.

Four of these are decisions rather than obvious defaults:

**Shaders live in one central tree, not beside the crates that use them.**
§2.11 chose Slang specifically for its module system — shared BRDF, lighting,
and struct definitions imported across passes. Scattering shaders across crate
directories fights the thing we picked the language for. They are also cooked
content (§2.8), not Rust source, so they do not belong under `src/`.

`shaders/lib/` is excluded from cooking: those files declare no entry points, so
compiling one standalone fails. Include them by a path from the shader root —
`#include "lib/bindless.slang"` — which resolves because the cook step puts
`shaders/` on the include path, so a shader moving between directories does not
break its includes.

Editing anything in `shaders/lib/` recooks **every** shader. That is deliberately
coarse: the alternative is per-shader dependency tracking, and the failure mode
of getting it wrong is a cache that is *incorrect* rather than merely stale —
an include changes what every dependent compiles to while no dependent's own
source changes, so every stamp still matches.

**Source and cooked assets are physically separate trees.** `assets/` is
committed and human-authored; `.slop/cache/` is generated, content-hash-keyed,
and gitignored. Interleaving them is how a cooked artifact eventually gets
committed and then silently goes stale against its source.

**WIT definitions live at `crates/slop-abi/wit/`,** the `wit-bindgen`
convention, so they sit with the crate that versions them (`DESIGN.md` §7).

**`guests/` is a separate Cargo workspace, excluded from the root one.** WASM
guest modules build for `wasm32-wasip2`, not the host target. Keeping them as
members of the main workspace means every `cargo build --workspace` tries to
build them for the host and every `cargo test` drags them in. This costs nothing
to arrange now and is genuinely annoying to untangle at M4.

## 2. Crate and module layout

Crates depend only downward (`DESIGN.md` §4). No cycles, no upward reach —
invert instead:

```rust
// ✗ slop-ecs must not know slop-render exists
use slop_render::Mesh;
pub fn extract_meshes(&self) -> Vec<Mesh> { .. }

// ✓ the lower crate defines the contract, the higher one satisfies it
pub trait Component: 'static + Send + Sync {}
```

Use `foo.rs` beside a `foo/` directory; never `mod.rs` — it makes every file in
the editor's file switcher read `mod.rs`.

**Coming from a layered web application:** a FastAPI or Spring app puts its
layers in directories — `routes/`, `services/`, `repositories/`, `models/` —
because the language cannot enforce the boundary between them. Cargo can. Here
the layers *are* the crates, the dependency direction is checked by the compiler,
and a cycle is a build error rather than a review comment. So the equivalent
structure lives one level up:

| Layered web app | Slop equivalent | Note |
|---|---|---|
| `models/` | component structs, in whichever crate owns them | no shared model layer; data lives with the system that uses it |
| `repositories/` | `slop-asset` for persistence, `SlotMap` registries for in-memory | one crate, not a folder repeated per feature |
| `services/` | ECS systems | free functions over columns, not stateful service objects |
| `routes/` | `slop-abi` WIT definitions, `slop-cli` | the external surface, versioned independently (§7) |
| `schemas/` | `slop-reflect` | derived from types, never hand-written (§2.4) |
| `middleware/` | render graph passes, job scheduling | |
| `config/` | `slop-app` | |

The consequence: **do not recreate those directories inside a crate.** A
`services/` folder in `slop-ecs` would be grouping by role again, and §2.2 is
about why that fails here.

### 2.1 `lib.rs` is a table of contents

It holds crate docs, module declarations, and the public re-exports. Logic in
`lib.rs` is how a crate starts becoming one file.

```rust
//! One-paragraph statement of what this crate owns, and the DESIGN.md section
//! it implements.

// Private by default. Each is one concept.
mod error;
mod concept_a;
mod concept_b;
mod concept_c;

pub mod prelude;

// The public surface, named explicitly. Never `pub use concept_a::*`, which
// makes the API whatever the module happens to contain today.
pub use concept_a::{TypeOne, TypeTwo};
pub use concept_b::TypeThree;
pub use error::CrateError;
```

Modules are private and the crate re-exports a chosen surface. That keeps the
internal file layout free to change without breaking callers, which is what makes
§2.4's "split it when it grows" cheap rather than a breaking change.

### 2.2 Split by concept, never by kind

Grouping by what a thing *is* rather than what it is *about* means every feature
change touches every file, and each file grows without bound.

```
✗ banned filenames — no domain meaning, unbounded growth
    utils.rs  helpers.rs  common.rs  misc.rs  shared.rs
    types.rs  traits.rs   structs.rs  impls.rs

✓ named for the concept they own
    swapchain.rs  descriptor.rs  barrier.rs  archetype.rs
```

If a helper has no home, it belongs to a concept that has not been named yet.
Name the concept and give it a file.

### 2.3 The standard crate skeleton

Every crate has the same shape. Three filenames are reserved — when a crate
needs one of these things, it goes in the file with that name, never somewhere
else — and the rest is named for whatever the crate owns.

Reserved does not mean mandatory. `lib.rs` always exists; `prelude.rs` appears
once a crate has a public surface worth one; `error.rs` appears once a crate has
a fallible operation. Creating either empty, in anticipation, is worse than not
having it: an empty `error.rs` invites the first error type to be shoved
somewhere convenient instead, and an empty prelude teaches callers that the
prelude is not worth importing.

```
crates/slop-<name>/
├── Cargo.toml
├── src/
│   ├── lib.rs          declarations and re-exports, no logic
│   ├── prelude.rs      the types a caller cannot avoid
│   ├── error.rs        this crate's error enum
│   ├── <concept>.rs    one file per concept the crate owns  ← the bulk
│   ├── <concept>/      only when that concept has real parts
│   └── backend/        FFI seam, only where the crate wraps something external
├── tests/              integration: round-trip, golden, determinism
├── benches/            frame-budget benchmarks (DESIGN.md §5)
└── examples/           runnable demos of this crate in isolation
```

**`backend/` is the one role-shaped directory that legitimately recurs.** Several
crates exist largely to wrap an external library — `slop-rhi` over `ash`,
`slop-host` over `wasmtime`, `slop-physics` over `rapier`, `slop-audio` over
`cpal`. Keeping the FFI seam in one directory means the rest of the crate is
written against our own types, and swapping or vendoring the dependency stays
contained. §2.11 already requires exactly this for the Slang bindings; making it
the general pattern costs nothing.

Everything else is vertical. A concept gets a file; a concept with genuine
internal parts gets a directory of the same name beside it.

**Promote to a directory when both hold:**

1. **Three or more modules share a subject.** One is a part, two is a pair,
   three is a group.
2. **The subject is already a name in the crate** — a type or concept its users
   would recognise. If you have to invent a word to name the directory, the
   grouping is not real and the files belong flat.

```
✗ a directory for a pair, and one whose name had to be invented
    src/surface.rs
    src/presentation/
        swapchain.rs

✓ three parts of a concept that already exists as a type
    src/device.rs          ← the Device type
    src/device/
        physical.rs        ← choosing which device
        features.rs        ← what a device must support
        queues.rs          ← a device's queue families
    src/instance.rs        ← a device's *parent*, not one of its parts
```

Note the shape: `device.rs` is not a façade or an empty re-export file — it is
the concept, and the directory holds the things that exist only to serve it.
Read top to bottom, the file is the answer and the directory is the working.

**Two guardrails:**

- **Never a directory for one or two files.** It adds a path segment and hides
  nothing, which is strictly worse than flat.
- **Keep the top-level list scannable — roughly ten entries.** Past that, go
  looking for a subject to promote. A smell, not a limit, in the sense of §2.4.

Do not create a directory in anticipation of parts that do not exist yet. The
trigger fires on its own soon enough.

### 2.4 Size is a smell, not a rule

There is no line limit — limits produce artificial splitting, which is its own
kind of mess. Use these signals instead:

- **A file past ~500 lines** is a prompt to ask whether it is still one concept.
  Often the answer is yes; `swapchain.rs` legitimately owns recreation, format
  selection, and present modes.
- **A function needing section comments** — `// --- setup ---`, `// --- record
  ---` — is already two functions and is telling you where to cut.
- **A `use` block importing from five sibling modules** means the file is
  coordinating rather than owning something.
- **Struggling to name a file** means its contents have no single reason to
  exist.

Vulkan-specific: every Vulkan object gets its own module with its own
constructor, so the top-level call reads as a summary rather than a script.

```rust
// ✗ one 800-line init() — the classic RHI god function
pub fn init(window: &Window) -> Result<Renderer, RhiError> { /* everything */ }

// ✓ each step owns its module; this function stays readable
pub fn init(window: &Window) -> Result<Renderer, RhiError> {
    let instance = Instance::new(InstanceConfig::default())?;
    let surface = Surface::new(&instance, window)?;
    let physical = physical::select(&instance, &surface, features::required())?;
    let device = Device::new(&instance, physical)?;
    let swapchain = Swapchain::new(&device, &surface, window.inner_size())?;
    Ok(Renderer { instance, surface, device, swapchain })
}
```

### 2.5 Visibility

`unreachable_pub` is on, so `pub` means genuinely public. Use `pub(crate)` for
cross-module internals — the moment you reach for it, you are deciding where the
crate's real seam is, which is the point.

Each consumer-facing crate exposes a prelude of types the caller cannot avoid.
Types and traits only, never functions, and never "everything in the crate":

```rust
// slop-core/src/prelude.rs
pub use crate::{Handle, JobSystem, SlotMap};
```

**Not yet true of most crates, and this is the honest note rather than the
aspiration.** `slop-core` has a prelude; nothing else does. The example above
used to name `slop-ecs/src/prelude.rs` exporting `Component`, `Entity`, `Query`
and `World` — a file that does not exist, exporting a `Component` type that
does not exist either.

That is `docs/reviews/2026-08-03.md` item 8 in a document the item did not audit. It only
counted stale paths in `PLAN.md` and `DESIGN.md`, and this is the worse kind:
not a file that moved, but a convention written in the present tense that no
crate follows. A rule nothing checks and nothing obeys has stopped being a
convention.

Adding the preludes is the fix, and it is deliberately not being done in the
same breath as noticing — the right export set for a crate is a design question
per crate, and `slop-ecs` in particular has no `Component` type to export
because `docs/DESIGN.md` §2.3 makes components any `Reflect` type rather than a
trait to implement.

### 2.6 Test placement

Colocated `#[cfg(test)] mod tests` at the bottom of the file under test. When the
tests outgrow the code — common for `slotmap.rs`, where the interesting cases are
all about reuse and staleness — move them to `slotmap/tests.rs` rather than
letting the file double in size.

### 2.7 The same rule applies to `shaders/`

A shader tree accumulates the way a module tree does — a pass per feature, an
include per subsystem — and it has no compiler telling anyone when a directory
has become a pile. So the rule from §2.2 and the promotion trigger from §2.3 are
in force there too, and this section says what the axes are so that the answer is
not re-argued each time.

**`passes/` groups by frame stage**, which is `docs/PLAN.md` §9.4's own
vocabulary and the same words `Graph::pass_names` reports:

| | |
|---|---|
| `passes/scene/` | Everything producing the lit HDR image — the cluster build, the forward pass, the skybox |
| `passes/post/` | Everything operating on it afterwards — tonemap, SSAO, bloom, TAA |
| `passes/ui/` | What draws over the resolved frame |

**`lib/` groups by subject**, exactly as a crate's modules do — `lib/lighting/`
holds the cluster grid, the environment, point lights and the shadow cascades.
`lib/bindless.slang` deliberately sits at the root: it is the descriptor-heap
ABI, every shader includes it whatever it is about, and putting it in a group
would be claiming it belongs to one.

**A shader owned by an example is not an engine pass.** `examples/` is a
sibling of `passes/`, not a stage within it. The two directories answer different
questions — "which part of the frame is this" versus "who does this belong to" —
and mixing them is how `cube.slang` and `triangle.slang` spent M0 through M3
sitting among the renderer's own passes.

**The trigger is §2.3's.** A stage directory is created when a third file wants
it, not in advance — with one exception that costs nothing and buys real
stability: a stage §9.4 has *already specified* may be created empty-ish, because
the alternative is reorganising the tree in the middle of the feature that fills
it. `post/` holding only `tonemap.slang` today is that case.

Two consequences worth stating, because both are easy to get wrong later:

- **A logical path mirrors the source tree, however deep.** `passes/scene/
  model.slang` cooks to `shaders/passes/scene/model.spv`, and that is the string
  both `Cache::artifact` and `Vfs::read` are handed. Flattening to a basename
  would make two shaders in different stages collide silently.
- **`lib/` is skipped by directory name, at any depth.** The cooker excludes the
  whole subtree rather than the immediate children, so nesting inside it is free.
  The include digest still walks all of it, which is what keeps a shared file's
  edit invalidating every shader that includes it.

## 3. Naming

Standard Rust naming (RFC 430), plus: **name for the data, not the role.** If no
precise name suggests itself, the type is usually doing two things.

```rust
// ✗ role-shaped — carries no information
struct TextureManager;
struct AssetHelper;
struct RenderData;
struct SceneUtils;

// ✓ named for what it holds
struct TextureCache;
struct AssetRegistry;
struct FrameSnapshot;
```

`get` is reserved for fallible lookup, matching `slice::get`:

```rust
// ✗
fn get_transform(&self) -> &Transform;

// ✓ plain accessor
fn transform(&self) -> &Transform;

// ✓ `get` earns its name
fn get(&self, handle: Handle<Node>) -> Option<&Node>;
```

Names in `slop-abi`'s WIT are permanent public contract (`DESIGN.md` §7).
Renaming one later breaks every third party.

## 4. Data-oriented rules

Implements `DESIGN.md` §1.2 principle 2. These are the rules most easily broken
by writing perfectly ordinary idiomatic Rust.

**Handles, never pointers, for engine-owned data** (§2.6):

```rust
// ✗ refcount churn, runtime borrow panics, no serialization path
struct Node {
    parent: Option<Rc<RefCell<Node>>>,
    children: Vec<Rc<RefCell<Node>>>,
}

// ✓ handles into a slotmap; serializes with no pointer fixups
struct Node {
    parent: Option<Handle<Node>>,
    children: Vec<Handle<Node>>,
}
```

`Arc` is fine for shared *immutable* payloads crossing threads — a loaded asset's
bytes — never to model a graph.

**No `HashMap` in per-frame paths.** Hash plus a cache miss per entity, every
frame, is the cost the ECS exists to delete.

```rust
// ✗
for e in &visible {
    let t = self.transforms.get(&e).unwrap();
}

// ✓ dense columns, linear scan
for (t, m) in transforms.iter().zip(meshes.iter()) { .. }
```

**No `dyn Trait` in per-frame paths** — indirect call, no inlining. Fine in
tooling, the editor, and asset import.

```rust
// ✗ virtual call per item, every frame
passes: Vec<Box<dyn RenderPass>>,

// ✓ enum dispatch
enum Pass { Shadow(ShadowPass), Forward(ForwardPass), Post(PostPass) }
```

**SoA where it is swept, AoS where it is looked up.** Do not apply SoA
reflexively.

```rust
// ✗ culling reads only `pos` but drags 36 bytes into cache per particle
struct Particle { pos: Vec3, vel: Vec3, color: [u8; 4], lifetime: f32 }
particles: Vec<Particle>,

// ✓ the swept field is contiguous
struct Particles {
    pos: Vec<Vec3>,
    vel: Vec<Vec3>,
    color: Vec<[u8; 4]>,
    lifetime: Vec<f32>,
}
```

## 5. API design

**No hidden global state.** No singletons, no `static mut`, no implicit "current
device." Globals are what make headless mode, multiple editor worlds, and
deterministic replay (§5) impossible.

```rust
// ✗
static DEVICE: OnceLock<Device> = OnceLock::new();
pub fn create_buffer(size: u64) -> Buffer {
    DEVICE.get().unwrap().alloc(size)
}

// ✓ context is a parameter
pub fn create_buffer(device: &Device, size: u64) -> Result<Buffer, RhiError>;
```

**Name the cost** (§1.2 principle 3). An innocuous call that hides an allocation,
a stall, or a file read is a bug factory.

```rust
// ✗ hides a disk read, a GPU upload, and a stall
fn texture(&self, path: &str) -> Texture;

// ✓
fn load_texture_blocking(&mut self, path: &Path)
    -> Result<Handle<Texture>, AssetError>;
```

**Bulk over per-item at any boundary with a crossing cost** — §2.3's thesis, and
it applies equally to GPU uploads and job dispatch.

```rust
// ✗ a million crossings per frame; this is the failure mode §2.3 exists to avoid
for e in entities {
    host.set_transform(e, t);
}

// ✓ one crossing, columnar
fn write_transforms(&mut self, entities: &[Entity], transforms: &[Transform]);
```

**Prefer `&mut self` to interior mutability.** Needing `RefCell` to present an
immutable API means the API is wrong; the borrow checker is describing real
aliasing.

### 5.1 Configuration flows downward, exactly like dependencies

Three different things get called "configuration" and must not share one
mechanism. Conflating them is what produces the god-`Settings` singleton every
engine regrets.

| Kind | Example | Lives |
|---|---|---|
| **Construction parameters** | `InstanceConfig`, arena capacity, thread count | Beside the type they configure |
| **User settings** | GPU choice, resolution, volume, keybinds | Persisted, player-editable, `slop-app` |
| **Developer knobs** | validation, log filter, forced device | Env and CLI, `slop-app` |

**The rule: engine crates take parameters. Only `slop-app` reads
configuration.**

```rust
// ✗ a library reaching for ambient configuration
pub fn init() {
    let filter = std::env::var("SLOP_LOG").unwrap_or_else(|_| "info".into());
    // ...
}

// ✓ mechanism only; the caller decides
pub fn init(filter: &str) { /* ... */ }
```

No engine crate opens a file or reads an environment variable. `slop-app` reads
them, and constructs each subsystem's parameter struct.

**There is no central `Config` type, ever.** A struct holding every subsystem's
settings would have to name every crate's types, inverting the dependency graph
§2 depends on, and every crate would end up depending on it. The sprawl stays
bounded because it is bounded *by the crate graph*: each crate owns its own
`*Config` next to the thing it configures, and `docs/<crate>/README.md` §4's
key-types table is where you find it.

```rust
// ✗ a dependency magnet that inverts the layering
pub struct Config { rhi: RhiSettings, ecs: EcsSettings, audio: AudioSettings }

// ✓ each crate owns its own, and slop-app assembles them
let instance = Instance::new(&InstanceConfig { validation, ..Default::default() })?;
```

**Reflection (§2.4) is what makes this scale.** Once it lands, config structs are
reflected types, so serialization, the settings UI, the schema, and the
documentation are all derived from one declaration rather than written per
setting.

## 6. Errors and panics

`thiserror` in libraries, `anyhow` only at application boundaries. Errors are
typed enums — a caller must be able to match and respond.

```rust
#[derive(Debug, thiserror::Error)]
pub enum RhiError {
    #[error("no Vulkan device supports the required feature set")]
    NoSuitableDevice,
    #[error("swapchain out of date; recreate before the next frame")]
    SwapchainOutOfDate,
    #[error("device lost during {op}")]
    DeviceLost { op: &'static str },
}
```

**Panic only for bugs, never for input.** §2.3 loads untrusted third-party WASM;
a malformed asset must not be able to take the process down.

```rust
// ✗ untrusted bytes must not panic
pub fn load(bytes: &[u8]) -> Module {
    parse(bytes).expect("valid module")
}

// ✓
pub fn load(bytes: &[u8]) -> Result<Module, ModuleError>;
```

**No bare `unwrap()` in library code.** `expect` states the invariant, so a
violation reports the broken assumption instead of a line number.

```rust
// ✗
let sc = self.swapchain.unwrap();

// ✓
let sc = self.swapchain.expect("swapchain recreated before frame begin");
```

Use `debug_assert!` for invariants too expensive to check in shipping builds.

## 7. `unsafe`

Confined to the places below, and adding another is a design discussion rather
than a review comment:

| Where | Why it is unavoidable |
|---|---|
| `slop-rhi` | Vulkan is a C API |
| The GPU allocator | Raw device memory |
| `slop-ecs`'s storage | Type-erased columns are pointer arithmetic by construction (`DESIGN.md` §2.4) — a `Column<T>` cannot exist for a `T` declared at runtime |
| `slop-reflect`'s `TypeInfo` | Holds a `drop_in_place` function pointer that the ECS calls on erased bytes. `Reflect` is an `unsafe trait` for the same reason |
| `slop-reflect-derive` | Emits the `unsafe impl Reflect` above |
| `slop-core`'s arena | A bump allocator hands out raw memory; this is the CPU-side counterpart of the row above it |
| `slop-app`'s surface creation | `raw-window-handle` cannot express "the window outlives the surface", so the obligation is discharged once here and applications get none of their own |

"`slop-ecs`'s storage" covers `column.rs` and `command.rs`: a command buffer
holds owned component values as bytes plus a destructor, for the same reason a
column does, and that is one place rather than two.

**This table said "three places" until M2, and the tree had seven.** The four
added above were all present and all justified; what was missing was the row.
`docs/reviews/2026-08-03.md` item 10 recorded a raw `queue_submit2` in an example's test
as "the only `unsafe` outside §7's three sanctioned homes" — the block was real
and is now gone, but the claim around it was not, and neither was the rule it
appealed to. A confinement rule that has quietly stopped listing where the code
actually is cannot be used to argue anything, which is `docs/reviews/2026-08-03.md` item
8 applied to this document.

The rule that matters is unchanged: `unsafe` appears where a foreign API or an
erased type makes it unavoidable, never for convenience and never for
performance without a benchmark, and a new home is argued for rather than
noticed afterwards.

Every block carries `// SAFETY:` stating the invariant that makes it sound —
enforced by `clippy::undocumented_unsafe_blocks`, so it fails the build, not
review.

```rust
// ✗ restates the code
// SAFETY: creates a slice from a raw pointer
let data = unsafe { slice::from_raw_parts(ptr, len) };

// ✓ states why it is sound
// SAFETY: `ptr` comes from `allocation.mapped_ptr()`, which stays valid for the
// lifetime of `self.allocation`, and is at least `len` bytes. The transfer that
// last wrote this range completed before the timeline wait above returned, so no
// GPU write aliases this read.
let data = unsafe { slice::from_raw_parts(ptr, len) };
```

Wrap at the lowest level and expose a safe API — the unsafe surface is a thin
layer over `ash`, not a family of unsafe functions propagating outward. No
`unsafe` for performance without a benchmark; §5's budget harness arbitrates.

**A type holding raw pointers states its invariants once, on the type**, and
every `// SAFETY:` comment inside cites them by number. Restating the same three
facts in fifteen blocks is how they drift apart.

**Run Miri on any crate with `unsafe` in it**, and write the tests that reach
those paths:

```
cargo +nightly miri test -p slop-ecs
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p slop-ecs
```

Both aliasing models, because they disagree — Tree Borrows accepts some patterns
Stacked Borrows rejects and is stricter about others, and code that has to hold
under whichever one Rust settles on should be checked against both.

Misaligned access, aliasing violations, deallocating with the wrong layout, and
reading uninitialized memory are all invisible to ordinary tests and usually
invisible on x86 at runtime — `dealloc` with a mismatched alignment simply works
until it doesn't. Miri reports them exactly, with a line number. It only sees
paths a test executes, so it is worth precisely as much as the coverage of the
unsafe code.

**Scale loop counts down under `cfg!(miri)`.** Miri interprets MIR rather than
running compiled code, and tracks initialization, provenance and a borrow stack
for every byte — so one pointer write becomes several bookkeeping operations and
the whole suite runs 50–400× slower. A test that churns two hundred entities is
free natively and minutes there.

What Miri checks is the **paths** a test reaches, not how many times it reaches
them. Eight entities exercise the same `unsafe` as two hundred, so:

```rust
let count = if cfg!(miri) { 8 } else { 200 };
```

Keep the native count meaningful — churn tests exist to shake out ordering bugs
that only appear at volume, and that is a job for the ordinary run. A Miri run
that nobody waits for is a Miri run that stops happening, which costs more than
the coverage the volume would have bought.

## 8. Allocation and performance

No heap allocation in per-frame paths — use the frame arena and reset once per
frame.

```rust
// ✗ allocates and frees every frame
let visible: Vec<Handle<Mesh>> = cull(&scene, &frustum);

// ✓ bump-allocated, freed wholesale at frame end
let visible = frame.arena.alloc_slice_from_iter(cull(&scene, &frustum));
```

Reserve when the size is known — mid-frame reallocation is a stall and a source
of frame-time *variance*, which is worse than a higher mean.

```rust
let mut draws = Vec::with_capacity(snapshot.mesh_count());
```

Measure with the §5 budget harness, not by eye. Performance claims in commit
messages cite a number. Instrument subsystem boundaries as they are written.

## 9. Concurrency

Use the job system, never raw threads (§2.5) — a subsystem spawning its own
threads is invisible to the scheduler and competes with it for cores.

```rust
// ✗
std::thread::spawn(move || bake_lighting(&scene));

// ✓
jobs.spawn(move || bake_lighting(&scene));
```

**Prefer partitioning to locking.** Disjoint archetypes go to disjoint threads
with no coordination (§2.10). A lock in a sim or render inner loop is a design
failure; locks are fine in asset loading and tooling.

**The renderer never reads live simulation state** (§2.9) — the most load-bearing
invariant in the engine. Pipelining, replay, and interpolation all evaporate the
moment one subsystem reaches across.

```rust
// ✗
fn render(&mut self, world: &World);

// ✓ immutable snapshot plus interpolation alpha
fn render(&mut self, snapshot: &RenderSnapshot, alpha: f32);
```

**[gap]** This is enforced only by discipline today. When the snapshot type
lands, make it structurally impossible for the renderer to hold a `&World`.

Structural ECS changes go through command buffers applied at explicit sync
points, never immediately (§2.10).

**Parallel results must not depend on the pool.** `DESIGN.md` §2.14 requires the
same build to produce the same simulation on any machine, and a thread pool's
worker count, task assignment, and completion order all legitimately vary.

```rust
// ✗ float addition is not associative, so the total depends on arrival order
let total = Mutex::new(0.0);
jobs.for_each(&chunks, |chunk| *total.lock() += chunk.energy());

// ✗ the order of arrival is the order of scheduling
let found = Mutex::new(Vec::new());
jobs.for_each(&chunks, |chunk| found.lock().extend(chunk.hits()));

// ✓ write into an indexed slot, reduce in index order on the caller
let mut partials = vec![0.0; chunks.len()];
jobs.for_each_mut(&chunks, &mut partials, |chunk, slot| *slot = chunk.energy());
let total: f32 = partials.iter().sum();
```

Also inside a task: no clock reads, no thread ids, no global counters. All three
vary per run by construction.

## 10. Platform portability

Each rule maps to a row of `DESIGN.md` §2.13's trap table.

```rust
// ✗ breaks on Linux, and silently
let p = format!("{}/textures/{}", root, name);

// ✓
let p = root.join("textures").join(name);
```

Never write one arm of a `cfg` without the other in the same change — a
half-written `cfg` is a Linux break scheduled for later.

```rust
// ✗
#[cfg(windows)]
fn create_surface(..) -> Surface { .. }
// (no unix arm — compiles here, fails there)

// ✓ better still: no cfg at all
ash_window::create_surface(&entry, &instance, display_handle, window_handle, None)
```

Asset paths are lowercase, enforced at cook time. Text files are LF, governed by
`.gitattributes`, with repo-local `core.autocrlf = false`.

### 10.1 Determinism

`DESIGN.md` §2.14: the same build produces the same simulation on any machine,
on Windows and on Linux. Three `std` defaults break that, all of them silently,
and none of them at the moment the mistake is made.

```rust
// ✗ platform C library; the Windows CRT and glibc disagree in the last bit
let offset = radius * angle.sin();

// ✓ the `libm` crate — same Rust source on every target
let offset = radius * slop_math::scalar::sin(angle);
```

```rust
// ✗ seeds from the OS; one call anywhere ends determinism for good
let jitter = rand::thread_rng().gen_range(-1.0..1.0);

// ✓ explicit seed, explicitly passed, no thread-local to reach for
let jitter = rng.range_f32(-1.0, 1.0);
```

```rust
// ✗ `RandomState` reseeds per process, so this iterates differently every run
let mut systems: HashMap<TypeId, System> = HashMap::new();

// ✓ fixed seed, so the same insertions give the same iteration order
let mut systems: FxHashMap<TypeId, System> = FxHashMap::default();
```

The first two are enforced by `clippy.toml`'s `disallowed-methods`; the third is
not mechanically checkable and rests on review.

`sqrt`, `abs`, `floor`, `ceil`, `round`, `trunc` and `mul_add` are exactly
specified by IEEE-754 and need no wrapper — use the `std` ones.

`FxHashMap` makes iteration *reproducible*, not *ordered*. Anything needing a
defined order — a serialization format, a content hash, a list shown to a user —
sorts, or uses a `BTreeMap`.

None of this applies outside the simulation. Tools, tests, importers, and the
editor may use whatever they like; determinism is a property of what runs the
game, not of everything in the repository.

## 11. Documentation

Module-level `//!` explains why the module exists and cites the design section.
Every public item documented — **[gap]**, enable `missing_docs` per crate as each
API stabilizes.

**Comment why, not what.** The code says what. Comments carry the reason, the
constraint, and most valuably the rejected alternative.

```rust
// ✗
// increment the generation
slot.generation = slot.generation.saturating_add(1);

// ✓
// Bump on free rather than on allocate: a handle captured before this call must
// stop resolving immediately, even if the slot is never handed out again.
slot.generation = slot.generation.saturating_add(1);
```

No commented-out code. Git remembers.

## 12. Testing

Implements §5. Unit tests colocated in `#[cfg(test)] mod tests`; integration and
golden-image tests in `tests/`.

```rust
// ✗ names the function
#[test]
fn test_handle() { .. }

// ✓ names the property
#[test]
fn stale_handle_does_not_resolve_after_slot_reuse() {
    let mut map = SlotMap::new();
    let a = map.insert(Node::default());
    map.remove(a);
    let b = map.insert(Node::default());

    assert_eq!(a.index(), b.index(), "slot should be reused");
    assert!(map.get(a).is_none(), "stale handle must not resolve");
}
```

No test may depend on wall-clock time, thread scheduling, or unseeded RNG — a
flaky test trains you to ignore red.

```rust
// ✗
let start = Instant::now();
assert!(start.elapsed() < Duration::from_millis(5));

// ✓
let mut rng = Rng::seed_from_u64(0x5109);
```

Bugs get a regression test written *before* the fix, so it is demonstrated to
fail first. Reflected types round-trip automatically once §2.4 lands — generated
from the registry, not written per type.

## 13. Logging and diagnostics

`tracing`, structured, never `println!`. Log fields, not prose — records can be
filtered and aggregated; sentences cannot.

```rust
// ✗
info!("loaded {} in {}ms", path.display(), ms);

// ✓
info!(asset = %path.display(), duration_ms = ms, "asset loaded");
```

**Log the decision, not just the outcome.** The first bug reports arrive as log
files from machines you cannot inspect.

```rust
// ✗
info!("device initialized");

// ✓ diagnosable by a stranger
info!(
    device = %props.device_name,
    kind = ?props.device_type,
    vram_mb = vram >> 20,
    "selected physical device"
);
```

| Level | Use for |
|---|---|
| `error` | Failed, and the user loses something |
| `warn` | Recovered, but a human should know |
| `info` | Lifecycle: device selected, module loaded |
| `debug` | Engine-developer detail, off in shipping |
| `trace` | Per-frame or per-item volume |

**Log teardown, not only construction.** A log that simply stops is
indistinguishable from a crash, and "did it shut down cleanly?" is the first
question asked of a log from a machine you cannot inspect. Construction without
destruction also hides leaks completely.

```rust
// ✓ the application's own lifecycle
info!(frames = self.frame_counter, "shutting down");

// ✓ engine internals, at debug — an application already knows it is exiting
debug!(images = self.images.len(), "destroying swapchain");
```

Nothing above `debug` fires per frame — a log line in the frame loop is a perf
bug that also drowns the signal. Use a span:

```rust
let _span = trace_span!("cull", entities = scene.len()).entered();
```

## 14. Dependencies

Adding one requires justifying it against `DESIGN.md` §3's write/take line. The
question is not "does this crate work" but "does this subsystem define engine
behavior." If it does, we write it.

Add with `cargo add`, never a hand-typed version guess. Versions live in one
place:

```toml
# workspace Cargo.toml
[workspace.dependencies]
ash = "0.38"

# crates/slop-rhi/Cargo.toml
[dependencies]
ash = { workspace = true }
```

Licenses must be MIT, Apache-2.0, or compatible — copyleft is a problem for a
platform third parties ship on (§7). Prefer one well-maintained dependency to
three convenient ones; each is a supply-chain entry and a build cost on both
platforms.

## 15. Lints, formatting, suppressions

rustfmt is the authority — formatting is never a review topic. Lints are
centralized in the root manifest's `[workspace.lints]` and inherited, never added
per crate. CI runs `-D warnings`: a codebase with 200 warnings has zero.

`#[allow]` is local, on an item, and carries its reason.

```rust
// ✗ blanket, unexplained, hides future problems
#![allow(clippy::too_many_arguments)]

// ✓
// Mirrors VkGraphicsPipelineCreateInfo field-for-field; grouping these into
// structs would obscure the mapping to the Vulkan spec.
#[allow(clippy::too_many_arguments)]
fn create_graphics_pipeline(..) -> Result<Pipeline, RhiError> { .. }
```

`PLAN.md` §4.2 makes "no `#[allow]` hiding real problems" an M0 exit criterion.

## 16. Commits

Imperative subject under ~72 chars. The body explains *why* — the diff says what.
Cite the design section, and note rejected alternatives.

```
Select physical device by score, not enumeration order

Both a discrete 5090 and an integrated UHD 770 enumerate on the dev
machine, and index 0 is not deterministic across driver versions.

Scores on device type, then VRAM. Rejected preferring the device with
the most queue families -- it correlates poorly with performance and
would pick the iGPU on some Intel configurations.

DESIGN.md 2.13, PLAN.md 2.2.
```

A commit that changes a decision updates the doc in the same commit. Decisions
living only in commit messages get silently reversed six months later. Work lands
directly on `main`; no feature branches.
