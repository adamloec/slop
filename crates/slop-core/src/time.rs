//! Fixed-timestep simulation and frame pacing — `docs/DESIGN.md` §2.7.
//!
//! Simulation advances in fixed increments; rendering happens at whatever rate
//! the display allows and interpolates between the two most recent simulation
//! states. Physics stability, determinism, netcode, and replay all depend on the
//! simulation step never varying.
//!
//! [`FixedTimestep`] is the accumulator that makes this work. It takes a delta
//! rather than reading a clock, which is what lets it be driven from recorded
//! or synthetic deltas in tests and in the deterministic headless mode
//! (`docs/DESIGN.md` §5). [`Clock`] is the thin wrapper that supplies real deltas at
//! runtime; nothing else in the engine should call [`Instant::now`].

use std::fmt;
use std::time::{Duration, Instant};

/// Default ceiling on simulation steps per [`FixedTimestep::advance`].
///
/// Without a ceiling, a frame that takes too long queues extra simulation
/// steps, which makes the next frame slower still, which queues more — the
/// simulation falls permanently behind and never recovers. That runaway is the
/// "spiral of death", and the only escape is to refuse to run the backlog.
const DEFAULT_MAX_STEPS: u32 = 8;

/// Accumulates elapsed time and releases it in fixed increments.
#[derive(Debug, Clone)]
pub struct FixedTimestep {
    step: Duration,
    accumulator: Duration,
    max_steps: u32,
    dropped_steps: u64,
}

impl FixedTimestep {
    /// A timestep running at `hz` simulation ticks per second.
    ///
    /// # Panics
    ///
    /// If `hz` is zero.
    pub fn from_hz(hz: u32) -> Self {
        assert!(hz > 0, "simulation rate must be nonzero");

        // Integer division, so the step is exact and identical on every machine
        // — a float-derived step would differ in the last bits across platforms
        // and break the determinism this type exists to provide.
        Self::from_step(Duration::from_nanos(1_000_000_000 / u64::from(hz)))
    }

    /// A timestep with an explicit step duration.
    ///
    /// # Panics
    ///
    /// If `step` is zero.
    pub fn from_step(step: Duration) -> Self {
        assert!(!step.is_zero(), "simulation step must be nonzero");

        Self {
            step,
            accumulator: Duration::ZERO,
            max_steps: DEFAULT_MAX_STEPS,
            dropped_steps: 0,
        }
    }

    /// Override how many steps a single [`advance`](Self::advance) may release.
    ///
    /// # Panics
    ///
    /// If `max_steps` is zero, which would stall the simulation entirely.
    pub fn with_max_steps(mut self, max_steps: u32) -> Self {
        assert!(
            max_steps > 0,
            "max_steps must be nonzero or time never advances"
        );

        self.max_steps = max_steps;
        self
    }

    /// The fixed simulation step.
    pub fn step(&self) -> Duration {
        self.step
    }

    /// Add elapsed time and return how many simulation steps to run now.
    ///
    /// Time beyond [`max_steps`](Self::with_max_steps) worth of steps is
    /// discarded rather than carried, so a slow frame cannot leave a backlog
    /// that makes the next frame slower still. Discarded steps are counted in
    /// [`dropped_steps`](Self::dropped_steps) — simulation time diverging from
    /// wall-clock time is worth surfacing, not hiding.
    pub fn advance(&mut self, delta: Duration) -> u32 {
        self.accumulator = self.accumulator.saturating_add(delta);

        let step_nanos = self.step.as_nanos();
        let total_nanos = self.accumulator.as_nanos();
        let available = total_nanos / step_nanos;

        // The remainder is always less than one step, so it fits a u64 for any
        // step shorter than a few hundred years.
        let remainder = total_nanos - available * step_nanos;
        self.accumulator = Duration::from_nanos(remainder as u64);

        // Clamp after consuming: the excess time is already out of the
        // accumulator, so refusing to run those steps discards them rather than
        // deferring them. Deferring is what produces the spiral.
        let capped = u64::from(self.max_steps);
        if available > u128::from(capped) {
            self.dropped_steps += (available - u128::from(capped)) as u64;
            return self.max_steps;
        }

        available as u32
    }

    /// How far the accumulator sits between simulation states, in `0.0..1.0`.
    ///
    /// This is the blend factor the renderer applies between the two most
    /// recent simulation states (`docs/DESIGN.md` §2.7). Rendering the newest state
    /// directly is what produces stutter when display and simulation rates
    /// disagree.
    pub fn alpha(&self) -> f32 {
        self.accumulator.as_secs_f32() / self.step.as_secs_f32()
    }

    /// Simulation steps discarded to avoid a backlog, since construction.
    ///
    /// Nonzero means the simulation could not keep up and time was lost. Worth
    /// reporting rather than silently absorbing.
    pub fn dropped_steps(&self) -> u64 {
        self.dropped_steps
    }

    /// Discard accumulated time without running steps — for use after a
    /// deliberate stall such as level loading, where the elapsed wall-clock time
    /// does not correspond to simulation that should have happened.
    pub fn discard_pending(&mut self) {
        self.accumulator = Duration::ZERO;
    }
}

/// Supplies real elapsed time to a [`FixedTimestep`].
///
/// The only place in the engine that reads the system clock. Keeping it in one
/// type is what allows every other component to be driven deterministically.
pub struct Clock {
    last: Instant,
    started: Instant,
    frame: u64,
}

impl Clock {
    /// Start a clock at the current instant.
    pub fn new() -> Self {
        let now = Instant::now();

        Self {
            last: now,
            started: now,
            frame: 0,
        }
    }

    /// Time since the previous call, and advance the frame counter.
    pub fn tick(&mut self) -> Duration {
        let now = Instant::now();
        let delta = now.duration_since(self.last);

        self.last = now;
        self.frame += 1;

        delta
    }

    /// Frames completed since construction.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Time since construction.
    pub fn elapsed(&self) -> Duration {
        self.last.duration_since(self.started)
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clock")
            .field("frame", &self.frame)
            .field("elapsed", &self.elapsed())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 10ms step (100 Hz), chosen so the arithmetic in these tests divides
    /// evenly and the expected step counts are obvious by inspection.
    fn timestep() -> FixedTimestep {
        FixedTimestep::from_step(Duration::from_millis(10))
    }

    #[test]
    fn exact_multiples_release_that_many_steps() {
        let mut time = timestep();

        assert_eq!(time.advance(Duration::from_millis(30)), 3);
        assert_eq!(time.alpha(), 0.0);
    }

    #[test]
    fn partial_time_releases_nothing_and_carries() {
        let mut time = timestep();

        assert_eq!(time.advance(Duration::from_millis(4)), 0);
        assert_eq!(time.advance(Duration::from_millis(4)), 0);
        assert_eq!(
            time.advance(Duration::from_millis(4)),
            1,
            "12ms crosses one step"
        );
    }

    #[test]
    fn leftover_carries_into_the_next_advance() {
        let mut time = timestep();

        assert_eq!(time.advance(Duration::from_millis(15)), 1);
        // 5ms carried, so 5ms more completes a second step.
        assert_eq!(time.advance(Duration::from_millis(5)), 1);
        assert_eq!(time.alpha(), 0.0);
    }

    #[test]
    fn alpha_reports_progress_toward_the_next_step() {
        let mut time = timestep();

        time.advance(Duration::from_millis(12));

        // 2ms of a 10ms step remain unconsumed.
        assert!(
            (time.alpha() - 0.2).abs() < 1e-6,
            "alpha was {}",
            time.alpha()
        );
    }

    #[test]
    fn alpha_stays_below_one() {
        let mut time = timestep();

        time.advance(Duration::from_micros(9_999));

        assert!(time.alpha() < 1.0);
    }

    #[test]
    fn runaway_delta_is_clamped_to_max_steps() {
        let mut time = timestep().with_max_steps(4);

        // One second at a 10ms step is 100 steps' worth.
        let steps = time.advance(Duration::from_millis(1000));

        assert_eq!(steps, 4);
    }

    #[test]
    fn clamping_discards_time_rather_than_deferring_it() {
        // The spiral of death: if the backlog were carried, the next advance
        // would return max_steps again, and again, forever.
        let mut time = timestep().with_max_steps(4);
        time.advance(Duration::from_millis(1000));

        let next = time.advance(Duration::from_millis(10));

        assert_eq!(next, 1, "the backlog must not have been carried");
        assert_eq!(time.dropped_steps(), 96);
    }

    #[test]
    fn dropped_steps_stay_zero_when_keeping_up() {
        let mut time = timestep().with_max_steps(4);

        for _ in 0..100 {
            time.advance(Duration::from_millis(10));
        }

        assert_eq!(time.dropped_steps(), 0);
    }

    #[test]
    fn discard_pending_clears_the_accumulator() {
        let mut time = timestep();
        time.advance(Duration::from_millis(7));

        time.discard_pending();

        assert_eq!(time.alpha(), 0.0);
        assert_eq!(time.advance(Duration::from_millis(3)), 0);
    }

    #[test]
    fn from_hz_produces_an_exact_step() {
        // Integer-derived, so this is bit-identical on every platform.
        assert_eq!(
            FixedTimestep::from_hz(100).step(),
            Duration::from_millis(10)
        );
        assert_eq!(FixedTimestep::from_hz(60).step().as_nanos(), 16_666_666);
    }

    #[test]
    fn identical_delta_sequences_produce_identical_step_counts() {
        // The determinism property docs/DESIGN.md §5 depends on.
        let deltas = [3_u64, 11, 7, 1, 40, 6, 9];

        let mut first = timestep();
        let mut second = timestep();

        let a: Vec<u32> = deltas
            .iter()
            .map(|ms| first.advance(Duration::from_millis(*ms)))
            .collect();
        let b: Vec<u32> = deltas
            .iter()
            .map(|ms| second.advance(Duration::from_millis(*ms)))
            .collect();

        assert_eq!(a, b);
        assert_eq!(first.alpha(), second.alpha());
    }

    #[test]
    fn clock_counts_frames() {
        // Deliberately asserts no timing — only that the counter advances.
        let mut clock = Clock::new();

        clock.tick();
        clock.tick();

        assert_eq!(clock.frame(), 2);
    }
}
