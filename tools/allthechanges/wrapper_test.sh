#!/usr/bin/env bash
# Asserts that the allthechanges wrapper exits 1 with its refusal when
# BUILD_WORKSPACE_DIRECTORY is unset, rather than running the tool in
# whatever directory it was started from.
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

wrapper="$(rlocation "$WRAPPER")"
if [[ ! -x "$wrapper" ]]; then
    echo "FAIL: wrapper $WRAPPER not found in runfiles" >&2
    exit 1
fi

# ALLTHECHANGES_BIN stays set, so without the guard the tool would run here
# and report on a directory that holds no fragments.
status=0
output="$(env -u BUILD_WORKSPACE_DIRECTORY "$wrapper" query 2>&1)" || status=$?

if [[ "$status" -ne 1 ]]; then
    echo "FAIL: wrapper exited $status without BUILD_WORKSPACE_DIRECTORY, expected 1" >&2
    printf '%s\n' "$output" >&2
    exit 1
fi
if [[ "$output" != *"BUILD_WORKSPACE_DIRECTORY is not set"* ]]; then
    echo "FAIL: wrapper output lacks the refusal:" >&2
    printf '%s\n' "$output" >&2
    exit 1
fi
echo "wrapper refuses outside bazel run"
