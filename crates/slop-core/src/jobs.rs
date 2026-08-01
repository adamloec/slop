//! Task scheduling — `docs/DESIGN.md` §2.5.
//!
//! Parallelism is foundational rather than additive: bolting it onto a
//! single-threaded engine is a rewrite, not an optimization. What must be right
//! from the start is therefore the *shape* of this API — that callers express
//! work as independent tasks over disjoint data, and never assume the calling
//! thread executes it.
//!
//! # This implementation is provisional
//!
//! M0 backs every entry point with [`std::thread::scope`], which spawns OS
//! threads per call. That is correct and safe but not fast: thread creation
//! costs far more than the work a single frame's job would do.
//!
//! The work-stealing pool replaces it at M1, once ECS system scheduling supplies
//! real requirements about task granularity and dependency shape. This is
//! `docs/DESIGN.md` §1.2 principle 6 applied deliberately — the implementation
//! is deferred, the seam is not. Callers written against this API do not change
//! when the pool lands.
//!
//! Do not build on the current cost model. Assume dispatch is cheap and tasks
//! are many, which is what will be true.
//!
//! # What is deliberately absent
//!
//! `docs/DESIGN.md` §2.5 also calls for systems declaring read/write sets so the
//! scheduler can auto-parallelize them. That is not here, because there is no
//! ECS yet to declare anything — an access-declaration API designed with no
//! consumers would be designed against imagined requirements. It lands with
//! `slop-ecs` at M1.

use std::num::NonZeroUsize;
use std::thread;

/// Dispatches work across threads.
///
/// Held and passed explicitly — there is no global job system. A singleton here
/// would make headless mode, multiple editor worlds, and deterministic replay
/// impossible for the same reason any other global would
/// (`docs/CONVENTIONS.md` §5).
#[derive(Debug, Clone)]
pub struct JobSystem {
    threads: NonZeroUsize,
}

impl JobSystem {
    /// One worker per available core, leaving one for the calling thread.
    ///
    /// Falls back to a single worker when the platform cannot report
    /// parallelism, which is correct but serial.
    pub fn new() -> Self {
        let cores = thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(1);

        Self::with_threads(cores.saturating_sub(1).max(1))
    }

    /// A system with an explicit degree of parallelism.
    ///
    /// One is legitimate and fully supported — it is how the deterministic
    /// headless mode in `docs/DESIGN.md` §5 removes scheduling as a variable.
    ///
    /// # Panics
    ///
    /// If `threads` is zero.
    pub fn with_threads(threads: usize) -> Self {
        Self {
            threads: NonZeroUsize::new(threads).expect("job system needs at least one thread"),
        }
    }

    /// How many ways work is split.
    pub fn thread_count(&self) -> usize {
        self.threads.get()
    }

    /// Run independent tasks that borrow from the caller's stack, and wait.
    ///
    /// Every task spawned inside the closure has completed by the time this
    /// returns, which is what lets tasks borrow local data.
    ///
    /// # Panics
    ///
    /// If any task panics, this call panics once the remaining tasks finish. A
    /// panicking job is a bug, and swallowing it would leave the engine running
    /// on partially computed state.
    ///
    /// The failing task's panic message reaches stderr, but **the payload is not
    /// currently forwarded** — the backing [`std::thread::scope`] raises its own
    /// panic instead. Preserving the original payload is worth doing when the M1
    /// pool replaces the implementation, since a job failure should identify
    /// itself without a log scrape.
    pub fn scope<'env, F, R>(&self, f: F) -> R
    where
        F: for<'scope> FnOnce(&Scope<'scope, 'env>) -> R,
    {
        thread::scope(|inner| f(&Scope { inner }))
    }

    /// Apply `f` to every item, in parallel.
    ///
    /// **Order is unspecified and must not be relied on.** Items are split into
    /// contiguous chunks across threads; which thread takes which chunk, and in
    /// what order results are produced, is not part of the contract and will
    /// change when the work-stealing pool lands. Anything order-dependent
    /// belongs in a sequential pass or must be sorted afterwards
    /// (`docs/DESIGN.md` §5).
    pub fn for_each<T, F>(&self, items: &[T], f: F)
    where
        T: Sync,
        F: Fn(&T) + Send + Sync,
    {
        if items.is_empty() {
            return;
        }

        let f = &f;
        thread::scope(|scope| {
            for chunk in items.chunks(self.chunk_size(items.len())) {
                scope.spawn(move || {
                    for item in chunk {
                        f(item);
                    }
                });
            }
        });
    }

    /// Apply `f` to every item mutably, in parallel.
    ///
    /// Chunks are disjoint, so this is parallelism by partitioning rather than
    /// by locking — the pattern `docs/CONVENTIONS.md` §9 prefers, and the one
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

        let chunk_size = self.chunk_size(items.len());
        let f = &f;
        thread::scope(|scope| {
            for chunk in items.chunks_mut(chunk_size) {
                scope.spawn(move || {
                    for item in chunk {
                        f(item);
                    }
                });
            }
        });
    }

    /// Split `len` items into at most [`thread_count`](Self::thread_count)
    /// chunks, rounding up so no trailing chunk is orphaned.
    fn chunk_size(&self, len: usize) -> usize {
        len.div_ceil(self.threads.get()).max(1)
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
/// This wraps [`std::thread::Scope`] rather than exposing it, so the M1 pool can
/// replace the backing without touching callers.
#[derive(Debug)]
pub struct Scope<'scope, 'env> {
    // The `'env: 'scope` bound is inferred from this field rather than written
    // out; `explicit_outlives_requirements` rejects restating it.
    inner: &'scope thread::Scope<'scope, 'env>,
}

impl<'scope, 'env> Scope<'scope, 'env> {
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
        self.inner.spawn(f);
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
    fn work_is_actually_split_across_threads() {
        // Guards the shape, not the speed: if this ever runs on one thread the
        // API has quietly become sequential and callers would start depending
        // on that.
        let jobs = JobSystem::with_threads(4);
        let items: Vec<usize> = (0..4096).collect();
        let threads = Mutex::new(std::collections::HashSet::new());

        jobs.for_each(&items, |_| {
            threads
                .lock()
                .expect("not poisoned")
                .insert(thread::current().id());
        });

        let count = threads.lock().expect("not poisoned").len();
        assert!(
            count > 1,
            "expected more than one worker thread, saw {count}"
        );
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
    #[should_panic]
    fn a_panicking_task_brings_down_the_scope() {
        // Swallowing it would leave the engine running on partially computed
        // state, which is worse than stopping.
        //
        // Deliberately not asserting on the message: std::thread::scope raises
        // its own panic rather than forwarding the task's payload, and pinning
        // this test to std's wording would make it fail on an unrelated std
        // change. Forwarding the payload is an M1 improvement — see `scope`.
        let jobs = JobSystem::with_threads(2);

        jobs.scope(|scope| {
            scope.spawn(|| panic!("job failed"));
        });
    }
}
