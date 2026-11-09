#!/usr/bin/env bash
# Smoke-check the running brain's provider surface — read-only, spends no
# model calls. Prints, per configured backend: kind, the spawn command, and
# how many models it advertises (or why it could not say). With `--probe` it
# also runs the reachability probe (spawns agents / dials gateways; an ACP
# probe costs a handshake plus session/new, never a completion).
#
#   scripts/provider-smoke.sh            # lists + model counts
#   scripts/provider-smoke.sh --probe    # plus reachability rows
#
# Exits 0 when the brain answered every call, 1 when it could not be reached,
# 2 when no owner token was found. A backend whose model list errors is
# reported, not fatal: some agents are honestly silent until configured.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATA="${SWARMBOT_DATA_DIR:-$ROOT/data}"
URL="${SWARMBOT_BRAIN_URL:-http://127.0.0.1:8787}"
TOK="${SWARMBOT_TOKEN:-$(cat "$DATA/owner-token" 2>/dev/null || true)}"
if [ -z "$TOK" ]; then
    echo "no owner token at $DATA/owner-token (is the brain running with this data dir?)" >&2
    exit 2
fi
export SWARMBOT_BRAIN_URL="$URL" SWARMBOT_TOKEN="$TOK"

health="$(curl -sf -m 5 -H "Authorization: Bearer $TOK" "$URL/api/health")" || {
    echo "brain unreachable at $URL" >&2
    exit 1
}
echo "brain: $health"

BACKENDS="$(curl -sf -m 10 -H "Authorization: Bearer $TOK" "$URL/api/backends")" || {
    echo "cannot list backends at $URL/api/backends" >&2
    exit 1
}
export BACKENDS
python3 - "$URL" "$TOK" <<'PY'
import json, os, subprocess, sys

url, tok = sys.argv[1], sys.argv[2]
for backend in json.loads(os.environ["BACKENDS"]):
    options = backend.get("options") or {}
    command = options.get("command") or options.get("binary") or "-"
    args = " ".join(options.get("args") or [])
    runs = f"{command} {args}".strip()
    listing = subprocess.run(
        ["curl", "-sf", "-m", "90", "-H", f"Authorization: Bearer {tok}",
         f"{url}/api/backends/{backend['id']}/models"],
        capture_output=True, text=True,
    )
    if listing.returncode == 0:
        models = json.loads(listing.stdout or "[]")
        detail = f"{len(models)} model(s)"
        if models:
            detail += f", e.g. {models[0]['id']}"
    else:
        detail = f"model list unavailable: {(listing.stderr or listing.stdout)[:120].strip()}"
    print(f"  {backend['id']:<10} {backend['kind']:<15} runs `{runs}` — {detail}")
PY
status=$?

if [ "${1:-}" = "--probe" ]; then
    echo "probe (spawns agents / dials gateways; no completions):"
    PROBES="$(curl -sf -m 300 -H "Authorization: Bearer $TOK" "$URL/api/backends/probe")" || {
        echo "probe call failed" >&2
        exit 1
    }
    export PROBES
    python3 - <<'PY'
import json, os
for row in json.loads(os.environ["PROBES"]):
    state = "ok  " if row["ok"] else "FAIL"
    print(f"  {state} {row['backend']:<10} {(row['detail'] or '')[:150]}")
PY
    status=$?
fi
exit "$status"
