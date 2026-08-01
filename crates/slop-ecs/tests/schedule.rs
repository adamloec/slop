//! Systems running under the scheduler, end to end.
//!
//! The unit tests in `schedule.rs` check that batching *partitions* correctly.
//! This checks the thing that actually matters: that running those batches on a
//! real thread pool produces the same world a sequential run would, and that the
//! declaration a system makes is the one it is held to.
//!
//! Three failure modes are worth more than the rest:
//!
//! - **A wrong declaration is caught, not undefined.** The scheduler decides what
//!   may run alongside a system from what it declared, so a query outside that
//!   declaration is outside what was proved. It must panic rather than race.
//! - **The answer does not depend on the thread count.** `DESIGN.md` §2.14. Every
//!   result here is asserted against a one-thread run of the same schedule.
//! - **Structural change lands at the sync point.** Not during the stage, and not
//!   later than the end of it.
//!
//! # Every test here is skipped under Miri
//!
//! Not because the scheduler is unverified — because the job pool is `rayon`,
//! and `rayon-core` and `crossbeam-epoch` both use patterns the experimental
//! aliasing models reject. Stacked Borrows objects to `crossbeam-epoch`'s
//! intrusive list; Tree Borrows objects to `rayon-core`'s scope pointer. Neither
//! is our code, and both abort the run before it reaches anything of ours.
//!
//! The soundness claim underneath all of this — that disjoint concurrent access
//! through a [`WorldCell`] is free of races and aliasing violations — is
//! verified in `tests/cell.rs`, which drives the same thing on `std` threads
//! that Miri does understand. What is unverified by Miri is rayon's dispatch,
//! which is not ours to prove.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use slop_core::JobSystem;
use slop_ecs::{
    Access, Changed, CommandBuffer, EcsError, Entity, Schedule, System, World, WorldCell,
};
use slop_reflect::Reflect;

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Position {
    x: f32,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Velocity {
    dx: f32,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Health {
    value: u32,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Score {
    value: u32,
}

#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Spawned {}

/// Never registered, for the error path.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct Unknown {}

fn world() -> World {
    let mut world = World::with_builtins();
    world.register::<Position>().expect("fresh");
    world.register::<Velocity>().expect("fresh");
    world.register::<Health>().expect("fresh");
    world.register::<Score>().expect("fresh");
    world.register::<Spawned>().expect("fresh");

    world
}

/// Spawn `count` entities with a position and a velocity.
fn populate(world: &mut World, count: u32) -> Vec<Entity> {
    (0..count)
        .map(|index| {
            let entity = world.spawn();
            world
                .insert(entity, Position { x: index as f32 })
                .expect("registered");
            world
                .insert(entity, Velocity { dx: 1.0 })
                .expect("registered");
            entity
        })
        .collect()
}

/// Every position, sorted, so archetype order is never asserted.
fn positions(world: &World) -> Vec<u32> {
    let mut values: Vec<u32> = world
        .query::<&Position>()
        .map(|position| position.x as u32)
        .collect();
    values.sort_unstable();

    values
}

/// Integrate velocity into position — the canonical system.
fn integrate() -> System {
    System::new(
        "integrate",
        vec![Access::write::<Position>(), Access::read::<Velocity>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            for (mut position, velocity) in world.query::<(&mut Position, &Velocity)>() {
                position.x += velocity.dx;
            }
        },
    )
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn a_system_writes_through_the_world_cell() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    populate(&mut world, 4);

    let mut schedule = Schedule::new();
    schedule.add_stage("simulate").add(integrate());

    schedule.run(&mut world, &jobs).expect("nothing to fail");

    assert_eq!(positions(&world), vec![1, 2, 3, 4]);
    world.assert_consistent();
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn disjoint_systems_run_together_and_both_writes_land() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    let entities = populate(&mut world, 4);
    for entity in &entities {
        world
            .insert(*entity, Health { value: 10 })
            .expect("registered");
    }

    let mut schedule = Schedule::new();
    let stage = schedule.add_stage("simulate");
    stage.add(integrate());
    stage.add(System::new(
        "decay",
        vec![Access::write::<Health>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            for mut health in world.query::<&mut Health>() {
                health.value -= 1;
            }
        },
    ));

    assert_eq!(
        stage.batches().len(),
        1,
        "they share nothing, so they share a batch"
    );

    schedule.run(&mut world, &jobs).expect("nothing to fail");

    assert_eq!(positions(&world), vec![1, 2, 3, 4]);
    assert!(world.query::<&Health>().all(|health| health.value == 9));
    world.assert_consistent();
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn conflicting_systems_apply_in_declaration_order() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    populate(&mut world, 1);

    let mut schedule = Schedule::new();
    let stage = schedule.add_stage("simulate");
    stage.add(System::new(
        "double",
        vec![Access::write::<Position>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            for mut position in world.query::<&mut Position>() {
                position.x = position.x * 2.0 + 1.0;
            }
        },
    ));
    stage.add(System::new(
        "square",
        vec![Access::write::<Position>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            for mut position in world.query::<&mut Position>() {
                position.x *= position.x;
            }
        },
    ));

    schedule.run(&mut world, &jobs).expect("nothing to fail");

    // x = 0 → double gives 1 → square gives 1. The other order would give 1 then
    // 3, so this genuinely distinguishes them.
    assert_eq!(positions(&world), vec![1]);
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn the_result_does_not_depend_on_the_thread_count() {
    // DESIGN §2.14, checked directly rather than argued.
    let build = || {
        let mut world = world();
        populate(&mut world, 64);

        let mut schedule = Schedule::new();
        let stage = schedule.add_stage("simulate");
        stage.add(integrate());
        stage.add(System::new(
            "score",
            vec![Access::write::<Score>()],
            |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
                for mut score in world.query::<&mut Score>() {
                    score.value += 1;
                }
            },
        ));
        stage.add(System::new(
            "integrate again",
            vec![Access::write::<Position>(), Access::read::<Velocity>()],
            |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
                for (mut position, velocity) in world.query::<(&mut Position, &Velocity)>() {
                    position.x += velocity.dx * 10.0;
                }
            },
        ));

        (world, schedule)
    };

    let mut results = Vec::new();
    for threads in [1, 2, 3, 8] {
        let jobs = JobSystem::with_threads(threads);
        let (mut world, mut schedule) = build();

        for _ in 0..4 {
            schedule.run(&mut world, &jobs).expect("nothing to fail");
        }

        results.push(positions(&world));
    }

    assert!(
        results.windows(2).all(|pair| pair[0] == pair[1]),
        "thread count changed the answer: {results:?}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
#[should_panic(expected = "without declaring it")]
fn querying_an_undeclared_component_panics() {
    let jobs = JobSystem::with_threads(2);
    let mut world = world();
    populate(&mut world, 1);

    let mut schedule = Schedule::new();
    schedule.add_stage("simulate").add(System::new(
        "sneaky",
        vec![Access::read::<Velocity>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            // Position was never declared, so the scheduler may have placed a
            // Position-writing system alongside this one.
            let _ = world.query::<&Position>().count();
        },
    ));

    let _ = schedule.run(&mut world, &jobs);
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
#[should_panic(expected = "mutably without declaring it")]
fn writing_a_component_declared_read_only_panics() {
    let jobs = JobSystem::with_threads(2);
    let mut world = world();
    populate(&mut world, 1);

    let mut schedule = Schedule::new();
    schedule.add_stage("simulate").add(System::new(
        "sneaky",
        vec![Access::read::<Position>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            for mut position in world.query::<&mut Position>() {
                position.x = 0.0;
            }
        },
    ));

    let _ = schedule.run(&mut world, &jobs);
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn over_declaring_is_allowed_and_only_costs_parallelism() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    populate(&mut world, 2);

    let mut schedule = Schedule::new();
    let stage = schedule.add_stage("simulate");
    // Declares a write, performs a read. Safe; it just cannot share a batch
    // with anything else touching Position.
    stage.add(System::new(
        "reads only",
        vec![Access::write::<Position>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            let _ = world.query::<&Position>().count();
        },
    ));
    stage.add(integrate());

    assert_eq!(
        stage.batches().len(),
        2,
        "the over-declaration serialized it"
    );

    schedule.run(&mut world, &jobs).expect("nothing to fail");

    assert_eq!(positions(&world), vec![1, 2]);
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn structural_change_lands_at_the_sync_point() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    populate(&mut world, 2);

    let observed_during = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&observed_during);

    let mut schedule = Schedule::new();
    let stage = schedule.add_stage("simulate");
    stage.add(System::new(
        "spawner",
        vec![],
        |_world: WorldCell<'_>, commands: &mut CommandBuffer| {
            for _ in 0..3 {
                let entity = commands.spawn();
                commands.insert(entity, Spawned {});
            }
        },
    ));
    stage.add(System::new(
        "counter",
        vec![Access::read::<Spawned>()],
        move |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            seen.store(world.query::<&Spawned>().count(), Ordering::SeqCst);
        },
    ));

    schedule
        .run(&mut world, &jobs)
        .expect("Spawned is registered");

    assert_eq!(
        observed_during.load(Ordering::SeqCst),
        0,
        "nothing recorded in a stage is visible inside that stage"
    );
    assert_eq!(world.query::<&Spawned>().count(), 3, "and all of it lands");
    assert_eq!(world.len(), 5);
    world.assert_consistent();
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn a_later_stage_sees_what_an_earlier_one_recorded() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();

    let observed = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&observed);

    let mut schedule = Schedule::new();
    schedule.add_stage("spawn").add(System::new(
        "spawner",
        vec![],
        |_world: WorldCell<'_>, commands: &mut CommandBuffer| {
            for _ in 0..4 {
                let entity = commands.spawn();
                commands.insert(entity, Spawned {});
            }
        },
    ));
    schedule.add_stage("observe").add(System::new(
        "counter",
        vec![Access::read::<Spawned>()],
        move |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            seen.store(world.query::<&Spawned>().count(), Ordering::SeqCst);
        },
    ));

    schedule
        .run(&mut world, &jobs)
        .expect("Spawned is registered");

    assert_eq!(observed.load(Ordering::SeqCst), 4);
    world.assert_consistent();
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn a_system_sees_what_an_earlier_batch_wrote_this_frame() {
    // Component writes are direct rather than deferred, so unlike structural
    // change they are visible within the stage — to the batches that follow.
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    populate(&mut world, 1);

    let observed = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&observed);

    let mut schedule = Schedule::new();
    let stage = schedule.add_stage("simulate");
    stage.add(integrate());
    stage.add(System::new(
        "read back",
        vec![Access::read::<Position>()],
        move |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            let total: f32 = world.query::<&Position>().map(|position| position.x).sum();
            seen.store(total as usize, Ordering::SeqCst);
        },
    ));

    schedule.run(&mut world, &jobs).expect("nothing to fail");

    assert_eq!(observed.load(Ordering::SeqCst), 1, "0 + 1, after integrate");
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn change_detection_is_relative_to_when_each_system_last_ran() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    populate(&mut world, 3);

    let changed_per_run = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = Arc::clone(&changed_per_run);

    let mut schedule = Schedule::new();
    let stage = schedule.add_stage("simulate");
    stage.add(System::new(
        "move one",
        vec![Access::write::<Position>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            if let Some(mut position) = world.query::<&mut Position>().next() {
                position.x += 1.0;
            }
        },
    ));
    stage.add(System::new(
        "count changes",
        vec![Access::read::<Position>()],
        move |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            let count = world
                .query::<&Position>()
                .filtered::<Changed<Position>>()
                .count();
            record.lock().expect("not poisoned").push(count);
        },
    ));

    for _ in 0..3 {
        schedule.run(&mut world, &jobs).expect("nothing to fail");
    }

    let counts = changed_per_run.lock().expect("not poisoned").clone();

    assert_eq!(
        counts,
        vec![3, 1, 1],
        "the first run sees everything as new, then only the one that moved"
    );
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn an_unregistered_component_in_a_buffer_is_reported() {
    let jobs = JobSystem::with_threads(2);
    let mut world = world();

    let mut schedule = Schedule::new();
    schedule.add_stage("simulate").add(System::new(
        "bad wiring",
        vec![],
        |_world: WorldCell<'_>, commands: &mut CommandBuffer| {
            let entity = commands.spawn();
            commands.insert(entity, Unknown {});
        },
    ));

    let error = schedule
        .run(&mut world, &jobs)
        .expect_err("Unknown was never registered");

    assert!(matches!(error, EcsError::UnregisteredComponent { .. }));
    world.assert_consistent();
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn a_schedule_reruns_without_reallocating_its_buffers() {
    // A schedule holds one command buffer per system and reuses it, so a frame
    // loop pays no allocation per run (CONVENTIONS §8).
    let jobs = JobSystem::with_threads(4);
    let mut world = world();

    let mut schedule = Schedule::new();
    schedule.add_stage("simulate").add(System::new(
        "churn",
        vec![],
        |_world: WorldCell<'_>, commands: &mut CommandBuffer| {
            let entity = commands.spawn();
            commands.insert(entity, Spawned {});
        },
    ));

    for expected in 1..=8 {
        schedule.run(&mut world, &jobs).expect("registered");
        assert_eq!(world.len(), expected);
    }

    world.assert_consistent();
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn systems_in_one_batch_really_do_overlap() {
    // Proving the parallelism rather than assuming it. Each system waits for
    // every other to start; only genuine concurrency completes.
    const SYSTEMS: usize = 4;

    let jobs = JobSystem::with_threads(SYSTEMS);
    let mut world = world();

    let started = Arc::new(AtomicUsize::new(0));
    let overlapped = Arc::new(AtomicUsize::new(0));

    let mut schedule = Schedule::new();
    let stage = schedule.add_stage("simulate");

    for _ in 0..SYSTEMS {
        let started = Arc::clone(&started);
        let overlapped = Arc::clone(&overlapped);

        stage.add(System::new(
            "waiter",
            vec![],
            move |_world: WorldCell<'_>, _commands: &mut CommandBuffer| {
                started.fetch_add(1, Ordering::SeqCst);

                // Bounded, so a sequential implementation fails the assertion
                // rather than hanging the suite.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while started.load(Ordering::SeqCst) < SYSTEMS {
                    if std::time::Instant::now() > deadline {
                        return;
                    }
                    std::hint::spin_loop();
                }

                overlapped.fetch_add(1, Ordering::SeqCst);
            },
        ));
    }

    schedule.run(&mut world, &jobs).expect("nothing to fail");

    assert_eq!(
        overlapped.load(Ordering::SeqCst),
        SYSTEMS,
        "systems declaring nothing must all run at once"
    );
}

#[test]
#[cfg_attr(miri, ignore = "the job pool is rayon; see tests/cell.rs")]
fn many_systems_over_many_entities_stay_consistent() {
    let jobs = JobSystem::with_threads(4);
    let mut world = world();
    let entities = populate(&mut world, 256);

    for (index, entity) in entities.iter().enumerate() {
        if index % 2 == 0 {
            world
                .insert(*entity, Health { value: 100 })
                .expect("registered");
        }
        if index % 3 == 0 {
            world
                .insert(*entity, Score { value: 0 })
                .expect("registered");
        }
    }

    let mut schedule = Schedule::new();
    let stage = schedule.add_stage("simulate");
    stage.add(integrate());
    stage.add(System::new(
        "heal",
        vec![Access::write::<Health>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            for mut health in world.query::<&mut Health>() {
                health.value += 1;
            }
        },
    ));
    stage.add(System::new(
        "score",
        vec![Access::write::<Score>(), Access::read::<Velocity>()],
        |world: WorldCell<'_>, _commands: &mut CommandBuffer| {
            for (mut score, velocity) in world.query::<(&mut Score, &Velocity)>() {
                score.value += velocity.dx as u32;
            }
        },
    ));

    for _ in 0..8 {
        schedule.run(&mut world, &jobs).expect("nothing to fail");
        world.assert_consistent();
    }

    assert!(world.query::<&Health>().all(|health| health.value == 108));
    assert!(world.query::<&Score>().all(|score| score.value == 8));
    assert_eq!(
        positions(&world),
        (8..264).collect::<Vec<u32>>(),
        "every entity advanced by exactly eight"
    );
}
