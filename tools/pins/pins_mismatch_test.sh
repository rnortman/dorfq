#!/usr/bin/env bash
# Runs pins_test.sh against copies of the pinned files with one pin broken at
# a time, and asserts each run fails with the message for that pin. The
# unbroken copies must pass first, so a failure below comes from the break and
# not from the harness.
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

pins_test="$(rlocation "$PINS_TEST")"
vars=(MODULE_BAZEL RUST_TOOLCHAIN_TOML CARGO_TOML CLIPPY_TOML CRATE_BUILD)

failures=0

# run_case NAME VAR SED_SCRIPT EXPECTED: copies the pinned files, applies
# SED_SCRIPT to the one VAR names (none when VAR is empty), runs pins_test.sh
# on the copies, and checks it exits 1 with EXPECTED on stderr, or exits 0
# when EXPECTED is empty.
run_case() {
    local name="$1" var="$2" script="$3" expected="$4"
    local dir="$TEST_TMPDIR/$name" v rc
    local -a env_args=()
    mkdir -p "$dir"
    for v in "${vars[@]}"; do
        cp "$(rlocation "${!v}")" "$dir/$v"
        env_args+=("$v=$dir/$v")
    done
    if [[ -n "$var" ]]; then
        sed -i "$script" "$dir/$var"
        if cmp -s "$(rlocation "${!var}")" "$dir/$var"; then
            echo "FAIL: $name: the edit left $var unchanged" >&2
            failures=$((failures + 1))
            return
        fi
    fi
    if env "${env_args[@]}" bash "$pins_test" >"$dir/out" 2>"$dir/err"; then
        rc=0
    else
        rc=$?
    fi
    if [[ -z "$expected" ]]; then
        if [[ "$rc" -ne 0 ]]; then
            echo "FAIL: $name: exit $rc on unbroken copies" >&2
            cat "$dir/err" >&2
            failures=$((failures + 1))
        fi
    elif [[ "$rc" -ne 1 ]] || ! grep -qF -- "$expected" "$dir/err"; then
        echo "FAIL: $name: expected exit 1 and \"$expected\", got exit $rc" >&2
        cat "$dir/err" >&2
        failures=$((failures + 1))
    fi
}

run_case unbroken "" "" ""
run_case channel RUST_TOOLCHAIN_TOML \
    's/^channel = ".*"$/channel = "0.0.0"/' \
    "FAIL: RUST_VERSION in MODULE.bazel is"
run_case rust_version CARGO_TOML \
    's/^rust-version = ".*"$/rust-version = "0.0"/' \
    "FAIL: rust-version in Cargo.toml is 0.0"
run_case crate_version CRATE_BUILD \
    's/^VERSION = ".*"$/VERSION = "9.9.9"/' \
    "FAIL: version in Cargo.toml [workspace.package] is"
run_case clippy_msrv CLIPPY_TOML \
    's/^msrv = ".*"$/msrv = "0.0"/' \
    "FAIL: msrv in clippy.toml is 0.0"
run_case missing_value RUST_TOOLCHAIN_TOML \
    '/^channel = /d' \
    "FAIL: expected one channel in"
run_case duplicate_value CLIPPY_TOML \
    '/^msrv = /p' \
    "FAIL: expected one msrv in"

if [[ "$failures" -ne 0 ]]; then
    exit 1
fi
echo "pins_test fails on each broken pin"
