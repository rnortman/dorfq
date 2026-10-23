# Changelog

Notable changes to dorfq. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial repository scaffolding; there is no merge-queue functionality yet.
  The `dorfq` binary is a placeholder that proves the build works.
- A Bazel-only build: Rust, LLVM and buildtools toolchains and every tool the
  gate runs are pinned and fetched by checksum, so a clone needs only
  bazelisk, git and make. `make check` is the single gate (build, tests,
  clippy, rustfmt, buildifier, secret scan).
- Secret scanning with brenn-scrub, built through Bazel, wired into the
  pre-commit and pre-push git hooks (`make setup`) and Claude Code write hooks.
- GitHub Actions CI running `make check` on every push and pull request.
