#!/bin/bash
# Claude Code PreToolUse: scrub added text before it lands on disk.
# Exit 2 blocks the write and feeds stderr back to the agent. The hook is
# registered with onFailure=block, so a missing bazel, a failed build or a
# timeout also block: this gate never fails open.
set -u
cd "$CLAUDE_PROJECT_DIR"
exec bazel run --noshow_progress --ui_event_filters=-info \
    //tools/scrub:brenn-scrub -- hook
