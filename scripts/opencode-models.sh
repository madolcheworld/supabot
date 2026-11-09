#!/usr/bin/env bash
# Fast & resilient model lister for opencode in SwarmBot.
# Attempts native `opencode models` first with a tight timeout;
# falls back immediately to reading ~/.config/opencode/opencode.json
# so that the UI model list never hangs or returns HTTP 502.

if output=$(timeout 2 opencode models 2>/dev/null) && [ -n "$output" ]; then
    echo "$output"
    exit 0
fi

python3 -c "
import json, os, sys
p = os.path.expanduser('~/.config/opencode/opencode.json')
if not os.path.exists(p):
    sys.exit(0)
try:
    with open(p, 'r') as f:
        data = json.load(f)
    for p_id, p_val in data.get('provider', {}).items():
        if isinstance(p_val, dict):
            for m_id in p_val.get('models', {}).keys():
                print(f'{p_id}/{m_id}')
except Exception:
    pass
"
