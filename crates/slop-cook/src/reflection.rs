//! Turning `slangc -reflection-json` output into the cooked reflection format.
//!
//! # The premise this replaced
//!
//! `docs/DESIGN.md` §2.11 stated that reflection "cannot be queried from shaders
//! compiled with the `slangc` command-line tool — it is available only through
//! the compilation API", and gave that as the reason library integration was
//! "mandatory, not a preference". It is not true: `-reflection-json` emits
//! everything a pipeline needs. The design document has been corrected, and the
//! library remains wanted for *other* reasons — link-time specialization above
//! all — which is a different and much weaker argument than the one it replaced.
//!
//! # Reading Slang's schema
//!
//! Navigated as a [`serde_json::Value`] rather than deserialized into typed
//! structs. Slang's schema is large, versioned by Slang, and this reads four
//! fields of it; a mirror of the whole thing would be a maintenance burden that
//! bought nothing, and the failure mode is identical either way — a schema change
//! is a runtime error, not a compile error. What matters is that the error says
//! *which* field was missing, which is what the paths below are for.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use slop_asset::shader::{Reflection, VertexFormat, VertexInput};

/// Translate reflection JSON into what the engine loads.
pub(crate) fn parse(json: &str) -> Result<Reflection> {
    let root: Value = serde_json::from_str(json).context("parsing reflection JSON")?;

    Ok(Reflection {
        push_constant_bytes: push_constant_bytes(&root)?,
        vertex_inputs: vertex_inputs(&root)?,
    })
}

/// The size of the push constant block, or zero if the shader has none.
///
/// Found by looking for the parameter bound as a `pushConstantBuffer` and
/// summing what its fields occupy. Slang reports each field's `offset` and
/// `size`, so the block is the furthest reach of any of them rather than the sum
/// — a struct with padding between fields is bigger than its parts.
fn push_constant_bytes(root: &Value) -> Result<u32> {
    let Some(parameters) = root.get("parameters").and_then(Value::as_array) else {
        return Ok(0);
    };

    let Some(block) = parameters.iter().find(|parameter| {
        parameter
            .pointer("/binding/kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "pushConstantBuffer")
    }) else {
        return Ok(0);
    };

    let Some(fields) = block
        .pointer("/type/elementType/fields")
        .and_then(Value::as_array)
    else {
        bail!("push constant block has no fields at /type/elementType/fields");
    };

    let mut reach = 0;

    for field in fields {
        let offset = field
            .pointer("/binding/offset")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let size = field
            .pointer("/binding/size")
            .and_then(Value::as_u64)
            .with_context(|| {
                format!(
                    "push constant field {} has no /binding/size",
                    field.get("name").and_then(Value::as_str).unwrap_or("?")
                )
            })?;

        reach = reach.max(offset + size);
    }

    u32::try_from(reach).context("push constant block is larger than 4 GiB, which cannot be right")
}

/// Every input the vertex entry point reads, in ascending location order.
fn vertex_inputs(root: &Value) -> Result<Vec<VertexInput>> {
    let Some(entry_points) = root.get("entryPoints").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let vertex: Vec<&Value> = entry_points
        .iter()
        .filter(|entry| {
            entry
                .get("stage")
                .and_then(Value::as_str)
                .is_some_and(|stage| stage == "vertex")
        })
        .collect();

    // One per file, because one file cooks to one artifact and the artifact
    // carries one layout. Refused rather than guessed: picking the first would
    // silently bind the wrong layout for the other. Supporting several means
    // keying the reflection by entry point and bumping `COOKER_VERSION`, which
    // is cheap by design — but it should happen when a shader needs it.
    let entry = match vertex.as_slice() {
        [] => return Ok(Vec::new()),
        [only] => only,
        several => bail!(
            "{} vertex entry points in one file; only one is supported, and the cooked \
             reflection carries a single vertex layout",
            several.len()
        ),
    };

    let Some(parameters) = entry.get("parameters").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut inputs = Vec::new();

    for parameter in parameters {
        collect(parameter, &mut inputs)?;
    }

    // Ascending location, which is the order a vertex buffer is packed in and
    // the order `Reflection::interleaved` assumes. Slang emits them in
    // declaration order, which is usually the same and is not guaranteed to be.
    inputs.sort_by_key(|input| input.location);

    Ok(inputs)
}

/// Collect the varying inputs of one parameter, descending through structs.
///
/// A vertex entry point usually takes a single `struct` parameter, and each of
/// its fields carries its own location. The struct itself also carries a binding
/// spanning all of them, which is why the fields are preferred when present:
/// taking the outer one would produce a single input at location 0 covering
/// everything.
fn collect(parameter: &Value, inputs: &mut Vec<VertexInput>) -> Result<()> {
    if let Some(fields) = parameter.pointer("/type/fields").and_then(Value::as_array) {
        for field in fields {
            collect(field, inputs)?;
        }

        return Ok(());
    }

    let kind = parameter
        .pointer("/binding/kind")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if kind != "varyingInput" {
        return Ok(());
    }

    let name = parameter
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<unnamed>");

    let location = parameter
        .pointer("/binding/index")
        .and_then(Value::as_u64)
        .with_context(|| format!("vertex input '{name}' has no /binding/index"))?;

    let format = format_of(parameter)
        .with_context(|| format!("vertex input '{name}' has a type this cooker cannot express"))?;

    inputs.push(VertexInput {
        location: u32::try_from(location).context("a vertex location above 4 billion")?,
        format,
    });

    Ok(())
}

/// The vertex format of one input's type.
///
/// Only float scalars and vectors, matching what the cooked format carries.
/// Anything else — an integer for skinning joints, a matrix spanning several
/// locations — is refused rather than approximated, because a wrong format binds
/// the buffer and renders garbage instead of failing.
fn format_of(parameter: &Value) -> Result<VertexFormat> {
    let kind = parameter
        .pointer("/type/kind")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let components = match kind {
        "scalar" => {
            scalar_is_float(parameter.pointer("/type/scalarType"))?;
            1
        }
        "vector" => {
            scalar_is_float(parameter.pointer("/type/elementType/scalarType"))?;
            parameter
                .pointer("/type/elementCount")
                .and_then(Value::as_u64)
                .context("a vector with no elementCount")?
        }
        other => bail!("type kind '{other}' is not a vertex format this cooker knows"),
    };

    let components = u32::try_from(components).context("an implausible component count")?;

    VertexFormat::from_components(components)
        .with_context(|| format!("{components} components is not a vertex format"))
}

/// Refuse anything that is not a 32-bit float.
fn scalar_is_float(scalar: Option<&Value>) -> Result<()> {
    match scalar.and_then(Value::as_str) {
        Some("float32") => Ok(()),
        Some(other) => bail!("scalar type '{other}' is not supported; only float32 is"),
        None => bail!("a type with no scalarType"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reflection for a shader shaped like `shaders/passes/cube.slang`.
    const CUBE: &str = r#"{
        "parameters": [
            {
                "name": "g_textures",
                "binding": {"kind": "descriptorTableSlot", "index": 0}
            },
            {
                "name": "g_push",
                "binding": {"kind": "pushConstantBuffer", "index": 0},
                "type": {
                    "kind": "constantBuffer",
                    "elementType": {
                        "kind": "struct",
                        "fields": [
                            {"name": "mvp", "binding": {"kind": "uniform", "offset": 0, "size": 64}},
                            {"name": "model", "binding": {"kind": "uniform", "offset": 64, "size": 64}},
                            {"name": "texture", "binding": {"kind": "uniform", "offset": 128, "size": 4}},
                            {"name": "sampler", "binding": {"kind": "uniform", "offset": 132, "size": 4}}
                        ]
                    }
                }
            }
        ],
        "entryPoints": [
            {
                "name": "vertexMain",
                "stage": "vertex",
                "parameters": [
                    {
                        "name": "input",
                        "binding": {"kind": "varyingInput", "index": 0, "count": 3},
                        "type": {
                            "kind": "struct",
                            "fields": [
                                {
                                    "name": "position",
                                    "binding": {"kind": "varyingInput", "index": 0},
                                    "type": {"kind": "vector", "elementCount": 3,
                                             "elementType": {"kind": "scalar", "scalarType": "float32"}}
                                },
                                {
                                    "name": "normal",
                                    "binding": {"kind": "varyingInput", "index": 1},
                                    "type": {"kind": "vector", "elementCount": 3,
                                             "elementType": {"kind": "scalar", "scalarType": "float32"}}
                                },
                                {
                                    "name": "uv",
                                    "binding": {"kind": "varyingInput", "index": 2},
                                    "type": {"kind": "vector", "elementCount": 2,
                                             "elementType": {"kind": "scalar", "scalarType": "float32"}}
                                }
                            ]
                        }
                    }
                ]
            },
            {"name": "fragmentMain", "stage": "fragment", "parameters": []}
        ]
    }"#;

    #[test]
    fn the_cube_shader_reflects_three_inputs_and_a_push_block() {
        let reflection = parse(CUBE).expect("valid reflection");

        assert_eq!(reflection.push_constant_bytes, 136);
        assert_eq!(
            reflection.vertex_inputs,
            vec![
                VertexInput {
                    location: 0,
                    format: VertexFormat::Float32x3
                },
                VertexInput {
                    location: 1,
                    format: VertexFormat::Float32x3
                },
                VertexInput {
                    location: 2,
                    format: VertexFormat::Float32x2
                },
            ]
        );
    }

    #[test]
    fn the_layout_it_produces_is_the_one_the_mesh_format_writes() {
        // The check that closes the loop: the cooked mesh packs position,
        // normal and uv tightly in that order, and this must agree or every
        // vertex is read at the wrong offset.
        let (placed, stride) = parse(CUBE).expect("valid").interleaved();

        assert_eq!(stride, 32, "the cooked Vertex is 32 bytes");
        assert_eq!(placed[0].offset, 0);
        assert_eq!(placed[1].offset, 12);
        assert_eq!(placed[2].offset, 24);
    }

    #[test]
    fn a_shader_with_no_vertex_stage_reflects_nothing() {
        let json = r#"{"entryPoints": [{"name": "main", "stage": "compute", "parameters": []}]}"#;
        let reflection = parse(json).expect("valid");

        assert!(reflection.vertex_inputs.is_empty());
        assert_eq!(reflection.push_constant_bytes, 0);
    }

    #[test]
    fn a_shader_with_no_push_constants_reports_zero() {
        // The triangle. A block size invented here would size a pipeline layout
        // for constants the shader never reads.
        let json = r#"{
            "parameters": [{"name": "g", "binding": {"kind": "descriptorTableSlot", "index": 0}}],
            "entryPoints": []
        }"#;

        assert_eq!(parse(json).expect("valid").push_constant_bytes, 0);
    }

    #[test]
    fn push_constant_size_spans_padding_rather_than_summing_fields() {
        // A struct whose fields do not abut. Summing sizes would under-report
        // the block and truncate the last field on upload.
        let json = r#"{
            "parameters": [{
                "name": "g_push",
                "binding": {"kind": "pushConstantBuffer", "index": 0},
                "type": {"kind": "constantBuffer", "elementType": {"kind": "struct", "fields": [
                    {"name": "a", "binding": {"kind": "uniform", "offset": 0, "size": 4}},
                    {"name": "b", "binding": {"kind": "uniform", "offset": 16, "size": 4}}
                ]}}
            }],
            "entryPoints": []
        }"#;

        assert_eq!(parse(json).expect("valid").push_constant_bytes, 20);
    }

    #[test]
    fn inputs_come_back_in_location_order_whatever_order_they_appear_in() {
        let json = r#"{
            "entryPoints": [{"name": "v", "stage": "vertex", "parameters": [
                {"name": "uv", "binding": {"kind": "varyingInput", "index": 2},
                 "type": {"kind": "vector", "elementCount": 2,
                          "elementType": {"kind": "scalar", "scalarType": "float32"}}},
                {"name": "position", "binding": {"kind": "varyingInput", "index": 0},
                 "type": {"kind": "vector", "elementCount": 3,
                          "elementType": {"kind": "scalar", "scalarType": "float32"}}}
            ]}]
        }"#;

        let locations: Vec<u32> = parse(json)
            .expect("valid")
            .vertex_inputs
            .iter()
            .map(|input| input.location)
            .collect();

        assert_eq!(locations, vec![0, 2]);
    }

    #[test]
    fn two_vertex_entry_points_are_refused_rather_than_guessed() {
        // Picking the first would silently bind the wrong layout for the other.
        let json = r#"{"entryPoints": [
            {"name": "a", "stage": "vertex", "parameters": []},
            {"name": "b", "stage": "vertex", "parameters": []}
        ]}"#;

        let error = parse(json).expect_err("two vertex entry points");

        assert!(
            error.to_string().contains("only one is supported"),
            "{error}"
        );
    }

    #[test]
    fn an_integer_input_is_refused_rather_than_read_as_float() {
        // Skinning joint indices, eventually. A wrong format does not fail — it
        // binds the buffer and renders garbage.
        let json = r#"{
            "entryPoints": [{"name": "v", "stage": "vertex", "parameters": [
                {"name": "joints", "binding": {"kind": "varyingInput", "index": 0},
                 "type": {"kind": "vector", "elementCount": 4,
                          "elementType": {"kind": "scalar", "scalarType": "uint32"}}}
            ]}]
        }"#;

        let error = parse(json).expect_err("integer input");

        assert!(error.to_string().contains("joints"), "{error}");
    }

    #[test]
    fn a_system_value_input_is_not_a_vertex_attribute() {
        // `SV_VertexID` is not bound from a buffer, so it must not appear in the
        // layout. The triangle is entirely this.
        let json = r#"{
            "entryPoints": [{"name": "v", "stage": "vertex", "parameters": [
                {"name": "id", "binding": {"kind": "systemValue"},
                 "type": {"kind": "scalar", "scalarType": "uint32"}}
            ]}]
        }"#;

        assert!(parse(json).expect("valid").vertex_inputs.is_empty());
    }

    #[test]
    fn malformed_json_says_so() {
        assert!(parse("{not json").is_err());
    }
}
