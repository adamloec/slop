//! How long recent frames took.
//!
//! # Milliseconds, not frames per second
//!
//! FPS is a rate, and rates do not add up or compare linearly: the difference
//! between 60 and 50 fps is 3.3 ms, and between 20 and 10 fps is 50 ms — the
//! same ten-fps gap, fifteen times the cost. A frame has a millisecond budget
//! (16.7 at 60 Hz, 8.3 at 120) and every feature spends part of it, so
//! milliseconds are what a decision is actually made in.
//!
//! FPS is reported too, because it is what a display's refresh rate is quoted in
//! and the comparison is the point.
//!
//! # Why the worst frame and not the average
//!
//! An average hides exactly the thing a player feels. Sixty smooth frames and
//! one that took 50 ms average out to something respectable and read as a
//! visible hitch. Tracking the worst of a recent window surfaces the stutter the
//! mean smooths away — the same reason profilers report 1% lows rather than a
//! single headline number.
//!
//! # What this does *not* measure
//!
//! **CPU wall-clock between frames, not GPU time.** It includes waiting for the
//! GPU, so it is an honest measure of how fast frames arrive and a poor one for
//! attributing cost to a pass. That needs GPU timestamp queries written into the
//! command buffer, which arrive with the render graph (`docs/PLAN.md` §9.2 item
//! E) — the graph is what will know which pass a timestamp belongs to.

use std::time::Instant;

/// How many frames the window covers, when nothing says otherwise.
///
/// A couple of seconds at a typical refresh rate: long enough that one slow
/// frame stays visible for long enough to read, short enough that the worst
/// figure reflects what is happening now rather than what happened a minute ago.
pub const DEFAULT_SAMPLES: usize = 240;

/// A ring of recent frame durations, in milliseconds.
#[derive(Debug)]
pub struct FrameTimes {
    samples: Vec<f32>,
    /// Where the next sample goes.
    next: usize,
    /// The most recent sample, tracked separately from the ring.
    ///
    /// **Not `samples.last()`.** Once the ring wraps, writes go to
    /// `samples[next]` while `last()` keeps returning the final slot — which is
    /// only the newest sample on one frame in every `samples.len()`. Reading it
    /// froze the displayed frame time for 239 frames at a stretch, updating once
    /// per cycle. Kept as its own field because deriving it from `next` is the
    /// same trap one indirection further away.
    latest: f32,
    capacity: usize,
    last_tick: Instant,
}

/// What to show about recent frames, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timing {
    /// The most recent frame.
    pub last: f32,
    /// The slowest frame in the window.
    pub worst: f32,
    /// How many frames the window covers.
    pub window: usize,
}

impl Timing {
    /// The same number a display's refresh rate is quoted in.
    #[must_use]
    pub fn fps(&self) -> f32 {
        if self.last > 0.0 {
            1000.0 / self.last
        } else {
            0.0
        }
    }
}

impl Default for FrameTimes {
    fn default() -> Self {
        Self::new(DEFAULT_SAMPLES)
    }
}

impl FrameTimes {
    /// A window covering `samples` frames.
    ///
    /// # Panics
    ///
    /// Panics if `samples` is zero — a window of no frames has no worst frame,
    /// and every reading would be a silent zero.
    #[must_use]
    pub fn new(samples: usize) -> Self {
        assert!(samples > 0, "a frame-time window needs at least one sample");

        Self {
            samples: Vec::with_capacity(samples),
            next: 0,
            latest: 0.0,
            capacity: samples,
            last_tick: Instant::now(),
        }
    }

    /// Record the time since the previous call.
    ///
    /// Call once per frame, at the same point each time — the measurement is
    /// between calls, so moving it changes what is being measured.
    pub fn tick(&mut self) {
        let elapsed = self.last_tick.elapsed().as_secs_f32() * 1000.0;
        self.last_tick = Instant::now();

        if self.samples.len() < self.capacity {
            self.samples.push(elapsed);
        } else {
            self.samples[self.next] = elapsed;
        }

        self.next = (self.next + 1) % self.capacity;
        self.latest = elapsed;
    }

    /// What to display.
    #[must_use]
    pub fn summary(&self) -> Timing {
        Timing {
            last: self.latest,
            worst: self.samples.iter().copied().fold(0.0, f32::max),
            window: self.samples.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `count` frames of known duration, bypassing the clock.
    fn feed(times: &mut FrameTimes, durations: &[f32]) {
        for duration in durations {
            if times.samples.len() < times.capacity {
                times.samples.push(*duration);
            } else {
                times.samples[times.next] = *duration;
            }
            times.next = (times.next + 1) % times.capacity;
            times.latest = *duration;
        }
    }

    #[test]
    fn the_latest_sample_survives_the_ring_wrapping() {
        // The regression this type exists to not repeat. With `last` read from
        // `samples.last()`, this asserts 3.0 and gets 1.0 — the displayed frame
        // time freezing for a whole cycle at a time.
        let mut times = FrameTimes::new(4);

        feed(&mut times, &[1.0, 1.0, 1.0, 1.0]);
        assert_eq!(times.summary().last, 1.0, "before wrapping");

        feed(&mut times, &[3.0]);
        assert_eq!(
            times.summary().last,
            3.0,
            "the sample written after wrapping is the newest one"
        );
    }

    #[test]
    fn the_worst_frame_is_the_worst_in_the_window() {
        let mut times = FrameTimes::new(4);
        feed(&mut times, &[1.0, 50.0, 1.0, 1.0]);

        assert_eq!(times.summary().worst, 50.0);
    }

    #[test]
    fn an_old_spike_leaves_the_window() {
        // The point of a window rather than an all-time maximum: a hitch from a
        // minute ago must stop being reported, or the number never recovers and
        // stops meaning anything.
        let mut times = FrameTimes::new(4);

        feed(&mut times, &[50.0, 1.0, 1.0, 1.0]);
        assert_eq!(times.summary().worst, 50.0);

        feed(&mut times, &[1.0]);
        assert_eq!(times.summary().worst, 1.0, "the spike aged out");
    }

    #[test]
    fn fps_is_the_reciprocal_and_zero_is_not_a_division() {
        let mut times = FrameTimes::new(4);
        feed(&mut times, &[16.0]);

        assert!((times.summary().fps() - 62.5).abs() < 0.01);

        // Before the first tick there is no frame to take a rate of. Returning
        // zero rather than infinity keeps a fresh window printable.
        assert_eq!(FrameTimes::new(4).summary().fps(), 0.0);
    }

    #[test]
    #[should_panic(expected = "at least one sample")]
    fn a_zero_length_window_is_rejected() {
        let _ = FrameTimes::new(0);
    }
}
