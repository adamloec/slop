//! Resources: data the world holds exactly one of.
//!
//! Two things are being checked, and the second is the reason resources landed
//! when they did rather than later:
//!
//! - **A resource behaves like a component that has no entity.** Registered,
//!   dropped exactly once, change-detectable, replaceable.
//! - **The scheduler treats it the same way.** Two systems writing one resource
//!   are serialized; two writing different resources are not; and a system that
//!   touches a resource it did not declare panics rather than racing. None of
//!   that needed new scheduler code — it needed `Access` to have a kind, which
//!   is why this could not wait until after the scheduler hardened.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use slop_core::JobSystem;
use slop_ecs::{
    Access, AccessKind, CommandBuffer, EcsError, Schedule, System, World, WorldCell, conflicts,
};
use slop_reflect::{Reflect, Transfer, TypeInfo, TypeKind};

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Clock {
    frame: u64,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Gravity {
    value: f32,
}

/// Also used as a component, to prove the two namespaces are separate.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Position {
    x: f32,
}

/// Owns a heap allocation, so a leak or a double free is observable.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[repr(C)]
struct Title {
    text: String,
}

/// Never registered.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Unknown {
    value: u32,
}

/// Counts its own destructor.
#[derive(Debug, Clone)]
struct Tracked {
    drops: Arc<AtomicU32>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: the path is unique to this test, `Owning` is correct because an `Rc`
// means nothing outside this address space, and the destructor is `Tracked`'s.
unsafe impl Reflect for Tracked {
    const PATH: &'static str = "slop_ecs::tests::resource::Tracked";
    const TRANSFER: Transfer = Transfer::Owning;

    fn type_info() -> TypeInfo {
        unsafe fn drop_tracked(pointer: *mut u8) {
            // SAFETY: only ever called on an initialized, aligned `Tracked`.
            unsafe { std::ptr::drop_in_place(pointer.cast::<Tracked>()) };
        }

        // SAFETY: the layout and destructor are both `Tracked`'s own.
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
    world.register::<Clock>().expect("fresh");
    world.register::<Gravity>().expect("fresh");
    world.register::<Position>().expect("fresh");
    world.register::<Title>().expect("fresh");
    world.register::<Tracked>().expect("fresh");

    world
}

#[test]
fn a_world_starts_with_no_resources() {
    let world = world();

    assert_eq!(world.resource_count(), 0);
    assert!(!world.contains_resource::<Clock>());
    assert_eq!(world.resource::<Clock>(), None);
    world.assert_consistent();
}

#[test]
fn an_inserted_resource_reads_back() {
    let mut world = world();

    world
        .insert_resource(Clock { frame: 7 })
        .expect("registered");

    assert!(world.contains_resource::<Clock>());
    assert_eq!(world.resource::<Clock>(), Some(&Clock { frame: 7 }));
    assert_eq!(world.resource_count(), 1);
    world.assert_consistent();
}

#[test]
fn an_unregistered_resource_is_refused() {
    let mut world = world();

    let error = world
        .insert_resource(Unknown { value: 1 })
        .expect_err("never registered");

    assert_eq!(
        error,
        EcsError::UnregisteredResource {
            type_id: <Unknown as Reflect>::type_id()
        }
    );
}

#[test]
fn inserting_twice_replaces_and_drops_the_old_value() {
    let drops = Arc::new(AtomicU32::new(0));
    let mut world = world();

    world
        .insert_resource(Tracked {
            drops: Arc::clone(&drops),
        })
        .expect("registered");
    assert_eq!(drops.load(Ordering::Relaxed), 0);

    world
        .insert_resource(Tracked {
            drops: Arc::clone(&drops),
        })
        .expect("registered");

    assert_eq!(
        drops.load(Ordering::Relaxed),
        1,
        "the first value was destroyed"
    );
    assert_eq!(world.resource_count(), 1, "and there is still only one");
    world.assert_consistent();
}

#[test]
fn removing_a_resource_drops_it_exactly_once() {
    let drops = Arc::new(AtomicU32::new(0));
    let mut world = world();

    world
        .insert_resource(Tracked {
            drops: Arc::clone(&drops),
        })
        .expect("registered");

    assert!(world.remove_resource::<Tracked>());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(!world.remove_resource::<Tracked>(), "already gone");
    assert_eq!(drops.load(Ordering::Relaxed), 1, "and not dropped twice");
    world.assert_consistent();
}

#[test]
fn dropping_the_world_drops_its_resources() {
    let drops = Arc::new(AtomicU32::new(0));

    {
        let mut world = world();
        world
            .insert_resource(Tracked {
                drops: Arc::clone(&drops),
            })
            .expect("registered");
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }

    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn a_resource_can_be_mutated_in_place() {
    let mut world = world();
    world
        .insert_resource(Clock { frame: 1 })
        .expect("registered");

    world.resource_mut::<Clock>().expect("present").frame = 42;

    assert_eq!(world.resource::<Clock>(), Some(&Clock { frame: 42 }));
}

#[test]
fn an_owning_resource_survives_replacement() {
    let mut world = world();

    world
        .insert_resource(Title {
            text: "first".to_owned(),
        })
        .expect("registered");
    world
        .insert_resource(Title {
            text: "a considerably longer second title".to_owned(),
        })
        .expect("registered");

    assert_eq!(
        world.resource::<Title>().map(|title| title.text.as_str()),
        Some("a considerably longer second title")
    );
    world.assert_consistent();
}

#[test]
fn a_resource_and_a_component_of_one_type_are_separate() {
    // The reason `Access` carries a kind. `Position` is both here, and the two
    // must not see each other.
    let mut world = world();

    let entity = world.spawn();
    world
        .insert(entity, Position { x: 1.0 })
        .expect("registered");
    world
        .insert_resource(Position { x: 99.0 })
        .expect("registered");

    assert_eq!(world.get::<Position>(entity), Some(&Position { x: 1.0 }));
    assert_eq!(world.resource::<Position>(), Some(&Position { x: 99.0 }));

    world.remove_resource::<Position>();

    assert_eq!(
        world.get::<Position>(entity),
        Some(&Position { x: 1.0 }),
        "removing the resource left the component alone"
    );
    world.assert_consistent();
}

#[test]
fn a_resource_access_never_conflicts_with_a_component_access() {
    let component = [Access::write::<Position>()];
    let resource = [Access::write_resource::<Position>()];

    assert!(!conflicts(&component, &resource));
    assert_eq!(component[0].kind, AccessKind::Component);
    assert_eq!(resource[0].kind, AccessKind::Resource);
}

#[test]
fn two_writers_of_one_resource_conflict() {
    let left = [Access::write_resource::<Clock>()];
    let right = [Access::read_resource::<Clock>()];

    assert!(conflicts(&left, &right));
}

#[test]
fn two_writers_of_different_resources_do_not_conflict() {
    let left = [Access::write_resource::<Clock>()];
    let right = [Access::write_resource::<Gravity>()];

    assert!(!conflicts(&left, &right));
}

#[test]
fn resource_changes_are_detectable() {
    let mut world = world();
    world
        .insert_resource(Clock { frame: 1 })
        .expect("registered");

    let added = world.resource_ticks::<Clock>().expect("present");
    assert_eq!(added.added, added.changed, "a fresh insert stamps both");

    let seen = world.tick();
    world.advance_tick();
    world.resource_mut::<Clock>().expect("present").frame = 2;

    let after = world.resource_ticks::<Clock>().expect("present");

    assert!(after.changed.is_newer_than(seen, world.tick()));
    assert_eq!(after.added, added.added, "writing is not adding");
}

#[test]
fn resource_types_come_back_in_a_defined_order() {
    let mut world = world();
    world
        .insert_resource(Gravity { value: -9.8 })
        .expect("registered");
    world
        .insert_resource(Clock { frame: 0 })
        .expect("registered");

    let first = world.resource_types();
    let second = world.resource_types();

    assert_eq!(first.len(), 2);
    assert_eq!(first, second, "a serializer needs this stable");
    assert!(first.windows(2).all(|pair| pair[0] <= pair[1]), "sorted");
}

#[test]
#[cfg_attr(miri, ignore = "drives the job pool; see tests/cell.rs")]
fn a_system_reads_a_resource_it_declared() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    world
        .insert_resource(Gravity { value: -9.8 })
        .expect("registered");

    for _ in 0..4 {
        let entity = world.spawn();
        world
            .insert(entity, Position { x: 0.0 })
            .expect("registered");
    }

    let mut schedule = Schedule::new();
    schedule.add_stage("simulate").add(System::new(
        "fall",
        vec![
            Access::write::<Position>(),
            Access::read_resource::<Gravity>(),
        ],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            let gravity = world.resource::<Gravity>().expect("installed").value;

            for mut position in world.query::<&mut Position>() {
                position.x += gravity;
            }
        },
    ));

    schedule.run(&mut world, &jobs).expect("nothing to fail");

    assert!(world.query::<&Position>().all(|p| p.x == -9.8));
    world.assert_consistent();
}

#[test]
#[cfg_attr(miri, ignore = "drives the job pool; see tests/cell.rs")]
fn a_system_writes_a_resource_it_declared() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    world
        .insert_resource(Clock { frame: 0 })
        .expect("registered");

    let mut schedule = Schedule::new();
    schedule.add_stage("simulate").add(System::new(
        "tick",
        vec![Access::write_resource::<Clock>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            if let Some(mut clock) = world.resource_mut::<Clock>() {
                clock.frame += 1;
            }
        },
    ));

    for expected in 1..=5 {
        schedule.run(&mut world, &jobs).expect("nothing to fail");
        assert_eq!(world.resource::<Clock>().expect("present").frame, expected);
    }
}

#[test]
#[cfg_attr(miri, ignore = "drives the job pool; see tests/cell.rs")]
#[should_panic(expected = "resource")]
fn a_system_reading_an_undeclared_resource_panics() {
    let jobs = JobSystem::with_threads(2);
    let mut world = world();
    world
        .insert_resource(Gravity { value: -9.8 })
        .expect("registered");

    let mut schedule = Schedule::new();
    schedule.add_stage("simulate").add(System::new(
        "sneaky",
        vec![],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            let _ = world.resource::<Gravity>();
        },
    ));

    let _ = schedule.run(&mut world, &jobs);
}

#[test]
#[cfg_attr(miri, ignore = "drives the job pool; see tests/cell.rs")]
#[should_panic(expected = "mutably without declaring it")]
fn a_system_writing_a_resource_it_only_reads_panics() {
    let jobs = JobSystem::with_threads(2);
    let mut world = world();
    world
        .insert_resource(Clock { frame: 0 })
        .expect("registered");

    let mut schedule = Schedule::new();
    schedule.add_stage("simulate").add(System::new(
        "sneaky",
        vec![Access::read_resource::<Clock>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            let _ = world.resource_mut::<Clock>();
        },
    ));

    let _ = schedule.run(&mut world, &jobs);
}

#[test]
fn systems_using_different_resources_share_a_batch() {
    let mut world = world();
    world
        .insert_resource(Clock { frame: 0 })
        .expect("registered");
    world
        .insert_resource(Gravity { value: -9.8 })
        .expect("registered");

    let mut schedule = Schedule::new();
    let stage = schedule.add_stage("simulate");
    stage.add(System::new(
        "tick",
        vec![Access::write_resource::<Clock>()],
        |_world: WorldCell<'_>, _commands: &mut CommandBuffer| {},
    ));
    stage.add(System::new(
        "gravity",
        vec![Access::write_resource::<Gravity>()],
        |_world: WorldCell<'_>, _commands: &mut CommandBuffer| {},
    ));

    assert_eq!(stage.batches().len(), 1);
}

#[test]
#[cfg_attr(miri, ignore = "drives the job pool; see tests/cell.rs")]
fn systems_writing_one_resource_are_serialized_in_order() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    world
        .insert_resource(Clock { frame: 0 })
        .expect("registered");

    let mut schedule = Schedule::new();
    let stage = schedule.add_stage("simulate");
    stage.add(System::new(
        "double",
        vec![Access::write_resource::<Clock>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            if let Some(mut clock) = world.resource_mut::<Clock>() {
                clock.frame = clock.frame * 2 + 1;
            }
        },
    ));
    stage.add(System::new(
        "square",
        vec![Access::write_resource::<Clock>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            if let Some(mut clock) = world.resource_mut::<Clock>() {
                clock.frame *= clock.frame;
            }
        },
    ));

    assert_eq!(stage.batches().len(), 2, "they share a resource");

    schedule.run(&mut world, &jobs).expect("nothing to fail");

    // 0 → double gives 1 → square gives 1. The other order gives 1 then 3.
    assert_eq!(world.resource::<Clock>().expect("present").frame, 1);
}

#[test]
#[cfg_attr(miri, ignore = "drives the job pool; see tests/cell.rs")]
fn a_resource_absent_from_the_world_reads_as_none_inside_a_system() {
    let jobs = JobSystem::with_threads(2);
    let mut world = world();

    let saw = Arc::new(AtomicUsize::new(0));
    let record = Arc::clone(&saw);

    let mut schedule = Schedule::new();
    schedule.add_stage("simulate").add(System::new(
        "optional",
        vec![Access::read_resource::<Gravity>()],
        move |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            record.store(
                usize::from(world.contains_resource::<Gravity>()),
                Ordering::SeqCst,
            );
            assert!(world.resource::<Gravity>().is_none());
        },
    ));

    schedule.run(&mut world, &jobs).expect("nothing to fail");

    assert_eq!(saw.load(Ordering::SeqCst), 0);
}

#[test]
#[cfg_attr(miri, ignore = "drives the job pool; see tests/cell.rs")]
fn a_resource_write_is_visible_to_a_later_batch_in_the_same_stage() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    world
        .insert_resource(Clock { frame: 5 })
        .expect("registered");

    let observed = Arc::new(AtomicUsize::new(0));
    let record = Arc::clone(&observed);

    let mut schedule = Schedule::new();
    let stage = schedule.add_stage("simulate");
    stage.add(System::new(
        "advance",
        vec![Access::write_resource::<Clock>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            if let Some(mut clock) = world.resource_mut::<Clock>() {
                clock.frame += 10;
            }
        },
    ));
    stage.add(System::new(
        "observe",
        vec![Access::read_resource::<Clock>()],
        move |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            let frame = world.resource::<Clock>().expect("present").frame;
            record.store(frame as usize, Ordering::SeqCst);
        },
    ));

    schedule.run(&mut world, &jobs).expect("nothing to fail");

    assert_eq!(observed.load(Ordering::SeqCst), 15);
}
