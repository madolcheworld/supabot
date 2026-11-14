#!/usr/bin/env bash
# Prove that an ACP backend on *this* machine can complete a turn — not just
# answer `initialize`.
#
#   scripts/opencode-live.sh                 # opencode, the configured default
#   scripts/opencode-live.sh claude          # claude-code-acp
#   scripts/opencode-live.sh codex           # codex-acp
#
# Why this exists: `probe` (and `GET /api/backends/probe`) stops after
# `initialize` + `session/new`, which costs no model call — so a backend whose
# *account* is not usable still reports `ok: true`. On 2026-11-14 all three ACP
# agents on this box probed green and only opencode answered: claude-code-acp
# refused `session/prompt` with "Authentication required" and codex-acp's
# ChatGPT account rejected every model it advertises. This script runs the one
# ignored test that spends a single small model call and asserts on the reply,
# which is the only proof that the backend is actually wired end to end.
#
# Environment overrides (all optional):
#   ACP_CMD    binary to spawn        (default: per backend id below)
#   ACP_ARGS   extra argv             (default: per backend id below)
#   ACP_MODEL  model id               (default: the model pinned in
#                                      config/swarmbot.toml for opencode)
#
# Exits 0 only when the turn answered PONG. A failed turn prints the agent's own
# refusal, not a summary of it.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BACKEND="${1:-opencode}"
case "$BACKEND" in
    opencode)
        CMD="${ACP_CMD:-opencode}"
        ARGS="${ACP_ARGS:-acp}"
        # The model pinned in config/swarmbot.toml. Without a model opencode
        # picks its own default and dies with
        # `no provider's ask matches your max-per-mtok bid`.
        MODEL="${ACP_MODEL:-inferhub/ali/qwen3.8-2.4t-a95b}"
        ;;
    claude | claude-code-acp)
        CMD="${ACP_CMD:-claude-code-acp}"
        ARGS="${ACP_ARGS:-}"
        MODEL="${ACP_MODEL:-}"
        ;;
    codex | codex-acp)
        CMD="${ACP_CMD:-codex-acp}"
        ARGS="${ACP_ARGS:-}"
        MODEL="${ACP_MODEL:-}"
        ;;
    -h | --help)
        sed -n '2,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "unknown backend: $BACKEND (try: opencode claude codex)" >&2
        exit 2
        ;;
esac

if ! command -v "$CMD" >/dev/null 2>&1; then
    echo "cannot find \`$CMD\` on PATH — install it, or set ACP_CMD" >&2
    exit 1
fi

echo "backend:  $BACKEND (spawns \`$CMD $ARGS\`)"
echo "model:    ${MODEL:-<the agent default>}"
echo "this spends one small model call."
echo

# shellcheck source=scripts/env.sh
source "$ROOT/scripts/env.sh" >/dev/null

PROTO_LIVE_ACP=1 \
PROTO_LIVE_ACP_CMD="$CMD" \
PROTO_LIVE_ACP_ARGS="$ARGS" \
PROTO_LIVE_ACP_MODEL="$MODEL" \
PROTO_LIVE_ACP_PROMPT=1 \
    cargo test -p swarmbot-llm --test acp -- --ignored --nocapture \
        live_acp_agent_handshake_and_one_cheap_turn 2>&1 | tail -n 25
exit "${PIPESTATUS[0]}"
