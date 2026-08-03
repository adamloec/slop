//! The cooked albedo, checked against what the renderer needs of it.
//!
//! These assertions used to live beside a `checkerboard()` generator in
//! `src/mesh.rs`. The generator is gone — the texture is `assets/checker.png`
//! now — so the checks moved to the artifact the renderer actually samples.
//!
//! # What changed when BC7 landed
//!
//! The artifact is no longer readable from the CPU. It is 4×4 blocks that the
//! GPU decompresses in its texture units, and nothing here decodes them —
//! deliberately, because nothing in a shipped build ever would either.
//!
//! So the texel-level assertions this file used to make (every texel opaque,
//! adjacent squares differing by an exact value) are now carried by
//! `tests/golden.rs`. A dropped alpha channel or a flattened pattern changes the
//! rendered image, and the reference it is compared against predates this whole
//! pipeline. That is a **stronger** check than reading bytes here was, because
//! it runs through the real hardware decoder rather than through a second
//! implementation of one that could be wrong in the same direction.
//!
//! What is left is what can still be said about the artifact directly, and all
//! of it is the kind of mistake that produces a plausible-looking file: a wrong
//! block count, a lost dimension, or a texture that compressed to nothing
//! because it arrived solid.

use std::collections::HashSet;
use std::path::PathBuf;

use slop_asset::{Format, Texture, Vfs};

/// The cooked albedo, or `None` with an explanation if it has not been cooked.
fn cooked() -> Option<Texture> {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");

    match Vfs::for_project(&project).read("textures/checker.tex") {
        Ok(bytes) => Some(Texture::read(&bytes).expect("a cooked texture must decode")),
        Err(error) => {
            eprintln!("skipping: {error} — run `cargo run -p slop-cli -- cook`");
            None
        }
    }
}

#[test]
fn the_albedo_is_the_size_it_claims() {
    let Some(texture) = cooked() else { return };

    assert_eq!(texture.width, 64);
    assert_eq!(texture.height, 64);
    assert_eq!(texture.format, Format::Bc7);
    assert_eq!(
        texture.pixels.len(),
        texture.payload_bytes(),
        "the payload must be exactly the blocks the header implies"
    );
}

#[test]
fn the_albedo_is_a_quarter_the_size_of_raw_pixels() {
    // The reason block compression exists, asserted rather than assumed. The
    // saving is in VRAM and sample bandwidth rather than on disk: the GPU never
    // expands these bytes.
    //
    // Measured on **level zero**, because the payload now carries a mip chain
    // too. Comparing the whole payload against raw level-zero pixels would be
    // comparing two different things, and the answer would drift every time the
    // chain changed.
    let Some(texture) = cooked() else { return };

    let raw = texture.width as usize * texture.height as usize * 4;
    let level_zero = texture.level(0).expect("every texture has a level zero");

    assert_eq!(level_zero.bytes, raw / 4);
    assert_eq!(level_zero.bytes, 16 * 16 * 16, "16x16 blocks of 16 bytes");
}

#[test]
fn the_albedo_carries_a_full_mip_chain() {
    // Mips are what stop a surface drawn smaller than its texture from
    // shimmering, and a texture cooked without them fails silently — it looks
    // right up close and aliases at distance, which no unit test sees.
    let Some(texture) = cooked() else { return };

    assert_eq!(
        texture.mip_levels, 7,
        "64x64 halves down to 1x1 in seven levels"
    );

    let smallest = texture
        .level(texture.mip_levels - 1)
        .expect("the last level exists");
    assert_eq!((smallest.width, smallest.height), (1, 1));

    // A third larger than level zero alone, which is the geometric-series bound
    // every mip chain pays and the reason mips are affordable at all.
    let level_zero = texture.level(0).expect("level zero").bytes;
    assert!(
        texture.pixels.len() < level_zero * 3 / 2,
        "a chain costs about a third more, not half again: {} over {level_zero}",
        texture.pixels.len()
    );
}

#[test]
fn the_albedo_did_not_compress_to_a_flat_colour() {
    // The failure the golden image would also catch, caught here without a GPU.
    // A texture that arrived solid — a dropped channel, a wrong stride, a
    // generator that stopped generating — compresses to a run of identical
    // blocks, because every block would hold the same two endpoints.
    //
    // The checkerboard has eight-pixel squares and a block is four texels wide,
    // so each block sits entirely inside one square and neighbouring blocks must
    // differ.
    let Some(texture) = cooked() else { return };

    let distinct: HashSet<&[u8]> = texture.pixels.chunks_exact(16).collect();

    assert!(
        distinct.len() > 1,
        "every block is identical, so the albedo is a flat colour"
    );
}
