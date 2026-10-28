- `make musl` builds `dorfq` as an optimised, statically linked musl binary
  that runs on any x86_64 Linux host regardless of its libc, and prints its
  path, `bazel-out/k8-opt/bin/crates/dorfq/dorfq-musl`. `make check` builds it
  unoptimised and runs the CLI suite against it.
