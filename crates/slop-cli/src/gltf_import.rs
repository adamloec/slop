//! Turning glTF files into cooked meshes — `docs/DESIGN.md` §2.8.
//!
//! The second asset kind, and the one that decides whether a `Cooker` trait
//! should exist. It does not, yet, and this is why: **a shader is one source to
//! one artifact; a glTF is one source to many.** One file holds a scene of
//! meshes, each of several primitives, and each primitive is one draw call. A
//! trait shaped around the shader case would have had to be unpicked here.
//!
//! What *is* shared is the cache, and this drives the same
//! [`Cache`] the shader cooker does.
//!
//! # Naming
//!
//! Each primitive becomes its own artifact, because each is drawn separately:
//!
//! ```text
//! assets/props/crate.gltf   mesh "Body", primitive 0  →  meshes/props/crate.Body.0.mesh
//!                           mesh "Body", primitive 1  →  meshes/props/crate.Body.1.mesh
//! ```
//!
//! The mesh's own name is used when it has one, and its index otherwise. A name
//! is what an author typed and survives the file being re-exported with meshes
//! reordered; an index does not. Names are sanitised, since a logical path is
//! restricted in ways a glTF name is not.
//!
//! # What is read, and what is not
//!
//! Positions, normals, texture coordinates and indices — enough to replace the
//! cube's hardcoded geometry, which is the consumer that exists. Materials,
//! tangents, skinning, animation and the scene hierarchy are recorded in
//! `docs/PLAN.md` §6.1; each is another attribute through the same pipeline
//! rather than a different one.
//!
//! Two things are **generated** when a file omits them, rather than refused:
//!
//! - **Normals**, from the triangle's own plane. A mesh with no normals is
//!   common from a CAD exporter, and flat normals are the honest answer for one.
//! - **Texture coordinates**, as zero. A mesh with no UVs is untextured, and the
//!   vertex layout has the field either way.
//!
//! Positions and indices have no such fallback. A primitive without positions is
//! not geometry, and one without indices would need a triangle list synthesised
//! from draw order — which is a real glTF mode (`mode: 4` unindexed) and is
//! handled by generating the sequence, since that is what it means.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use slop_asset::mesh::{Mesh, Vertex};
use slop_asset::texture::{Format, Texture};
use slop_asset::{AlphaMode, Cache, CacheKey, Instance, Material, Model, TextureSlot};
use slop_core::diagnostics::tracing::{debug, info, warn};

use crate::cook::Summary;

/// Bump to invalidate every cooked mesh.
///
/// Independent of the shader cooker's version: changing how meshes are imported
/// should not recompile shaders, and a shared constant would make it.
/// 2 — materials and the images they reference are cooked too.
/// 3 — the node hierarchy is flattened into a cooked model.
const COOKER_VERSION: u32 = 3;

/// Where source models live, relative to the project root.
const SOURCE_DIRECTORY: &str = "assets";

/// Cook every glTF under `root/assets` into `root/.slop/cache/meshes`.
///
/// Incremental, as shaders are: an artifact whose stamp still matches is left
/// alone, and `force` ignores stamps.
///
/// # Errors
///
/// Fails if a file cannot be read or parsed, or the cache cannot be written.
pub(crate) fn meshes(root: &Path, force: bool) -> Result<Summary> {
    let source_root = root.join(SOURCE_DIRECTORY);
    let cache = Cache::for_project(root);

    if !source_root.is_dir() {
        warn!(path = %source_root.display(), "no assets directory; nothing to cook");
        return Ok(Summary::default());
    }

    let mut sources = Vec::new();
    collect_models(&source_root, &mut sources)?;
    sources.sort();

    let mut summary = Summary::default();

    for source in &sources {
        let relative = source
            .strip_prefix(&source_root)
            .expect("collected paths are under the source root");

        cook_file(&cache, source, relative, force, &mut summary)?;
    }

    Ok(summary)
}

/// Cook every primitive in one glTF file.
fn cook_file(
    cache: &Cache,
    source: &Path,
    relative: &Path,
    force: bool,
    summary: &mut Summary,
) -> Result<()> {
    let bytes =
        std::fs::read(source).with_context(|| format!("reading model {}", source.display()))?;

    // Parsed *before* the key is computed, because the key needs the buffers.
    let (document, buffers, images) =
        gltf::import(source).with_context(|| format!("parsing {}", source.display()))?;

    // Every buffer is an input, not just the `.gltf` itself.
    //
    // A glTF commonly stores its vertex data in a sibling `.bin`, and keying on
    // the JSON alone would mean editing that `.bin` changed every mesh while
    // every stamp still matched — a cache that is *wrong* rather than stale.
    // That is exactly the bug the shader cooker already had once, arriving by a
    // different door.
    //
    // Hashing the **resolved** buffers rather than the file names covers all
    // three storage forms uniformly: base64 embedded in the JSON, an external
    // `.bin`, and a GLB's binary chunk.
    //
    // The cost is parsing even when the artifact turns out to be up to date.
    // That is the right trade — parsing a glTF is fast, and a cache that lies is
    // a debugging session.
    let mut key = CacheKey::builder()
        .input("cooker", &COOKER_VERSION.to_le_bytes())
        .input("mesh format", &slop_asset::mesh::VERSION.to_le_bytes())
        .input(
            "material format",
            &slop_asset::material::VERSION.to_le_bytes(),
        )
        .input(
            "texture format",
            &slop_asset::texture::VERSION.to_le_bytes(),
        )
        .input("source", &bytes);

    for buffer in &buffers {
        key = key.input("buffer", &buffer.0);
    }

    let key = key.finish();

    // Before the meshes, because a primitive names the material it uses and
    // naming one that was never written would leave a dangling reference in the
    // cache.
    let materials = cook_materials(cache, relative, &document, &images, &key, force, summary)?;

    // Where each mesh sits. Cooked from the same parse, so a model can never
    // name a mesh this run did not write.
    cook_model(cache, relative, &document, &key, force, summary)?;

    for (index, mesh) in document.meshes().enumerate() {
        let name = mesh_name(&mesh, index);

        for (primitive_index, primitive) in mesh.primitives().enumerate() {
            let logical = logical_path(relative, &name, primitive_index);
            let artifact = cache.artifact(&logical);

            if !force && cache.is_current(&artifact, &key) {
                debug!(logical, "up to date");
                summary.skipped += 1;
                continue;
            }

            let cooked = read_primitive(&primitive, &buffers, &materials).with_context(|| {
                format!(
                    "reading primitive {primitive_index} of mesh '{name}' in {}",
                    source.display()
                )
            })?;

            cache.prepare(&artifact)?;
            std::fs::write(&artifact, cooked.write())
                .with_context(|| format!("writing {}", artifact.display()))?;
            cache.record(&artifact, &key)?;

            info!(
                logical,
                vertices = cooked.vertices.len(),
                triangles = cooked.triangles(),
                "cooked"
            );
            summary.cooked += 1;
        }
    }

    Ok(())
}

/// A mesh's name, or a stand-in derived from its index.
fn mesh_name(mesh: &gltf::Mesh<'_>, index: usize) -> String {
    mesh.name()
        .map(sanitise)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("mesh{index}"))
}

/// Replace anything a logical path cannot carry.
///
/// A glTF name is arbitrary text; a logical path is `/`-separated and must not
/// climb. Rather than refusing an awkward name — which would mean an artist's
/// file failing to cook over a space — anything outside a conservative set
/// becomes `_`.
fn sanitise(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// The source file's path, sanitised, without its extension.
///
/// Shared by every artifact a glTF produces, so a mesh, its material and its
/// textures all sort together in the cache.
fn stem_segments(relative: &Path) -> Vec<String> {
    relative
        .with_extension("")
        .components()
        .map(|segment| sanitise(&segment.as_os_str().to_string_lossy()))
        .collect()
}

/// Where a primitive's artifact lives.
fn logical_path(relative: &Path, mesh: &str, primitive: usize) -> String {
    format!(
        "meshes/{}.{mesh}.{primitive}.mesh",
        stem_segments(relative).join("/")
    )
}

/// Read one primitive into the cooked layout.
fn read_primitive(
    primitive: &gltf::Primitive<'_>,
    buffers: &[gltf::buffer::Data],
    materials: &[String],
) -> Result<Mesh> {
    if primitive.mode() != gltf::mesh::Mode::Triangles {
        bail!(
            "primitive mode {:?} is not supported; only triangle lists are",
            primitive.mode()
        );
    }

    let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| &data.0[..]));

    let positions: Vec<[f32; 3]> = reader
        .read_positions()
        .context("primitive has no positions, so it is not geometry")?
        .collect();

    let mut indices: Vec<u32> = match reader.read_indices() {
        Some(indices) => indices.into_u32().collect(),
        // A real glTF mode: an unindexed primitive draws its vertices in order,
        // so that sequence *is* its index buffer.
        None => (0..positions.len() as u32).collect(),
    };

    if !indices.len().is_multiple_of(3) {
        bail!(
            "index count {} is not a multiple of three, so it is not a triangle list",
            indices.len()
        );
    }

    for &index in &indices {
        if index as usize >= positions.len() {
            bail!(
                "index {index} names a vertex the primitive does not have ({} present)",
                positions.len()
            );
        }
    }

    let uvs: Vec<[f32; 2]> = reader
        .read_tex_coords(0)
        .map(|coords| coords.into_f32().collect())
        .unwrap_or_default();

    let normals: Vec<[f32; 3]> = reader
        .read_normals()
        .map(Iterator::collect)
        .unwrap_or_default();

    let mut vertices: Vec<Vertex> = positions
        .iter()
        .enumerate()
        .map(|(index, position)| Vertex {
            position: *position,
            normal: normals.get(index).copied().unwrap_or([0.0, 0.0, 0.0]),
            uv: uvs.get(index).copied().unwrap_or([0.0, 0.0]),
        })
        .collect();

    if normals.is_empty() {
        generate_flat_normals(&mut vertices, &mut indices);
    }

    // `None` when the primitive names no material, which glTF permits and which
    // means glTF's default rather than "draw it untextured".
    let material = primitive
        .material()
        .index()
        .and_then(|index| materials.get(index).cloned());

    Ok(Mesh {
        vertices,
        indices,
        material,
    })
}

/// Give every triangle the normal of its own plane.
///
/// A mesh with no normals is common from a CAD exporter, and flat normals are
/// the honest answer for one — averaging into shared vertices would invent a
/// smoothness the file does not claim.
///
/// Vertices are **split** so each triangle owns its three, because a shared
/// vertex can only carry one normal. That is the same reason the cube has
/// twenty-four vertices rather than eight.
fn generate_flat_normals(vertices: &mut Vec<Vertex>, indices: &mut Vec<u32>) {
    let mut split = Vec::with_capacity(indices.len());

    for triangle in indices.chunks_exact(3) {
        let corners = [
            vertices[triangle[0] as usize],
            vertices[triangle[1] as usize],
            vertices[triangle[2] as usize],
        ];

        let edge_one = subtract(corners[1].position, corners[0].position);
        let edge_two = subtract(corners[2].position, corners[0].position);
        let normal = normalize(cross(edge_one, edge_two));

        for corner in corners {
            split.push(Vertex { normal, ..corner });
        }
    }

    *indices = (0..split.len() as u32).collect();
    *vertices = split;
}

fn subtract(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn cross(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

/// Scale to unit length, or return the zero vector for a degenerate triangle.
///
/// A zero normal is wrong for lighting and is the correct thing to produce for a
/// triangle that has no plane — inventing a direction would hide the degenerate
/// geometry rather than making it visible.
fn normalize(vector: [f32; 3]) -> [f32; 3] {
    let length = (vector[0] * vector[0] + vector[1] * vector[1] + vector[2] * vector[2]).sqrt();

    if length == 0.0 {
        return [0.0, 0.0, 0.0];
    }

    [vector[0] / length, vector[1] / length, vector[2] / length]
}

/// Recursively gather `.gltf` and `.glb` files.
fn collect_models(directory: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(directory)
        .with_context(|| format!("reading directory {}", directory.display()))?;

    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
        let path = entry.path();

        if path.is_dir() {
            collect_models(&path, found)?;
            continue;
        }

        let is_model = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension == "gltf" || extension == "glb");

        if is_model {
            found.push(path);
        }
    }

    Ok(())
}

/// Cook every material in one glTF file, returning each one's logical path by
/// material index.
///
/// Materials are cooked before meshes because a primitive names the material it
/// uses, and naming one that was never written would be a dangling reference in
/// the cache.
fn cook_materials(
    cache: &Cache,
    relative: &Path,
    document: &gltf::Document,
    images: &[gltf::image::Data],
    key: &CacheKey,
    force: bool,
    summary: &mut Summary,
) -> Result<Vec<String>> {
    // glTF's *default* material, for primitives that name none. It is a real
    // material with defined values rather than an absence, so cooking it means a
    // primitive always has something to bind.
    let mut paths = Vec::with_capacity(document.materials().len() + 1);

    for (index, material) in document.materials().enumerate() {
        let name = material_name(&material, index);
        let logical = material_path(relative, &name);
        let artifact = cache.artifact(&logical);

        let cooked = read_material(&material, relative, images, cache, key, force, summary)?;

        if force || !cache.is_current(&artifact, key) {
            cache.prepare(&artifact)?;
            std::fs::write(&artifact, cooked.write())
                .with_context(|| format!("writing {}", artifact.display()))?;
            cache.record(&artifact, key)?;

            info!(
                logical,
                textures = cooked.textures.len(),
                alpha = ?cooked.alpha_mode,
                "cooked material"
            );
            summary.cooked += 1;
        } else {
            debug!(logical, "up to date");
            summary.skipped += 1;
        }

        paths.push(logical);
    }

    Ok(paths)
}

/// A material's name, or a stand-in derived from its index.
fn material_name(material: &gltf::Material<'_>, index: usize) -> String {
    material
        .name()
        .map(sanitise)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("material{index}"))
}

/// Where a cooked material lives.
fn material_path(relative: &Path, name: &str) -> String {
    let stem = stem_segments(relative);

    format!("materials/{}.{name}.mat", stem.join("/"))
}

/// Translate one glTF material, cooking the images it references.
fn read_material(
    material: &gltf::Material<'_>,
    relative: &Path,
    images: &[gltf::image::Data],
    cache: &Cache,
    key: &CacheKey,
    force: bool,
    summary: &mut Summary,
) -> Result<Material> {
    let pbr = material.pbr_metallic_roughness();
    let mut textures = Vec::new();

    let mut take = |slot: TextureSlot, info: Option<gltf::texture::Texture<'_>>| -> Result<()> {
        let Some(texture) = info else {
            return Ok(());
        };

        let index = texture.source().index();
        let logical = image_path(relative, index);

        cook_image(cache, &logical, images, index, key, force, summary)?;
        textures.push((slot, logical));

        Ok(())
    };

    take(
        TextureSlot::BaseColor,
        pbr.base_color_texture().map(|info| info.texture()),
    )?;
    take(
        TextureSlot::MetallicRoughness,
        pbr.metallic_roughness_texture().map(|info| info.texture()),
    )?;
    take(
        TextureSlot::Normal,
        material.normal_texture().map(|info| info.texture()),
    )?;
    take(
        TextureSlot::Emissive,
        material.emissive_texture().map(|info| info.texture()),
    )?;

    Ok(Material {
        base_color: pbr.base_color_factor(),
        metallic: pbr.metallic_factor(),
        roughness: pbr.roughness_factor(),
        emissive: material.emissive_factor(),
        // glTF only defines a cutoff for the masked mode and leaves it absent
        // otherwise; the format stores one unconditionally, so an unused value
        // still has to be something rather than uninitialised.
        alpha_cutoff: material.alpha_cutoff().unwrap_or(0.5),
        alpha_mode: match material.alpha_mode() {
            gltf::material::AlphaMode::Opaque => AlphaMode::Opaque,
            gltf::material::AlphaMode::Mask => AlphaMode::Mask,
            gltf::material::AlphaMode::Blend => AlphaMode::Blend,
        },
        double_sided: material.double_sided(),
        textures,
    })
}

/// Where a cooked image from inside a glTF lives.
///
/// Keyed by the *image* index rather than the texture index: several textures
/// can share one image with different samplers, and cooking it twice would put
/// the same pixels in the cache under two names.
fn image_path(relative: &Path, image: usize) -> String {
    let stem = stem_segments(relative);

    format!("textures/{}.{image}.tex", stem.join("/"))
}

/// Cook one image out of a glTF, if it is not already current.
///
/// These do not go through `texture_import`, which scans for `.png` files on
/// disk: a glTF's images may be embedded as base64 or live in a GLB's binary
/// chunk, where there is no file to find. `gltf::import` has already decoded
/// them, so this cooks the pixels it was handed.
fn cook_image(
    cache: &Cache,
    logical: &str,
    images: &[gltf::image::Data],
    index: usize,
    key: &CacheKey,
    force: bool,
    summary: &mut Summary,
) -> Result<()> {
    let artifact = cache.artifact(logical);

    if !force && cache.is_current(&artifact, key) {
        debug!(logical, "up to date");
        summary.skipped += 1;
        return Ok(());
    }

    let image = images
        .get(index)
        .with_context(|| format!("image {index} is referenced but not present"))?;

    let decoded = Texture {
        width: image.width,
        height: image.height,
        format: Format::Rgba8,
        pixels: to_rgba8(image)?,
    };

    let compressed = crate::texture_import::compress(decoded);

    cache.prepare(&artifact)?;
    std::fs::write(&artifact, compressed.write())
        .with_context(|| format!("writing {}", artifact.display()))?;
    cache.record(&artifact, key)?;

    info!(
        logical,
        width = compressed.width,
        height = compressed.height,
        "cooked texture"
    );
    summary.cooked += 1;

    Ok(())
}

/// Expand a glTF image into eight-bit RGBA.
///
/// The same widening `texture_import` does for PNGs, over the formats
/// `gltf::import` produces. Sixteen-bit samples are narrowed to their high byte,
/// and formats without alpha gain an opaque one — a missing alpha channel read
/// as zero is the failure that looks like the object vanished.
fn to_rgba8(image: &gltf::image::Data) -> Result<Vec<u8>> {
    use gltf::image::Format as In;

    let texels = image.width as usize * image.height as usize;
    let mut pixels = Vec::with_capacity(texels * 4);

    // Index 0 of each channel is its high byte at either depth, which narrows
    // sixteen-bit samples without a separate branch.
    let mut push = |red: u8, green: u8, blue: u8, alpha: u8| {
        pixels.extend_from_slice(&[red, green, blue, alpha]);
    };

    match image.format {
        In::R8 => {
            for sample in &image.pixels {
                push(*sample, *sample, *sample, 255);
            }
        }
        In::R8G8 => {
            for sample in image.pixels.chunks_exact(2) {
                push(sample[0], sample[0], sample[0], sample[1]);
            }
        }
        In::R8G8B8 => {
            for sample in image.pixels.chunks_exact(3) {
                push(sample[0], sample[1], sample[2], 255);
            }
        }
        In::R8G8B8A8 => {
            for sample in image.pixels.chunks_exact(4) {
                push(sample[0], sample[1], sample[2], sample[3]);
            }
        }
        In::R16 => {
            for sample in image.pixels.chunks_exact(2) {
                push(sample[1], sample[1], sample[1], 255);
            }
        }
        In::R16G16 => {
            for sample in image.pixels.chunks_exact(4) {
                push(sample[1], sample[1], sample[1], sample[3]);
            }
        }
        In::R16G16B16 => {
            for sample in image.pixels.chunks_exact(6) {
                push(sample[1], sample[3], sample[5], 255);
            }
        }
        In::R16G16B16A16 => {
            for sample in image.pixels.chunks_exact(8) {
                push(sample[1], sample[3], sample[5], sample[7]);
            }
        }
        In::R32G32B32FLOAT | In::R32G32B32A32FLOAT => {
            bail!(
                "floating-point images are not supported; {:?} would need an HDR \
                 cooked format, which arrives with IBL",
                image.format
            )
        }
    }

    Ok(pixels)
}
/// Flatten the node hierarchy into placed mesh instances.
///
/// A glTF node carries a transform and may reference a mesh; children inherit
/// their parent's transform. Walking the tree with the accumulated matrix is
/// what turns "this pillar, relative to this bay, relative to the building" into
/// a world position. Without it every mesh would sit at the origin, which for a
/// building means several hundred meshes in one heap.
///
/// The default scene is used when the file names one, and every scene otherwise
/// — a glTF may define several, and silently taking the first would drop the
/// rest with nothing to say so.
fn cook_model(
    cache: &Cache,
    relative: &Path,
    document: &gltf::Document,
    key: &CacheKey,
    force: bool,
    summary: &mut Summary,
) -> Result<()> {
    let logical = model_path(relative);
    let artifact = cache.artifact(&logical);

    if !force && cache.is_current(&artifact, key) {
        debug!(logical, "up to date");
        summary.skipped += 1;
        return Ok(());
    }

    let mut model = Model::default();

    let roots: Vec<gltf::Node<'_>> = match document.default_scene() {
        Some(scene) => scene.nodes().collect(),
        None => document.scenes().flat_map(|scene| scene.nodes()).collect(),
    };

    for node in roots {
        place(&node, IDENTITY, relative, &mut model);
    }

    cache.prepare(&artifact)?;
    std::fs::write(&artifact, model.write())
        .with_context(|| format!("writing {}", artifact.display()))?;
    cache.record(&artifact, key)?;

    info!(
        logical,
        instances = model.instances.len(),
        meshes = model.meshes().len(),
        "cooked model"
    );
    summary.cooked += 1;

    Ok(())
}

/// Column-major identity.
const IDENTITY: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0,
];

/// Where a cooked model lives.
fn model_path(relative: &Path) -> String {
    format!("models/{}.model", stem_segments(relative).join("/"))
}

/// Record one node's primitives, then recurse into its children.
fn place(node: &gltf::Node<'_>, parent: [f32; 16], relative: &Path, model: &mut Model) {
    let world = multiply(parent, flatten(node.transform().matrix()));

    if let Some(mesh) = node.mesh() {
        let name = mesh_name(&mesh, mesh.index());

        // One instance per *primitive*, because each primitive is its own
        // cooked artifact and its own draw call — a mesh of three materials is
        // three draws at the same transform.
        for primitive in 0..mesh.primitives().len() {
            model.instances.push(Instance {
                mesh: logical_path(relative, &name, primitive),
                transform: world,
            });
        }
    }

    for child in node.children() {
        place(&child, world, relative, model);
    }
}

/// `[[f32; 4]; 4]` as sixteen floats, preserving column-major order.
fn flatten(matrix: [[f32; 4]; 4]) -> [f32; 16] {
    let mut out = [0.0; 16];

    for (column, values) in matrix.iter().enumerate() {
        out[column * 4..column * 4 + 4].copy_from_slice(values);
    }

    out
}

/// Column-major matrix product: the transform `left` then `right` describes.
///
/// Written out rather than taken from `glam`, because `slop-cli` has no reason
/// to depend on the maths crate for one multiply — and because writing it makes
/// the column-major convention visible at the point it matters.
fn multiply(left: [f32; 16], right: [f32; 16]) -> [f32; 16] {
    let mut out = [0.0; 16];

    for column in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0;

            for step in 0..4 {
                // Column-major: element (row, step) of `left` is at
                // `step * 4 + row`.
                sum += left[step * 4 + row] * right[column * 4 + step];
            }

            out[column * 4 + row] = sum;
        }
    }

    out
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Column-major translation.
    fn translate(x: f32, y: f32, z: f32) -> [f32; 16] {
        let mut out = IDENTITY;
        out[12] = x;
        out[13] = y;
        out[14] = z;
        out
    }

    /// Column-major uniform scale.
    fn scale(factor: f32) -> [f32; 16] {
        let mut out = IDENTITY;
        out[0] = factor;
        out[5] = factor;
        out[10] = factor;
        out
    }

    #[test]
    fn flattening_preserves_column_major_order() {
        // glTF stores matrices column-major and so does the cooked format. A
        // row-major read transposes every rotation, which places objects at
        // plausible-but-wrong angles rather than failing.
        let matrix = [
            [1.0, 2.0, 3.0, 4.0],
            [5.0, 6.0, 7.0, 8.0],
            [9.0, 10.0, 11.0, 12.0],
            [13.0, 14.0, 15.0, 16.0],
        ];

        assert_eq!(
            flatten(matrix),
            [
                1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0,
                16.0
            ]
        );
    }

    #[test]
    fn multiplying_by_identity_changes_nothing() {
        let moved = translate(3.0, -1.0, 2.0);

        assert_eq!(multiply(IDENTITY, moved), moved);
        assert_eq!(multiply(moved, IDENTITY), moved);
    }

    #[test]
    fn a_parent_transform_applies_to_its_child() {
        // The operand order that is easy to invert and impossible to see. A
        // parent scaled by two, holding a child translated one unit along X,
        // puts that child at *two* units — the parent scales the child's offset.
        // Swapping the operands puts it at one, which reads as a plausible
        // layout rather than as a bug.
        let world = multiply(scale(2.0), translate(1.0, 0.0, 0.0));

        assert_eq!(world[12..15], [2.0, 0.0, 0.0]);
    }

    #[test]
    fn nested_translations_accumulate() {
        let world = multiply(
            multiply(translate(1.0, 0.0, 0.0), translate(0.0, 2.0, 0.0)),
            translate(0.0, 0.0, 3.0),
        );

        assert_eq!(world[12..15], [1.0, 2.0, 3.0]);
    }

    #[test]
    fn a_name_becomes_a_logical_path() {
        assert_eq!(
            logical_path(Path::new("props").join("crate.gltf").as_path(), "Body", 0),
            "meshes/props/crate.Body.0.mesh"
        );
    }

    #[test]
    fn a_logical_path_uses_forward_slashes_on_every_platform() {
        let logical = logical_path(Path::new("a").join("b").join("c.glb").as_path(), "M", 2);

        assert!(!logical.contains('\\'), "{logical}");
        assert_eq!(logical, "meshes/a/b/c.M.2.mesh");
    }

    #[test]
    fn each_primitive_gets_its_own_artifact() {
        // One source to many artifacts — the case a `Cooker` trait shaped around
        // shaders would have broken on.
        let source = Path::new("crate.gltf");

        assert_ne!(
            logical_path(source, "Body", 0),
            logical_path(source, "Body", 1)
        );
    }

    #[test]
    fn an_awkward_name_is_sanitised_rather_than_refused() {
        // An artist's file must not fail to cook over a space in a mesh name.
        assert_eq!(sanitise("Front Left/Wheel"), "Front_Left_Wheel");
        assert_eq!(sanitise("../escape"), "___escape");
        assert_eq!(sanitise("plain-name_1"), "plain-name_1");
    }

    #[test]
    fn a_sanitised_name_cannot_climb_out_of_the_cache() {
        // The VFS refuses `..` too, but a name that reached it would be a cook
        // failure rather than a load failure, and much later.
        let logical = logical_path(Path::new("m.gltf"), &sanitise("../../etc"), 0);

        assert!(!logical.contains(".."), "{logical}");
    }

    #[test]
    fn a_nameless_mesh_falls_back_to_its_index() {
        // Not an error: plenty of exporters omit names. The index is a worse key
        // because reordering the file changes it, which is why a name wins.
        assert_eq!(sanitise(""), "");
    }

    #[test]
    fn flat_normals_face_the_triangles_plane() {
        // A counter-clockwise triangle in the XY plane faces +Z.
        let mut vertices = vec![
            Vertex {
                position: [0.0, 0.0, 0.0],
                normal: [0.0, 0.0, 0.0],
                uv: [0.0, 0.0],
            },
            Vertex {
                position: [1.0, 0.0, 0.0],
                normal: [0.0, 0.0, 0.0],
                uv: [0.0, 0.0],
            },
            Vertex {
                position: [0.0, 1.0, 0.0],
                normal: [0.0, 0.0, 0.0],
                uv: [0.0, 0.0],
            },
        ];
        let mut indices = vec![0, 1, 2];

        generate_flat_normals(&mut vertices, &mut indices);

        assert_eq!(vertices.len(), 3);
        for vertex in &vertices {
            assert_eq!(vertex.normal, [0.0, 0.0, 1.0]);
        }
    }

    #[test]
    fn generating_normals_splits_shared_vertices() {
        // A shared vertex can carry only one normal, so two triangles meeting at
        // one corner must not average into it — the same reason the cube has
        // twenty-four vertices rather than eight.
        let mut vertices = vec![
            Vertex {
                position: [0.0, 0.0, 0.0],
                normal: [0.0; 3],
                uv: [0.0; 2],
            },
            Vertex {
                position: [1.0, 0.0, 0.0],
                normal: [0.0; 3],
                uv: [0.0; 2],
            },
            Vertex {
                position: [0.0, 1.0, 0.0],
                normal: [0.0; 3],
                uv: [0.0; 2],
            },
            Vertex {
                position: [0.0, 0.0, 1.0],
                normal: [0.0; 3],
                uv: [0.0; 2],
            },
        ];
        // Two triangles sharing vertex 0, in different planes.
        let mut indices = vec![0, 1, 2, 0, 3, 1];

        generate_flat_normals(&mut vertices, &mut indices);

        assert_eq!(vertices.len(), 6, "every triangle owns its corners");
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 5]);
        assert_ne!(
            vertices[0].normal, vertices[3].normal,
            "the two faces point different ways"
        );
    }

    #[test]
    fn a_degenerate_triangle_gets_a_zero_normal_rather_than_a_guess() {
        // Wrong for lighting and correct as a report: the triangle has no plane,
        // and inventing a direction would hide that.
        let mut vertices = vec![
            Vertex {
                position: [0.0, 0.0, 0.0],
                normal: [0.0; 3],
                uv: [0.0; 2],
            };
            3
        ];
        let mut indices = vec![0, 1, 2];

        generate_flat_normals(&mut vertices, &mut indices);

        assert_eq!(vertices[0].normal, [0.0, 0.0, 0.0]);
    }
}
