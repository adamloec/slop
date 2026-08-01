# slop-app

**Last updated:** 2026-08-01

## 1. Purpose

The application layer — main loop, module and plugin wiring, configuration.

This is a **library, not a framework entry point**. The game owns `main()` and
can drive the loop itself (`DESIGN.md` §1.2 principle 4). Runnable targets live
in `examples/`, and the editor embeds this crate exactly as a shipping game does
— it is not a privileged mode inside the engine (`DESIGN.md` §2.12).

## 2. Status

Stub.

| Area | State | Milestone |
|---|---|---|
| `logging` — log filter policy, `SLOP_LOG` | Landed | M0 |
| Window creation via `winit` | Planned | M0 |
| Main loop wiring sim and render | Planned | M0 |
| Configuration file, CLI arguments | Planned | M2 |
| Module and plugin wiring | Planned | M4 |

## 3. The three consumers

The same crate, embedded three ways. Nothing here may assume which one it is
running under — that assumption is what turns an engine into a framework.

```mermaid
flowchart TD
    app["slop-app"]
    game["shipping game"]
    editor["slop-editor"]
    headless["headless / CI"]

    game --> app
    editor --> app
    headless --> app

    app --> owns["each owns main and drives the loop"]
```

Headless is not a debug convenience. It is what makes deterministic replay,
golden-image tests, and the frame-budget harness possible (`DESIGN.md` §5), so
it is a first-class consumer from M0.

## 4. The only layer that reads configuration

`CONVENTIONS.md` §5.1: engine crates take parameters, and this crate turns
files, environment variables, and command-line arguments into those parameters.
Nothing below here knows a config file exists.

```mermaid
flowchart TD
    sources[("config file, env, CLI")] --> app["slop-app"]
    app -->|"InstanceConfig"| rhi["slop-rhi"]
    app -->|"filter string"| core["slop-core diagnostics"]
    app -->|"DeviceSelection"| rhi

    rhi -.->|"never reads env or files"| sources
    core -.->|"never reads env or files"| sources
```

Three things this buys, none of which survive a shared global:

- A game, the editor, a test harness, and headless CI each configure the same
  engine differently, without any of them fighting over ambient state.
- A crate's behaviour is a function of its arguments, so a test can set up any
  configuration without touching the process environment.
- There is no central `Config` type to become a dependency magnet — each crate
  owns its own, and this crate assembles them.

`logging` is the worked example: `slop-core::diagnostics` takes a filter string
and reads nothing, while `logging::filter_from_env` decides that the filter comes
from `SLOP_LOG`.

## 5. Decisions

| Decision | Where |
|---|---|
| The engine is a library, not a framework | `DESIGN.md` §1.2 principle 4 |
| Editor is a host application, not an engine mode | `DESIGN.md` §2.12 |
| Fixed-timestep sim, interpolated rendering | `DESIGN.md` §2.7 |
| Renderer consumes a snapshot, never live world state | `DESIGN.md` §2.9 |

## 6. Invariants

1. **No hidden global state.** No singleton device, world, or "current app".
   Globals are exactly what make headless mode, multiple editor worlds, and
   deterministic replay impossible.
2. **The main loop is drivable step by step.** A caller must be able to run one
   iteration rather than surrendering the thread to a `run()` that never
   returns — the editor and the test harness both require this.
3. **The renderer receives a snapshot, never the live world** (`DESIGN.md` §2.9).
4. **Headless is a supported path, tested in CI**, not a flag that rots.
5. **Configuration is read here and nowhere below.** A crate reaching for an
   environment variable or a file is a layering violation, not a shortcut — it
   picks up configuration the application never chose.
6. **No central `Config` struct.** It would have to name every crate's types,
   inverting the dependency graph, and every crate would come to depend on it.
   Each crate owns its own; this one assembles them.
