//! The entity inspector — every component of every entity, live and editable.
//!
//! `docs/DESIGN.md` §10.2, and the last of M2's exit criteria. What makes this
//! worth having is that **it knows nothing about any component type**. It walks
//! the [`TypeRegistry`](slop_reflect::TypeRegistry), asks each type what fields
//! it has, and builds widgets from the answer — so a component added tomorrow,
//! or one declared by a guest module that this binary was never compiled
//! against, appears without a line of UI code being written for it.
//!
//! That is the whole reason `slop-reflect` exists, and this is the first thing
//! to actually depend on it at runtime rather than in a test.
//!
//! # Reading and writing go through the same reflection path
//!
//! [`World::component_value`] produces a [`Value`], and
//! [`World::insert_value`] consumes one. Neither is generic over a Rust type, so
//! a host-native component and a guest-declared one are inspected identically —
//! which is what `docs/DESIGN.md` §2.13 needs of the module boundary.
//!
//! # Why an edit is written back whole
//!
//! A change to one field rebuilds the entire component value and re-inserts it.
//! That is more work than patching bytes at a field's offset, and it is the
//! right trade: `insert_value` validates the whole value before writing
//! anything, so a malformed edit cannot leave a component half-written. Nothing
//! here is in a hot path — a human is typing.

use slop_ecs::{Entity, World};
use slop_reflect::{TypeId, Value};

/// Which entity the inspector is showing, and which are expanded.
///
/// Kept by the caller across frames because immediate mode means this function
/// runs from scratch every frame — see [`crate::debug_ui`]. Selection is state
/// about the *interface*, not about the world, so it lives here rather than
/// being invented from what the world happens to contain.
#[derive(Debug, Default)]
pub struct InspectorState {
    /// The entity whose components are shown, if it still exists.
    selected: Option<Entity>,
    /// Text being typed into a field, keyed by the field's path.
    ///
    /// **Held as text rather than parsed on every keystroke.** Parsing directly
    /// would make "1.5" unreachable: typing `1.` parses as `1`, which is written
    /// back and re-rendered as "1", deleting the decimal point as it is typed.
    editing: std::collections::HashMap<String, String>,
}

impl InspectorState {
    /// Which entity is selected, if any.
    #[must_use]
    pub fn selected(&self) -> Option<Entity> {
        self.selected
    }

    /// Select an entity, or clear the selection with `None`.
    pub fn select(&mut self, entity: Option<Entity>) {
        if self.selected != entity {
            // Dropped on purpose: half-typed text belongs to the field it was
            // typed into, and carrying it to another entity's identically named
            // field would apply an edit somewhere the user never looked.
            self.editing.clear();
        }

        self.selected = entity;
    }
}

/// Draw the inspector into `ui`.
///
/// Lists every entity, and for the selected one every component and field.
/// Editing a numeric or boolean field writes it back immediately.
///
/// Returns whether anything was changed, so a caller can react — re-running a
/// system, marking a scene dirty — without diffing the world itself.
pub fn inspector(ui: &mut egui::Ui, world: &mut World, state: &mut InspectorState) -> bool {
    // Collected before anything is drawn, because the closures below take
    // `&mut World` and cannot also hold a borrow of its archetypes.
    let entities: Vec<Entity> = world
        .archetypes()
        .iter()
        .flat_map(|archetype| archetype.entities().iter().copied())
        .collect();

    // A despawn between frames must not leave the inspector showing a component
    // list for something that no longer exists.
    if state.selected.is_some_and(|entity| !world.contains(entity)) {
        state.select(None);
    }

    ui.label(format!("{} entities", entities.len()));

    if entities.is_empty() {
        ui.label("nothing spawned");
        return false;
    }

    let mut selection = state.selected;

    egui::ComboBox::from_label("entity")
        .selected_text(
            selection.map_or_else(|| String::from("none"), |entity| describe(entity, world)),
        )
        .show_ui(ui, |ui| {
            for entity in &entities {
                let label = describe(*entity, world);
                ui.selectable_value(&mut selection, Some(*entity), label);
            }
        });

    if selection != state.selected {
        state.select(selection);
    }

    let Some(entity) = state.selected else {
        return false;
    };

    ui.separator();

    // The entity's own archetype says which components it has, which is both the
    // authoritative answer and the only one that does not require probing every
    // registered type in turn.
    let components: Vec<TypeId> = world
        .archetypes()
        .iter()
        .find(|archetype| archetype.entities().contains(&entity))
        .map(|archetype| archetype.signature().types().to_vec())
        .unwrap_or_default();

    if components.is_empty() {
        ui.label("no components");
        return false;
    }

    let mut changed = false;

    for type_id in components {
        let name = world.registry().get(type_id).map_or_else(
            || String::from("<unregistered>"),
            |info| info.path().as_str().to_owned(),
        );

        let Ok(value) = world.component_value(entity, type_id) else {
            // An opaque component has no describable fields. Named rather than
            // hidden: "this exists and cannot be shown" is information, and
            // omitting it would read as the component not being there.
            ui.collapsing(short_name(&name), |ui| {
                ui.label("opaque — no reflected fields");
            });
            continue;
        };

        let mut edited = value.clone();

        egui::CollapsingHeader::new(short_name(&name))
            .default_open(true)
            .show(ui, |ui| {
                if field(ui, &name, &mut edited, state) {
                    changed = true;
                }
            });

        if changed && edited != value && world.insert_value(entity, type_id, &edited).is_err() {
            // Logged rather than propagated: a rejected edit is the user typing
            // something the type cannot hold, not a reason to take the frame
            // down. The displayed value reverts on the next frame because it is
            // re-read from the world.
            slop_core::diagnostics::tracing::warn!(
                component = name,
                "the inspector's edit was rejected"
            );
        }
    }

    changed
}

/// Draw one value, recursing into structs. Returns whether it was edited.
///
/// `path` is the dotted route to this value and is what keys the in-progress
/// edit text — two `f32` fields called `x` on different components must not
/// share a text buffer.
fn field(ui: &mut egui::Ui, path: &str, value: &mut Value, state: &mut InspectorState) -> bool {
    match value {
        Value::Struct(structure) => {
            let mut changed = false;

            // Cloned because `Struct` hands out its fields by shared reference
            // and they are about to be replaced.
            let fields: Vec<(String, Value)> = structure.fields().to_vec();
            let mut rebuilt = Vec::with_capacity(fields.len());

            for (name, mut inner) in fields {
                let inner_path = format!("{path}.{name}");

                ui.horizontal(|ui| {
                    ui.label(&name);
                    if field(ui, &inner_path, &mut inner, state) {
                        changed = true;
                    }
                });

                rebuilt.push((name, inner));
            }

            if changed {
                *structure = slop_reflect::Struct::new(structure.path().clone(), rebuilt);
            }

            changed
        }

        Value::Bool(flag) => ui.checkbox(flag, "").changed(),

        Value::F32(number) => number_field(ui, path, state, number),
        Value::F64(number) => number_field(ui, path, state, number),
        Value::I8(number) => number_field(ui, path, state, number),
        Value::I16(number) => number_field(ui, path, state, number),
        Value::I32(number) => number_field(ui, path, state, number),
        Value::I64(number) => number_field(ui, path, state, number),
        Value::Isize(number) => number_field(ui, path, state, number),
        Value::U8(number) => number_field(ui, path, state, number),
        Value::U16(number) => number_field(ui, path, state, number),
        Value::U32(number) => number_field(ui, path, state, number),
        Value::U64(number) => number_field(ui, path, state, number),
        Value::Usize(number) => number_field(ui, path, state, number),

        Value::String(text) => ui.text_edit_singleline(text).changed(),

        // Shown, not edited. A `char` field is rare enough that a text box
        // accepting exactly one character is more surprising than useful.
        Value::Char(character) => {
            ui.label(character.to_string());
            false
        }
    }
}

/// A text box over anything parseable, editing through a held string.
///
/// The string is the point. Writing back on every keystroke and re-rendering
/// from the parsed value makes fractional input impossible: typing `1.` parses
/// to `1`, which renders as "1", and the decimal point vanishes as it is typed.
/// So the text is kept as typed, and the value is updated only when it parses.
fn number_field<T>(ui: &mut egui::Ui, path: &str, state: &mut InspectorState, value: &mut T) -> bool
where
    T: std::str::FromStr + std::fmt::Display + PartialEq + Copy,
{
    let mut text = state
        .editing
        .get(path)
        .cloned()
        .unwrap_or_else(|| value.to_string());

    let response = ui.add(egui::TextEdit::singleline(&mut text).desired_width(80.0));

    if !response.changed() {
        // Dropped once the field loses focus, so it re-renders from the world
        // and an unparseable leftover does not persist as a phantom edit.
        if response.lost_focus() {
            state.editing.remove(path);
        }

        return false;
    }

    state.editing.insert(String::from(path), text.clone());

    match text.parse::<T>() {
        Ok(parsed) if parsed != *value => {
            *value = parsed;
            true
        }
        // Unparseable, or parsed to what is already there. Neither is an error:
        // "1e" is a legitimate step on the way to typing "1e5".
        _ => false,
    }
}

/// `Position` rather than `game::components::Position`.
///
/// The full path is what disambiguates two types with the same name and is far
/// too long for a panel this narrow, so the header shows the last segment.
fn short_name(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

/// How an entity is labelled in the list.
fn describe(entity: Entity, world: &World) -> String {
    let components = world
        .archetypes()
        .iter()
        .find(|archetype| archetype.entities().contains(&entity))
        .map_or(0, |archetype| archetype.signature().types().len());

    // The generation is shown as well as the index because they are what makes a
    // handle unique: a despawn and respawn reuses the index, and two entities
    // labelled "3" in one session would be indistinguishable.
    format!(
        "{}v{} ({components} components)",
        entity.index(),
        entity.generation()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(slop_reflect::Reflect, Debug, Clone, Copy, PartialEq)]
    #[repr(C)]
    struct Position {
        x: f32,
        y: f32,
        visible: bool,
    }

    /// A world with one entity carrying one `Position`.
    fn world_with_a_position() -> (World, Entity) {
        let mut world = World::with_builtins();
        world
            .registry_mut()
            .register_native::<Position>()
            .expect("Position registers");

        let entity = world.spawn();
        world
            .insert(
                entity,
                Position {
                    x: 1.5,
                    y: -2.0,
                    visible: true,
                },
            )
            .expect("the entity is alive and Position is registered");

        (world, entity)
    }

    /// Run the inspector with no window, and collect every string it drew.
    ///
    /// egui needs no display: it turns input into shapes, and a text shape
    /// carries the string it will render. That is what makes this assertable
    /// without a GPU — and it is the same trick the overlay's golden test uses.
    fn drawn_text(world: &mut World, state: &mut InspectorState) -> Vec<String> {
        let context = egui::Context::default();

        let output = context.run_ui(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                inspector(ui, world, state);
            });
        });

        let mut text = Vec::new();
        collect_text(&output.shapes, &mut text);
        text
    }

    fn collect_text(shapes: &[egui::epaint::ClippedShape], out: &mut Vec<String>) {
        fn walk(shape: &egui::epaint::Shape, out: &mut Vec<String>) {
            match shape {
                egui::epaint::Shape::Text(text) => out.push(text.galley.text().to_owned()),
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, out);
                    }
                }
                _ => {}
            }
        }

        for clipped in shapes {
            walk(&clipped.shape, out);
        }
    }

    #[test]
    fn the_inspector_names_every_field_of_the_selected_component() {
        // The check that matters, and the one validation silence cannot give:
        // that field names reach the screen. An inspector that walked the
        // registry correctly and drew nothing would pass every other test here.
        //
        // **No UI code names `Position` or its fields.** They appear because
        // `slop-reflect` describes the type, which is the whole point.
        let (mut world, entity) = world_with_a_position();
        let mut state = InspectorState::default();
        state.select(Some(entity));

        let text = drawn_text(&mut world, &mut state).join(" ");

        assert!(text.contains("Position"), "component name missing: {text}");
        for field in ["x", "y", "visible"] {
            assert!(text.contains(field), "field '{field}' missing from: {text}");
        }
        assert!(text.contains("1.5"), "x's value missing from: {text}");
        assert!(text.contains("-2"), "y's value missing from: {text}");
    }

    #[test]
    fn nothing_is_shown_until_an_entity_is_selected() {
        let (mut world, _) = world_with_a_position();
        let mut state = InspectorState::default();

        let text = drawn_text(&mut world, &mut state).join(" ");

        assert!(text.contains("1 entities"), "the count is always shown");
        assert!(
            !text.contains("visible"),
            "no component fields before a selection: {text}"
        );
    }

    #[test]
    fn a_despawned_selection_clears_itself() {
        // Otherwise the next frame asks a dead entity for its components and
        // shows a stale list, or panics reading a row that has been reused.
        let (mut world, entity) = world_with_a_position();
        let mut state = InspectorState::default();
        state.select(Some(entity));

        assert!(world.despawn(entity));

        let text = drawn_text(&mut world, &mut state).join(" ");

        assert_eq!(state.selected(), None, "the selection must be dropped");
        assert!(text.contains("nothing spawned"), "{text}");
    }

    #[test]
    fn an_empty_world_says_so_rather_than_drawing_nothing() {
        let mut world = World::with_builtins();
        let mut state = InspectorState::default();

        let text = drawn_text(&mut world, &mut state).join(" ");

        assert!(text.contains("nothing spawned"), "{text}");
    }

    #[test]
    fn a_short_name_is_the_last_path_segment() {
        assert_eq!(short_name("game::components::Position"), "Position");
        assert_eq!(short_name("Position"), "Position");
        assert_eq!(short_name(""), "");
    }

    /// A handle that is merely distinct — nothing here dereferences it.
    fn entity(index: u32) -> Entity {
        Entity::from_raw(slop_core::RawHandle::from_bits(
            u64::from(index) | (1 << 32),
        ))
        .expect("generation 1 is a valid handle")
    }

    #[test]
    fn changing_the_selection_drops_half_typed_text() {
        // Otherwise text typed into one entity's `x` is applied to the next
        // entity's `x` — an edit somewhere the user never looked.
        let mut state = InspectorState::default();
        state.select(Some(entity(1)));
        state
            .editing
            .insert(String::from("Position.x"), String::from("1."));

        state.select(Some(entity(2)));

        assert!(state.editing.is_empty());
    }

    #[test]
    fn reselecting_the_same_entity_keeps_the_text() {
        // The guard matters: `select` is called every frame with whatever the
        // combo box reports, so clearing unconditionally would wipe the buffer
        // on every keystroke — and a decimal point would be untypable, which is
        // the bug the held text exists to prevent in the first place.
        let mut state = InspectorState::default();
        state.select(Some(entity(1)));
        state
            .editing
            .insert(String::from("Position.x"), String::from("1."));

        state.select(Some(entity(1)));

        assert_eq!(
            state.editing.get("Position.x").map(String::as_str),
            Some("1.")
        );
    }
}
