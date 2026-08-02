//! A whole world as text, and back.
//!
//! ```text
//! slop world 1
//!
//! resource game::Gravity {
//!     value: -9.8,
//! }
//!
//! entity 0 {
//!     game::Position {
//!         x: 1.0,
//!         y: 2.0,
//!         z: 0.0,
//!     }
//!     game::Health {
//!         current: 100,
//!         maximum: 100,
//!     }
//! }
//! ```
//!
//! # "World", not "scene"
//!
//! `docs/DESIGN.md` §4 reserves `slop-scene` for the runtime spatial structure —
//! hierarchy, transform propagation, BVH culling, LOD, streaming. That is a
//! different thing from a file, and letting two crates both claim the word would
//! cost a paragraph of explanation every time either came up.
//!
//! So this is the *world* as a document, and it lives here because what it
//! serializes is `slop-ecs`'s own data model. Putting it in `slop-scene` would
//! mean anything wanting to save a world depended on the culling crate.
//!
//! The layering, top to bottom:
//!
//! | | Owns |
//! |---|---|
//! | `slop-reflect` | [`Value`] ↔ text |
//! | `slop-ecs` | [`World`] ↔ `Value`s, and this container |
//! | `slop-asset` (M2) | the file — VFS, async load, hot reload, cooking |
//!
//! # Entity indices are written, and that is deliberate
//!
//! An entity's runtime id carries a generation and a slot that mean nothing in
//! the next process, so the file numbers entities itself and
//! [`load`] returns the mapping from file index to freshly spawned [`Entity`].
//!
//! Nothing needs that mapping yet — no component can hold an `Entity`, because
//! `Entity` is not [`Reflect`](slop_reflect::Reflect). It is written now because
//! the alternative is discovering at M2, when hierarchy lands, that every file
//! ever saved has no way to express "my parent is that one". The index is the
//! seam; resolving references through it is the implementation.
//!
//! # Ordering
//!
//! `docs/DESIGN.md` §2.14 wants the same world to produce the same file. Both
//! resources and each entity's components are written **sorted by type path** —
//! not by [`TypeId`], which is a hash and would order a
//! file arbitrarily for a human reading it.
//!
//! Entities are grouped by archetype, and the archetypes are visited in
//! **signature order** rather than in the order the world happened to create
//! them. Creation order depends on the sequence of inserts a world saw, and a
//! world rebuilt by [`load`] sees a different one — so writing in it would mean
//! a file was not a fixed point, and every save-after-load would be a spurious
//! diff. A test pins that: save, load, save again, and compare the text.
//!
//! What is *not* canonical is the order of entities within one archetype, which
//! is the order they arrived. Two worlds holding the same entities, built in
//! different orders, still produce files that differ in entity order — fine for
//! a round trip, unhelpful for comparing two independently built worlds, and
//! recorded in `docs/PLAN.md` §6.1.

use slop_core::FxHashMap;
use slop_reflect::{Reader, TextError, TypeId, TypePath, Value, to_text_body};
use thiserror::Error;

use crate::{EcsError, Entity, World};

/// The format's magic and version, on the first line of every file.
const HEADER: &str = "slop world";

/// What this module knows how to read.
///
/// Written into every file and checked on load. A version bump is what lets an
/// old file be recognised as old rather than as malformed.
const VERSION: u64 = 1;

/// Spaces per nesting level, matching the value format.
const INDENT: usize = 4;

/// Why a world could not be read from text.
#[derive(Debug, PartialEq, Eq, Error)]
pub enum LoadError {
    /// The text is not this format, or not a version this understands.
    #[error("line {line}: {message}")]
    Malformed {
        /// One-based line number.
        line: usize,
        /// What was wrong.
        message: String,
    },

    /// A value did not parse.
    #[error(transparent)]
    Text(#[from] TextError),

    /// The world refused what the file asked for.
    #[error(transparent)]
    World(#[from] EcsError),

    /// The file names a type the registry does not have.
    ///
    /// Loudly, rather than skipping it: a component silently dropped on load is
    /// silently *lost* on the next save, which is the failure `docs/DESIGN.md`
    /// §2.4 exists to prevent.
    #[error("line {line}: `{path}` is not a registered type")]
    UnknownType {
        /// One-based line number.
        line: usize,
        /// The path the file named.
        path: String,
    },
}

/// What [`save`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    /// The file's contents.
    pub text: String,

    /// Types left out because nothing can look inside them.
    ///
    /// [`TypeKind::Opaque`](slop_reflect::TypeKind::Opaque) is the statement
    /// "its internals are the owning crate's business", so an opaque component
    /// has nothing to write. Refusing to save the whole world over one would
    /// make a runtime-only component — a cached GPU handle, say — permanently
    /// unsaveable; dropping it silently is the data loss §2.4 warns about.
    ///
    /// So it is reported, and the caller decides. A caller that wants strictness
    /// asserts this is empty.
    pub skipped: Vec<TypePath>,
}

/// What [`load`] produced.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    /// File entity index to the entity spawned for it.
    ///
    /// The remapping table an entity-valued component will resolve through — see
    /// the module documentation.
    pub entities: FxHashMap<u64, Entity>,
}

/// Write the whole world as text.
///
/// Entities keep no identity across this: [`load`] spawns fresh ones and reports
/// the mapping.
pub fn save(world: &World) -> Saved {
    let mut out = String::new();
    let mut skipped = Vec::new();

    out.push_str(HEADER);
    out.push(' ');
    out.push_str(&VERSION.to_string());
    out.push('\n');

    for type_id in world.resource_types() {
        let Some(info) = world.registry().get(type_id) else {
            continue;
        };

        match world.resource_value(type_id) {
            Ok(Some(value)) => {
                out.push('\n');
                out.push_str("resource ");
                out.push_str(info.path().as_str());
                out.push(' ');
                out.push_str(&reindent(&to_text_body(&value), 0));
                out.push('\n');
            }
            // Opaque, or a field type that has gone missing. Either way there is
            // nothing to write, and the caller is told which.
            Ok(None) | Err(_) => skipped.push(info.path().clone()),
        }
    }

    // Archetypes in signature order rather than in creation order. Creation
    // order depends on the sequence of inserts a world happened to see, which
    // differs between a world built by a game and the same world rebuilt by
    // `load` — so writing in it would mean a file was not a fixed point, and
    // every save-after-load would be a spurious diff.
    let mut tables: Vec<(Vec<TypePath>, &crate::Archetype)> = world
        .archetypes()
        .iter()
        .filter(|archetype| !archetype.is_empty())
        .map(|archetype| {
            (
                sorted_paths(archetype.signature().types(), world),
                archetype,
            )
        })
        .collect();
    tables.sort_by(|left, right| left.0.cmp(&right.0));

    let mut index = 0_u64;
    for (paths, archetype) in tables {
        // Sorted by path so the file reads and diffs in a human order rather
        // than in `TypeId` hash order.
        let types: Vec<TypeId> = paths
            .iter()
            .filter_map(|path| world.registry().get_by_path(path.as_str()))
            .map(slop_reflect::TypeInfo::id)
            .collect();

        for entity in archetype.entities() {
            out.push('\n');
            out.push_str(&format!("entity {index} {{\n"));

            for type_id in &types {
                let Some(info) = world.registry().get(*type_id) else {
                    continue;
                };

                match world.component_value(*entity, *type_id) {
                    Ok(value) => {
                        out.push_str(&" ".repeat(INDENT));
                        out.push_str(info.path().as_str());
                        out.push(' ');
                        out.push_str(&reindent(&to_text_body(&value), 1));
                        out.push('\n');
                    }
                    Err(_) => {
                        if !skipped.contains(info.path()) {
                            skipped.push(info.path().clone());
                        }
                    }
                }
            }

            out.push_str("}\n");
            index += 1;
        }
    }

    Saved { text: out, skipped }
}

/// A signature's type paths, sorted, for ordering both tables and components.
///
/// By path rather than by [`TypeId`], which is a hash — ordering a file by one
/// would put components in an order that looks random to whoever reads it.
fn sorted_paths(types: &[TypeId], world: &World) -> Vec<TypePath> {
    let mut paths: Vec<TypePath> = types
        .iter()
        .filter_map(|type_id| world.registry().get(*type_id))
        .map(|info| info.path().clone())
        .collect();
    paths.sort();

    paths
}

/// Shift every line after the first by `depth` levels.
///
/// The value writer produces text at depth zero; a component sits inside an
/// entity block. Rewriting the text is safe because the format never emits a
/// literal newline inside a value — a `\n` in a string is written escaped.
fn reindent(text: &str, depth: usize) -> String {
    if depth == 0 {
        return text.to_owned();
    }

    let padding = " ".repeat(depth * INDENT);
    let mut lines = text.lines();
    let mut out = lines.next().unwrap_or_default().to_owned();

    for line in lines {
        out.push('\n');
        out.push_str(&padding);
        out.push_str(line);
    }

    out
}

/// Read a world from text, spawning into `world`.
///
/// **Additive**: entities are spawned alongside whatever is already there, and
/// resources replace those of the same type. Loading into a fresh
/// [`World`] is what "replace" means, and is the caller's to do — there is no
/// clear-and-load, because a half-cleared world after a failed load would be
/// worse than either outcome.
///
/// # Errors
///
/// [`LoadError`] for a malformed file, a value that does not parse, a type the
/// registry does not have, or a value the world refuses.
///
/// **Nothing is spawned when this returns an error.** The whole file is parsed
/// and every value checked before the world is touched, so a file with one bad
/// field leaves no half-loaded remains — the same check-then-commit the
/// `serialize` module uses per value, applied to the file.
pub fn load(text: &str, world: &mut World) -> Result<Loaded, LoadError> {
    let parsed = parse(text, world)?;

    let mut loaded = Loaded::default();

    for (type_id, value) in parsed.resources {
        world.insert_resource_value(type_id, &value)?;
    }

    for (index, components) in parsed.entities {
        let entity = world.spawn();
        loaded.entities.insert(index, entity);

        for (type_id, value) in components {
            world.insert_value(entity, type_id, &value)?;
        }
    }

    Ok(loaded)
}

/// Everything the file said, checked, before any of it is applied.
struct Parsed {
    resources: Vec<(TypeId, Value)>,
    entities: Vec<(u64, Vec<(TypeId, Value)>)>,
}

/// Read the file without touching the world.
fn parse(text: &str, world: &World) -> Result<Parsed, LoadError> {
    let mut reader = Reader::new(text);

    reader.skip_trivia();
    if !reader.accept(HEADER) {
        return Err(LoadError::Malformed {
            line: 1,
            message: format!("expected a `{HEADER}` header"),
        });
    }

    let version = reader.index()?;
    if version != VERSION {
        return Err(LoadError::Malformed {
            line: 1,
            message: format!("version {version} is not {VERSION}"),
        });
    }

    let mut parsed = Parsed {
        resources: Vec::new(),
        entities: Vec::new(),
    };
    let mut seen_indices = Vec::new();

    loop {
        reader.skip_trivia();
        if reader.at_end() {
            break;
        }

        if reader.accept("resource") {
            let (type_id, value) = read_typed_value(&mut reader, world)?;
            parsed.resources.push((type_id, value));
            continue;
        }

        if reader.accept("entity") {
            let index = reader.index()?;

            if seen_indices.contains(&index) {
                return Err(LoadError::Malformed {
                    line: reader.error("").line,
                    message: format!("entity {index} is declared twice"),
                });
            }
            seen_indices.push(index);

            reader.expect("{")?;

            let mut components = Vec::new();
            while !reader.peek_is('}') {
                if reader.at_end() {
                    return Err(LoadError::Malformed {
                        line: reader.error("").line,
                        message: format!("entity {index} is not closed"),
                    });
                }

                components.push(read_typed_value(&mut reader, world)?);
            }
            reader.expect("}")?;

            parsed.entities.push((index, components));
            continue;
        }

        return Err(LoadError::Malformed {
            line: reader.error("").line,
            message: "expected `resource`, `entity`, or end of file".to_owned(),
        });
    }

    Ok(parsed)
}

/// Read `<path> <value>`, resolving the path to decide how to read the value.
///
/// The path is written by the container rather than by the value, so that a
/// primitive component is identifiable too — `u32 7` says as much as
/// `game::Position { … }` does.
fn read_typed_value(reader: &mut Reader<'_>, world: &World) -> Result<(TypeId, Value), LoadError> {
    reader.skip_trivia();

    let line = reader.error("").line;
    let path = reader.path();

    if path.is_empty() {
        return Err(LoadError::Malformed {
            line,
            message: "expected a type path".to_owned(),
        });
    }

    let info = world
        .registry()
        .get_by_path(path)
        .ok_or_else(|| LoadError::UnknownType {
            line,
            path: path.to_owned(),
        })?;

    let value = reader.value(info, world.registry())?;

    Ok((info.id(), value))
}
