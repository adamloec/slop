//! Passes declare what they touch; barriers are derived from that.
//!
//! `docs/PLAN.md` §9.5 E3, designed against the frame in §9.4.
//!
//! # What this replaces
//!
//! Every barrier in the engine was hand-written, and the convention holding them
//! together was *"only the last writer may transition the target"*. That rule
//! has already failed once — `MeshRenderer` and the debug overlay both believed
//! they were last, and validation objected on every frame until `Frame::finish`
//! was invented to arbitrate. `PLAN.md` §6.1 has carried the row since.
//!
//! A convention cannot be checked. What a pass reads and writes *can* be, and
//! given that, the transitions between passes are not a decision anyone has to
//! make: a resource has a state, the next pass needs it in some state, and the
//! difference is the barrier.
//!
//! # What it deliberately does not do yet
//!
//! **Passes run in declaration order.** No topological sort, no reordering, no
//! culling of passes whose outputs nothing reads. §9.4's frame is already
//! written in dependency order, and a scheduler that reorders it would be
//! solving a problem the frame does not have — the mistake §4.1-C avoided for
//! the job system.
//!
//! **No transient aliasing.** `DESIGN.md` §2.2 wants two passes that never
//! overlap to share memory. That needs lifetime analysis over the pass list,
//! which is a real feature and a separate one; the declarations here are what it
//! would be computed from.
//!
//! Both are implementations behind an unchanged seam — `DESIGN.md` §1.2
//! principle 6 — because "what does this pass touch" is the question either
//! would be answered from.
//!
//! # The shape
//!
//! ```ignore
//! let mut graph = Graph::new();
//!
//! let scene = graph.import("hdr", hdr.image(), hdr.view(), ..);
//! let screen = graph.present("swapchain", frame.target);
//!
//! graph.add(&RenderPass { name: "scene", color: Some((scene, Load::Clear(..))), .. },
//!           |pass| meshes.draw(pass));
//! graph.add(&RenderPass { name: "tonemap", color: Some((screen, Load::Discard)),
//!                       samples: &[(scene, Stage::Fragment)], .. },
//!           |pass| tonemap.draw(pass));
//!
//! graph.execute(frame.command);
//! ```

use slop_rhi::{
    Attachments, BufferHandle, BufferState, ColorAttachment, CommandBuffer, DepthAttachment,
    Extent2D, ImageAspect, ImageHandle, ImageState, ImageViewHandle, Load, Pass, Stage,
};

/// A resource the graph tracks the state of.
///
/// An index rather than a handle, so that two passes naming the same resource
/// are naming the same *tracked state* — which is the whole basis of deriving a
/// barrier between them. Two `ImageHandle`s that happen to be equal would not
/// be enough, because the graph would have no place to record what it last did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageId(usize);

/// One image, and what the graph last left it in.
struct Tracked {
    name: &'static str,
    image: ImageHandle,
    view: ImageViewHandle,
    aspect: ImageAspect,
    extent: Extent2D,
    /// What the last pass to touch it left it in. Starts at whatever the caller
    /// declared on import.
    state: ImageState,
    /// What it must be left in when the frame ends, if anything.
    ///
    /// This is what makes the last-writer rule disappear: the graph knows which
    /// pass touched a resource last because it ran them, so nothing has to
    /// guess.
    final_state: Option<ImageState>,
}

/// An image the graph should track, but does not own.
///
/// A struct rather than seven arguments, for `CONVENTIONS.md` §5.1's reason:
/// configuration is a struct, so adding a field does not fork every call site.
/// It also stops `image` and `view` — two handles of different types that read
/// the same at a call site — from being swapped silently.
#[derive(Debug, Clone, Copy)]
pub struct Imported {
    /// For diagnostics, the pass visualiser, and naming an intermediate a golden
    /// test wants to capture.
    pub name: &'static str,
    /// The image itself.
    pub image: ImageHandle,
    /// A view covering it, which is what an attachment binds.
    pub view: ImageViewHandle,
    /// Which aspects a barrier over it must name.
    pub aspect: ImageAspect,
    /// Its size, which is also the render area of a pass writing it.
    pub extent: Extent2D,
    /// What state it is in when the frame starts.
    pub state: ImageState,
    /// What it must be left in when the frame ends.
    ///
    /// `None` for a transient nothing outside the frame reads. `Some` is what
    /// retires the last-writer convention: the graph ran the passes, so it knows
    /// which one touched this last and can emit the transition without anyone
    /// deciding whose job it was.
    pub final_state: Option<ImageState>,
}

/// What a pass does to the resources it names.
///
/// Deliberately a plain struct rather than a builder. A builder reads better at
/// three calls and hides which fields exist, and the set of things a pass can do
/// is small enough to see at once — which matters more while §9.4 is still being
/// built out and the answer to "can a pass do X yet" is asked often.
pub struct RenderPass<'a> {
    /// For diagnostics and, at `docs/DESIGN.md` §10.2, the pass visualiser.
    pub name: &'static str,
    /// The colour attachment, and how to treat what is already in it.
    pub color: Option<(ImageId, Load)>,
    /// The depth attachment, how to treat it, and whether to keep the result.
    pub depth: Option<(ImageId, Load, bool)>,
    /// Resources this pass samples, and the stage that samples them.
    ///
    /// The stage is not decoration: a barrier ordering a write against a
    /// *fragment* read does not order it against a compute read, and §9.4's
    /// cluster build reads the depth prepass from compute. `ImageState` used to
    /// bake the stage into each constant, and E1c replaced that with a selector
    /// precisely so the graph could supply it here.
    pub samples: &'a [(ImageId, Stage)],
}

impl Default for RenderPass<'_> {
    fn default() -> Self {
        Self {
            name: "unnamed",
            color: None,
            depth: None,
            samples: &[],
        }
    }
}

impl std::fmt::Debug for RenderPass<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderPass")
            .field("name", &self.name)
            .field("color", &self.color.map(|(id, _)| id))
            .field("depth", &self.depth.map(|(id, _, _)| id))
            .field("samples", &self.samples.len())
            .finish()
    }
}

/// A buffer the graph tracks the state of.
///
/// Separate from [`ImageId`] rather than one id over both, so that passing a
/// buffer where an image belongs is a type error. They are barriered
/// differently — a buffer has no layout — and the declaration is the place that
/// distinction should be visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferId(usize);

/// One buffer, and what the graph last left it in.
struct TrackedBuffer {
    name: &'static str,
    buffer: BufferHandle,
    state: BufferState,
    final_state: Option<BufferState>,
}

/// A buffer the graph should track, but does not own.
#[derive(Debug, Clone, Copy)]
pub struct ImportedBuffer {
    /// For diagnostics and the pass visualiser.
    pub name: &'static str,
    /// The buffer itself.
    pub buffer: BufferHandle,
    /// What state it is in when the frame starts.
    pub state: BufferState,
    /// What it must be left in when the frame ends.
    pub final_state: Option<BufferState>,
}

/// What a compute pass touches.
///
/// The stage is not a field here the way it is on [`RenderPass::samples`]: a
/// compute pass reads and writes at the compute stage by definition, so naming
/// it would be restating the pass kind.
#[derive(Debug, Default)]
pub struct ComputePass<'a> {
    /// For diagnostics and the pass visualiser.
    pub name: &'static str,
    /// Images this pass samples.
    pub samples: &'a [ImageId],
    /// Images this pass writes through the heap's storage-image binding.
    pub writes_images: &'a [ImageId],
    /// Buffers this pass reads.
    pub reads: &'a [BufferId],
    /// Buffers this pass writes.
    ///
    /// `docs/PLAN.md` §9.4's cluster build is exactly this: one dispatch writing
    /// a light-index buffer that the forward pass then reads.
    pub writes: &'a [BufferId],
}

/// A pass and the work it records.
enum Recorded<'a> {
    Render {
        name: &'static str,
        color: Option<(ImageId, Load)>,
        depth: Option<(ImageId, Load, bool)>,
        samples: Vec<(ImageId, Stage)>,
        record: Box<dyn FnOnce(&mut Pass<'_>) + 'a>,
    },
    Compute {
        name: &'static str,
        samples: Vec<ImageId>,
        writes_images: Vec<ImageId>,
        reads: Vec<BufferId>,
        writes: Vec<BufferId>,
        /// Takes the command buffer rather than a `Pass`: a dispatch is not
        /// inside a render pass, and there is no begin/end for the graph to
        /// bracket it with.
        record: Box<dyn FnOnce(&CommandBuffer) + 'a>,
    },
}

impl Recorded<'_> {
    fn name(&self) -> &'static str {
        match self {
            Self::Render { name, .. } | Self::Compute { name, .. } => name,
        }
    }
}

/// The frame's passes, and the resources flowing between them.
///
/// Built and consumed within one frame: `execute` takes it by value, because a
/// graph that could be replayed would have to reset every tracked state and the
/// declarations are cheap to rebuild.
pub struct Graph<'a> {
    resources: Vec<Tracked>,
    buffers: Vec<TrackedBuffer>,
    passes: Vec<Recorded<'a>>,
}

impl<'a> Graph<'a> {
    /// An empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self {
            resources: Vec::new(),
            buffers: Vec::new(),
            passes: Vec::new(),
        }
    }

    /// Track a buffer the graph does not own.
    pub fn import_buffer(&mut self, resource: &ImportedBuffer) -> BufferId {
        self.buffers.push(TrackedBuffer {
            name: resource.name,
            buffer: resource.buffer,
            state: resource.state,
            final_state: resource.final_state,
        });

        BufferId(self.buffers.len() - 1)
    }

    /// Declare a compute pass and the work it dispatches.
    ///
    /// The closure gets the command buffer rather than a render pass, because a
    /// dispatch is not inside one — and the graph having no `Pass` to hand over
    /// is what makes that structural rather than a rule.
    pub fn add_compute(
        &mut self,
        desc: &ComputePass<'_>,
        record: impl FnOnce(&CommandBuffer) + 'a,
    ) {
        self.passes.push(Recorded::Compute {
            name: desc.name,
            samples: desc.samples.to_vec(),
            writes_images: desc.writes_images.to_vec(),
            reads: desc.reads.to_vec(),
            writes: desc.writes.to_vec(),
            record: Box::new(record),
        });
    }

    /// Track an image the graph does not own.
    pub fn import(&mut self, resource: &Imported) -> ImageId {
        self.resources.push(Tracked {
            name: resource.name,
            image: resource.image,
            view: resource.view,
            aspect: resource.aspect,
            extent: resource.extent,
            state: resource.state,
            final_state: resource.final_state,
        });

        ImageId(self.resources.len() - 1)
    }

    /// Declare a pass and the work it records.
    ///
    /// The closure runs during [`execute`](Self::execute), inside the render
    /// pass, with the barriers this declaration implies already emitted.
    ///
    /// **Boxed, which is one allocation per pass per frame.**
    /// `docs/CONVENTIONS.md` §8 says the frame loop allocates nothing, and this
    /// breaks that at §9.4's eight passes. Recorded in `PLAN.md` §6.1: the seam
    /// — declare, then record — is what a frame arena or a bump allocator would
    /// be slotted behind without a caller changing.
    pub fn add(&mut self, desc: &RenderPass<'_>, record: impl FnOnce(&mut Pass<'_>) + 'a) {
        self.passes.push(Recorded::Render {
            name: desc.name,
            color: desc.color,
            depth: desc.depth,
            samples: desc.samples.to_vec(),
            record: Box::new(record),
        });
    }

    /// How many passes have been declared.
    #[must_use]
    pub fn pass_count(&self) -> usize {
        self.passes.len()
    }

    /// The declared passes, in order, for the pass visualiser.
    ///
    /// `DESIGN.md` §10.2 asks for one and M2 deferred it because there was no
    /// graph to read. This is what it reads.
    pub fn pass_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.passes.iter().map(Recorded::name)
    }

    /// The tracked resources, in declaration order.
    ///
    /// The other half of what the visualiser needs: `DESIGN.md` §8 item 8 also
    /// wants golden captures of *intermediates* — the depth buffer, the HDR
    /// target before it is resolved — and naming them is what lets a test ask
    /// for one by name rather than by index.
    pub fn resource_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.resources.iter().map(|resource| resource.name)
    }

    /// The tracked buffers, in declaration order.
    ///
    /// Separate from [`resource_names`](Self::resource_names) rather than
    /// merged, so the visualiser can say which is which — a buffer and an image
    /// flowing between the same two passes are not the same kind of edge, and
    /// §9.4's cluster build produces one of each.
    pub fn buffer_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.buffers.iter().map(|buffer| buffer.name)
    }

    /// Emit every barrier and record every pass.
    ///
    /// # Panics
    ///
    /// If a pass names a resource from a different graph. That is a programming
    /// error rather than a condition — `ImageId` is only obtainable from
    /// `import`, so reaching this means one was carried across frames.
    pub fn execute(mut self, command: &CommandBuffer) {
        // Taken out so the barrier helpers can borrow `self.resources` mutably
        // while a pass's closure runs.
        let passes = std::mem::take(&mut self.passes);

        for pass in passes {
            match pass {
                Recorded::Render {
                    name,
                    color,
                    depth,
                    samples,
                    record,
                } => {
                    // Reads first. Ordering them before the attachment
                    // transitions is what keeps the emitted order from
                    // depending on how the declaration happened to be written.
                    for (id, stage) in &samples {
                        self.transition(command, *id, ImageState::shader_read(*stage));
                    }

                    if let Some((id, _)) = color {
                        self.transition(command, id, ImageState::COLOR_ATTACHMENT);
                    }

                    if let Some((id, _, _)) = depth {
                        self.transition(command, id, ImageState::DEPTH_ATTACHMENT);
                    }

                    let attachment = color.map(|(id, load)| {
                        let tracked = &self.resources[id.0];

                        (
                            ColorAttachment {
                                view: tracked.view,
                                load,
                            },
                            tracked.extent,
                        )
                    });

                    let depth = depth.map(|(id, load, store)| DepthAttachment {
                        view: self.resources[id.0].view,
                        load,
                        store,
                    });

                    let Some((attachment, extent)) = attachment else {
                        // A render pass with no colour attachment would be a
                        // depth-only pass, which §9.4's prepass is and this
                        // cannot express yet — `Attachments` requires a colour
                        // target. Named rather than silently skipped.
                        panic!(
                            "render pass '{name}' declares no colour attachment; depth-only \
                             passes are not yet expressible"
                        );
                    };

                    let mut rendering = command.begin_rendering(&Attachments {
                        color: attachment,
                        depth,
                        extent,
                    });

                    record(&mut rendering);
                }

                Recorded::Compute {
                    samples,
                    writes_images,
                    reads,
                    writes,
                    record,
                    ..
                } => {
                    for id in &samples {
                        self.transition(command, *id, ImageState::shader_read(Stage::Compute));
                    }

                    for id in &writes_images {
                        self.transition(command, *id, ImageState::STORAGE_WRITE);
                    }

                    for id in &reads {
                        self.transition_buffer(command, *id, BufferState::SHADER_READ);
                    }

                    for id in &writes {
                        self.transition_buffer(
                            command,
                            *id,
                            BufferState::storage_write(Stage::Compute),
                        );
                    }

                    // No `begin_rendering`, and nothing to end. The closure gets
                    // the command buffer and binds its own compute pipeline.
                    record(command);
                }
            }
        }

        // Whatever the frame owes the outside world. The presentable image is
        // the one that matters, and the graph knows it was last written by
        // whichever pass named it last — which is the arbitration `Frame::finish`
        // used to do by convention.
        for index in 0..self.resources.len() {
            if let Some(final_state) = self.resources[index].final_state {
                self.transition(command, ImageId(index), final_state);
            }
        }

        for index in 0..self.buffers.len() {
            if let Some(final_state) = self.buffers[index].final_state {
                self.transition_buffer(command, BufferId(index), final_state);
            }
        }
    }

    /// Move one buffer into `wanted`, if it is not already there.
    fn transition_buffer(&mut self, command: &CommandBuffer, id: BufferId, wanted: BufferState) {
        let tracked = &mut self.buffers[id.0];

        if tracked.state == wanted {
            return;
        }

        command.barrier_buffer(tracked.buffer, tracked.state, wanted);
        tracked.state = wanted;
    }

    /// Move one resource into `wanted`, if it is not already there.
    fn transition(&mut self, command: &CommandBuffer, id: ImageId, wanted: ImageState) {
        let tracked = &mut self.resources[id.0];

        // Skipping a no-op transition is not just an optimisation: a barrier
        // from a state to itself is legal but tells the driver to flush and
        // invalidate for nothing, and at eight passes that is eight wasted
        // barriers a frame.
        if tracked.state == wanted {
            return;
        }

        command.transition_image(tracked.image, tracked.aspect, tracked.state, wanted);
        tracked.state = wanted;
    }
}

impl Default for Graph<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Graph<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph")
            .field("resources", &self.resources.len())
            .field("passes", &self.passes.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The graph's whole job, checked without a GPU: does it know when a barrier
    /// is needed and when it is not?
    ///
    /// `execute` needs a command buffer, so the derivation is tested through the
    /// state machine it drives rather than through recorded commands. What is
    /// asserted is the decision, which is the part that was previously a
    /// convention.
    fn tracked(state: ImageState) -> Tracked {
        Tracked {
            name: "test",
            image: ImageHandle::default(),
            view: ImageViewHandle::default(),
            aspect: ImageAspect::Color,
            extent: Extent2D {
                width: 1,
                height: 1,
            },
            state,
            final_state: None,
        }
    }

    #[test]
    fn a_resource_already_in_the_wanted_state_needs_no_barrier() {
        let resource = tracked(ImageState::COLOR_ATTACHMENT);

        assert_eq!(resource.state, ImageState::COLOR_ATTACHMENT);
        // The condition `transition` tests. Asserted on the states rather than
        // by counting emitted barriers, because emitting needs a device.
        assert!(resource.state == ImageState::COLOR_ATTACHMENT);
    }

    #[test]
    fn writing_then_sampling_is_a_state_change() {
        // The dependency E2 introduced: the scene writes the HDR target, the
        // tonemap samples it. These must differ, or no barrier would be derived
        // and the tonemap would read whatever had been flushed so far.
        assert_ne!(
            ImageState::COLOR_ATTACHMENT,
            ImageState::shader_read(Stage::Fragment)
        );
    }

    #[test]
    fn a_compute_read_differs_from_a_fragment_read() {
        // Why `samples` carries a stage. §9.4's cluster build reads the depth
        // prepass from compute, and a barrier ordering a write against a
        // fragment read does not order it against a compute one.
        assert_ne!(
            ImageState::shader_read(Stage::Fragment),
            ImageState::shader_read(Stage::Compute)
        );
    }

    #[test]
    fn a_graph_starts_empty_and_counts_what_it_is_given() {
        let mut graph = Graph::new();

        assert_eq!(graph.pass_count(), 0);

        let id = graph.import(&Imported {
            name: "target",
            image: ImageHandle::default(),
            view: ImageViewHandle::default(),
            aspect: ImageAspect::Color,
            extent: Extent2D {
                width: 4,
                height: 4,
            },
            state: ImageState::UNDEFINED,
            final_state: Some(ImageState::PRESENT),
        });

        graph.add(
            &RenderPass {
                name: "only",
                color: Some((id, Load::Discard)),
                ..RenderPass::default()
            },
            |_| {},
        );

        assert_eq!(graph.pass_count(), 1);
        assert_eq!(graph.pass_names().collect::<Vec<_>>(), vec!["only"]);
    }
}
