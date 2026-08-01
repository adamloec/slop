//! Task scheduling — `docs/DESIGN.md` §2.5.
//!
//! Parallelism is foundational rather than additive: bolting it onto a
//! single-threaded engine is a rewrite, not an optimization. What must be right
//! from the start is therefore the *shape* of this API — that callers express
//! work as independent tasks over disjoint data, and never assume the calling
//! thread executes it.
//!
//! # The pool, and why it is not ours
//!
//! M0 backed every entry point with [`std::thread::scope`], which spawns OS
//! threads per call. M1 replaced it with a persistent work-stealing pool, which
//! is what §2.5 means by foundational: dispatch is now cheap, so callers may
//! assume tasks are many and small.
//!
//! That pool is `rayon`, held privately. The reason is one line of the problem
//! statement: a persistent pool that supports [`scope`](JobSystem::scope) —
//! tasks borrowing the caller's stack — must push a `'scope` closure into a
//! queue shared with `'static` workers, and erasing that lifetime **has no safe
//! formulation**. Writing our own would put that `unsafe` in the single crate
//! `docs/CONVENTIONS.md` §7 does not sanction it in, and would mean proving a
//! concurrent algorithm whose failure modes ordinary tests do not catch.
//!
//! What that costs, stated plainly: rayon has no fibers (a job cannot yield
//! mid-execution while waiting on a dependency) and no priority or deadline
//! lanes. Both are rewrites of the pool whichever way this went.
//!
//! **The containment rule, which is what keeps the exit cheap:** no `rayon` type
//! appears in any public signature here or anywhere else in the engine. [`Scope`]
//! wraps rayon's exactly as it previously wrapped [`std::thread::Scope`], and
//! the ECS scheduler takes `&JobSystem` rather than a pool. Replacing the
//! backing is this one file.
//!
//! # The determinism contract
//!
//! `docs/DESIGN.md` §2.14 requires the same build to produce the same
//! simulation results on any machine. A thread pool is the easiest way to break
//! that, because the pool's own behaviour is legitimately nondeterministic — how
//! many workers exist, which one picks up which task, and what order tasks
//! finish in all vary per run and per machine.
//!
//! So the contract is on **callers**, and it is one sentence:
//!
//! > The result of a dispatch must not depend on how many threads ran it, which
//! > thread ran what, or the order tasks completed in.
//!
//! Three ways that gets broken, all of which look correct:
//!
//! - **Accumulating floats across tasks.** Addition is not associative in
//!   floating point, so summing into a shared total gives a different answer
//!   depending on completion order. Accumulate per-task into an indexed slot,
//!   then reduce in index order on the calling thread.
//! - **Pushing results onto a shared collection.** The order of arrival is the
//!   order of scheduling. Write into `output[index]`, do not push.
//! - **Reading a clock, a thread id, or a global counter inside a task.** All
//!   three vary per run by construction.
//!
//! None of this is checkable by the type system, and the failure mode is a
//! divergence that appears once in twenty runs. It is written down here, at the
//! seam, because the pool that replaces this implementation will make the
//! nondeterminism *more* pronounced rather than less — work stealing exists
//! precisely to vary who does what.
//!
//! # Where the read/write half lives
//!
//! `docs/DESIGN.md` §2.5 also calls for systems declaring read/write sets so the
//! scheduler can auto-parallelize them. That is `slop-ecs`'s, not this crate's:
//! an access set is a statement about components, and this crate does not know
//! what a component is. `slop-ecs::Schedule` consumes `&JobSystem` and supplies
//! the disjointness proof; everything here does is run what it is given.

use std::num::NonZeroUsize;
use std::thread;

use rayon::iter::ParallelIterator;
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, IntoParallelRefMutIterator};

/// Dispatches work across threads.
///
/// Held and passed explicitly — there is no global job system. A singleton here
/// would make headless mode, multiple editor worlds, and deterministic replay
/// impossible for the same reason any other global would
/// (`docs/CONVENTIONS.md` §5).
#[derive(Debug)]
pub struct JobSystem {
    pool: rayon::ThreadPool,
}

impl JobSystem {
    /// One worker per available core.
    ///
    /// Not cores minus one, which is what the M0 implementation reserved for the
    /// calling thread. A caller outside the pool **blocks** inside
    /// [`scope`](Self::scope) rather than executing tasks, so holding a core back
    /// for it would leave that core idle for the whole dispatch.
    ///
    /// Falls back to a single worker when the platform cannot report
    /// parallelism, which is correct but serial.
    pub fn new() -> Self {
        let cores = thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(1);

        Self::with_threads(cores)
    }

    /// A system with an explicit degree of parallelism.
    ///
    /// One is legitimate and fully supported — it is how the deterministic
    /// headless mode in `docs/DESIGN.md` §5 removes scheduling as a variable.
    ///
    /// # Panics
    ///
    /// If `threads` is zero, or if the pool cannot be built. The second is a
    /// failure to spawn OS threads at startup, which nothing downstream could
    /// meaningfully recover from.
    pub fn with_threads(threads: usize) -> Self {
        let threads = NonZeroUsize::new(threads).expect("job system needs at least one thread");

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads.get())
            // Named so a profiler and a crash dump both say which pool a thread
            // belongs to. There is more than one pool in a process the moment
            // the editor opens a second project (`docs/DESIGN.md` §2.12).
            .thread_name(|index| format!("slop-worker-{index}"))
            .build()
            .expect("the job system could not start its worker threads");

        Self { pool }
    }

    /// How many ways work is split.
    pub fn thread_count(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Which worker is running this code, if any.
    ///
    /// `None` on a thread outside the pool — including the thread that called
    /// [`scope`](Self::scope), which blocks rather than executing.
    ///
    /// The index is stable for the life of the pool and below
    /// [`thread_count`](Self::thread_count), which is what makes it usable to
    /// index per-worker state. That is the intended use: `slop-core`'s
    /// [`FrameArena`](crate::FrameArena) is `Send` but not `Sync`, so parallel
    /// code gives each worker its own rather than sharing one.
    ///
    /// It is **not** a determinism-safe input. Which worker runs which task
    /// varies per run by construction, so reading this to decide *what* to
    /// compute breaks `docs/DESIGN.md` §2.14; reading it to decide *where to put
    /// scratch space* does not.
    pub fn worker_index(&self) -> Option<usize> {
        self.pool.current_thread_index()
    }

    /// Run independent tasks that borrow from the caller's stack, and wait.
    ///
    /// Every task spawned inside the closure has completed by the time this
    /// returns, which is what lets tasks borrow local data.
    ///
    /// **Nesting is supported and cheap.** A task may call `scope` again — that
    /// is the normal shape once systems parallelize internally. The inner call
    /// runs on the worker that is already executing, which participates in the
    /// pool while it waits rather than blocking a thread and spawning more. The
    /// M0 implementation could not do this: each nested call spawned another
    /// round of OS threads.
    ///
    /// # Panics
    ///
    /// If any task panics, this call panics once the remaining tasks finish. A
    /// panicking job is a bug, and swallowing it would leave the engine running
    /// on partially computed state.
    ///
    /// **The original payload is preserved** and re-raised on the calling
    /// thread, so a job failure identifies itself without a log scrape. The M0
    /// implementation lost it.
    pub fn scope<'scope, F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Scope<'scope, '_>) -> R,
    {
        self.pool.in_place_scope(|inner| f(&Scope { inner }))
    }

    /// Apply `f` to every item, in parallel.
    ///
    /// **Order is unspecified and must not be relied on.** Which worker takes
    /// which items, and what order results are produced in, is not part of the
    /// contract. Anything order-dependent belongs in a sequential pass or must
    /// be sorted afterwards (`docs/DESIGN.md` §5).
    ///
    /// Splitting is adaptive rather than one fixed chunk per thread: a range
    /// that turns out to be slow is subdivided and stolen, so one expensive item
    /// does not hold the whole dispatch open. That is the point of a
    /// work-stealing pool, and it is why the M0 `chunk_size` helper is gone.
    pub fn for_each<T, F>(&self, items: &[T], f: F)
    where
        T: Sync,
        F: Fn(&T) + Send + Sync,
    {
        if items.is_empty() {
            return;
        }

        // `install` rather than a bare `par_iter`: without it the iterator runs
        // on rayon's global pool, and `docs/CONVENTIONS.md` §5's no-globals rule
        // is the whole reason this type is passed explicitly.
        self.pool.install(|| items.par_iter().for_each(&f));
    }

    /// Apply `f` to every item mutably, in parallel.
    ///
    /// Items are disjoint, so this is parallelism by partitioning rather than by
    /// locking — the pattern `docs/CONVENTIONS.md` §9 prefers, and the one
    /// archetype tables are shaped for. Order is unspecified, as in
    /// [`for_each`](Self::for_each).
    pub fn for_each_mut<T, F>(&self, items: &mut [T], f: F)
    where
        T: Send,
        F: Fn(&mut T) + Send + Sync,
    {
        if items.is_empty() {
            return;
        }

        self.pool.install(|| items.par_iter_mut().for_each(&f));
    }

    /// Apply `f` to every item alongside its index, in parallel.
    ///
    /// The determinism-safe way to produce results: write into `output[index]`
    /// rather than pushing onto a shared collection, and the answer no longer
    /// depends on completion order. `jobs.rs`'s module documentation names that
    /// as one of the three ways the contract gets broken; this is the shape that
    /// avoids it.
    pub fn for_each_indexed<T, F>(&self, items: &mut [T], f: F)
    where
        T: Send,
        F: Fn(usize, &mut T) + Send + Sync,
    {
        if items.is_empty() {
            return;
        }

        self.pool.install(|| {
            items
                .par_iter_mut()
                .enumerate()
                .for_each(|(index, item)| f(index, item));
        });
    }
}

impl Default for JobSystem {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawns tasks that may borrow from the enclosing
/// [`scope`](JobSystem::scope) call's stack.
///
/// Wraps the backing pool's scope rather than exposing it. That is the
/// containment rule from this module's documentation, and it is what makes
/// replacing the pool a change to one file: nothing outside can name, store, or
/// depend on the type underneath.
///
/// `'scope` is how long spawned tasks may borrow for; `'a` is the borrow of the
/// scope itself and is always inferred.
pub struct Scope<'scope, 'a> {
    inner: &'a rayon::Scope<'scope>,
}

impl<'scope> Scope<'scope, '_> {
    /// Queue a task. It runs concurrently and is guaranteed complete when the
    /// enclosing [`scope`](JobSystem::scope) returns.
    ///
    /// There is no handle and no result. Tasks communicate through the data they
    /// were given — returning values would imply an ordering the scheduler does
    /// not promise.
    pub fn spawn<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'scope,
    {
        // The scope handed to the body is discarded: a task that wants to spawn
        // further work calls `JobSystem::scope` again, which nests correctly.
        // Passing rayon's scope through would leak the backing type into a
        // public signature.
        self.inner.spawn(|_| f());
    }
}

impl std::fmt::Debug for Scope<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scope").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn defaults_to_at_least_one_thread() {
        assert!(JobSystem::new().thread_count() >= 1);
    }

    #[test]
    fn single_threaded_configuration_is_supported() {
        // Determinism work depends on being able to remove scheduling as a
        // variable, so one thread must be a real configuration, not a fallback.
        let jobs = JobSystem::with_threads(1);
        let mut items = [1, 2, 3];

        jobs.for_each_mut(&mut items, |n| *n *= 2);

        assert_eq!(items, [2, 4, 6]);
    }

    #[test]
    #[should_panic(expected = "at least one thread")]
    fn zero_threads_is_rejected() {
        JobSystem::with_threads(0);
    }

    #[test]
    fn for_each_visits_every_item_exactly_once() {
        let jobs = JobSystem::with_threads(4);
        let items: Vec<usize> = (0..1000).collect();
        let seen = AtomicUsize::new(0);

        jobs.for_each(&items, |n| {
            seen.fetch_add(*n, Ordering::Relaxed);
        });

        assert_eq!(seen.load(Ordering::Relaxed), (0..1000).sum::<usize>());
    }

    #[test]
    fn for_each_mut_writes_through_to_every_item() {
        let jobs = JobSystem::with_threads(4);
        let mut items: Vec<usize> = (0..1000).collect();

        jobs.for_each_mut(&mut items, |n| *n += 1);

        assert_eq!(items, (1..1001).collect::<Vec<_>>());
    }

    #[test]
    fn tasks_really_do_run_concurrently() {
        // Guards the shape, not the speed: if this ever runs on one thread the
        // API has quietly become sequential and callers would start depending
        // on that.
        //
        // Counting distinct thread ids inside `for_each` would seem simpler and
        // is unreliable — splitting is adaptive, so a trivial closure can finish
        // on one worker before any other wakes to steal. This instead makes the
        // tasks genuinely overlap: none may finish until all have started, which
        // only one thread cannot do.
        const TASKS: usize = 4;

        let jobs = JobSystem::with_threads(TASKS);
        let started = AtomicUsize::new(0);
        let all_started = AtomicUsize::new(0);

        jobs.scope(|scope| {
            for _ in 0..TASKS {
                scope.spawn(|| {
                    started.fetch_add(1, Ordering::SeqCst);

                    // Bounded, so a sequential implementation fails the
                    // assertion below rather than hanging the suite.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while started.load(Ordering::SeqCst) < TASKS {
                        if std::time::Instant::now() > deadline {
                            return;
                        }
                        std::hint::spin_loop();
                    }

                    all_started.fetch_add(1, Ordering::SeqCst);
                });
            }
        });

        assert_eq!(
            all_started.load(Ordering::SeqCst),
            TASKS,
            "every task must be able to observe every other task running"
        );
    }

    #[test]
    fn a_nested_scope_runs_without_spawning_more_threads() {
        // The M0 implementation could not do this: each nested call spawned
        // another round of OS threads, so a system that parallelized internally
        // multiplied the thread count. Here the inner call runs on the worker
        // already executing.
        let jobs = JobSystem::with_threads(4);
        let inner_ran = AtomicUsize::new(0);

        jobs.scope(|outer| {
            for _ in 0..4 {
                outer.spawn(|| {
                    jobs.scope(|inner| {
                        for _ in 0..4 {
                            inner.spawn(|| {
                                inner_ran.fetch_add(1, Ordering::SeqCst);
                            });
                        }
                    });
                });
            }
        });

        assert_eq!(inner_ran.load(Ordering::SeqCst), 16);
        assert_eq!(jobs.thread_count(), 4, "the pool did not grow");
    }

    #[test]
    fn nested_parallel_iteration_terminates() {
        // The shape a system that parallelizes internally actually takes, and
        // the one most likely to deadlock a naive pool: `for_each` inside
        // `for_each`, where the outer dispatch occupies every worker.
        let jobs = JobSystem::with_threads(4);
        let outer: Vec<usize> = (0..16).collect();
        let total = AtomicUsize::new(0);

        jobs.for_each(&outer, |_| {
            let inner: Vec<usize> = (0..8).collect();
            jobs.for_each(&inner, |n| {
                total.fetch_add(*n, Ordering::Relaxed);
            });
        });

        assert_eq!(total.load(Ordering::Relaxed), 16 * (0..8).sum::<usize>());
    }

    #[test]
    fn a_worker_can_identify_itself_for_per_worker_state() {
        // Why this exists: `FrameArena` is `Send` but not `Sync`, so parallel
        // code gives each worker its own rather than sharing one, and needs an
        // index to reach it.
        let jobs = JobSystem::with_threads(4);
        let seen = Mutex::new(std::collections::HashSet::new());

        jobs.scope(|scope| {
            for _ in 0..64 {
                scope.spawn(|| {
                    let index = jobs.worker_index().expect("running on a worker");
                    assert!(index < 4, "index {index} is outside the pool");
                    seen.lock().expect("not poisoned").insert(index);
                });
            }
        });

        assert!(!seen.lock().expect("not poisoned").is_empty());
        assert_eq!(
            jobs.worker_index(),
            None,
            "the calling thread is not a worker"
        );
    }

    #[test]
    fn for_each_indexed_writes_results_without_depending_on_order() {
        // The determinism-safe output shape from the module documentation:
        // write into `output[index]`, never push onto a shared collection.
        let jobs = JobSystem::with_threads(4);
        let mut output = vec![0_usize; 1000];

        jobs.for_each_indexed(&mut output, |index, slot| *slot = index * 2);

        assert_eq!(output, (0..1000).map(|n| n * 2).collect::<Vec<_>>());
    }

    #[test]
    fn empty_input_is_a_no_op() {
        let jobs = JobSystem::with_threads(4);
        let empty: [usize; 0] = [];
        let mut empty_mut: [usize; 0] = [];

        jobs.for_each(&empty, |_| unreachable!("must not run for empty input"));
        jobs.for_each_mut(&mut empty_mut, |_| {
            unreachable!("must not run for empty input")
        });
    }

    #[test]
    fn fewer_items_than_threads_still_visits_all() {
        let jobs = JobSystem::with_threads(16);
        let mut items = [1, 2, 3];

        jobs.for_each_mut(&mut items, |n| *n += 10);

        assert_eq!(items, [11, 12, 13]);
    }

    #[test]
    fn scope_completes_every_task_before_returning() {
        let jobs = JobSystem::new();
        let done = AtomicUsize::new(0);

        jobs.scope(|scope| {
            for _ in 0..32 {
                scope.spawn(|| {
                    done.fetch_add(1, Ordering::Relaxed);
                });
            }
        });

        // No join, no sleep: the scope itself is the synchronization point.
        assert_eq!(done.load(Ordering::Relaxed), 32);
    }

    #[test]
    fn scope_tasks_can_borrow_caller_stack_data() {
        let jobs = JobSystem::with_threads(4);
        let source = vec![1, 2, 3, 4];
        let total = AtomicUsize::new(0);

        jobs.scope(|scope| {
            for n in &source {
                scope.spawn(|| {
                    total.fetch_add(*n, Ordering::Relaxed);
                });
            }
        });

        assert_eq!(total.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn scope_returns_the_closure_value() {
        let jobs = JobSystem::with_threads(2);

        let result = jobs.scope(|_| 7);

        assert_eq!(result, 7);
    }

    #[test]
    #[should_panic(expected = "the shader compiler exploded")]
    fn a_panicking_task_brings_down_the_scope_with_its_own_message() {
        // Swallowing it would leave the engine running on partially computed
        // state, which is worse than stopping.
        //
        // The message is asserted now, which it could not be under M0: the
        // backing `std::thread::scope` raised its own panic instead of
        // forwarding the task's payload, so a job failure could only be
        // identified by scraping stderr.
        let jobs = JobSystem::with_threads(2);

        jobs.scope(|scope| {
            scope.spawn(|| panic!("the shader compiler exploded"));
        });
    }

    #[test]
    fn the_remaining_tasks_finish_before_the_panic_surfaces() {
        // Stopping mid-dispatch would leave siblings running against data the
        // unwinding stack is about to free.
        let jobs = JobSystem::with_threads(4);
        let finished = AtomicUsize::new(0);

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            jobs.scope(|scope| {
                scope.spawn(|| panic!("one task fails"));

                for _ in 0..8 {
                    scope.spawn(|| {
                        finished.fetch_add(1, Ordering::SeqCst);
                    });
                }
            });
        }));

        assert!(outcome.is_err(), "the panic must reach the caller");
        assert_eq!(
            finished.load(Ordering::SeqCst),
            8,
            "every sibling task ran to completion"
        );
    }

    #[test]
    fn a_single_threaded_pool_still_runs_nested_work() {
        // One thread is a supported configuration rather than a degraded
        // fallback — it is how deterministic runs remove scheduling as a
        // variable — so the nesting that a real schedule performs must not
        // deadlock when there is nobody to steal from.
        let jobs = JobSystem::with_threads(1);
        let total = AtomicUsize::new(0);

        jobs.scope(|outer| {
            outer.spawn(|| {
                jobs.scope(|inner| {
                    inner.spawn(|| {
                        total.fetch_add(1, Ordering::SeqCst);
                    });
                });
                total.fetch_add(1, Ordering::SeqCst);
            });
        });

        assert_eq!(total.load(Ordering::SeqCst), 2);
    }
}
