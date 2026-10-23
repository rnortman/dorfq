# Changelog

Notable changes to dorfq. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A GitHub forge (`kind = "github"` in the configuration): dorfq can run a
  queue on a GitHub or GitHub Enterprise repository through the REST API,
  polling once per tick. A label marks a pull request as queued; batches are
  built from merge commits on `dorfq/batch/<id>` branches and landed by
  fast-forwarding trunk once their required checks pass; authors are told
  each step in a pull request comment. Queue state lives in the repository
  itself, so a restarted queue picks up where it left off. See
  `crates/dorfq/examples/github.toml` and "Running against GitHub" in the
  README. Example systemd units, user and system, are in
  `crates/dorfq/examples/systemd/`.
- Operators should know that the queue label is the landing authority:
  anyone who can apply it (Triage role included) lands the pull request at
  the head it has when batched, without branch protection's review rules,
  and batch CI runs that code with the repository's secrets, fork pull
  requests included. Restrict who can label accordingly.
- `make live-test`: exercises the GitHub forge end to end against a
  disposable repository whose name contains `scratch` or `playground`.
- `dorfq run --config <path>`: the first end-to-end loop. It reads a TOML
  configuration naming a forge (the service hosting the repository and its
  pull requests) and a scheduler (the part that decides what to batch, land
  or eject), then repeatedly asks the forge what has changed and carries out
  the scheduler's plan. `--exit-when-idle` stops once there is nothing left
  to do. Running `dorfq` with no subcommand is now a usage error.
- Only stand-ins exist on either side so far: a deliberately simple
  `all-at-once` scheduler that tests every ready pull request in one batch
  and ejects them all if it fails, and an in-memory `mock` forge that plays
  a scripted scenario. Try it with the shipped example,
  `crates/dorfq/examples/mock.toml`.
- The contract between scheduler and forge (`dorfq-core`) is fixed, so the
  two sides can now be developed independently. Its rules are written down
  in `AGENTS.md`.
- Initial repository scaffolding.
- A Bazel-only build: Rust, LLVM and buildtools toolchains and every tool the
  gate runs are pinned and fetched by checksum, so a clone needs only
  bazelisk, git and make. `make check` is the single gate (build, tests,
  clippy, rustfmt, buildifier, secret scan).
- Secret scanning with brenn-scrub, built through Bazel, wired into the
  pre-commit and pre-push git hooks (`make setup`) and Claude Code write hooks.
- GitHub Actions CI running `make check` on every push and pull request.

### Changed

- CI also runs on pushes to `dorfq/batch/**`, so batches the queue builds get
  the `check` it waits for before landing.
