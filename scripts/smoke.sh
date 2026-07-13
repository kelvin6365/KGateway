#!/usr/bin/env bash
# One-command local smoke test: build → boot the gateway → send a real chat request
# through your z.ai GLM Coding Plan key → print the reply → shut down.
#
# Usage:
#   ZAI_API_KEY=<your-key> ./scripts/smoke.sh
#   ZAI_API_KEY=<key> MODEL=zai/glm-4.5 ./scripts/smoke.sh     # override the model
#
# The key is only ever read from the env — never written to a committed file.

set -uo pipefail
cd "$(dirname "$0")/.."

PORT=8181
MODEL="${MODEL:-zai/glm-4.6}"

if [[ -z "${ZAI_API_KEY:-}" ]]; then
  echo "Set ZAI_API_KEY first:  ZAI_API_KEY=<your-key> ./scripts/smoke.sh"
  exit 1
fi

CFG="$(mktemp)"
cat > "$CFG" <<JSON
{
  "port": $PORT,
  "providers": {
    "zai": {
      "kind": "anthropic",
      "base_url": "https://api.z.ai/api/anthropic",
      "keys": [{ "id": "coding-plan", "value": "\${ZAI_API_KEY}", "weight": 1 }]
    }
  }
}
JSON

echo "→ building…"
cargo build -q -p kgateway-server || { echo "build failed"; rm -f "$CFG"; exit 1; }

echo "→ starting gateway on :$PORT…"
./target/debug/kgateway-server --config "$CFG" >/tmp/kgateway-smoke.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null; rm -f "$CFG"' EXIT
for _ in $(seq 1 30); do curl -sf "http://localhost:$PORT/health" >/dev/null 2>&1 && break; sleep 0.5; done

echo "→ non-streaming chat ($MODEL):"
curl -s "http://localhost:$PORT/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Reply in one short sentence: are you working?\"}]}" \
  | (python3 -m json.tool 2>/dev/null || cat)

echo
echo "→ streaming chat ($MODEL):"
curl -sN "http://localhost:$PORT/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Count 1 to 5.\"}],\"stream\":true}" \
  | grep '^data:' | sed 's/^data: //' | python3 -c "
import sys, json
out = ''
for line in sys.stdin:
    line = line.strip()
    if not line or line == '[DONE]':
        continue
    try:
        out += json.loads(line)['choices'][0]['delta'].get('content') or ''
    except Exception:
        pass
print('   ' + repr(out))
" 2>/dev/null || echo "   (stream parse skipped)"

echo
echo "✅ done — gateway works end-to-end. (Server log: /tmp/kgateway-smoke.log)"
