//! Formats against a real device — what the unit tests in `src/format.rs`
//! cannot assert.
//!
//! Those check the mapping to Vulkan and the aspect a format implies, both of
//! which are pure functions. Whether the *device* will accept a format for a
//! given use is not: it is a driver-side table this crate has no business
//! guessing at, and `docs/PLAN.md` §9.4 picks an HDR format on the assumption
//! that it is universally usable as a colour attachment. That assumption is
//! worth one test rather than a paragraph.

mod support;

use slop_rhi::{Extent2D, Format, Image, ImageConfig, ImageKind, ImageUsage, RhiError};

/// What §9.4 chose, used the way E2 will use it.
///
/// The HDR target is rendered into and then read by the tonemap pass, so it
/// needs both features at once. A format supporting only the first is the
/// failure this asserts against — and it is the plausible one, because colour
/// attachment support is far more widely guaranteed than sampled support.
#[test]
fn the_hdr_target_format_can_be_rendered_into_and_sampled() {
    let Some((_device, allocator)) = support::device_and_allocator() else {
        return;
    };

    let image = Image::new(
        &allocator,
        &ImageConfig {
            name: "hdr target support probe",
            extent: Extent2D {
                width: 64,
                height: 64,
            },
            format: Format::Rgba16Float,
            usage: ImageUsage::COLOR_ATTACHMENT | ImageUsage::SAMPLED,
            mip_levels: 1,
            kind: ImageKind::Flat,
        },
    );

    assert!(
        image.is_ok(),
        "Rgba16Float must serve as a sampled colour attachment; PLAN.md §9.4 \
         chose it on that basis"
    );
}

/// The cheaper alternative the `PLAN.md` §6.1 row is about.
///
/// Recorded rather than required: if this ever fails on a real device, the row
/// stops being a free swap and the reasoning behind it needs revisiting. That
/// is worth learning from a named test rather than from a wrong picture.
#[test]
fn the_packed_hdr_format_is_also_a_usable_colour_attachment() {
    let Some((_device, allocator)) = support::device_and_allocator() else {
        return;
    };

    let image = Image::new(
        &allocator,
        &ImageConfig {
            name: "packed hdr support probe",
            extent: Extent2D {
                width: 64,
                height: 64,
            },
            format: Format::R11G11B10Float,
            usage: ImageUsage::COLOR_ATTACHMENT | ImageUsage::SAMPLED,
            mip_levels: 1,
            kind: ImageKind::Flat,
        },
    );

    assert!(
        image.is_ok(),
        "R11G11B10Float is not usable as a sampled colour attachment here; \
         PLAN.md §6.1's swap to it is not the free change that row assumes"
    );
}

/// The check reports the *use*, not just a rejection.
///
/// A block-compressed format can never be a colour attachment — the spec
/// forbids it rather than leaving it to the device, which is what makes this a
/// portable negative case instead of a guess about one GPU.
///
/// **Verified by removing the check**, which is the only way to know this test
/// asserts anything. Without it `Image::new` *succeeds*: the driver hands back a
/// usable `VkImage` for a format that can never be rendered into, and the
/// undefined behaviour waits until something tries. The check is not a nicer
/// message in front of a rejection that was coming anyway — it is the rejection.
#[test]
fn an_impossible_usage_is_refused_by_name_before_the_driver_sees_it() {
    let Some((_device, allocator)) = support::device_and_allocator() else {
        return;
    };

    let failure = Image::new(
        &allocator,
        &ImageConfig {
            name: "block compressed attachment",
            extent: Extent2D {
                width: 64,
                height: 64,
            },
            format: Format::Bc7Unorm,
            usage: ImageUsage::COLOR_ATTACHMENT,
            mip_levels: 1,
            kind: ImageKind::Flat,
        },
    )
    .expect_err("a block-compressed colour attachment must be refused");

    match failure {
        RhiError::FormatUnsupported { format, missing } => {
            assert_eq!(format, Format::Bc7Unorm);
            assert_eq!(
                missing, "colour attachment",
                "the error must name which use was unsupported"
            );
        }
        other => panic!("expected FormatUnsupported, got {other}"),
    }
}

/// The same format, used the way it is meant to be, still works.
///
/// Guards the check being too eager: refusing BC7 outright rather than refusing
/// it *as an attachment* would break every cooked texture in the engine, and
/// the negative test above cannot tell the two apart on its own.
#[test]
fn a_block_compressed_format_is_still_fine_for_sampling() {
    let Some((_device, allocator)) = support::device_and_allocator() else {
        return;
    };

    let image = Image::new(
        &allocator,
        &ImageConfig {
            name: "block compressed texture",
            extent: Extent2D {
                width: 64,
                height: 64,
            },
            format: Format::Bc7Unorm,
            usage: ImageUsage::SAMPLED | ImageUsage::TRANSFER_DST,
            mip_levels: 1,
            kind: ImageKind::Flat,
        },
    );

    assert!(
        image.is_ok(),
        "BC7 must remain usable as a sampled texture; it is what every cooked \
         texture in the engine is"
    );
}
