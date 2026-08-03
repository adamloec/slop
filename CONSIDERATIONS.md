# Considerations

**Future ideas and technology worth revisiting.** Not commitments, not scheduled,
and not authoritative for anything — the holding pen for things that would
otherwise be re-discovered from scratch. Nothing here is a decision until it
moves into `DESIGN.md` or `PLAN.md`.

Two neighbours, so this file stays one thing:

- **`docs/reviews/`** — completed codebase reviews, kept by date. Debt that was
  *found* rather than chosen, and the record of what was done about it. Other
  documents cite those findings by number, so a review is kept rather than
  deleted once it closes.
- **`PLAN.md` §6.1** — the register of things deferred *deliberately, behind a
  seam*. Chosen rather than found, which is the distinction that keeps the two
  apart.

## C# as the gameplay language

The idea: keep the engine in Rust, but let game developers write C# — a Unity
replacement with a familiar language on a faster core. `DESIGN.md` §2.3 currently
puts *all* gameplay and extensions in WASM.

**These are two audiences, not one.** Game code is trusted (it's the developer's
own); marketplace plugins are not. They do not need the same mechanism.

- **C# — the gameplay path.** Embedded .NET, native speed, familiar tooling.
- **WASM — the extension path.** Kept, deliberately narrow: editor tools, asset
  importers, custom cookers. Edit-time and occasional, not per-frame.

Two *peer* gameplay ABIs would be the mistake — every API change lands twice and
one path rots. One large + one small is the shape that works (and is what Unity
does).

**What it takes:**

1. Runtime hosting — embed CoreCLR, load assemblies, call in
2. A C ABI seam — engine functions as `extern "C"` for P/Invoke
3. Binding generation — engine APIs to C# classes, schema from `slop-reflect`
4. Zero-copy marshalling — ECS columns as `Span<T>`, or the perf is gone
5. GC coordination — no collection pauses mid-frame; also a determinism risk
   (§2.14), since a collector schedules itself
6. Build integration — `slop-cli` drives `dotnet build`, cooks assemblies
7. Debugger attach and assembly hot reload — table stakes for the pitch, not small

4, 5 and 7 are the hard ones; 1–3 are mechanical.

**Verdict:** a milestone of its own, not a feature. Decide it before M4 builds the
WASM gameplay ABI, since that scope shrinks a lot if C# takes the gameplay half.

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