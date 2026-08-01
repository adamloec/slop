//! Running systems, in parallel where their declarations allow.
//!
//! `docs/DESIGN.md` §2.5: *systems declare read/write dependencies so the
//! scheduler can auto-parallelize*. This is the consumer §4.1-C deferred
//! `slop-core`'s work-stealing pool for.
//!
//! # Stages, and batches inside them
//!
//! A [`Schedule`] is ordered [`Stage`]s. A stage is systems that may run in any
//! order relative to each other; a stage boundary is a sync point, where every
//! recorded structural change lands.
//!
//! Inside a stage, systems are packed into **batches** by first fit in
//! declaration order: a system joins the first batch holding nothing it
//! conflicts with. Batches run one after another, and everything inside one runs
//! at once.
//!
//! ```text
//! stage "simulate"
//!   ├ batch 0   integrate ‖ animate ‖ tick timers      (disjoint)
//!   ├ batch 1   collide                                (writes Position)
//!   └ sync      every command buffer applies, in order
//! ```
//!
//! Two properties fall out of first fit that are worth relying on:
//!
//! - **Conflicting systems keep their declaration order.** If A and B conflict
//!   and A was added first, A is in an earlier batch. So `Position`-writing
//!   systems run in the order they were written down, which is the only order a
//!   reader could reasonably expect.
//! - **Non-conflicting systems have no order at all** — and cannot observe one,
//!   since observing it would require the shared access that put them in
//!   different batches.
//!
//! # Why stages rather than a dependency graph
//!
//! A graph — run a system the moment its predecessors finish — packs the same
//! systems into less wall-clock time. It is also derived from *exactly* the same
//! information: the access sets already here. So it is a scheduling policy
//! change, not a data model change, and `docs/PLAN.md` §6.1 carries it as such.
//! Bevy made the same move in that order.
//!
//! What a graph additionally needs is a deterministic tie-break, so that command
//! buffers apply in an order fixed by the schedule rather than by which system
//! happened to finish first. Batching gets that for free, which is the other
//! reason to start here.
//!
//! # Determinism
//!
//! `docs/DESIGN.md` §2.14 requires the same build to produce the same results on
//! any machine. Three things make that true here, and all three are structural
//! rather than best-effort:
//!
//! 1. Batching is a pure function of declaration order, so the partition does
//!    not vary per run.
//! 2. Systems that run together cannot observe each other, because sharing any
//!    component with either side mutable is what would have separated them.
//! 3. Command buffers apply in batch order — fixed by 1 — rather than in
//!    completion order.
//!
//! The one thing left to the caller is the contract `slop_core::jobs` states: a
//! system's own result must not depend on how many threads ran the dispatch.

use std::ops::Range;

use slop_core::JobSystem;

use crate::{CommandBuffer, EcsError, System, Tick, World};

/// Systems that may run in any order relative to each other.
///
/// The boundary between stages is a sync point: structural change recorded
/// inside a stage is not visible until the stage ends.
pub struct Stage {
    name: Box<str>,
    systems: Vec<System>,
    /// One per system, indexed by position in `plan.order` rather than by system
    /// index, so each batch's buffers are a contiguous slice. Reused across
    /// frames — a schedule allocates nothing per run.
    buffers: Vec<CommandBuffer>,
    /// `None` when a system has been added since it was last computed.
    plan: Option<Plan>,
}

/// How a stage's systems are packed into batches.
///
/// `order` lists system indices, batch by batch; `batches` slices it. Buffers
/// are indexed by position in `order`, which is what makes a batch's buffers a
/// contiguous `&mut` slice rather than a gather.
#[derive(Debug)]
struct Plan {
    order: Vec<usize>,
    batches: Vec<Range<usize>>,
}

impl Plan {
    /// Pack `systems` by first fit in declaration order.
    fn build(systems: &[System]) -> Self {
        let mut order = Vec::with_capacity(systems.len());
        let mut batches = Vec::new();
        let mut placed = vec![false; systems.len()];
        let mut remaining = systems.len();

        while remaining > 0 {
            let start = order.len();

            for (index, system) in systems.iter().enumerate() {
                if placed[index] {
                    continue;
                }

                // Compared against this batch only. A system conflicting with
                // something in an *earlier* batch is already ordered after it,
                // which is the declaration-order guarantee.
                let fits = !order[start..]
                    .iter()
                    .any(|&placed: &usize| systems[placed].conflicts_with(system));

                if fits {
                    order.push(index);
                    placed[index] = true;
                    remaining -= 1;
                }
            }

            debug_assert!(
                order.len() > start,
                "a system conflicting with nothing already in an empty batch \
                 must fit, so a batch is never empty"
            );

            batches.push(start..order.len());
        }

        Self { order, batches }
    }
}

impl Stage {
    /// An empty stage.
    pub fn new(name: impl Into<Box<str>>) -> Self {
        Self {
            name: name.into(),
            systems: Vec::new(),
            buffers: Vec::new(),
            plan: None,
        }
    }

    /// What this stage is called.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Add a system, which runs after anything already here that it conflicts
    /// with.
    pub fn add(&mut self, system: System) -> &mut Self {
        self.systems.push(system);
        self.buffers.push(CommandBuffer::new());
        self.plan = None;

        self
    }

    /// The systems, in the order they were added.
    pub fn systems(&self) -> &[System] {
        &self.systems
    }

    /// How many systems there are.
    pub fn len(&self) -> usize {
        self.systems.len()
    }

    /// Whether the stage holds no systems.
    pub fn is_empty(&self) -> bool {
        self.systems.is_empty()
    }

    /// The batches this stage runs as, each a list of system indices.
    ///
    /// Exposed because "why is my system not parallel?" is otherwise
    /// unanswerable without a profiler, and the answer is always a declared
    /// conflict.
    pub fn batches(&mut self) -> Vec<Vec<usize>> {
        self.ensure_plan();

        let plan = self.plan.as_ref().expect("just computed");

        plan.batches
            .iter()
            .map(|range| plan.order[range.clone()].to_vec())
            .collect()
    }

    /// Compute the batch plan if a system has been added since the last one.
    fn ensure_plan(&mut self) {
        if self.plan.is_none() {
            self.plan = Some(Plan::build(&self.systems));
        }
    }

    /// Run every system, then apply what they recorded.
    ///
    /// # Errors
    ///
    /// The first error any command buffer produced. Every buffer is applied
    /// regardless, as [`World::apply`].
    fn run(&mut self, world: &mut World, jobs: &JobSystem, this_run: Tick) -> Result<(), EcsError> {
        self.ensure_plan();

        let plan = self.plan.as_ref().expect("just computed");
        let systems = &self.systems;

        // Shared for the whole parallel phase. Nothing here takes `&mut World`;
        // structural change is recorded and applied below, after this borrow
        // has ended.
        let shared: &World = world;
        let mut remaining: &mut [CommandBuffer] = &mut self.buffers;

        for range in &plan.batches {
            let (batch, rest) = remaining.split_at_mut(range.len());
            remaining = rest;

            jobs.scope(|scope| {
                for (slot, commands) in batch.iter_mut().enumerate() {
                    let system = &systems[plan.order[range.start + slot]];

                    scope.spawn(move || {
                        // SAFETY: `WorldCell::new`'s three obligations.
                        //
                        // 1. No `&mut World` exists: `shared` is a shared borrow
                        //    held for this whole phase, and `world` is not
                        //    touched mutably until it ends.
                        // 2. Every system in this batch was placed by `Plan`
                        //    only after being checked against every other system
                        //    already in it, so their declared access sets are
                        //    pairwise non-conflicting — no two name the same
                        //    component with either one mutable. `WorldCell` then
                        //    checks each query against the declaration it was
                        //    given, so a system cannot reach outside it.
                        // 3. No structural change: `WorldCell` exposes none, and
                        //    each system's `commands` is its own buffer, applied
                        //    after this loop.
                        unsafe { system.run(shared, this_run, commands) };
                    });
                }
            });
        }

        // The sync point. In `order`, so the result does not depend on which
        // system finished first.
        let mut first_error = None;
        for slot in &plan.order {
            let outcome = world.apply(&mut self.buffers[*slot]);

            if let Err(error) = outcome {
                first_error.get_or_insert(error);
            }
        }

        for system in &mut self.systems {
            system.mark_run(this_run);
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl std::fmt::Debug for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stage")
            .field("name", &self.name)
            .field("systems", &self.systems.len())
            .finish_non_exhaustive()
    }
}

/// Ordered stages, run once per call.
///
/// Held and passed explicitly. There is no global schedule for the same reason
/// there is no global world (`docs/CONVENTIONS.md` §5) — the editor runs several
/// at once, and headless replay needs to drive one step by step.
#[derive(Debug, Default)]
pub struct Schedule {
    stages: Vec<Stage>,
}

impl Schedule {
    /// A schedule with no stages.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a stage and return it, for adding systems to.
    pub fn add_stage(&mut self, name: impl Into<Box<str>>) -> &mut Stage {
        self.stages.push(Stage::new(name));

        self.stages.last_mut().expect("just pushed")
    }

    /// The stage called `name`, if there is one.
    pub fn stage(&mut self, name: &str) -> Option<&mut Stage> {
        self.stages.iter_mut().find(|stage| stage.name() == name)
    }

    /// The stages, in order.
    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    /// How many systems the whole schedule holds.
    pub fn len(&self) -> usize {
        self.stages.iter().map(Stage::len).sum()
    }

    /// Whether the schedule holds no systems.
    pub fn is_empty(&self) -> bool {
        self.stages.iter().all(Stage::is_empty)
    }

    /// Run every stage in order.
    ///
    /// Advances the world's tick once, so every system stamps its writes with
    /// the same tick and every system sees what ran before it in this call.
    /// Once per run rather than once per system: a system's change-detection
    /// window is "since I last ran", which is already exact, and a tick per
    /// system would only make stamps less comparable across a frame.
    ///
    /// # Errors
    ///
    /// The first error any stage produced. Later stages still run — a command
    /// naming an unregistered component is a wiring bug, and stopping the frame
    /// half way would leave the world in a state no caller could describe.
    pub fn run(&mut self, world: &mut World, jobs: &JobSystem) -> Result<(), EcsError> {
        let this_run = world.advance_tick();

        let mut first_error = None;
        for stage in &mut self.stages {
            if let Err(error) = stage.run(world, jobs, this_run) {
                first_error.get_or_insert(error);
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Access;

    fn system(name: &str, access: Vec<Access>) -> System {
        System::new(name, access, |_world, _commands| {})
    }

    /// The batch each system landed in.
    fn batch_of(stage: &mut Stage) -> Vec<usize> {
        let batches = stage.batches();
        let mut out = vec![0; stage.len()];

        for (number, batch) in batches.iter().enumerate() {
            for &index in batch {
                out[index] = number;
            }
        }

        out
    }

    #[test]
    fn disjoint_systems_share_one_batch() {
        let mut stage = Stage::new("simulate");
        stage.add(system("a", vec![Access::write::<u32>()]));
        stage.add(system("b", vec![Access::write::<f32>()]));
        stage.add(system("c", vec![Access::read::<u64>()]));

        assert_eq!(stage.batches().len(), 1);
    }

    #[test]
    fn conflicting_systems_are_separated() {
        let mut stage = Stage::new("simulate");
        stage.add(system("a", vec![Access::write::<u32>()]));
        stage.add(system("b", vec![Access::read::<u32>()]));

        assert_eq!(batch_of(&mut stage), vec![0, 1]);
    }

    #[test]
    fn conflicting_systems_keep_declaration_order() {
        // The property worth relying on: `Position`-writing systems run in the
        // order they were written down.
        let mut stage = Stage::new("simulate");
        stage.add(system("first", vec![Access::write::<u32>()]));
        stage.add(system("second", vec![Access::write::<u32>()]));
        stage.add(system("third", vec![Access::write::<u32>()]));

        assert_eq!(batch_of(&mut stage), vec![0, 1, 2]);
    }

    #[test]
    fn a_disjoint_system_joins_the_earliest_batch_it_fits() {
        // `c` conflicts with nothing, so it packs into batch 0 alongside `a`
        // rather than waiting for `b`.
        let mut stage = Stage::new("simulate");
        stage.add(system("a", vec![Access::write::<u32>()]));
        stage.add(system("b", vec![Access::write::<u32>()]));
        stage.add(system("c", vec![Access::write::<f32>()]));

        assert_eq!(batch_of(&mut stage), vec![0, 1, 0]);
    }

    #[test]
    fn reads_of_one_component_all_share_a_batch() {
        let mut stage = Stage::new("render extract");
        for name in ["a", "b", "c", "d"] {
            stage.add(system(name, vec![Access::read::<u32>()]));
        }

        assert_eq!(stage.batches().len(), 1);
    }

    #[test]
    fn a_system_declaring_nothing_never_blocks_anything() {
        let mut stage = Stage::new("simulate");
        stage.add(system("a", vec![Access::write::<u32>()]));
        stage.add(system("bare", vec![]));

        assert_eq!(stage.batches().len(), 1);
    }

    #[test]
    fn adding_a_system_recomputes_the_plan() {
        let mut stage = Stage::new("simulate");
        stage.add(system("a", vec![Access::write::<u32>()]));
        assert_eq!(stage.batches().len(), 1);

        stage.add(system("b", vec![Access::write::<u32>()]));

        assert_eq!(stage.batches().len(), 2, "the cached plan was stale");
    }

    #[test]
    fn an_empty_stage_has_no_batches() {
        let mut stage = Stage::new("empty");

        assert!(stage.batches().is_empty());
    }

    #[test]
    fn every_system_is_placed_exactly_once() {
        let mut stage = Stage::new("simulate");
        stage.add(system("a", vec![Access::write::<u32>()]));
        stage.add(system(
            "b",
            vec![Access::write::<u32>(), Access::read::<f32>()],
        ));
        stage.add(system("c", vec![Access::read::<f32>()]));
        stage.add(system("d", vec![Access::write::<f32>()]));
        stage.add(system("e", vec![]));

        let mut seen: Vec<usize> = stage.batches().into_iter().flatten().collect();
        seen.sort_unstable();

        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn batching_is_the_same_every_time_it_is_computed() {
        // §2.14: the partition must not vary per run, or command buffers would
        // apply in a different order on a different machine.
        let build = || {
            let mut stage = Stage::new("simulate");
            stage.add(system("a", vec![Access::write::<u32>()]));
            stage.add(system("b", vec![Access::read::<u32>()]));
            stage.add(system("c", vec![Access::write::<f32>()]));
            stage.add(system("d", vec![Access::write::<u32>()]));
            stage
        };

        assert_eq!(batch_of(&mut build()), batch_of(&mut build()));
    }
}
