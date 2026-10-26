# Developing DorfQ

## Building

Install [bazelisk](https://github.com/bazelbuild/bazelisk) as `bazel`; it
reads `.bazelversion` and fetches the pinned Bazel. Every toolchain is pinned
in `MODULE.bazel` and fetched by checksum, so nothing else is needed on the
host but git and make.

```sh
make setup   # once per clone: wires the git hooks
make check   # the full gate, exactly what CI runs
make test    # every test, without the lint aspects
make run     # build and run the dorfq binary (ARGS=... for arguments)
make help    # every target
```

The `Makefile` is a frontend: every target is a spelled-out `bazel`
invocation.

The gate, the changelog fragments and the TODO system are documented in
[`AGENTS.md`](../AGENTS.md).

## Layout

- `crates/` — the Rust workspace. `Cargo.toml` and `Cargo.lock` are the
  source of truth for crate versions; Bazel builds them through
  `crate_universe`.
  - `crates/dorfq-core/` — the contract between scheduler and forge: the
    observation and plan types, the `Scheduler` and `Forge` traits, and the
    rules both sides keep.
  - `crates/dorfq-scheduler/` — schedulers and their configuration: the
    `Bisect` baseline and the `Bayes` scheduler.
  - `crates/dorfq-forge-mock/` — the in-memory mock forge and its scenario
    format.
  - `crates/dorfq-forge-github/` — the GitHub forge, over the REST API, and
    the live lane (`make live-test`), which runs it against a repository
    whose name contains `scratch` or `playground` and treats that repository
    as disposable.
  - `crates/dorfq/` — the application: configuration, provider construction,
    the driver loop and the `dorfq` binary.
  - `crates/dorfq-sim/` — the simulator: a seeded world that implements the
    forge side of the contract, the loop that runs a scheduler against it,
    the metrics and the `dorfq-sim` binary.
  - `crates/dorfq-sim-wasm/` — the simulator as a WebAssembly module for the
    simulation page's playground.
- `site/` — the Pages site: the home page, the simulation page, and the Bazel
  rules that run the simulator examples into the simulation page's reports.
  `site/sim-worker.js` runs the playground's simulator off the page's thread;
  `site/wasm.bzl` builds it for wasm32, optimised.
- `docs/` — the operator's and simulator guides, and this file.
- `tools/scrub/` — the secret scanner the commit, push and CI gates run,
  built by Bazel from its published crate with a pinned gitleaks.
- `tools/allthechanges/` — the changelog and TODO fragment tool, built by
  Bazel from its published crate.
- `changelog.d/` and `todo.d/` — the changelog and TODO fragment directories:
  each change's entry is a file here until a release (or, for TODOs,
  `make todo-gather`) folds it into `CHANGELOG.md` or `TODO.md`.
- `tools/pins/` — a test holding the version pins that appear in more than
  one file equal.

## Lock files

- `MODULE.bazel.lock` pins the Bazel module graph.
- `cargo-bazel-lock.json` is the rendered view of `Cargo.lock` for the
  workspace's crates.
- `tools/scrub/Cargo.lock` and `tools/scrub/cargo-bazel-lock.json` pin the
  scanner's crates; `tools/allthechanges/Cargo.lock` and
  `tools/allthechanges/cargo-bazel-lock.json` the fragment tool's.

`make repin` re-renders the crate locks after a manifest change.
