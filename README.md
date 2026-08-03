# Slop Engine

The sloppiest game engine. Built on Rust.

A from-scratch 3D engine and platform targeting Windows and Linux desktop, with
an owned Vulkan RHI, archetype ECS, and WebAssembly gameplay modules.

**Status:** pre-alpha. M0, M1 and M2 complete; M3 — the renderer — is next.
Nothing here is usable as an engine yet.

A Sponza-scale glTF scene loads through the cook pipeline and renders with its
own materials, mip chains and normal maps. Underneath it: an archetype ECS with
a work-stealing scheduler and world serialization, runtime reflection with a
derive macro, a content pipeline that cooks glTF, PNG and Slang keyed on content
hash, and a debug overlay that inspects a live entity. 838 tests.

What is *not* there is a render graph, real shading, shadows or post-processing.
That is M3.

## Documentation

Everything lives in [`docs/`](docs/).

| | |
|---|---|
| [DESIGN.md](docs/DESIGN.md) | **What** we are building, and why each decision was made |
| [PLAN.md](docs/PLAN.md) | **When** — milestones, task breakdown, environment |
| [CONVENTIONS.md](docs/CONVENTIONS.md) | **How** code is written |
| [architecture.md](docs/architecture.md) | Cross-crate diagrams — layering, frame flow, data movement |
| [CONSIDERATIONS.md](CONSIDERATIONS.md) | Ideas not committed to, and debt found by review |

Start with `DESIGN.md`, in full, before reading any source.

## Crates

| Crate | Owns | Docs |
|---|---|---|
| `slop-math` | Linear algebra, transforms, portable scalars | [docs](docs/slop-math/) |
| `slop-core` | Handles, storage, arenas, time, jobs, diagnostics | [docs](docs/slop-core/) |
| `slop-reflect` | Runtime type information and the derive macro | [docs](docs/slop-reflect/) |
| `slop-ecs` | Archetype storage, queries, scheduling, serialization | [docs](docs/slop-ecs/) |
| `slop-asset` | VFS, cooked formats, content-hash cache, hot reload | [docs](docs/slop-asset/) |
| `slop-cook` | Source→cooked importers — glTF, PNG, Slang. Never linked by a game | [docs](docs/slop-cook/) |
| `slop-rhi` | Render hardware interface — Vulkan via `ash` | [docs](docs/slop-rhi/) |
| `slop-render` | Frame loop and mesh drawing; the render graph is M3 | [docs](docs/slop-render/) |
| `slop-editor` | Debug overlay, frame timing, entity inspector | [docs](docs/slop-editor/) |
| `slop-app` | Window, device bring-up, timing, configuration | [docs](docs/slop-app/) |
| `slop-cli` | `cook`, `fetch` — a front end over `slop-cook` | — |
| `slop-verify` | Golden images and approval; dev-dependency only | [docs](docs/slop-verify/) |
| `slop-reflect-derive` | `#[derive(Reflect)]`; a proc macro must be its own crate | — |

The remaining crates in `DESIGN.md` §4 — `slop-scene`, `slop-physics`,
`slop-audio`, `slop-abi`, `slop-host` — land at their milestones.

## Building

Requires the Vulkan SDK and, on Windows, the MSVC toolchain. The Rust toolchain
is pinned in `rust-toolchain.toml`.

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

Content is cooked before it can be rendered, and the runtime parses no source
format (`DESIGN.md` §2.8):

```sh
cargo run -p slop-cli -- cook          # glTF, PNG and Slang into .slop/cache
cargo run -p slop-cli -- fetch sponza  # vendored test assets, not committed
```

## Running the examples

Each is a workspace member that owns its own `main()`, which is what
`DESIGN.md` §1.2 principle 4 says a game does.

```sh
cargo run -p example-window     # a window and a device, drawing nothing
cargo run -p example-triangle   # one pipeline, no vertex buffer
cargo run -p example-cube       # a lit textured cube with hot reload
cargo run -p example-model      # a cooked model; Sponza if it has been fetched
```

`SLOP_FRAMES=n` exits after n frames, which is how shutdown is verified without
a human closing a window.

## License

MIT OR Apache-2.0.
