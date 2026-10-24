Share the Bazel setup between `.github/workflows/ci.yml` and
`.github/workflows/pages.yml`. Both restore the repository and disk caches
under the same `hashFiles(...)` keys and write the same `.bazelrc.user`
lines, copied by hand so that Pages is served from what CI built. A cache
key or Bazel flag changed in one file and not the other fails silently: the
Pages job restores cold by prefix or writes entries CI never reads. Move
both cache steps and the `.bazelrc.user` lines into a local composite
action (for example `.github/actions/bazel-ci/action.yml`, with an input for
CI's extra `test --keep_going`) and call it from both workflows. This
rewrites the gate's own workflow, so it waits for a moment when no other
branch is editing `ci.yml`.
The `TODO(quality-pages-workflow-copies-ci-bazel-setup)` comment sits above the cache steps in `.github/workflows/pages.yml`.
