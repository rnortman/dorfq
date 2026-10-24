# dorfq — agent instructions

## What this is

DorfQ is a merge queue for large monorepos that pools pull requests into
batched CI runs and infers the culprit when a batch fails. Charter and status:
`README.md`. Read it first.

The scheduler–forge contract and its rules are documented in
`crates/dorfq-core`; the GitHub forge's repository layout in
`crates/dorfq-forge-github`.

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
| `make sim ARGS=...` | Build and run the `dorfq-sim` simulator |
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

## Code standards

- Rust edition 2024, the pinned stable toolchain. The edition is set once
  for Bazel, on the toolchain in `MODULE.bazel`; BUILD targets do not
  restate it.
- `make check` must pass: clippy with warnings denied, rustfmt, buildifier,
  every test.
- No unused code. No backwards-compatibility shims.
- Schedulers use ordered collections (`BTreeMap`, `BTreeSet`); `HashMap`'s
  iteration order is per-process random and would make `step`
  nondeterministic.
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
