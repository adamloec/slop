# slop-reflect-derive

**Last updated:** 2026-08-03

## 1. Purpose

`#[derive(Reflect)]` — the compile-time front end to `slop-reflect`.

**Depend on `slop-reflect`, not on this crate.** It re-exports the macro behind
its `derive` feature. A proc macro must live in its own crate, which is a Rust
restriction rather than a design decision, and this README exists because the
crate does.

273 lines, one public macro, three dependencies. It is the smallest crate in the
workspace and should stay that way: anything that can be a `const fn` in
`slop-reflect` belongs there, where it can be tested without expanding a macro.

## 2. Status

| Area | State | Milestone |
|---|---|---|
| `#[derive(Reflect)]` for structs with named fields | Landed | M1 |
| `#[reflect(path = "…")]` — pin a type's identity | Landed | M1 |
| Layout, destructor and blittability computed rather than declared | Landed — see §4 | M1 |
| Enums | Absent — no caller yet | — |
| Tuple structs | Absent — no caller yet | — |

## 3. What the macro will not let you get wrong

`DESIGN.md` §2.4 makes `TypeInfo` a value, and three of its fields are trusted by
the ECS in ways that are memory-unsafe or ABI-unsafe to get wrong. None is taken
from the author:

| Field | How it is obtained |
|---|---|
| Layout | `Layout::new::<Self>()`. Never stated. |
| Destructor | Installed if and only if `std::mem::needs_drop::<Self>()`, which is exact and `const`. A type that gains a `String` field gains a destructor with no edit. |
| Blittability | *Computed*: `#[repr(C)]`, no destructor, and every field blittable. All three fold at compile time. |

A struct containing a `String` cannot claim to cross into a guest's linear
memory however it is annotated. And `#[repr(Rust)]` field ordering is
unspecified, so a type without `#[repr(C)]` is never blittable even if every
field is.

This is why `slop-reflect`'s hand-written impls are rare and each carries a
`// SAFETY:` comment: **the derive cannot get these wrong, and a human can.**

## 4. What the author does control

The path, because identity is a decision rather than a fact.

```rust
#[derive(Reflect)]
#[reflect(path = "game::Inventory")]
struct Inventory { /* … */ }
```

Without the attribute the path comes from `module_path!()` and the type's name.
The override exists because **moving a type between modules changes its
identity**, and identity is what every save file, scene file and network packet
is written against. Refactoring is routine; silently invalidating stored data is
not. Pinning the path lets a type move without a migration.

## 5. `Reflect` is an `unsafe trait`, and what the derive therefore promises

The trait's contract is that `TypeInfo`'s layout is `Layout::new::<Self>()` and
its drop function drops `Self` in place — §3 is how this crate satisfies that
without asking.

Since M2 the contract also requires `Send + Sync`. That clause is enforced by
the supertrait rather than by generated code, so the derive gets it for free: a
type with a non-`Send` field fails to satisfy `Reflect` at the impl site with an
ordinary trait error. `CONSIDERATIONS.md` item 6 records why the clause was
missing and what it let through — `slop_ecs::Column` asserts `Send` and `Sync`
unconditionally over type-erased bytes and defers the claim upward, and before
the bound existed that deferral pointed nowhere.

## 6. Related

- `docs/slop-reflect/README.md` — the trait, `TypeInfo`, and the registry
- `docs/slop-ecs/README.md` — what trusts these fields, and why being wrong is unsafe
- `CONSIDERATIONS.md` item 6 — the `Send + Sync` clause
