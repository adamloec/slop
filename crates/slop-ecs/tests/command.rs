//! Deferred structural change, end to end.
//!
//! Two failure modes are worth more than the rest and most of this file is
//! aimed at them:
//!
//! - **A staged component is destroyed exactly once.** A buffer holds owned
//!   values, and every exit — applied, dropped, cleared, target already dead,
//!   type unregistered — has to run one destructor. `Tracked` counts them, so a
//!   leak and a double free are both assertion failures rather than something
//!   only Miri notices.
//! - **A `Target` addresses the entity it was meant to.** Ordinals are resolved
//!   against a list built during application, and getting that wrong reads as one
//!   entity acquiring another's components.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use slop_ecs::{CommandBuffer, EcsError, Entity, Target, World};
use slop_reflect::{Reflect, Transfer, TypeInfo, TypeKind};

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Position {
    x: f32,
    y: f32,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Velocity {
    dx: f32,
    dy: f32,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Health {
    value: u32,
}

/// Owns a heap allocation, so a leak or a double free is observable.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[repr(C)]
struct Name {
    text: String,
}

/// Zero-sized. The staging area allocates nothing for it, which is a distinct
/// path from every other component here.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Player {}

/// Alignment 16 with a one-byte payload, which is what catches a staging area
/// that aligns offsets but not its base pointer.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C, align(16))]
struct Aligned {
    tag: u8,
}

/// A component that is never registered, for the error path.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Unknown {
    value: u32,
}

/// Counts its own destructor. Not `Copy`, not blittable, and the only way to
/// tell a leak from a double free without a sanitizer.
///
/// Hand-written rather than derived because `Arc<AtomicU32>` is not itself
/// `Reflect` — the derive insists every field be describable, which is the point
/// of it, and an `Arc` is exactly the sort of thing an editor cannot show and a
/// serializer cannot write.
#[derive(Debug, Clone)]
struct Tracked {
    drops: Arc<AtomicU32>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: the path is unique to this test, `Transfer::Owning` is correct
// because a pointer means nothing outside this address space, and the
// destructor installed below is `Tracked`'s own.
unsafe impl Reflect for Tracked {
    const PATH: &'static str = "slop_ecs::tests::command::Tracked";
    const TRANSFER: Transfer = Transfer::Owning;

    fn type_info() -> TypeInfo {
        unsafe fn drop_tracked(pointer: *mut u8) {
            // SAFETY: the registry only ever calls this on an initialized,
            // aligned `Tracked`, which is what `TypeInfo::with_drop` promises.
            unsafe { std::ptr::drop_in_place(pointer.cast::<Tracked>()) };
        }

        // SAFETY: the layout and the destructor are both `Tracked`'s own.
        unsafe {
            TypeInfo::with_drop(
                Self::PATH,
                std::alloc::Layout::new::<Self>(),
                Self::TRANSFER,
                TypeKind::Opaque,
                drop_tracked,
            )
        }
    }
}

fn world() -> World {
    let mut world = World::with_builtins();
    world.register::<Position>().expect("fresh");
    world.register::<Velocity>().expect("fresh");
    world.register::<Health>().expect("fresh");
    world.register::<Name>().expect("fresh");
    world.register::<Player>().expect("fresh");
    world.register::<Aligned>().expect("fresh");
    world.register::<Tracked>().expect("fresh");

    world
}

#[test]
fn an_empty_buffer_applies_to_nothing() {
    let mut world = world();
    let mut commands = CommandBuffer::new();

    assert!(commands.is_empty());
    world.apply(&mut commands).expect("nothing to fail at");

    assert_eq!(world.len(), 0);
    world.assert_consistent();
}

#[test]
fn nothing_reaches_the_world_before_the_sync_point() {
    let mut world = world();
    let mut commands = CommandBuffer::new();

    let entity = commands.spawn();
    commands.insert(entity, Position { x: 1.0, y: 2.0 });

    assert_eq!(world.len(), 0, "recording must not touch the world");
    assert_eq!(commands.len(), 2);

    world.apply(&mut commands).expect("Position is registered");

    assert_eq!(world.len(), 1);
    world.assert_consistent();
}

#[test]
fn a_spawn_target_addresses_the_entity_it_created() {
    let mut world = world();
    let mut commands = CommandBuffer::new();

    let first = commands.spawn();
    let second = commands.spawn();
    commands.insert(first, Health { value: 10 });
    commands.insert(second, Health { value: 20 });

    world.apply(&mut commands).expect("Health is registered");

    // The world has no notion of "first" and "second", so this reads the health
    // values back through a query and checks the pair.
    let mut values: Vec<u32> = world
        .query::<&Health>()
        .map(|health| health.value)
        .collect();
    values.sort_unstable();

    assert_eq!(values, vec![10, 20]);
    world.assert_consistent();
}

#[test]
fn ordinals_are_per_buffer_and_restart_after_applying() {
    let mut world = world();
    let mut commands = CommandBuffer::new();

    assert_eq!(commands.spawn(), Target::Pending(0));
    assert_eq!(commands.spawn(), Target::Pending(1));

    world.apply(&mut commands).expect("no components");

    assert_eq!(
        commands.spawn(),
        Target::Pending(0),
        "a reused buffer numbers from zero again"
    );
}

#[test]
fn an_existing_entity_is_a_target_without_ceremony() {
    let mut world = world();
    let entity = world.spawn();

    let mut commands = CommandBuffer::new();
    commands.insert(entity, Position { x: 3.0, y: 4.0 });

    world.apply(&mut commands).expect("Position is registered");

    assert_eq!(
        world.get::<Position>(entity),
        Some(&Position { x: 3.0, y: 4.0 })
    );
    world.assert_consistent();
}

#[test]
fn commands_apply_in_the_order_they_were_recorded() {
    let mut world = world();
    let entity = world.spawn();

    let mut commands = CommandBuffer::new();
    commands.insert(entity, Health { value: 1 });
    commands.insert(entity, Health { value: 2 });
    commands.insert(entity, Health { value: 3 });

    world.apply(&mut commands).expect("Health is registered");

    assert_eq!(world.get::<Health>(entity), Some(&Health { value: 3 }));
    world.assert_consistent();
}

#[test]
fn a_recorded_despawn_takes_the_entity_and_its_components() {
    let mut world = world();
    let entity = world.spawn();
    world
        .insert(entity, Position { x: 1.0, y: 1.0 })
        .expect("ok");

    let mut commands = CommandBuffer::new();
    commands.despawn(entity);

    assert!(world.contains(entity), "not yet");
    world.apply(&mut commands).expect("no components staged");

    assert!(!world.contains(entity));
    assert_eq!(world.len(), 0);
    world.assert_consistent();
}

#[test]
fn a_recorded_remove_takes_one_component_and_leaves_the_rest() {
    let mut world = world();
    let entity = world.spawn();
    world
        .insert(entity, Position { x: 1.0, y: 1.0 })
        .expect("ok");
    world
        .insert(entity, Velocity { dx: 0.5, dy: 0.5 })
        .expect("ok");

    let mut commands = CommandBuffer::new();
    commands.remove::<Velocity>(entity);

    world.apply(&mut commands).expect("no components staged");

    assert!(world.has::<Position>(entity));
    assert!(!world.has::<Velocity>(entity));
    world.assert_consistent();
}

#[test]
fn an_entity_can_be_spawned_and_despawned_in_one_buffer() {
    let mut world = world();
    let mut commands = CommandBuffer::new();

    let entity = commands.spawn();
    commands.insert(entity, Position { x: 1.0, y: 1.0 });
    commands.despawn(entity);

    world.apply(&mut commands).expect("Position is registered");

    assert_eq!(world.len(), 0);
    world.assert_consistent();
}

#[test]
fn a_zero_sized_component_survives_the_staging_area() {
    let mut world = world();
    let mut commands = CommandBuffer::new();

    let entity = commands.spawn();
    commands.insert(entity, Player {});
    commands.insert(entity, Health { value: 7 });

    world.apply(&mut commands).expect("both are registered");

    let tagged: Vec<u32> = world
        .query::<(&Player, &Health)>()
        .map(|(_, health)| health.value)
        .collect();

    assert_eq!(tagged, vec![7]);
    world.assert_consistent();
}

#[test]
fn an_over_aligned_component_lands_correctly_aligned() {
    // The bug this exists for: a `Vec<u8>` staging area gives an allocation
    // aligned to 1, so a 16-aligned component placed at a 16-aligned *offset* is
    // still misaligned in memory. Interleaving a one-byte component forces the
    // offsets apart so a base-alignment mistake cannot hide behind malloc
    // happening to return an aligned block.
    let mut world = world();
    let mut commands = CommandBuffer::new();

    let mut entities = Vec::new();
    for tag in 0..8u8 {
        let entity = commands.spawn();
        commands.insert(
            entity,
            Health {
                value: u32::from(tag),
            },
        );
        commands.insert(entity, Aligned { tag });
        entities.push(entity);
    }

    world.apply(&mut commands).expect("both are registered");

    let mut tags: Vec<u8> = world
        .query::<&Aligned>()
        .map(|aligned| aligned.tag)
        .collect();
    tags.sort_unstable();

    assert_eq!(tags, (0..8u8).collect::<Vec<_>>());

    for aligned in world.query::<&Aligned>() {
        assert_eq!(
            std::ptr::from_ref(aligned).addr() % align_of::<Aligned>(),
            0,
            "a component must be aligned once it reaches its column"
        );
    }

    world.assert_consistent();
}

#[test]
fn the_staging_area_survives_reallocating_under_a_stricter_alignment() {
    // Fill the staging area past its initial capacity with a loosely aligned
    // component, then demand a stricter alignment. The reallocation has to
    // preserve every offset already handed out.
    let mut world = world();
    let mut commands = CommandBuffer::new();

    let mut expected = Vec::new();
    // Miri interprets rather than executes, so volume is expensive there and the
    // paths are what it checks — `docs/CONVENTIONS.md` §7.
    let count = if cfg!(miri) { 16 } else { 200 };

    for value in 0..count {
        let entity = commands.spawn();
        commands.insert(entity, Health { value });
        expected.push(value);
    }

    let late = commands.spawn();
    commands.insert(late, Aligned { tag: 9 });

    world.apply(&mut commands).expect("both are registered");

    let mut values: Vec<u32> = world
        .query::<&Health>()
        .map(|health| health.value)
        .collect();
    values.sort_unstable();

    assert_eq!(values, expected);
    assert_eq!(world.query::<&Aligned>().count(), 1);
    world.assert_consistent();
}

#[test]
fn a_command_targeting_a_despawned_entity_is_skipped() {
    let mut world = world();
    let entity = world.spawn();

    let mut commands = CommandBuffer::new();
    commands.insert(entity, Position { x: 1.0, y: 1.0 });
    commands.remove::<Position>(entity);
    commands.despawn(entity);

    world.despawn(entity);

    world
        .apply(&mut commands)
        .expect("a dead target is routine, not an error");

    assert_eq!(world.len(), 0);
    world.assert_consistent();
}

#[test]
fn a_component_bound_for_a_dead_entity_is_destroyed_not_leaked() {
    let drops = Arc::new(AtomicU32::new(0));

    let mut world = world();
    let entity = world.spawn();

    let mut commands = CommandBuffer::new();
    commands.insert(
        entity,
        Tracked {
            drops: Arc::clone(&drops),
        },
    );

    world.despawn(entity);
    world
        .apply(&mut commands)
        .expect("a dead target is routine");

    assert_eq!(
        drops.load(Ordering::Relaxed),
        1,
        "exactly one destructor, not zero or two"
    );
}

#[test]
fn an_unregistered_component_is_reported_and_destroyed() {
    let drops = Arc::new(AtomicU32::new(0));

    let mut world = world();
    let entity = world.spawn();

    let mut commands = CommandBuffer::new();
    commands.insert(entity, Unknown { value: 1 });
    commands.insert(
        entity,
        Tracked {
            drops: Arc::clone(&drops),
        },
    );
    commands.insert(entity, Health { value: 5 });

    let error = world
        .apply(&mut commands)
        .expect_err("Unknown was never registered");

    assert_eq!(
        error,
        EcsError::UnregisteredComponent {
            type_id: <Unknown as Reflect>::type_id()
        }
    );
    assert_eq!(
        world.get::<Health>(entity),
        Some(&Health { value: 5 }),
        "the commands after the failure still applied"
    );
    assert_eq!(drops.load(Ordering::Relaxed), 0, "the registered component was taken");
    world.assert_consistent();
}

#[test]
fn the_first_error_is_the_one_returned() {
    #[derive(Reflect, Debug, Clone, Copy)]
    #[repr(C)]
    struct AlsoUnknown {
        value: u32,
    }

    let mut world = world();
    let entity = world.spawn();

    let mut commands = CommandBuffer::new();
    commands.insert(entity, Unknown { value: 1 });
    commands.insert(entity, AlsoUnknown { value: 2 });

    let error = world
        .apply(&mut commands)
        .expect_err("neither is registered");

    assert_eq!(
        error,
        EcsError::UnregisteredComponent {
            type_id: <Unknown as Reflect>::type_id()
        }
    );
}

#[test]
fn applying_empties_the_buffer() {
    let mut world = world();
    let mut commands = CommandBuffer::new();

    let entity = commands.spawn();
    commands.insert(entity, Health { value: 1 });

    world.apply(&mut commands).expect("Health is registered");

    assert!(commands.is_empty());
    assert_eq!(commands.len(), 0);

    world.apply(&mut commands).expect("nothing left to apply");

    assert_eq!(world.len(), 1, "an emptied buffer does not replay");
    world.assert_consistent();
}

#[test]
fn a_buffer_dropped_unapplied_destroys_what_it_staged() {
    let drops = Arc::new(AtomicU32::new(0));

    {
        let mut commands = CommandBuffer::new();
        let entity = commands.spawn();

        for _ in 0..3 {
            commands.insert(
                entity,
                Tracked {
                    drops: Arc::clone(&drops),
                },
            );
        }

        assert_eq!(drops.load(Ordering::Relaxed), 0, "still staged");
    }

    assert_eq!(drops.load(Ordering::Relaxed), 3);
}

#[test]
fn clearing_destroys_what_was_staged_and_leaves_the_buffer_reusable() {
    let drops = Arc::new(AtomicU32::new(0));

    let mut world = world();
    let mut commands = CommandBuffer::new();

    let entity = commands.spawn();
    commands.insert(
        entity,
        Tracked {
            drops: Arc::clone(&drops),
        },
    );

    commands.clear();

    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(commands.is_empty());

    let reused = commands.spawn();
    commands.insert(reused, Health { value: 42 });
    world.apply(&mut commands).expect("Health is registered");

    assert_eq!(world.len(), 1);
    assert_eq!(drops.load(Ordering::Relaxed), 1, "clearing did not disturb the count");
    world.assert_consistent();
}

#[test]
fn an_applied_component_is_destroyed_by_the_world_not_the_buffer() {
    let drops = Arc::new(AtomicU32::new(0));

    let mut world = world();
    let mut commands = CommandBuffer::new();

    let entity = commands.spawn();
    commands.insert(
        entity,
        Tracked {
            drops: Arc::clone(&drops),
        },
    );

    world.apply(&mut commands).expect("Tracked is registered");
    assert_eq!(drops.load(Ordering::Relaxed), 0, "the world owns it now");

    drop(commands);
    assert_eq!(drops.load(Ordering::Relaxed), 0, "the buffer must not double free");

    drop(world);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn a_string_component_round_trips_through_the_staging_area() {
    let mut world = world();
    let mut commands = CommandBuffer::new();

    let entity = commands.spawn();
    commands.insert(
        entity,
        Name {
            text: "a heap allocation that must survive being moved twice".to_owned(),
        },
    );

    world.apply(&mut commands).expect("Name is registered");

    let names: Vec<&str> = world
        .query::<&Name>()
        .map(|name| name.text.as_str())
        .collect();

    assert_eq!(
        names,
        vec!["a heap allocation that must survive being moved twice"]
    );
    world.assert_consistent();
}

#[test]
fn a_target_from_another_buffer_addresses_nothing_it_should_not() {
    // Documented as a bug rather than prevented, so what is asserted here is
    // that it fails safely: an ordinal past the end of the applying buffer's
    // spawn list is skipped rather than resolving to an arbitrary entity.
    let mut world = world();

    let mut donor = CommandBuffer::new();
    donor.spawn();
    donor.spawn();
    let stranger = donor.spawn();
    donor.clear();

    let mut commands = CommandBuffer::new();
    commands.spawn();
    commands.insert(stranger, Health { value: 1 });

    world.apply(&mut commands).expect("the ordinal is skipped");

    assert_eq!(world.len(), 1);
    assert_eq!(world.query::<&Health>().count(), 0);
    world.assert_consistent();
}

#[test]
fn a_query_can_record_structural_change_it_could_not_perform() {
    // The motivating case, and the reason §2.10 calls this required: the query
    // borrows the world, so nothing inside this loop could take `&mut World`.
    let mut world = world();

    for value in [0, 5, 0, 9] {
        let entity = world.spawn();
        world.insert(entity, Health { value }).expect("registered");
    }

    let mut commands = CommandBuffer::new();

    for (entity, health) in world.query::<(Entity, &Health)>() {
        if health.value == 0 {
            commands.despawn(entity);
        } else {
            commands.insert(entity, Velocity { dx: 1.0, dy: 0.0 });
        }
    }

    world.apply(&mut commands).expect("Velocity is registered");

    assert_eq!(world.len(), 2);
    assert_eq!(world.query::<&Velocity>().count(), 2);
    world.assert_consistent();
}

#[test]
fn a_buffer_can_be_sent_to_another_thread() {
    // `Send` is the whole point: the scheduler fills a buffer on a worker and
    // applies it on the thread that owns the world.
    fn assert_send<T: Send>() {}
    assert_send::<CommandBuffer>();

    let mut commands = CommandBuffer::new();
    let entity = commands.spawn();
    commands.insert(entity, Health { value: 3 });

    let mut commands = std::thread::spawn(move || {
        let other = commands.spawn();
        commands.insert(other, Health { value: 4 });
        commands
    })
    .join()
    .expect("the worker did not panic");

    let mut world = world();
    world.apply(&mut commands).expect("Health is registered");

    let mut values: Vec<u32> = world
        .query::<&Health>()
        .map(|health| health.value)
        .collect();
    values.sort_unstable();

    assert_eq!(values, vec![3, 4]);
    world.assert_consistent();
}
