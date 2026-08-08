#!/usr/bin/env bash
# Quick start: run the KGateway server locally on :8080 (stays running; Ctrl-C to stop).
# Creates config.json from your env on first run if it doesn't exist, then asks whether
# to start the dashboard (Next.js, :3000) alongside the gateway.
#
# Usage:
#   ZAI_API_KEY=<your-key> ./scripts/start.sh
#
# Optional env:
#   OPENAI_API_KEY, ANTHROPIC_API_KEY — added to config.json when present.
#   KGATEWAY_START_UI=1|0            — answer the dashboard prompt non-interactively
#                                      (no TTY and unset ⇒ dashboard is not started).
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

# Dashboard? The server banner only prints a dashboard URL when one is actually running
# (via KGATEWAY_DASHBOARD_URL), so answer honestly.
start_ui=no
if [[ -n "${KGATEWAY_START_UI:-}" ]]; then
  case "$KGATEWAY_START_UI" in
    1|true|yes|y) start_ui=yes ;;
  esac
elif [[ -t 0 ]]; then
  read -r -p "→ start the dashboard too? [Y/n] " answer
  case "${answer:-Y}" in
    [Nn]*) start_ui=no ;;
    *) start_ui=yes ;;
  esac
fi

ui_pid=""
if [[ "$start_ui" == yes ]]; then
  if ! command -v pnpm >/dev/null 2>&1; then
    echo "→ pnpm not found — skipping the dashboard (install pnpm, or run: cd ui && npm run dev)"
    start_ui=no
  else
    if [[ ! -d ui/node_modules ]]; then
      echo "→ installing dashboard dependencies (first run)…"
      pnpm --dir ui install || { echo "→ pnpm install failed — skipping the dashboard"; start_ui=no; }
    fi
  fi
fi
if [[ "$start_ui" == yes ]]; then
  echo "→ starting dashboard on http://localhost:3000  (logs: /tmp/kgateway-ui.log)"
  # Job control on: the background job gets its own process group, so the trap can kill
  # the whole tree (pnpm wrapper AND the next-dev server it spawns), not just the wrapper.
  set -m
  NEXT_PUBLIC_KGATEWAY_URL=http://localhost:8080 pnpm --dir ui dev > /tmp/kgateway-ui.log 2>&1 &
  ui_pid=$!
  set +m
  # The dashboard lives and dies with the gateway.
  trap '[[ -n "$ui_pid" ]] && kill -- "-$ui_pid" 2>/dev/null' EXIT INT TERM
  export KGATEWAY_DASHBOARD_URL=http://localhost:3000
fi

echo "→ starting KGateway on http://localhost:8080  (Ctrl-C to stop)"
echo "   try:  curl -s localhost:8080/v1/chat/completions -H 'content-type: application/json' \\"
echo "           -d '{\"model\":\"zai/glm-4.6\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}'"
if [[ "$start_ui" == no ]]; then
  echo "   dashboard (not started):  cd ui && NEXT_PUBLIC_KGATEWAY_URL=http://localhost:8080 pnpm dev"
fi
echo

if [[ -n "$ui_pid" ]]; then
  # Foreground (no exec) so the EXIT trap can reap the dashboard when the gateway stops.
  ./target/debug/kgateway-server --config config.json
else
  exec ./target/debug/kgateway-server --config config.json
fi
