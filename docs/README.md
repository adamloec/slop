# Slop Engine — Documentation

**Last updated:** 2026-08-03

## The documents

| Document | Answers | Authoritative for |
|---|---|---|
| [DESIGN.md](DESIGN.md) | **What** we are building | Every architectural decision, with rationale |
| [PLAN.md](PLAN.md) | **When** | Milestones, task breakdown, environment, invariants |
| [CONVENTIONS.md](CONVENTIONS.md) | **How** code is written | Layout, naming, errors, `unsafe`, testing, logging |
| [architecture.md](architecture.md) | **How the pieces fit** | Cross-crate diagrams — layering, frame flow, data movement |
| [../CONSIDERATIONS.md](../CONSIDERATIONS.md) | **What we might change** | Technology worth revisiting; not commitments |
| [reviews/](reviews/) | **What was found, and when** | Completed codebase reviews, kept by date |

Read `DESIGN.md` first and in full before writing any code.

`CONSIDERATIONS.md` sits at the repository root rather than here because it is
not authoritative for anything — it is the holding pen for ideas and technology
worth revisiting. Nothing in it is a decision until it moves into `DESIGN.md` or
`PLAN.md`.

**`reviews/` is where a review goes once it is finished**, and the split is worth
keeping. `CONSIDERATIONS.md` is forward-looking — things that might happen. A
completed review is a record of what *did*, and it stays readable because other
documents cite its findings by number. Deleting one costs those citations; the
2026-08-03 review had fourteen across nine files.

| Review | Tree | Outcome |
|---|---|---|
| [2026-08-03](reviews/2026-08-03.md) | `e9fe35f`, at the M2/M3 boundary | Twelve findings, all acted on |

## Per-crate documentation

One directory per crate, named exactly as the crate is.

| Crate | Docs | Status |
|---|---|---|
| `slop-math` | [slop-math/](slop-math/) | Transforms, projections, portable scalars landed |
| `slop-core` | [slop-core/](slop-core/) | Handles, storage, arena, time, jobs, determinism primitives landed |
| `slop-reflect` | [slop-reflect/](slop-reflect/) | Type model, registry, derive, value and text format landed |
| `slop-ecs` | [slop-ecs/](slop-ecs/) | Data model, scheduler, resources and world serialization landed |
| `slop-asset` | [slop-asset/](slop-asset/) | Cook cache, VFS, the cooked formats and hot reload landed; async streaming outstanding |
| `slop-cook` | [slop-cook/](slop-cook/) | glTF, PNG and Slang importers landed; never linked by a game |
| `slop-rhi` | [slop-rhi/](slop-rhi/) | Vulkan backend through the bindless heap landed |
| `slop-render` | [slop-render/](slop-render/) | Frame loop and `MeshRenderer` landed; the render graph is M3 |
| `slop-editor` | [slop-editor/](slop-editor/) | Debug overlay, frame timing and the entity inspector landed; §10.1's editor is M6 |
| `slop-app` | [slop-app/](slop-app/) | Window, surface, device bring-up, timing and logging landed |
| `slop-verify` | [slop-verify/](slop-verify/) | Golden-image harness landed |
| `slop-cli` | — | A thin front end over `slop-cook`; its behaviour is documented there |
| `slop-reflect-derive` | — | Covered by [slop-reflect/](slop-reflect/); a proc macro must be its own crate |

Crates from `DESIGN.md` §4 that do not exist yet get a directory when they do,
not before.

**A crate document is not optional once the crate exists.** `slop-cook` and
`slop-editor` were created during M2 and went four commits without one, which is
how [architecture.md](architecture.md)'s layering diagram came to describe a
dependency graph that no longer existed. The two crates without a directory are
the two deliberate exceptions above, each with a stated reason.

## Conventions for these documents

Docs here and the rustdoc in the source have different jobs and should not
duplicate each other:

- **Rustdoc** — what an item does and how to call it. Lives with the code so it
  cannot drift from the signature.
- **These docs** — why a crate is shaped the way it is, how its pieces relate,
  and what a newcomer needs before reading the source. Diagrams live here
  because rustdoc cannot render them.

### Every crate document follows the same order

1. **Purpose** — one paragraph. What this crate owns and what it deliberately does not.
2. **Status** — what exists today, what is planned, and at which milestone.
3. **Module map** — a diagram of the crate's internal structure.
4. **Key types** — a table: type, role, and where its decision is recorded.
5. **Diagrams** — lifecycles, data flow, state machines. Whatever the crate's hard part is.
6. **Decisions** — links into `DESIGN.md` and `PLAN.md`. Reasoning lives there; this section points.
7. **Invariants** — what callers and future maintainers must not break.

Sections with nothing to say are omitted rather than left empty.

### Style

- **Prose states a claim and its reason.** The same rule `CONVENTIONS.md` uses:
  a statement without a reason is a statement that gets ignored.
- **Reasoning is never duplicated.** It lives in `DESIGN.md` or `PLAN.md` and is
  cited by section number. Two copies of a rationale become two different
  rationales.
- **Prefer a table to a list** when entries share a shape, and a diagram to a
  table when the subject is flow or state.
- **Cite sections, not page positions** — `DESIGN.md` §2.6, never "see above".

### Diagrams

- **Mermaid, always.** It renders natively on GitHub, diffs as text, and
  normalizes under `.gitattributes` like everything else. No binary image files
  and no external diagramming tools.
- **One diagram, one question.** A diagram that answers three questions answers
  none of them legibly. Split it.
- **No styling or colors.** Defaults render correctly in both light and dark
  themes; hand-picked colors do not.
- **Consistent shapes across the whole docset**, so a reader learns the
  vocabulary once:

| Shape | Means | Mermaid |
|---|---|---|
| Rectangle | A crate, module, or type | `A[slop-core]` |
| Rounded | A process or stage | `A(cook)` |
| Cylinder | Stored or owned data | `A[(assets)]` |
| Diamond | A decision or branch | `A{live?}` |
| Dashed arrow | Deferred, optional, or planned | `A -.-> B` |

- **Direction is meaningful.** `flowchart TD` for layering and hierarchy, where
  down means "depends on". `flowchart LR` for pipelines, where right means
  "later in time".
