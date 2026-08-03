# slop-render

**Last updated:** 2026-08-03

## 1. Purpose

The renderer — `DESIGN.md` §4. Three things today: the loop that turns a
swapchain into a stream of frames, the graph that derives the barriers between
its passes, and the passes that draw a cooked model into HDR and resolve it.

```
 window size ──► prepare ──► (resize your attachments)
                   │
                   ▼
              render(|frame| …) ──► acquire, record, submit, present
                   │
                   ├──► Graph::add        — declare each pass and what it touches
                   │      ├─ depth prepass  MeshRenderer::draw_depth
                   │      ├─ scene          MeshRenderer::draw       ──► HDR
                   │      └─ tonemap        Tonemap::draw            ──► swapchain
                   ├──► Graph::execute   — every barrier, derived from the above
                   ├──► slop_editor::DebugUi::draw  — still outside the graph
                   └──► Frame::finish               — and what keeps it alive
```

Nothing in that declaration names a barrier. The remaining passes of Stage A —
shadows, cluster build, IBL, the post stack — arrive across `PLAN.md` §9.5
E4–E7, and are added to the same declaration rather than to a call order.

## 2. Status

| Area | State | Milestone |
|---|---|---|
| `FrameRenderer` — acquire, submit, present, frames in flight | Landed | M2 |
| Swapchain recreation, reported so callers can resize with it | Landed | M2 |
| Both examples driven by it, goldens unchanged | Landed | M2 |
| `VertexBinding` — vertex layout from cooked reflection | Landed | M2 |
| `MeshRenderer` — draws a cooked model, one pipeline for every material | Landed — see §10 | M2 |
| Material system — glTF materials, bindless textures, alpha modes | Landed | M2 |
| `Overlay` — the debug UI's Vulkan backend | Moved to `slop-editor` — see §8 | M2 |
| Render graph — passes declaring reads and writes | Landed — see §12 | M3, E3 |
| Compute passes and tracked buffers in the graph | Landed | M3, E4 |
| HDR target and tonemap resolve | Landed | M3, E2 |
| Depth prepass, including the alpha-masked half | Landed — masked half untested, `PLAN.md` §6.1 | M3, E4 |
| Clustered forward+, shadows, IBL, post stack | Planned | M3, E4–E7 |
| The overlay drawing inside the graph | **Absent** — the last caller of `Frame::finish` | M3 |
| `MeshRenderer` decomposition — it is a god object today | Planned — `docs/reviews/2026-08-03.md` item 2 | M3 |
| Automated coverage of the loop itself | **Absent** — see §6 | M3 |

## 3. Key types

| Type | Role |
|---|---|
| `FrameRenderer` | Owns the swapchain and per-frame synchronisation |
| `FrameRendererConfig` | Frames in flight, present mode, acquire timeout |
| `Frame` | What a caller records into: a command buffer, a target, a frame number |
| `VertexBinding` | A pipeline vertex layout derived from a shader's cooked reflection |
| `Target` | The image being drawn to, and the states it enters and leaves in |
| `FrameOutcome` | Whether a frame was presented or skipped |
| `MeshRenderer` | Loads a cooked model and draws every mesh it places |
| `Graph` | The frame's passes, and the barriers derived from what each declares |
| `Imported` / `ImportedBuffer` | A resource the graph tracks but does not own |
| `ImageId` / `BufferId` | Names a tracked resource; separate types because they barrier differently |
| `RenderPass` / `ComputePass` | What one pass reads and writes |
| `HdrTarget` | The floating-point image the scene is drawn into |
| `Tonemap` | The fullscreen pass that resolves it onto the swapchain |

## 4. Why two calls and not one

```rust
if let Some(extent) = renderer.prepare(&surface, window_size)? {
    scene.resize(&allocator, extent)?;     // depth buffer, offscreen targets
}
renderer.render(|frame| scene.record(frame.command, frame.target, frame.number))?;
```

Attachments that must match the target — a depth buffer above all — have to be
resized **before** the frame that uses them is recorded. A single call would
either hide the resize from the caller or hand back a frame already recorded
against a stale attachment, and the symptom is a validation error on the first
frame after every resize.

`invalidate()` is separate again, because a window event and a frame are
different moments. Resize events arrive in bursts while a window is being
dragged; rebuilding a swapchain per event is work thrown away three times over.

There is no `run()` and no trait to implement. `DESIGN.md` §1.2 principle 4 makes
the event loop the application's, where the platform already put it.

## 5. Where this came from, and what did not come with it

This loop existed twice before this crate did, copied between `examples/cube` and
`examples/triangle`. `PLAN.md` §6.1 recorded a third copy as the signal to
extract it; the debug UI would have been that copy.

**It was rewritten against those two, not lifted from them** (`PLAN.md` §9.1).
They are trustworthy about *what the loop must do*, having been debugged into
working against real validation output — swapchain recreation on resize and on a
suboptimal acquire, a command pool reset per in-flight slot, a timeline wait
before touching one, semaphores per swapchain image rather than per frame. All
of that carried over.

Three things did not:

| In the examples | Here | Why |
|---|---|---|
| `Result<(), String>` throughout | `RenderError` | `CONVENTIONS.md` §6: a library owes typed errors. A swapchain needing recreation and a lost device are not the same problem. |
| `const FRAMES_IN_FLIGHT: usize = 2` | `FrameRendererConfig` | A caller's decision. One is useful for debugging, three trades latency for throughput. |
| `present.unwrap_or(graphics)` | `RenderError::NoPresentQueue` | Silently correct where the two families coincide, a spec violation where they do not — a bug that only appears on someone else's GPU. |

The last is the one worth dwelling on. It was not a shortcut anyone took
knowingly; it is what a fallback looks like when the failing case never occurs on
the machine in front of you.

**What says the rewrite is equivalent:** both examples render through this crate,
and neither golden image moved.

## 6. What is not covered

`FrameRenderer` has no automated test, and that is a real gap rather than an
oversight to be quietly carried.

Everything it does needs a surface, a surface needs a window, and a window needs
an event loop — none of which a test harness has. The golden tests render
headlessly and therefore exercise `Scene` and `MeshRenderer`, not this. So the
check is running the examples:

```
SLOP_FRAMES=120 cargo run -p example-cube
SLOP_FRAMES=120 cargo run -p example-triangle
SLOP_FRAMES=120 cargo run -p example-model
```

`MeshRenderer` **is** covered, as of M2: `examples/model/tests/golden.rs` drives
it headlessly against two references — the cube model, which always runs, and
Sponza, which skips by name when it has not been fetched.

Both exit non-zero on failure and run with validation layers on, so this catches
synchronisation mistakes — but it is a command someone has to type. Recorded in
`PLAN.md` §6.1 with the resize path specifically called out: nothing automated
covers it at all, because `SLOP_FRAMES` never resizes the window.

## 7. Invariants

1. **A slot's pool is reset only after its timeline value is reached.** Resetting
   a pool whose buffers are still pending is undefined, and the timeline exists
   for precisely this.
2. **Render-finished semaphores are per swapchain image, never per frame in
   flight.** Present waits on one and there is no way to observe when it is done;
   tying it to the image means `acquire` handing the image back is the same event
   that releases the semaphore.
3. **The acquire semaphore is waited at `COLOR_ATTACHMENT_OUTPUT`**, not at the
   top of the pipe. Vertex work has no reason to wait for an image it never
   touches.
4. **`prepare` runs before `render`, not after.** Attachments must agree with the
   target while it is being recorded.
5. **A zero-sized window leaves the swapchain alone and stays stale.** Zero is
   not a valid extent; minimising a window on Windows produces one.
6. **The device is waited idle before the renderer's fields drop.** Destroying a
   semaphore a pending submission still references is undefined.

## 8. The debug overlay lives in `slop-editor`

`Overlay` was here and moved out in M2. Both halves of the reasoning that put it
here are still true — it takes tessellated triangles, owns no egui context and
does not know what a window is — but "what draws the UI" and "what feeds the UI"
are one concern split across two types, and a renderer crate carrying a UI
backend is a renderer crate with an opinion about UI toolkits.

**`slop-render` now depends on nothing egui-shaped.** The overlay's own design
notes moved with it, to `docs/slop-editor/README.md` §6.

What stayed behind is the part that is genuinely about this crate: `Frame::slot`
exists because a UI writes GPU memory per frame and needs one copy per in-flight
slot, and §9 below is why reflection alone cannot derive that UI's vertex layout.

## 9. What reflection cannot tell you

`VertexBinding::interleaved` derives a layout from a cooked shader, and that is
right whenever the buffer supplies exactly what the shader reads — which is every
attribute the cube has.

The overlay is where it stops being true. egui's vertex packs colour as four
normalized bytes, and the shader reads a `float4`; the hardware converts. So
reflection reports `Float32x4` and the correct buffer format is
`R8G8B8A8_UNORM`. **Deriving the layout from reflection alone would produce
`R32G32B32A32_SFLOAT` and read each vertex at four times the right stride.**

The split is real rather than a gap: reflection is a fact about the shader, and
the buffer format is a decision about memory. Where they coincide, deriving is
right. Where they do not, the caller states the format and reflection is used to
*check* the shader still reads what was assumed — which is what `check_layout`
does. A per-location override on `VertexBinding` is recorded in `PLAN.md` §6.1
for when a second packed layout appears.

## 10. The mesh renderer

`MeshRenderer` loads whatever a cooked `Model` names — meshes, materials, the
textures those materials reference — and draws all of it.

```
 models/sponza.model ──► meshes ──► vertex/index buffers
                    ├──► materials ──► one storage buffer, indexed per draw
                    └──► textures ──► bindless heap slots
```

**One pipeline, however many materials.** A pipeline per material is the shape an
engine grows into by accident, and it costs a bind and a barrier per surface.
Here a material is a row in a storage buffer and a texture is a slot in the heap,
so consecutive draws differ only by two integers in their push constants. That is
also what makes `DESIGN.md` §4.2 stage B reachable: a draw list built on the GPU
has nothing per-draw left to bind.

**Why the material buffer exists at all.** Vulkan guarantees only 128 bytes of
push constants, and a model-view-projection plus a normal matrix is already 112.
The material's colour, factors and four texture indices do not fit. Putting them
in an indexed buffer is what the bindless storage-buffer binding was added for.

**The normal matrix is not the model matrix.** They agree for a rigid transform
and diverge under non-uniform scale, where using the model matrix tilts every
normal — which reads as a lighting bug rather than a transform one. glTF scenes
scale non-uniformly often enough that this is not theoretical, and two tests pin
it: a rotation must come back unchanged, and a 4× stretch along X must *divide*
the normal's X by four.

**`NO_TEXTURE` is `u32::MAX`, not zero.** Zero is a perfectly good heap slot, so
a material defaulting to it would sample whichever texture happened to load
first — a plausible-looking picture rather than an error.

### Why `examples/cube` was not migrated onto it

It would have been the natural demonstration, and it is the wrong one. The cube's
golden image is guarded by a reference approved *before* the content pipeline
existed (§5.4), which is what makes it an oracle rather than a record of what the
code currently does. Rendering it through a different shader changes the pixels
and forces re-approval, and the property is gone for good.

So `examples/model` is the consumer instead, and it knows nothing about what it
draws: point it at a model with `SLOP_MODEL` and it frames the thing from the
bounds of its own geometry.

## 11. What the mesh renderer does not do yet

Recorded here as well as in `PLAN.md` §6.1, because a reader of this crate should
not have to find out by rendering something:

- **Back-face culling is off entirely.** `double_sided` is a per-material
  property and culling is per-pipeline, so honouring it needs two pipelines and a
  sort. Culling everything would erase single-sided foliage; culling nothing
  costs fill rate and hides inverted winding.
- **sRGB is not applied.** Textures upload as `UNORM` and the shader reads raw
  bytes, so an albedo authored in sRGB is displayed too dark.
  `TextureSlot::is_srgb` already records which textures need the transfer;
  applying it belongs with real shading at M3.
- **Alpha blending is not sorted.** `AlphaMode::Mask` works — it is a `discard` —
  and `Blend` currently renders as opaque, because blending without a back-to-
  front sort is worse than not blending.
- **Nothing is culled.** Every instance is drawn every frame. Frustum culling and
  the BVH are `slop-scene`'s at M3.

## 12. The graph, and the one rule that is not obvious

A pass declares what it touches; `execute` derives the barriers. That replaced a
convention — *"only the last writer may transition the target"* — which had
already failed once, because two passes both believed they were last.

The derivation is nearly the obvious one: a resource has a state, the next pass
wants it in some state, and the difference is the barrier. **The exception is
worth knowing about**, because it is invisible in the declaration and cost a
measurement to find:

> Two passes can want a resource in the *same* state and still need a barrier
> between them.

The depth prepass and the forward pass after it are both `DEPTH_ATTACHMENT` —
identical layout, identical stages, identical access. Nothing about the state
changes, and Vulkan orders the two rendering scopes against each other only if
something says to. A graph barriering on change-of-state alone emits nothing.

So `Tracked` carries `written` beside `state`, and the skip needs the previous
use to have been a *read* as well: read-after-read is free, anything after a
write is not. `ImageState::writes` and `BufferState::writes` answer it from the
access mask, and `slop-rhi` has a test walking every named state so the
hand-written list of write flags cannot fall behind them.

**Synchronization validation did not report the missing barrier**, with the
layer on and `validate_sync` enabled. That is the third gap in its coverage this
milestone has measured; `crates/slop-render/tests/compute.rs` records another.
Barriers here rest on reading the code more than the tooling suggests.

What the graph deliberately does not do yet — no topological ordering, no
culling of passes nothing reads, no transient aliasing — is in the module's own
documentation, with the reasoning.
