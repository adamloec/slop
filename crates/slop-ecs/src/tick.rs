//! When a component was last written.
//!
//! Change detection exists so a system can skip work: a renderer re-uploads only
//! the transforms that moved, a physics broadphase reinserts only the colliders
//! that changed shape. What it costs is a stamp on every component of every
//! entity, and what it must never do is report a change that did not happen —
//! a false positive means the work happens anyway and the machinery paid for
//! nothing.
//!
//! # Granularity: per component, per entity
//!
//! Three options, and the choice is not close:
//!
//! | | Cost | Precision |
//! |---|---|---|
//! | Per archetype, per component | One tick per column | Useless — one moving entity marks every static entity in the table |
//! | **Per entity, per component** | 8 bytes per component per entity | Exact |
//! | Per chunk | One tick per chunk | Middle, but needs chunked storage `docs/DESIGN.md` §2.10 did not ask for |
//!
//! The first defeats the purpose: transform propagation writing one entity would
//! dirty every sibling sharing its table. The third is Unity's answer and only
//! makes sense because its archetypes are already split into fixed-size chunks;
//! ours are single growable columns, so it would mean restructuring storage to
//! buy precision the second option already has.
//!
//! Two ticks per element, not one. `Added` and `Changed` are different
//! questions — "did this entity gain a `Mesh`?" is what an upload system asks,
//! and it cannot be derived from "was this `Mesh` written?", since an insert is
//! also a write.
//!
//! # Wrapping
//!
//! A [`Tick`] is a `u32` and the world's counter wraps. Comparison is therefore
//! by *age* rather than by ordering: `this_run - stamp` in wrapping arithmetic,
//! clamped at [`MAX_AGE`]. That keeps every comparison correct across the wrap
//! point, at the cost of one thing — a component untouched for more than
//! `MAX_AGE` ticks becomes indistinguishable from one touched exactly `MAX_AGE`
//! ago, and would read as recently changed.
//!
//! Closing that hole means periodically walking every column and clamping stamps
//! older than `MAX_AGE`, which is a scan `docs/PLAN.md` §6.1 records as
//! outstanding. It changes no signature: the comparison below is the seam, and
//! it is already correct.
//!
//! A `u64` would remove the hole outright and double the per-component cost. At
//! one tick per frame the `u32` window is over a year of continuous running,
//! which is the wrong thing to spend 4 bytes per component per entity on.

/// The oldest a stamp may be before it stops being distinguishable from now.
///
/// Half the range, so a wrapping difference always has an unambiguous sign.
pub const MAX_AGE: u32 = u32::MAX / 2;

/// A point on the world's change-detection timeline.
///
/// Not a time and not a frame number — [`World::advance_tick`](crate::World::advance_tick)
/// is what moves it, and how often that is called is the caller's business.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tick(u32);

impl Tick {
    /// Before anything happened.
    ///
    /// The default `last_run` for a query, which makes everything read as
    /// changed — a caller that has never run has not seen anything yet.
    pub const ZERO: Self = Self(0);

    /// A tick with an explicit value.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The raw counter.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The next tick. Wraps, which [`is_newer_than`](Self::is_newer_than) is
    /// built to survive.
    pub const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    /// Whether something stamped at `self` happened after `last_run`.
    ///
    /// Both ends are needed because the comparison is by age, not by ordering:
    /// `self > last_run` would be wrong the moment the counter wraps.
    ///
    /// A stamp equal to `last_run` is **not** newer. A system's own writes carry
    /// the tick it ran at, so the alternative would have every system see its
    /// own changes on the next run forever.
    pub fn is_newer_than(self, last_run: Tick, this_run: Tick) -> bool {
        let age_of_stamp = this_run.0.wrapping_sub(self.0).min(MAX_AGE);
        let age_of_run = this_run.0.wrapping_sub(last_run.0).min(MAX_AGE);

        age_of_run > age_of_stamp
    }
}

impl Default for Tick {
    fn default() -> Self {
        Self::ZERO
    }
}

/// The window a query asks its change-detection questions about.
///
/// `last_run` is when the caller last looked; `this_run` is now. A query built
/// without saying otherwise uses [`Tick::ZERO`] for the first, which reports
/// everything as changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticks {
    /// When the caller last ran.
    pub last_run: Tick,
    /// The world's current tick.
    pub this_run: Tick,
}

impl Ticks {
    /// A window covering everything up to `this_run`.
    pub const fn everything(this_run: Tick) -> Self {
        Self {
            last_run: Tick::ZERO,
            this_run,
        }
    }
}

/// When one element was added and when it was last written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElementTicks {
    /// When the component was attached to its entity.
    pub added: Tick,
    /// When it was last written.
    pub changed: Tick,
}

impl ElementTicks {
    /// Both stamps set to `tick`, which is what a fresh insert produces.
    pub const fn new(tick: Tick) -> Self {
        Self {
            added: tick,
            changed: tick,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_from_this_run_is_newer_than_the_last() {
        let last_run = Tick::new(5);
        let this_run = Tick::new(9);

        assert!(Tick::new(9).is_newer_than(last_run, this_run));
        assert!(Tick::new(6).is_newer_than(last_run, this_run));
    }

    #[test]
    fn a_stamp_from_before_the_last_run_is_not_newer() {
        let last_run = Tick::new(5);
        let this_run = Tick::new(9);

        assert!(!Tick::new(4).is_newer_than(last_run, this_run));
        assert!(!Tick::new(0).is_newer_than(last_run, this_run));
    }

    #[test]
    fn a_stamp_equal_to_the_last_run_is_not_newer() {
        // A system's own writes carry the tick it ran at. Counting them would
        // make every system see its own changes forever.
        let last_run = Tick::new(5);

        assert!(!Tick::new(5).is_newer_than(last_run, Tick::new(9)));
    }

    #[test]
    fn nothing_is_newer_when_no_time_has_passed() {
        let tick = Tick::new(7);

        assert!(!tick.is_newer_than(tick, tick));
        assert!(!Tick::new(3).is_newer_than(tick, tick));
    }

    #[test]
    fn everything_is_newer_than_the_zero_tick() {
        // The default for a query that has never run.
        let this_run = Tick::new(1);

        assert!(Tick::new(1).is_newer_than(Tick::ZERO, this_run));
    }

    #[test]
    fn comparison_survives_the_counter_wrapping() {
        // The whole reason the comparison is by age rather than by ordering.
        // `last_run` is before the wrap and both the stamp and `this_run` are
        // after it, so a plain `>` would report the stamp as older.
        let last_run = Tick::new(u32::MAX - 2);
        let this_run = Tick::new(3);
        let stamp = Tick::new(1);

        assert!(stamp.get() < last_run.get(), "the raw values are inverted");
        assert!(stamp.is_newer_than(last_run, this_run));
    }

    #[test]
    fn a_stamp_older_than_the_window_is_not_newer_across_a_wrap() {
        let last_run = Tick::new(1);
        let this_run = Tick::new(3);
        let stamp = Tick::new(u32::MAX - 2);

        assert!(!stamp.is_newer_than(last_run, this_run));
    }

    #[test]
    fn ages_are_clamped_so_an_ancient_stamp_cannot_read_as_ordered() {
        // The documented hole: past `MAX_AGE` a stamp is indistinguishable from
        // one exactly `MAX_AGE` old. Pinned so the periodic clamp that closes it
        // has something to change.
        let this_run = Tick::new(0);
        let ancient = Tick::new(1);
        let merely_old = Tick::new(u32::MAX - MAX_AGE + 1);

        assert_eq!(
            ancient.is_newer_than(Tick::new(u32::MAX), this_run),
            merely_old.is_newer_than(Tick::new(u32::MAX), this_run),
        );
    }

    #[test]
    fn next_wraps_rather_than_overflowing() {
        assert_eq!(Tick::new(u32::MAX).next(), Tick::new(0));
        assert_eq!(Tick::new(7).next(), Tick::new(8));
    }

    #[test]
    fn a_fresh_element_carries_one_tick_for_both_questions() {
        let ticks = ElementTicks::new(Tick::new(4));

        assert_eq!(ticks.added, Tick::new(4));
        assert_eq!(ticks.changed, Tick::new(4));
    }
}
