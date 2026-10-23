#!/bin/bash
# Claude Code PostToolUse: format the one file just written, with the same
# pinned tools the gate checks it with. Silent on success; a parse error in
# mid-edit code is the gate's business, not this hook's.
set -u
cd "$CLAUDE_PROJECT_DIR"
file=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("tool_input",{}).get("file_path",""))' 2>/dev/null)
case "$file" in
    *.rs)
        bazel run --noshow_progress --ui_event_filters=-info \
            @rules_rust//tools/upstream_wrapper:rustfmt -- "$file" >/dev/null 2>&1 ;;
    *.bazel|*.bzl|BUILD|MODULE.bazel|*/BUILD|*/MODULE.bazel)
        bazel run --noshow_progress --ui_event_filters=-info \
            @buildifier_prebuilt//:buildifier -- "$file" >/dev/null 2>&1 ;;
esac
exit 0
