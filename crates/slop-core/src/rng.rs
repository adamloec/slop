//! Deterministic pseudorandom numbers — `docs/DESIGN.md` §2.14.
//!
//! # Why this exists rather than `rand`
//!
//! Not because `rand` is bad. Because the *default* way to use it is
//! `rand::thread_rng()`, which seeds from the OS per thread. One call to it
//! anywhere in simulation code silently ends determinism, and nothing fails —
//! the replay simply stops reproducing, weeks later, and the cause is a line
//! nobody remembers writing.
//!
//! A generator that must be constructed from an explicit seed and passed
//! explicitly makes that mistake impossible to make by accident. There is no
//! `Rng::default()` and no thread-local instance, for the same reason
//! `docs/CONVENTIONS.md` §5 rejects globals generally.
//!
//! `rand` remains the right choice for tools, tests, and anything outside the
//! simulation. This is for the simulation.
//!
//! # The algorithm
//!
//! PCG32 — a 64-bit LCG whose output is permuted down to 32 bits. Chosen
//! because it is:
//!
//! - **Specified exactly**, down to the multiplier. `docs/DESIGN.md` §2.14's
//!   cross-platform tier needs the same seed to produce the same bytes on
//!   Windows and Linux, and that is a property of the algorithm being pinned in
//!   this file rather than of any crate's version.
//! - **Small** — sixteen bytes of state, so per-entity generators are viable.
//!   That matters: one shared generator makes results depend on the order
//!   systems happen to run in, which is exactly what a job system takes away.
//! - **Good enough.** It passes TestU01 BigCrush. This is not for cryptography,
//!   and [`Rng`] must never be used for anything security-bearing.
//!
//! # Floats are the subtle part
//!
//! [`Rng::next_f32`] builds its result from integer bits rather than dividing,
//! because a division's rounding is one more thing that has to agree across
//! platforms. See its documentation.

/// A deterministic pseudorandom generator.
///
/// Same seed, same sequence — on any machine running the same build, per
/// `docs/DESIGN.md` §2.14. Not cryptographically secure and never to be used as
/// though it were.
///
/// Deliberately **not** [`Default`]: a generator with an unstated seed is the
/// bug this type exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rng {
    state: u64,
    /// The stream selector, always odd. Two generators with the same seed and
    /// different streams produce unrelated sequences, which is what makes
    /// per-entity generators independent without needing distinct seeds.
    increment: u64,
}

/// PCG32's multiplier. Part of the algorithm, not a tunable.
const MULTIPLIER: u64 = 6_364_136_223_846_793_005;

/// The default stream, used when a caller does not choose one. Must be odd.
const DEFAULT_STREAM: u64 = 1_442_695_040_888_963_407;

impl Rng {
    /// A generator for `seed`.
    ///
    /// Every seed is valid, including zero — the stream increment being odd is
    /// what guarantees a full period regardless.
    pub const fn new(seed: u64) -> Self {
        Self::with_stream(seed, DEFAULT_STREAM)
    }

    /// A generator for `seed` on an independent `stream`.
    ///
    /// Streams let one world seed produce independent sequences per subsystem
    /// or per entity, so adding a system that consumes randomness does not
    /// shift every other system's numbers. Without that, inserting one call
    /// anywhere changes the entire simulation downstream of it.
    ///
    /// Callers may pass anything — an entity index, a system's hash. Every
    /// `stream` value maps to a distinct sequence, except that the top bit is
    /// discarded, so `n` and `n + 2^63` collide. Nothing that would use an
    /// index or a counter can reach that.
    pub const fn with_stream(seed: u64, stream: u64) -> Self {
        // The increment must be odd — an even one collapses the LCG's period —
        // so the stream is shifted up and the low bit set. Shifting rather than
        // masking is what keeps distinct streams distinct: `stream | 1` would
        // map 2 and 3 to the same sequence, which for per-entity generators
        // would silently pair up half the entities in the world.
        let increment = (stream << 1) | 1;

        // The standard PCG seeding procedure: start from the increment, then
        // advance once with the seed folded in. Starting from the seed directly
        // makes low seeds produce correlated first outputs.
        let mut rng = Self {
            state: 0,
            increment,
        };
        rng.state = rng.state.wrapping_mul(MULTIPLIER).wrapping_add(increment);
        rng.state = rng.state.wrapping_add(seed);
        rng.state = rng.state.wrapping_mul(MULTIPLIER).wrapping_add(increment);

        rng
    }

    /// The next 32 bits.
    ///
    /// Every other method is built on this one, so the sequence is defined by
    /// this function alone.
    pub fn next_u32(&mut self) -> u32 {
        let previous = self.state;
        self.state = previous
            .wrapping_mul(MULTIPLIER)
            .wrapping_add(self.increment);

        // The permutation: xorshift the high bits down, then rotate by an
        // amount taken from the top five bits. This is what turns an LCG's
        // weak low bits into an output that passes BigCrush.
        let xorshifted = (((previous >> 18) ^ previous) >> 27) as u32;
        let rotation = (previous >> 59) as u32;

        xorshifted.rotate_right(rotation)
    }

    /// The next 64 bits, as two 32-bit draws.
    ///
    /// High word first. Arbitrary, but it has to be written down, because
    /// swapping it silently changes every seeded sequence in the engine.
    pub fn next_u64(&mut self) -> u64 {
        let high = u64::from(self.next_u32());
        let low = u64::from(self.next_u32());

        (high << 32) | low
    }

    /// A uniform float in `[0, 1)`.
    ///
    /// Built by planting 23 random bits in an `f32`'s mantissa with a fixed
    /// exponent — giving a number in `[1, 2)` — and subtracting one. No
    /// division, and no rounding step that could differ between platforms:
    /// every operation here is exact in IEEE-754.
    ///
    /// The naive `next_u32() as f32 / u32::MAX as f32` involves a rounded
    /// conversion and a rounded division, and can return exactly `1.0`, which
    /// callers indexing an array by `value * length` do not expect.
    pub fn next_f32(&mut self) -> f32 {
        let bits = self.next_u32() >> 9;

        f32::from_bits(0x3f80_0000 | bits) - 1.0
    }

    /// A uniform float in `[0, 1)`, with 52 bits of mantissa.
    ///
    /// Same construction as [`next_f32`](Self::next_f32).
    pub fn next_f64(&mut self) -> f64 {
        let bits = self.next_u64() >> 12;

        f64::from_bits(0x3ff0_0000_0000_0000 | bits) - 1.0
    }

    /// A uniform integer in `0..bound`, or `None` if `bound` is zero.
    ///
    /// Rejection sampling rather than `next_u32() % bound`, which is biased
    /// toward small values whenever `bound` does not divide 2³². For a bound of
    /// 3 the bias is negligible; for a bound near 2³¹ the smallest third of the
    /// range comes up twice as often. Rejecting is the only way to be uniform,
    /// and it terminates with probability 1.
    pub fn below(&mut self, bound: u32) -> Option<u32> {
        if bound == 0 {
            return None;
        }

        // Values at or above this threshold would land in the incomplete final
        // block, so they are drawn again.
        let threshold = bound.wrapping_neg() % bound;

        loop {
            let candidate = self.next_u32();

            if candidate >= threshold {
                return Some(candidate % bound);
            }
        }
    }

    /// A uniform float in `[low, high)`.
    ///
    /// Returns `low` when `high <= low`, rather than producing a value outside
    /// the range or panicking. An inverted range is a caller's bug, and a
    /// simulation is the wrong place to abort over one.
    pub fn range_f32(&mut self, low: f32, high: f32) -> f32 {
        if high <= low {
            return low;
        }

        low + self.next_f32() * (high - low)
    }

    /// Shuffle `items` in place.
    ///
    /// Fisher-Yates, iterating downward. Included here rather than left to
    /// callers because the upward variant that looks identical is not uniform,
    /// and it is a mistake worth making impossible.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            let bound = u32::try_from(index + 1).unwrap_or(u32::MAX);

            if let Some(swap) = self.below(bound) {
                items.swap(index, swap as usize);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_produces_the_same_sequence() {
        let mut first = Rng::new(12345);
        let mut second = Rng::new(12345);

        for _ in 0..1000 {
            assert_eq!(first.next_u32(), second.next_u32());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut first = Rng::new(1);
        let mut second = Rng::new(2);

        assert_ne!(first.next_u32(), second.next_u32());
    }

    #[test]
    fn different_streams_diverge_on_the_same_seed() {
        // What makes per-entity generators independent without needing a
        // distinct seed each.
        let mut first = Rng::with_stream(7, 1);
        let mut second = Rng::with_stream(7, 2);

        let left: Vec<u32> = (0..16).map(|_| first.next_u32()).collect();
        let right: Vec<u32> = (0..16).map(|_| second.next_u32()).collect();

        assert_ne!(left, right);
    }

    #[test]
    fn consecutive_streams_stay_distinct() {
        // The failure this guards: `stream | 1` instead of `(stream << 1) | 1`
        // also produces a valid odd increment, and also passes every other test
        // here — while pairing 2 with 3, 4 with 5, and so on. For per-entity
        // generators that means half the entities in the world share a
        // sequence, which looks like a gameplay bug and not a maths one.
        let drawn = |stream| Rng::with_stream(0, stream).next_u32();

        let sequences: Vec<u32> = (0..64).map(drawn).collect();
        let mut unique = sequences.clone();
        unique.sort_unstable();
        unique.dedup();

        assert_eq!(unique.len(), sequences.len(), "streams collided");
    }

    #[test]
    fn the_byte_sequence_is_pinned() {
        // The reason this test exists: `docs/DESIGN.md` §2.14 promises the same
        // seed gives the same numbers on Windows and Linux, for this build and
        // every future one. That promise is only kept if changing the algorithm
        // fails a test rather than silently invalidating every recorded replay
        // and golden image in the repository.
        //
        // If this fails, the generator changed. That may be intentional — but
        // it is a breaking change to saved data, not a refactor.
        let mut rng = Rng::new(42);
        let drawn: Vec<u32> = (0..8).map(|_| rng.next_u32()).collect();

        assert_eq!(
            drawn,
            vec![
                492_690_617,
                1_919_685_028,
                3_561_993_920,
                683_038_915,
                1_183_706_632,
                413_921_556,
                222_559_498,
                436_142_503,
            ]
        );
    }

    #[test]
    fn floats_stay_inside_the_unit_interval() {
        // `1.0` is the value that breaks `(value * length) as usize` indexing,
        // and a division-based implementation can return it.
        let mut rng = Rng::new(9);

        for _ in 0..100_000 {
            let value = rng.next_f32();

            assert!((0.0..1.0).contains(&value), "{value} escaped [0, 1)");
        }
    }

    #[test]
    fn f64_stays_inside_the_unit_interval() {
        let mut rng = Rng::new(9);

        for _ in 0..100_000 {
            let value = rng.next_f64();

            assert!((0.0..1.0).contains(&value), "{value} escaped [0, 1)");
        }
    }

    #[test]
    fn below_stays_under_its_bound_and_covers_it() {
        let mut rng = Rng::new(3);
        let mut seen = [false; 6];

        for _ in 0..10_000 {
            let value = rng.below(6).expect("a non-zero bound");

            assert!(value < 6);
            seen[value as usize] = true;
        }

        assert!(seen.iter().all(|&hit| hit), "every value should occur");
    }

    #[test]
    fn a_zero_bound_is_none_rather_than_a_panic() {
        assert_eq!(Rng::new(0).below(0), None);
    }

    #[test]
    fn range_handles_an_inverted_span_without_escaping_it() {
        let mut rng = Rng::new(11);

        assert_eq!(rng.range_f32(5.0, 5.0), 5.0);
        assert_eq!(rng.range_f32(5.0, 1.0), 5.0);

        for _ in 0..1000 {
            let value = rng.range_f32(-2.0, 3.0);

            assert!((-2.0..3.0).contains(&value), "{value} escaped [-2, 3)");
        }
    }

    #[test]
    fn shuffling_permutes_rather_than_losing_or_duplicating() {
        let mut rng = Rng::new(77);
        let mut items: Vec<u32> = (0..64).collect();

        rng.shuffle(&mut items);

        assert_ne!(items, (0..64).collect::<Vec<_>>(), "it should have moved");

        items.sort_unstable();
        assert_eq!(items, (0..64).collect::<Vec<_>>(), "nothing lost or added");
    }

    #[test]
    fn shuffling_is_reproducible() {
        let shuffled = |seed| {
            let mut rng = Rng::new(seed);
            let mut items: Vec<u32> = (0..32).collect();
            rng.shuffle(&mut items);
            items
        };

        assert_eq!(shuffled(5), shuffled(5));
        assert_ne!(shuffled(5), shuffled(6));
    }

    #[test]
    fn an_empty_or_single_slice_shuffles_without_panicking() {
        let mut rng = Rng::new(1);

        rng.shuffle::<u32>(&mut []);
        rng.shuffle(&mut [1]);
    }
}
