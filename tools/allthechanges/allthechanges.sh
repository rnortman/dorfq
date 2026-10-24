#!/usr/bin/env bash
# Runs allthechanges from runfiles in the workspace root, where changelog.d,
# CHANGELOG.md, todo.d and TODO.md live. The arguments pass through.
set -euo pipefail

# --- begin runfiles.bash initialization v3 ---
# Copy-pasted from the Bazel Bash runfiles library v3.
set -uo pipefail; set +e; f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f2- -d' ')" 2>/dev/null || \
  source "$0.runfiles/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "$0.runfiles_manifest" | cut -f2- -d' ')" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "$0.exe.runfiles_manifest" | cut -f2- -d' ')" 2>/dev/null || \
  { echo>&2 "ERROR: cannot find $f"; exit 1; }; f=; set -e
# --- end runfiles.bash initialization v3 ---

if [[ -z "${BUILD_WORKSPACE_DIRECTORY:-}" ]]; then
  echo "allthechanges: BUILD_WORKSPACE_DIRECTORY is not set; run this through 'bazel run', not from bazel-bin" >&2
  exit 1
fi

tool="$(rlocation "$ALLTHECHANGES_BIN")"
if [[ -z "$tool" || ! -x "$tool" ]]; then
  echo "allthechanges: runfile $ALLTHECHANGES_BIN not found" >&2
  exit 1
fi

cd "$BUILD_WORKSPACE_DIRECTORY"
exec "$tool" "$@"
