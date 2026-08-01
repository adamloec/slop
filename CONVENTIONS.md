# Slop Engine — Code Conventions

**Status:** Draft — pre-implementation
**Last updated:** 2026-07-31

Companion to `DESIGN.md` (architecture) and `PLAN.md` (task breakdown). Those two
answer *what* we are building and *in what order*. This file answers *how the
code is written*, and it is the file to check a diff against.

Two ground rules for the document itself:

- **Every rule states its reason.** A convention nobody understands is a
  convention that gets violated the first time it's inconvenient. Where a rule
  exists to protect a locked decision, it cites the section.
- **Prefer mechanical enforcement over documented intent.** A rule a lint can
  check is worth more than a paragraph here. Where a rule *can* be a lint and
  currently isn't, that is noted as a gap to close, not a rule to trust.

---

## 1. Crate and module layout

**Crates depend only downward**, in the order listed in `DESIGN.md` §4. No
cycles, and no upward reach — `slop-ecs` never knows `slop-render` exists. If a
lower crate seems to need something from a higher one, the dependency is
inverted: the lower crate defines a trait or data type and the higher one
supplies it.

**One concept per module.** Use `foo.rs` alongside a `foo/` directory for
submodules; do not use `mod.rs`. It makes files findable by name in an editor,
which `mod.rs` actively defeats.

**Keep the public surface small and deliberate.** `unreachable_pub` is on, so
anything `pub` is genuinely part of the crate's API. Re-export the intended
surface from the crate root with `pub use` and keep the internal module tree free
to change.

**Each consumer-facing crate exposes a `prelude`** containing the handful of
types a caller cannot avoid. Preludes contain types and traits, never functions,
and never grow to "everything in the crate."

## 2. Naming

Standard Rust naming (RFC 430) with three additions:

**Name for the data, not for the role.** `Manager`, `Handler`, `Helper`,
`Util`, `Info`, `Data`, and `Object` are banned as type-name suffixes. They carry
no information — `TextureManager` tells you nothing that `TextureCache` or
`TextureRegistry` doesn't tell you better. If no precise name suggests itself,
that is usually evidence the type is doing more than one thing.

**No `get_` prefix on accessors.** `transform()`, not `get_transform()`. Reserve
`get` for the fallible-lookup shape that returns `Option`, matching the standard
library's `slice::get`.

**Names of things that cross the ABI are permanent.** Anything appearing in
`slop-abi`'s WIT definitions (`DESIGN.md` §7) is versioned public contract.
Renaming it later is a breaking change for every third party. Spend the extra
minute up front.

## 3. Data-oriented rules

These implement `DESIGN.md` §1.2 principle 2 and are the ones most likely to be
violated by writing ordinary idiomatic Rust without thinking.

**Handles, never pointers, for engine-owned resources** (§2.6). No `Rc`, no
`Arc<Mutex<...>>`, and no `RefCell` inside engine data structures. `Arc` is
acceptable for genuinely shared immutable payloads crossing thread boundaries —
a loaded asset's bytes, for example — but never as a way to model an object graph.

**No trait objects in per-frame paths.** `dyn Trait` costs an indirect call and
blocks inlining. It is fine in tooling, the editor, and asset import, where the
call count is small and flexibility is worth more. In simulation and rendering
inner loops, use enums or generics.

**No `HashMap` in per-frame paths.** Hashing plus a cache miss per lookup, every
frame, is the exact cost the ECS exists to avoid. Use dense arrays indexed by
handle. `HashMap` is fine at load time, in tooling, and for one-off registries.

**Prefer `Vec<T>` and slices to anything graph-shaped.** If a linked or tree
structure seems necessary, model it as indices into a `Vec` — which is also how
it survives serialization (§2.4) without pointer fixups.

**Structure of arrays where it is swept, array of structures where it is
looked up.** Do not apply SoA reflexively; apply it where a hot loop reads one
field across many entities.

## 4. API design

**Explicit over implicit** (§1.2 principle 3). An API that hides a GPU
allocation, a synchronization point, or a file read behind an innocuous-looking
call is a bug factory. Name the cost in the function name where it exists.

**No hidden global state.** No singletons, no `static mut`, no implicit "current
device" or "current world." Context is passed explicitly. This is what makes
headless mode, multiple worlds in the editor, and deterministic replay (§5)
possible at all — each is impossible the moment a global exists.

**Prefer `&mut self` to interior mutability.** If a type needs `RefCell` to
present an immutable API, the API is wrong. The borrow checker is telling you
about real aliasing, and hiding it just moves the failure to runtime.

**Bulk over per-item at every boundary that has a crossing cost.** This is
§2.3's whole thesis and it is not limited to WASM: it applies to GPU uploads,
asset loads, and job dispatch equally. Prefer `fn update_all(&mut self,
items: &[T])` to calling `update` in a loop.

**Builders for anything with more than about four construction parameters,**
especially in `slop-rhi`, where this matches the shape of the underlying Vulkan
structs and keeps call sites readable.

## 5. Errors and panics

Per `PLAN.md` §7:

| Context | Mechanism |
|---|---|
| Library crates | `thiserror`, concrete error enums |
| Application boundaries only | `anyhow` |
| Programmer error / broken invariant | `panic!` |
| Expensive invariant checks | `debug_assert!` |

**Errors are typed enums, never strings.** A caller must be able to match on the
failure and respond differently. Stringly-typed errors force callers to either
ignore the distinction or parse prose.

**No bare `unwrap()` in library code.** Where an invariant genuinely guarantees
success, use `expect()` with a message that states the invariant — "swapchain is
recreated before use" — so a violation reports the broken assumption rather than
a line number. `unwrap()` is fine in tests and examples.

**Panic only for bugs, never for input.** A malformed asset, a missing file, or
a WASM module that misbehaves is expected input and returns `Result`. A violated
internal invariant is a bug and panics. Getting this backwards is what makes an
engine feel fragile: §2.3 explicitly loads untrusted third-party code, and it
must not be able to take the process down through the error path.

## 6. `unsafe`

**Confined to `slop-rhi` and the allocator** (`PLAN.md` §7). Any `unsafe`
appearing elsewhere is a design discussion, not a code review comment.

**Every `unsafe` block carries a `// SAFETY:` comment** stating the invariant
that makes it sound — not what the code does. This is enforced by
`clippy::undocumented_unsafe_blocks`, already enabled workspace-wide, so it is a
build failure rather than a review responsibility.

**Wrap at the lowest level and expose a safe API.** The unsafe surface should be
a thin layer directly over `ash`, not a family of unsafe functions propagating
outward through the crate.

**No `unsafe` for performance without a benchmark.** "This is probably faster"
is not a justification. `DESIGN.md` §5's frame-budget harness is the arbiter.

## 7. Allocation and performance

**No heap allocation in per-frame paths.** Use `slop-core`'s frame arena for
per-frame scratch, and reset it once per frame rather than freeing individually.

**Reserve capacity up front** where the size is known or bounded. Growth
reallocation mid-frame is both a stall and a source of frame-time variance,
which is worse than a slightly higher mean.

**Measure before optimizing, and measure with the budget harness** (§5), not by
eyeballing. Performance claims in commit messages should cite a number.

**Instrument subsystem boundaries with profiling markers** as they are written.
Retrofitting instrumentation is how you end up with an engine that is slow in a
way nobody can localize.

## 8. Concurrency

**Use the job system, never raw threads** (§2.5). A subsystem spawning its own
threads is invisible to the scheduler and competes with it for cores.

**Prefer partitioning to locking.** The archetype design (§2.10) exists partly so
disjoint tables go to disjoint threads with no coordination at all. A lock in a
simulation or rendering inner loop is a design failure; locks are fine in asset
loading and tooling.

**The renderer never reads live simulation state** (§2.9). It consumes the
immutable snapshot. This is the single most load-bearing invariant in the engine
— pipelining, replay, and interpolation all evaporate the moment one subsystem
reaches across — and it is currently enforced only by discipline. Encoding it in
the type system, so the renderer is structurally incapable of holding a reference
to the live world, is worth doing when the snapshot type lands.

**Structural ECS changes go through command buffers** applied at explicit sync
points, never immediately (§2.10).

## 9. Platform portability

Every rule here maps to a row of `DESIGN.md` §2.13's trap table.

- **`PathBuf` / `Path::join` exclusively.** No `\` or `/` literals in paths, no
  string concatenation to build one.
- **Asset paths are lowercase, always**, enforced at cook time. This is the most
  common real-world Windows → Linux breakage.
- **Never write `#[cfg(windows)]` without writing the other arm in the same
  change.** A `cfg` with one arm implemented is a Linux build break scheduled for
  whenever someone next builds on Linux.
- **Never hand-roll platform surface or window code.** `winit`, `ash-window`,
  and `raw-window-handle` exist precisely to absorb this.
- **Text files are LF**, governed by `.gitattributes`. Repo-local
  `core.autocrlf` is `false` so that file is the sole authority.

## 10. Documentation

**Every public item is documented.** Not yet lint-enforced; `missing_docs` should
be turned on per-crate as each crate's API stabilizes.

**Module-level `//!` docs explain why the module exists** and how it fits the
architecture, with a `DESIGN.md` section reference where one applies. The
existing stub `lib.rs` files show the intended register.

**Comment why, not what.** The code says what. Comments carry the reason, the
constraint, and — most valuably — the alternative that was rejected and why.
This project's design docs already do this well; the code should match.

**No commented-out code.** Git remembers.

## 11. Testing

Implements `DESIGN.md` §5, which treats verification as a subsystem rather than
polish. The premise: code can be produced faster than it can be reviewed line by
line, so automated truth is the only thing keeping subtly wrong architecture out.

- **Unit tests colocated** in `#[cfg(test)] mod tests`; integration and
  golden-image tests in `tests/`.
- **Test names state the property**, not the function:
  `stale_handle_does_not_resolve_after_slot_reuse`, not `test_handle`.
- **No test may depend on wall-clock time, thread scheduling, or unseeded RNG.**
  A flaky test is worse than no test — it trains you to ignore red.
- **Every reflected type round-trips automatically** once §2.4 lands. This is
  generated from the type registry, not written per type.
- **Golden images compare by exact match on lavapipe** in CI, with a
  real-hardware lane run separately. See `PLAN.md` §4.1-G.
- **Bugs get a regression test**, written before the fix, so the test is
  demonstrated to fail first.

## 12. Logging and diagnostics

**`tracing`, structured, never `println!`.** Log *fields*, not interpolated
prose: `warn!(asset = %path, "cook failed")` rather than baking the path into the
message. Structured records can be filtered and aggregated; sentences cannot.

**Spans around subsystem work**, so timing and causality are recoverable from a
log alone. Spans are also what make the §5 frame-budget harness and the profiler
integration read naturally rather than requiring parallel instrumentation.

**Levels mean specific things**, or they mean nothing:

| Level | Use for |
|---|---|
| `error` | The operation failed and the user loses something |
| `warn` | Recovered, but a human should know — asset fell back, feature unavailable |
| `info` | Lifecycle events a user would want: device selected, module loaded |
| `debug` | Engine-developer detail, off by default in shipping |
| `trace` | Per-frame or per-item volume |

**Nothing above `debug` fires per-frame.** A log line in the frame loop is a
performance bug and it drowns the signal in the log.

**Log the decision, not just the outcome** — "selected RTX 5090 (discrete) over
Intel UHD 770 (integrated)" is diagnosable from a user's log file; "device
initialized" is not. This applies especially to `slop-rhi` bring-up, where the
first bug reports will arrive as log files from machines we cannot inspect.

## 13. Dependencies

**Adding a dependency requires justifying it against `DESIGN.md` §3's
write/take line.** The question is not "does this crate work" but "does this
subsystem define engine behavior." If it does, we write it.

- **Add with `cargo add`**, never by hand-editing a version guess.
- **Version constraints live in `[workspace.dependencies]`** so every crate
  agrees on one version.
- **Licenses must be MIT, Apache-2.0, or compatible.** Anything copyleft is a
  problem for a platform third parties ship on (§7).
- **Prefer one well-maintained dependency to three convenient ones.** Every
  dependency is a supply chain entry and a build-time cost on both platforms.

## 14. Lints, formatting, and suppressions

**rustfmt is the authority.** Formatting is not a matter of opinion and never a
review topic. Config is stable-only options so it reproduces on the pinned
toolchain.

**Lints are centralized** in the root manifest's `[workspace.lints]` and
inherited by every crate. Add lints there, never per-crate, so the rule is
uniform.

**`#[allow]` requires a comment giving the reason,** and is expected to be rare
and local — on an item, never a whole module or crate. `PLAN.md` §4.2 makes "no
`#[allow]` suppressions hiding real problems" an M0 exit criterion. A suppression
that is genuinely correct is fine; one that silences a lint because fixing it is
inconvenient is a defect with a comment attached.

**CI runs with `-D warnings`.** Warnings do not accumulate, because a codebase
with 200 warnings has zero.

## 15. Commits

- **Imperative mood subject line**, under ~72 characters.
- **The body explains why**, not what — the diff already says what. Rejected
  alternatives are worth a line.
- **Cite the design section** a change implements or amends.
- **A commit that changes a decision updates the doc in the same commit.**
  Decisions that live only in a commit message are decisions that get silently
  reversed six months later.
- Work lands directly on `main`; this project does not use feature branches.
