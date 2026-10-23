#!/usr/bin/env bash
# Asserts that the version pins stated in more than one file agree:
#   1. RUST_VERSION in MODULE.bazel == channel in rust-toolchain.toml;
#   2. rust-version in Cargo.toml == major.minor of that version;
#   3. version under [workspace.package] in Cargo.toml == VERSION in
#      crates/dorfq/BUILD.bazel;
#   4. msrv in clippy.toml == rust-version in Cargo.toml.
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

module_bazel="$(rlocation "$MODULE_BAZEL")"
rust_toolchain_toml="$(rlocation "$RUST_TOOLCHAIN_TOML")"
cargo_toml="$(rlocation "$CARGO_TOML")"
clippy_toml="$(rlocation "$CLIPPY_TOML")"
crate_build="$(rlocation "$CRATE_BUILD")"

# Prints the one value a sed script extracts from a file; fails unless there
# is exactly one.
extract() {
    local what="$1" file="$2" script="$3" values count
    values="$(sed -n "$script" "$file")"
    count="$(grep -c . <<<"$values" || true)"
    if [[ "$count" -ne 1 ]]; then
        echo "FAIL: expected one $what in $file, found $count" >&2
        exit 1
    fi
    printf '%s\n' "$values"
}

rust_version="$(extract RUST_VERSION "$module_bazel" \
    's/^RUST_VERSION = "\([^"]*\)"$/\1/p')"
channel="$(extract channel "$rust_toolchain_toml" \
    's/^channel = "\([^"]*\)"$/\1/p')"
msrv="$(extract rust-version "$cargo_toml" \
    's/^rust-version = "\([^"]*\)"$/\1/p')"
workspace_version="$(extract "[workspace.package] version" "$cargo_toml" \
    '/^\[workspace\.package\]$/,/^\[/s/^version = "\([^"]*\)"$/\1/p')"
build_version="$(extract VERSION "$crate_build" \
    's/^VERSION = "\([^"]*\)"$/\1/p')"
clippy_msrv="$(extract msrv "$clippy_toml" \
    's/^msrv = "\([^"]*\)"$/\1/p')"

status=0

if [[ "$rust_version" != "$channel" ]]; then
    echo "FAIL: RUST_VERSION in MODULE.bazel is $rust_version but channel in" \
        "rust-toolchain.toml is $channel" >&2
    status=1
fi

major_minor="$(sed -n 's/^\([0-9]*\.[0-9]*\)\.[0-9]*$/\1/p' <<<"$rust_version")"
if [[ "$msrv" != "$major_minor" ]]; then
    echo "FAIL: rust-version in Cargo.toml is $msrv but RUST_VERSION in" \
        "MODULE.bazel is $rust_version (expected $major_minor)" >&2
    status=1
fi

if [[ "$workspace_version" != "$build_version" ]]; then
    echo "FAIL: version in Cargo.toml [workspace.package] is $workspace_version" \
        "but VERSION in crates/dorfq/BUILD.bazel is $build_version" >&2
    status=1
fi

if [[ "$clippy_msrv" != "$msrv" ]]; then
    echo "FAIL: msrv in clippy.toml is $clippy_msrv but rust-version in" \
        "Cargo.toml is $msrv" >&2
    status=1
fi

if [[ "$status" -eq 0 ]]; then
    echo "pins agree: rust $rust_version (msrv $msrv), dorfq $workspace_version"
fi
exit "$status"
