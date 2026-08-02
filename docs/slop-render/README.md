# slop-render

**Last updated:** 2026-08-02

## 1. Purpose

The renderer — `DESIGN.md` §4. Today it owns exactly one thing: the loop that
turns a swapchain into a stream of frames.

```
 window size ──► prepare ──► (resize your attachments)
                   │
                   ▼
              render(|frame| …) ──► acquire, record, submit, present
```

The render graph, materials, and the passes that make up Stage A arrive at M3
(`PLAN.md` §9). This crate exists now because everything M2 still owes — the
debug UI, materials, Sponza — needed a frame loop that was not copy-pasted.

## 2. Status

| Area | State | Milestone |
|---|---|---|
| `FrameRenderer` — acquire, submit, present, frames in flight | Landed | M2 |
| Swapchain recreation, reported so callers can resize with it | Landed | M2 |
| Both examples driven by it, goldens unchanged | Landed | M2 |
| `VertexBinding` — vertex layout from cooked reflection | Landed | M2 |
| `Overlay` — the debug UI's Vulkan backend (§8) | Landed | M2 |
| Render graph — passes declaring reads and writes | Planned | M3 |
| Material system | Planned — its blocker, shader reflection, has landed | M2 |
| Clustered forward+, shadows, IBL, HDR/tonemap | Planned | M3 |
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
| `Overlay` | Draws what an immediate-mode UI tessellated, over a target |

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
an event loop — none of which a test harness has. The cube's golden test renders
headlessly and therefore exercises `Scene`, not this. So the check is running
both examples:

```
SLOP_FRAMES=120 cargo run -p example-cube
SLOP_FRAMES=120 cargo run -p example-triangle
```

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

## 8. The debug overlay

`DESIGN.md` §10.2's overlay, and only its *renderer* half. `Overlay` takes the
triangles egui produced and draws them; it owns no egui context, reads no input,
and does not know what a window is. Those live in the application, where the
platform already put them.

```
 application            slop-render
 ───────────            ───────────
 egui::Context   ──▶  primitives ──▶  Overlay::draw
 egui_winit      ──▶  raw input
```

**egui rather than Dear ImGui**, and `DESIGN.md` §4 had already chosen it for
`slop-editor`. Checked again rather than assumed: egui is at 0.35 and pure Rust,
`imgui-rs` is at 0.12 and behind upstream, and the actively developed
`dear-imgui-rs` is at `0.16.0-alpha.1`. Dear ImGui also means a C++ build, which
`DESIGN.md` §2.13's trap table names as the dependency *most likely to actually
bite us* — the same surface the §2.11 correction had just removed.

**The backend is written rather than taken.** `egui-ash-renderer` exists, and it
brings its own descriptor pool, sampler and pipeline management — all of which
this engine already has in a bindless form a general-purpose backend cannot
assume. Taking it would mean two descriptor models in one frame. What is written
instead is an upload, a pipeline and a draw loop that sets a scissor rectangle;
the font rasterizer and layout engine, which are the genuinely hard parts, are
egui's.

Four things that are not obvious, each of which cost a debugging pass:

- **The overlay opens its own render pass.** A pipeline used inside a pass must
  declare the depth format that pass carries. The overlay wants no depth at all,
  so sharing the scene's pass would depth-test the interface against the cube and
  let geometry occlude the readout describing it.
- **The descriptor set is re-bound with the overlay's own layout.** Two pipeline
  layouts are compatible only if their push constant ranges match as well as
  their set layouts, and the overlay's block is a different size from the
  scene's. Validation catches this; nothing else would.
- **Vertex buffers are per in-flight slot.** `Frame::slot` exists for this. One
  shared buffer is corrupted by the frame still reading it — `FrameRenderer`
  waits for *this* slot before recording, which says nothing about the others.
- **Vertex positions are in points; scissor rectangles are in physical pixels.**
  The shader divides by the screen size *in points*, so a scaled display draws
  the interface at the right size. Dividing by the physical size instead draws
  it at 1/scale while its clip rectangles stay full size, which shaves the left
  edge off every label — invisible at 100% scaling, which is what a headless
  test defaults to.
- **The colour attribute is four bytes in the buffer and a `float4` in the
  shader.** Reflection describes the shader side and cannot see the buffer
  format, so this module states its layout explicitly and checks the shader
  against it. See §9.

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
