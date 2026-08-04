# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

Slop is a 3D game engine and platform written from scratch in Rust, targeting
Windows and Linux desktop with AAA-adjacent visual fidelity. It owns its render
hardware interface (Vulkan via `ash`), its ECS, its reflection system, and its
asset pipeline. Gameplay and third-party extensions run as WebAssembly modules.

It is a **library, not a framework** — the game owns `main()`. The editor is a
host application that embeds the engine exactly as a game does, not a privileged
mode inside it.

## Read the docs before writing code

Everything authoritative lives in `docs/`. Reasoning is written down once and
cited by section number; do not re-derive or duplicate it.

| Document | Authoritative for |
|---|---|
| `docs/DESIGN.md` | **What** — every architectural decision, with rationale |
| `docs/PLAN.md` | **When** — milestones, task breakdown, environment |
| `docs/CONVENTIONS.md` | **How** — layout, naming, errors, `unsafe`, testing, logging |
| `docs/architecture.md` | Cross-crate diagrams — layering, frame flow, data movement |
| `docs/<crate>/README.md` | Why a crate is shaped as it is, its module map and invariants |
| `CONSIDERATIONS.md` | Ideas not committed to. Nothing here is a decision |
| `docs/reviews/` | Completed codebase reviews, kept by date and cited by number |

`docs/DESIGN.md` §2 is the set of locked decisions. Changing one is a
re-architecture, not a refactor — raise it, do not route around it.
`docs/CONVENTIONS.md` is in force: a diff that breaks a rule there is a bug in
the diff.

## Layout

```
crates/        the engine — one directory per crate, workspace members
shaders/       Slang source. lib/ shared includes, grouped by subject; passes/
               engine entry points, grouped by frame stage (scene/post/ui);
               examples/ and tests/ hold entry points the engine does not own.
               CONVENTIONS.md §2.7 has the axes and the promotion trigger
assets/        SOURCE assets only, committed. Never cooked output
examples/      runnable demos, each its own workspace member owning main()
docs/          all documentation except the root README
.slop/cache/   cooked assets keyed by content hash — gitignored
```

Crates depend only downward, and Cargo enforces it — a cycle is a build error,
not a review comment. `crates/` currently holds `slop-math`, `slop-core`,
`slop-reflect` (+ `-derive`), `slop-ecs`, `slop-asset`, `slop-cook`, `slop-rhi`,
`slop-render`, `slop-editor`, `slop-app`, `slop-cli`, `slop-verify`;
`docs/DESIGN.md` §4 names the rest of the intended graph.

Three edges are load-bearing and easy to break by accident:

- **Nothing but `slop-cli` depends on `slop-cook`.** That is what makes "a
  shipping build never parses a glTF" a property of the dependency graph rather
  than a habit.
- **`slop-app` depends on neither `slop-render` nor `slop-ecs`.**
- **`slop-editor` does not depend on `slop-app`**, and vice versa — an example
  wires them together.

## Commands

```sh
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets      # must be clean under -D warnings
cargo fmt --all --check
cargo doc --workspace --no-deps --all-features
```

Content must be cooked before it renders; the runtime parses no source format:

```sh
cargo run -p slop-cli -- cook           # glTF, PNG and Slang into .slop/cache
cargo run -p slop-cli -- fetch sponza   # vendored test assets, not committed
```

Examples: `cargo run -p example-window | example-triangle | example-cube |
example-model`. `SLOP_FRAMES=n` exits after n frames, which is how shutdown is
verified without a human closing a window — **except `example-window`**, which
draws nothing by design (M0 task E), so its frame counter never advances and it
idles until the window is closed. Do not put it in a batch that waits.

Miri over any crate containing `unsafe`, under both aliasing models:

```sh
cargo +nightly miri test -p slop-ecs
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p slop-ecs
```

Building requires the Vulkan SDK and, on Windows, the MSVC toolchain. The Rust
toolchain is pinned in `rust-toolchain.toml`.

## Invariants that are expensive to restore

These are the ones that cost a rewrite rather than a refactor if broken:

- **The renderer never reads live simulation state.** It consumes an owned,
  immutable snapshot plus an interpolation alpha. Pipelining, deterministic
  replay and interpolated rendering all depend on this.
- **Handles, never pointers, for engine-owned data.** Generational indices into
  slotmaps. `Arc` is for shared immutable payloads crossing threads, never to
  model a graph; `Rc<RefCell<_>>` never.
- **Determinism: same build, any machine, either platform.** No `f32::sin` and
  friends (use `slop_math::scalar`), no OS-seeded RNG (use `slop_core::Rng` with
  an explicit seed), no default-hasher `HashMap` in the simulation (use
  `slop_core::FxHashMap`). The first two are enforced by `clippy.toml`. None of
  this applies to tools, tests, importers or the editor.
- **No hidden global state.** No singletons, no `static mut`, no implicit
  "current device" — context is a parameter. Engine crates take parameter
  structs; only `slop-app` reads environment variables or files. There is no
  central `Config` type, ever.
- **The WASM boundary is columnar and bulk, never per-entity.** This is why ECS
  storage is archetype rather than sparse-set.
- **Shipping builds never parse a source asset.** Cooking is a build step keyed
  on content hash plus importer version.

## Writing code here

- **Split by concept, never by kind.** `utils.rs`, `helpers.rs`, `types.rs`,
  `traits.rs`, `common.rs` and friends are banned filenames. If a helper has no
  home, name the concept it belongs to and give it a file.
- **`lib.rs` is a table of contents** — modules private, public surface named
  explicitly, no `pub use foo::*`, no logic.
- `foo.rs` beside `foo/`; never `mod.rs`. Promote to a directory only when three
  or more modules share a subject that is already a name in the crate.
- **Name for the data, not the role** — `TextureCache`, not `TextureManager`.
  `get` is reserved for fallible lookup.
- **No heap allocation, `HashMap`, or `dyn Trait` in per-frame paths.** Frame
  arena, dense columns, enum dispatch. Fine in tooling, import and the editor.
- `thiserror` in libraries, `anyhow` only in application crates. Panic for bugs,
  never for input. No bare `unwrap()` in library code — `expect` states the
  invariant.
- **`unsafe` is confined** to the homes listed in `docs/CONVENTIONS.md` §7;
  adding one is a design discussion. Every block carries a `// SAFETY:` stating
  why it is sound, not what it does — enforced by clippy.
- **Use the job system, never raw threads**, and prefer partitioning to locking.
  Parallel results must not depend on worker count or completion order.
- `tracing`, structured, never `println!`. Log fields, not prose; log the
  decision, not just the outcome; log teardown as well as construction. Nothing
  above `debug` fires per frame.
- Test names state the property, not the function. No test may depend on
  wall-clock time, thread scheduling, or unseeded RNG. Bugs get a regression test
  written before the fix.
- Comment *why*, not *what*. No commented-out code.
- Add dependencies with `cargo add`, hoisted into `[workspace.dependencies]`,
  justified against `docs/DESIGN.md` §3's write/take line. MIT/Apache-2.0 or
  compatible only.
- Lints are centralized in the root manifest's `[workspace.lints]`, never added
  per crate. `#[allow]` is local, on an item, and carries its reason.

**The overriding rule:** no shortcut that has to be unpicked later. Defer
*implementations* freely; never defer a *seam*. Applied test — if this turns out
wrong, is it a refactor or a rewrite? Refactor is fine.

## Committing

Work lands directly on `main`; no feature branches. Imperative subject under ~72
characters; the body explains *why* and cites the design section, since the diff
already says what. Note rejected alternatives. A commit that changes a decision
updates the document in the same commit — decisions living only in commit
messages get silently reversed six months later.

Performance claims cite a number from the budget harness, not an impression.
