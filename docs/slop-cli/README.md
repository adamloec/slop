# slop-cli

**Last updated:** 2026-08-03

## 1. Purpose

The `slop` command-line tool. `DESIGN.md` §4 scopes it as "build, cook, run,
inspect, test", and §7 counts tooling among the things that make this a platform
rather than an engine.

Today it cooks and it fetches. The other verbs arrive with the subsystems they
serve — there is nothing to `run` until there is a game runtime to run, and
nothing to `inspect` until the editor exists.

```
slop cook  [--root .] [--force] [--watch]   ──►  slop_cook::all
slop fetch [name] [--root .] [--force]      ──►  git, into assets/vendor/
```

## 2. Status

| Area | State | Milestone |
|---|---|---|
| `cook` — shaders, models and textures | Landed | M1 |
| `cook --watch` — recook on a timer | Landed — polls, see §5 | M1 |
| `cook --force` — ignore the content-hash cache | Landed | M1 |
| `fetch` — vendored test assets, `fetch` with no name lists them | Landed | M2 |
| `run` — launch a project | Planned | M4 |
| `inspect` — dump cooked artifacts | Planned | M4 |
| `test` — drive the golden suites | Planned | M5 |

## 3. Why this crate is thin

397 lines. It was 3,319, and 3,167 of those were the cooker itself, `pub(crate)`
inside a binary where nothing else could reach it. That moved to `slop-cook` at
M2 and this became a front end over it — the same library-plus-binary shape
everything else here has.

What is left is genuinely command-line-shaped: argument parsing, the watch loop,
the fetch catalogue, and installing a `tracing` subscriber. That last one is the
`CONVENTIONS.md` §5.1 line — **only the application layer knows how it was
launched**, so reading `SLOP_LOG` and installing a subscriber happens here and
in no library.

## 4. `anyhow`, correctly this time

This is an application, so `anyhow` is right rather than merely tolerated —
the distinction `CONVENTIONS.md` §6 draws. Nothing here is a library surface,
and a person reading a failure wants the context chain, not a variant to match
on.

`{error:#}` renders that whole chain on one line, which for a shader error is
the file, the stage and the message.

The same choice inside `slop-cook` is a library using `anyhow` and is argued
separately — see that crate's README §6, and `docs/reviews/2026-08-03.md` item 11 for its
recorded expiry.

## 5. `--watch` polls, and that is a first cut

It walks the tree and hashes every source four times a second. That is nothing
for this project and wrong for a large one, and `PLAN.md` §6.1 carries the row.

The reason to start here rather than with filesystem events: the content-hash
cache already decides what needs doing, so a poll that finds nothing changed
does no work beyond the walk. Correctness comes from the same code path as a
one-shot cook, rather than from a watcher being right about what changed. An
event-driven watcher replaces the loop and touches nothing else.

A cook that fails is reported and the loop continues. Exiting on a syntax error
would mean restarting the watcher every time a shader is mid-edit, which is most
of the time it is being watched.

## 6. `fetch` records how to get an asset, not the asset

Sponza is 51 MB across 71 files. Committing it would put those bytes in git
history permanently — every clone pays for them forever, and history is the one
thing a repository cannot take back. So `assets/vendor/` is gitignored and the
catalogue records the source.

The cost is real and worth naming: a fresh clone cannot render Sponza until
someone runs this, and any test needing Sponza must skip when it is absent.
Skipping is a hazard — the golden suite once reported green while the demo
refused to start, because every setup failure was treated as a skip. The rule
that came out of that holds here: **a missing vendored asset is the one
legitimate skip, checked for by name, and everything else is a failure.**

It shells out to `git` rather than using an HTTP client because the upstream
repository holds every Khronos sample — gigabytes — with no per-directory
archive, so fetching one model over HTTP means 71 URLs and 71 chances for a
partial download to look like success.

## 7. Related

- `docs/slop-cook/README.md` — the library this is a front end over
- `docs/slop-asset/README.md` — what reads the artifacts this writes
- `PLAN.md` §6.1 — the polling watcher's row
