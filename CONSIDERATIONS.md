# Considerations

Future ideas and tech worth revisiting later. Not commitments, not scheduled —
notes so they aren't lost or re-discovered from scratch.

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
