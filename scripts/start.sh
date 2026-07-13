#!/usr/bin/env bash
# Quick start: run the KGateway server locally on :8080 (stays running; Ctrl-C to stop).
# Creates config.json from your env on first run if it doesn't exist.
#
# Usage:
#   ZAI_API_KEY=<your-key> ./scripts/start.sh
#
# Optional env: OPENAI_API_KEY, ANTHROPIC_API_KEY — added to config.json when present.
# The keys stay in the env; config.json only stores ${ENV} references.

set -uo pipefail
cd "$(dirname "$0")/.."

# Generate config.json on first run.
if [[ ! -f config.json ]]; then
  echo "→ no config.json — generating one from your env…"
  {
    echo '{'
    echo '  "port": 8080,'
    echo '  "database": "sqlite://./kgateway.db?mode=rwc",'
    echo '  "providers": {'
    sep=""
    if [[ -n "${ZAI_API_KEY:-}" ]]; then
      printf '    %s"zai": { "kind": "anthropic", "base_url": "https://api.z.ai/api/anthropic", "keys": [{ "id": "coding-plan", "value": "${ZAI_API_KEY}", "weight": 1 }] }' "$sep"; sep=$',\n'
    fi
    if [[ -n "${OPENAI_API_KEY:-}" ]]; then
      printf '%s    "openai": { "keys": [{ "id": "default", "value": "${OPENAI_API_KEY}", "weight": 1 }] }' "$sep"; sep=$',\n'
    fi
    if [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
      printf '%s    "anthropic": { "keys": [{ "id": "default", "value": "${ANTHROPIC_API_KEY}", "weight": 1 }] }' "$sep"; sep=$',\n'
    fi
    echo ""
    echo '  }'
    echo '}'
  } > config.json
  echo "→ wrote config.json (edit it anytime; SIGHUP or the dashboard reloads it live)"
fi

echo "→ building…"
cargo build -q -p kgateway-server || { echo "build failed"; exit 1; }

echo "→ starting KGateway on http://localhost:8080  (Ctrl-C to stop)"
echo "   try:  curl -s localhost:8080/v1/chat/completions -H 'content-type: application/json' \\"
echo "           -d '{\"model\":\"zai/glm-4.6\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}'"
echo "   dashboard:  cd ui && NEXT_PUBLIC_KGATEWAY_URL=http://localhost:8080 pnpm dev"
echo
exec ./target/debug/kgateway-server --config config.json
