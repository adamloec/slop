//! The cooked albedo, checked against what the renderer needs of it.
//!
//! These assertions used to live beside a `checkerboard()` generator in
//! `src/mesh.rs`. The generator is gone — the texture is `assets/checker.png`
//! now — so the checks moved to the artifact the renderer actually samples.
//!
//! That is the better place for them anyway. A generator can only be wrong about
//! itself; a cooked asset can be wrong because the PNG changed, because the
//! importer dropped a channel, or because the cache served something stale.

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
    assert_eq!(texture.format, Format::Rgba8);
    assert_eq!(
        texture.pixels.len(),
        texture.width as usize * texture.height as usize * 4
    );
}

#[test]
fn the_checkerboard_alternates() {
    // A texture that came out solid would make the golden image pass while
    // proving nothing about sampling. It would also be exactly what a broken
    // importer produces — a dropped channel or a wrong stride flattens a
    // pattern into a wash.
    let Some(texture) = cooked() else { return };

    let texel = |x: u32, y: u32| texture.pixels[((y * texture.width + x) * 4) as usize];

    assert_ne!(texel(0, 0), texel(8, 0), "adjacent squares must differ");
    assert_eq!(texel(0, 0), texel(16, 0), "and repeat every two squares");
    assert_ne!(texel(0, 0), texel(0, 8), "in both axes");
}

#[test]
fn the_albedo_is_fully_opaque() {
    // The cube has no transparency, so an alpha other than 255 means the
    // importer invented one — the failure that looks like the object vanished.
    let Some(texture) = cooked() else { return };

    assert!(
        texture.pixels.chunks_exact(4).all(|texel| texel[3] == 255),
        "every texel must be opaque"
    );
}
