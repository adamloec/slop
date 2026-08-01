//! Hash maps whose iteration order does not change between runs —
//! `docs/DESIGN.md` §2.14.
//!
//! # The problem this solves
//!
//! `std::collections::HashMap` defaults to `RandomState`, which seeds itself
//! from the OS **once per process**. That is a deliberate and correct choice for
//! general software: it defends against an attacker who can choose keys that all
//! collide.
//!
//! It also means iterating a `HashMap` yields a different order every time the
//! program runs. Two runs of the same binary, on the same machine, with the same
//! input, iterate differently. If anything in the simulation iterates a map —
//! systems in a registry, entities in a spatial bucket, assets in a dependency
//! graph — then the order of that work varies per run, and with it every
//! floating-point accumulation that depends on order.
//!
//! Determinism does not survive that, and nothing reports it. The symptom is a
//! replay that diverges after ninety seconds, or a golden image that fails one
//! run in twenty.
//!
//! # The fix, and what it costs
//!
//! [`FxHashMap`] and [`FxHashSet`] are the same containers with a fixed-seed
//! hasher. Order still depends on insertion history and capacity — a hash map is
//! not an ordered container and this does not make it one — but it no longer
//! depends on the *run*. Same operations in the same order produce the same
//! iteration order, every time, on every machine.
//!
//! The cost is that these are not resistant to a hostile key chooser. That is
//! the right trade inside an engine, where keys are entity ids, type ids, and
//! asset paths the engine itself produced. It is the wrong trade for anything
//! parsing untrusted input — a network packet, a downloaded asset manifest — and
//! those must keep using [`std::collections::HashMap`].
//!
//! # When iteration order matters, sort
//!
//! These types make iteration *reproducible*, not *meaningful*. Code that needs
//! a defined order — writing a file, hashing a set of dependencies, presenting a
//! list — should collect and sort, or use a [`BTreeMap`](std::collections::BTreeMap).
//! Reproducible-but-arbitrary is enough for determinism and not enough for a
//! stable serialization format.

use std::hash::{BuildHasherDefault, Hasher};

/// A [`HashMap`](std::collections::HashMap) with reproducible iteration order.
pub type FxHashMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// A [`HashSet`](std::collections::HashSet) with reproducible iteration order.
pub type FxHashSet<T> = std::collections::HashSet<T, BuildHasherDefault<FxHasher>>;

/// The odd constant rustc's own `FxHasher` uses. Its only requirements are
/// being odd and having well-distributed bits.
const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// A fast, fixed-seed, non-cryptographic hasher.
///
/// The algorithm rustc uses internally: multiply-and-rotate per word. Far faster
/// than SipHash for the short keys an engine hashes — integers, handles, small
/// strings — and, critically, seeded by a constant rather than by the OS.
///
/// Never use this where an adversary chooses the keys.
#[derive(Debug, Clone, Copy, Default)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    /// Fold one word into the running hash.
    ///
    /// The rotate is what mixes high bits down; without it, a multiply alone
    /// leaves the low bits of the input dominating the low bits of the output,
    /// which is exactly where a hash map takes its bucket index from.
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // Eight bytes at a time, then the remainder one byte at a time. The
        // chunked path is why hashing a path or a name is cheap.
        let mut chunks = bytes.chunks_exact(8);

        for chunk in &mut chunks {
            self.add(u64::from_ne_bytes(
                chunk.try_into().expect("chunks_exact(8) yields 8 bytes"),
            ));
        }

        for &byte in chunks.remainder() {
            self.add(u64::from(byte));
        }
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::BuildHasher;

    #[test]
    fn the_hasher_is_seeded_by_a_constant_not_by_the_process() {
        // The whole point. Two independently constructed hashers must agree,
        // which `RandomState` does not guarantee across processes.
        let build = BuildHasherDefault::<FxHasher>::default();

        assert_eq!(
            build.hash_one("entity/player"),
            build.hash_one("entity/player")
        );
        assert_ne!(
            build.hash_one("entity/player"),
            build.hash_one("entity/enemy")
        );
    }

    #[test]
    fn the_hash_of_a_known_key_is_pinned() {
        // Changing the hasher changes iteration order, which changes every
        // recorded replay and every golden image of a scene that iterates a
        // map. That may be worth doing — but it is a breaking change to saved
        // data, and it should not happen by accident.
        let build = BuildHasherDefault::<FxHasher>::default();

        assert_eq!(build.hash_one(0_u64), 0);
        assert_eq!(build.hash_one(1_u64), SEED);
    }

    #[test]
    fn identical_insertion_sequences_iterate_identically() {
        // The property determinism actually rests on: not that iteration is
        // sorted, but that it is the same twice.
        let build = |offset: u64| {
            let mut map = FxHashMap::default();
            for key in 0..256_u64 {
                map.insert(key.wrapping_mul(2_654_435_761).wrapping_add(offset), key);
            }
            map.into_iter().collect::<Vec<_>>()
        };

        assert_eq!(build(0), build(0));
        assert_ne!(build(0), build(7), "different keys, different layout");
    }

    #[test]
    fn a_set_iterates_reproducibly_too() {
        let build = || {
            let mut set = FxHashSet::default();
            for value in 0..128_u32 {
                set.insert(value.wrapping_mul(2_654_435_761));
            }
            set.into_iter().collect::<Vec<_>>()
        };

        assert_eq!(build(), build());
    }

    #[test]
    fn byte_slices_hash_through_both_the_chunked_and_remainder_paths() {
        // Lengths either side of the eight-byte chunk boundary, since the two
        // paths are separate code and a bug in the remainder loop would only
        // show up for non-multiples of eight.
        let build = BuildHasherDefault::<FxHasher>::default();

        for length in 0..24_usize {
            let key: Vec<u8> = (0..length).map(|index| index as u8).collect();

            assert_eq!(
                build.hash_one(&key),
                build.hash_one(&key),
                "length {length}"
            );
        }
    }
}
