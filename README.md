# Slop Engine

The sloppiest game engine. Built on Rust.

A from-scratch 3D engine and platform targeting Windows and Linux desktop, with
an owned Vulkan RHI, archetype ECS, and WebAssembly gameplay modules.

**Status:** pre-alpha. M0 — foundation. Nothing here is usable yet.

## Documentation

Everything lives in [`docs/`](docs/).

| | |
|---|---|
| [DESIGN.md](docs/DESIGN.md) | **What** we are building, and why each decision was made |
| [PLAN.md](docs/PLAN.md) | **When** — milestones, task breakdown, environment |
| [CONVENTIONS.md](docs/CONVENTIONS.md) | **How** code is written |
| [architecture.md](docs/architecture.md) | Cross-crate diagrams — layering, frame flow, data movement |

Start with `DESIGN.md`, in full, before reading any source.

## Crates

| Crate | Owns | Docs |
|---|---|---|
| `slop-math` | Linear algebra and geometry | [docs](docs/slop-math/) |
| `slop-core` | Handles, storage, arenas, time, jobs | [docs](docs/slop-core/) |
| `slop-rhi` | Render hardware interface — Vulkan via `ash` | [docs](docs/slop-rhi/) |
| `slop-app` | Main loop, wiring, configuration | [docs](docs/slop-app/) |

The remaining crates in `DESIGN.md` §4 land at their milestones.

## Building

Requires the Vulkan SDK and, on Windows, the MSVC toolchain. The Rust toolchain
is pinned in `rust-toolchain.toml`.

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

## License

MIT OR Apache-2.0.
