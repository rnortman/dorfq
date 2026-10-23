# dorfq

DorfQ (Dorfman + queue, after Robert Dorfman's pooled testing) is a merge
queue for large monorepos with long CI runs. It pools queued pull requests
into batched CI runs, so one passing run lands many changes. When a batch
fails, it infers the culprit from what failed instead of bisecting blind,
keeps landing the rest, and tells every author what happened to their pull
request. It lands only the exact tree that passed CI, by fast-forwarding
trunk.

## Status

Scaffolding only: the build, the gates and a placeholder `dorfq` binary.
There is no queue yet. `CHANGELOG.md` tracks what exists.

## Building

Install [bazelisk](https://github.com/bazelbuild/bazelisk) as `bazel`; it
reads `.bazelversion` and fetches the pinned Bazel. Every toolchain is pinned
in `MODULE.bazel` and fetched by checksum, so nothing else is needed on the
host but git and make.

```sh
make setup   # once per clone: wires the git hooks
make check   # the full gate, exactly what CI runs
make run     # build and run the dorfq binary (ARGS=... for arguments)
make help    # every target
```

The `Makefile` is a frontend: every target is a spelled-out `bazel`
invocation.

## Layout

- `crates/` — the Rust workspace. `Cargo.toml` and `Cargo.lock` are the
  source of truth for crate versions; Bazel builds them through
  `crate_universe`.
- `tools/scrub/` — the secret scanner the commit, push and CI gates run,
  built by Bazel from its published crate with a pinned gitleaks.
- `tools/pins/` — a test holding the version pins that appear in more than
  one file equal.
- Lock files: `MODULE.bazel.lock` pins the Bazel module graph;
  `cargo-bazel-lock.json` is the rendered view of `Cargo.lock` for the
  workspace's crates; `tools/scrub/Cargo.lock` and
  `tools/scrub/cargo-bazel-lock.json` pin the scanner's crates. `make repin`
  re-renders the crate locks after a manifest change.

## License

Apache-2.0; see `LICENSE` and `NOTICE`.
