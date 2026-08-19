#!/usr/bin/env bash
# hānihi self-improvement driver: task mode → rebuild → eval → report.
#
# Usage:
#   scripts/self-improve.sh "TASK DESCRIPTION"
#
# Runs hānihi in task mode with write tools enabled against the enclosing
# repo, rebuilds the workspace, then runs the eval suite as the gate. The
# running hānihi process is never replaced mid-flight — the freshly built
# binary is only exercised by the eval run at the end.
set -euo pipefail

cd "$(dirname "$0")/.."

TASK="${1:?usage: scripts/self-improve.sh \"TASK DESCRIPTION\"}"
SESSION="self-improve-$(date +%Y%m%d-%H%M%S)"
export LLM_API_KEY="${LLM_API_KEY:?set LLM_API_KEY to run self-improve}"

echo "==> task mode (session: $SESSION, max turns: 100)"
cargo run -p hanihi-cli -- --write --task "$TASK" --new-session "$SESSION" --max-turns 100

echo "==> rebuild"
cargo build --workspace

echo "==> eval gate"
cargo run -p hanihi-eval -- --cases-dir ./evals/cases

echo "==> done: $SESSION"
