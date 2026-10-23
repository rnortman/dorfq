# TODO

The master list of deferred work. Each entry has a slug, a description, and the
reason it was deferred; the matching `TODO(slug)` comment in code marks the spot
where the work happens. Slugs are the join key: adding a TODO requires both
halves, and removing one removes both.

Every entry is public writing.

## Open

### dependency-audit-lane

A license and advisory audit of the Rust dependency graph (for example
`cargo deny`) belongs in `make check` before the first crate is published.
Running it through Bazel needs a decision first: the advisory check fetches
the RustSec database over the network, which a sandboxed test action must not
do, and the tool shells out to `cargo metadata`, so it needs the pinned
toolchain's cargo handed to it. The options are a repository rule that
fetches the advisory database at a pinned commit so the check is a sandboxed
test; an unsandboxed `bazel run` lane like the scrub gate; or a licenses-and-
bans test now with advisories later. The `TODO(dependency-audit-lane)` comment
sits above `check` in the `Makefile`.
