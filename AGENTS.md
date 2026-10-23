# dorfq — agent instructions

## What this is

DorfQ is a merge queue for large monorepos that pools pull requests into
batched CI runs and infers the culprit when a batch fails. Charter and status:
`README.md`. Read it first.

## Build: Bazel, and only Bazel

Bazel is the build. The Makefile is a frontend whose every target is a
spelled-out `bazel` invocation; there is no target that runs `cargo`, and the
gate never will.

- Every toolchain and every tool the gate runs is pinned in `MODULE.bazel` and
  fetched by checksum. The only host prerequisites are bazelisk (as `bazel`),
  git and make.
- Nothing in a build or test action may reach for a host-installed program.
  A test that needs a service, a fixture generator or a linter gets it as a
  Bazel dependency. If a tool is genuinely impractical to bring in through
  Bazel, pick a different tool, and if that is not possible either, surface it
  as a decision rather than adding a host dependency.
- `Cargo.toml` + `Cargo.lock` remain the source of truth for crate versions
  and features; `cargo-bazel-lock.json` is their rendered view. A second crate
  hub under `tools/scrub/` pins the secret scanner the same way, with its own
  `Cargo.lock` and `cargo-bazel-lock.json`.
- `make repin` re-renders both hubs at once. After any manifest change or
  scanner bump, run it and commit every lock file that changed.
- `RUST_VERSION` in `MODULE.bazel`, the channel in `rust-toolchain.toml`,
  `rust-version` in `Cargo.toml` and `msrv` in `clippy.toml` are held equal
  by `//tools/pins:pins_test`;
  so are the crate version in `Cargo.toml` and `VERSION` in
  `crates/dorfq/BUILD.bazel`. `rust-toolchain.toml` only serves the tools that
  run outside Bazel (rust-analyzer).

## Commands

| Command | What it does |
|---|---|
| `make check` | The gate: scrub kit check, build, every test, clippy, rustfmt, buildifier, tree scan |
| `make build` | Build every target |
| `make test` | Every test, without the lint aspects (the inner loop) |
| `make run ARGS=...` | Build and run the `dorfq` binary |
| `make release` | Build the `dorfq` binary optimised |
| `make fix` | rustfmt and buildifier the tree in place |
| `make repin` | Re-render every crate hub's lock files |
| `make ide` | Write `rust-project.json` for rust-analyzer |
| `make setup` | Wire git at `.githooks` (once per clone) |
| `make scrub-tree` | Whole-tree secret sweep |
| `make clean` | Drop Bazel's outputs for this workspace |

## Gates

- A commit scans the staged change for secrets and then runs `make check`.
- A push scans the range being pushed.
- CI runs `make check` on every push and pull request, with the module
  lockfile in error mode, so a resolution nobody committed is a red build.
- The Claude Code write hook scrubs every write before it lands and blocks on
  any failure, including a scanner that fails to start.

Every scan runs `bazel run //tools/scrub:brenn-scrub`. Treat the local hooks
as the gate and CI as a backstop, not the other way round: a CI run says
nothing about the commit sitting in your working tree.

If a gate blocks a write or a commit, **surface it**. Never route around it.
The gate being wrong is a thing that happens and is worth reporting; a
bypassed gate is not recoverable once it has been pushed.

## Principles

- Fail fast. Invalid input is rejected, not coerced. No fallbacks that hide a
  missing authority.
- No over-engineering, no premature abstraction, no hypothetical features.
  Scope discipline: do what was asked.
- The queue's own invariants are written here when the code that has them
  arrives.

## Boundary

The scheduler and the forge meet only in `crates/dorfq-core`: the forge
reports an `Observation`, the scheduler turns it into a `Plan`, and the
driver in `crates/dorfq` executes the plan through the forge. The scheduler
never calls the forge; the forge never sees the scheduler. The **latest
observation** is the one `observe()` most recently returned; a pull request
or batch is **known** when the latest observation reported it and no effect
since has removed it: `eject` removes the pull request, `retire` the batch,
and a successful `land` every member of the batch it landed; a refused
landing removes nothing.

1. **Facts flow one way.** `observe()` is the only source of facts. Effects
   return `Ok(())`; their consequences appear in the next observation. A
   forge keeps a batch in `batches` from `start_batch` until `retire`,
   whatever its state.
2. **The forge enforces the landing invariants.** `land(b)` is accepted
   only for a batch the latest observation reported as
   `Testing { ci: Passed }`. Trunk is fast-forwarded to its tip only if its
   base is the current trunk and every member is still enqueued at the head
   it was built with; otherwise the batch becomes `LandRefused` and the call
   returns `Ok(())`. The forge does not re-check CI at `land`.
3. **Plan items are requests.** The scheduler learns facts only from
   observations and never assumes a planned effect happened. It may remember
   what it emitted, never what it inferred the forge did with it. A batch id
   that never appears in an observation was never started.
4. **The scheduler is deterministic.** `step` reads no clock, randomness or
   I/O; its output is a function of its own state and the observations it
   has been given, in order. Time, when needed, arrives as an observation
   field.
5. **Plans are well-formed; batch ids are the scheduler's.** A new batch id
   is not in the latest observation; members are non-empty, distinct and
   known; the base is the latest trunk or a tracked batch's tip. Within a
   plan, `eject` names a pull request at most once, each of `start`, `land`,
   `retire` names a batch at most once, and no member of a batch `land`
   names is named by `start` or `eject`, whether or not the landing is
   refused. A violation is `Permanent`.
6. **Effects on unknown things are bugs.** `land` or `retire` on an unknown
   batch, `land` on a batch not reported `Testing { ci: Passed }`, `eject`
   on an unknown pull request: all `Permanent`. Knownness is judged against
   the latest observation and the forge's own effects since, not against
   what the world did meanwhile.
7. **The driver executes `land`, `start`, `eject`, `retire`, in that order,
   and stops at the first error.** `Transient` abandons the rest of the plan
   and ticks again; `Permanent` stops the process.
8. **Ordering is canonical.** `prs` is sorted by number, `batches` by id.
9. **The forge talks to authors** about every transition the queue causes
   or observes: entry, inclusion in a batch, landing, ejection with the
   scheduler's reason, and leaving by neither. Nothing about messages
   crosses the boundary.

A new field or variant in the contract is a coordinated change of
`dorfq-core` and both sides, landed on `main` before either builds on it.

## Code standards

- Rust edition 2024, the pinned stable toolchain. The edition is set once
  for Bazel, on the toolchain in `MODULE.bazel`; BUILD targets do not
  restate it.
- `make check` must pass: clippy with warnings denied, rustfmt, buildifier,
  every test.
- No unused code. No backwards-compatibility shims.
- Default branch: `main`.

## TODO system

Two pieces that stay in sync:

- `TODO.md` at the repo root: the master list. Each entry has a slug, a
  description, and the deferral context.
- `TODO(slug)` comments in code, marking the spot where the work happens.

Slugs are the join key; adding a TODO requires both halves. Don't use TODOs for
vague aspirations. Every TODO describes a concrete thing, in a place where
"done" is obvious.

## Public tree

Everything tracked here is published. `TODO.md`, `CHANGELOG.md`, commit
messages and every code comment are public writing: no internal host names,
no absolute paths into other trees, no references to documents a public reader
cannot follow. Design documents live outside this repository and are never
cited from code.
