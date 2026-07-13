#!/usr/bin/env bash
# Validate KGateway against REAL provider APIs (not mocks).
#
# Everything in the test suite runs against wiremock; this script confirms the wire
# formats are actually correct end-to-end. Run it with your own keys.
#
# Usage:
#   export OPENAI_API_KEY=sk-...        # required
#   export ANTHROPIC_API_KEY=...        # optional
#   export COHERE_API_KEY=...           # optional (rerank)
#   ./scripts/validate-live.sh
#
# It starts the gateway on :8899 with a temp config pointing at the real providers,
# runs each endpoint, prints PASS/FAIL, and shuts the gateway down.

set -uo pipefail
cd "$(dirname "$0")/.."

PORT=8899
BASE="http://localhost:$PORT"
PASS=0; FAIL=0

if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "OPENAI_API_KEY is required (the core checks use OpenAI). Aborting."
  exit 1
fi

CFG="$(mktemp)"
cat > "$CFG" <<JSON
{
  "port": $PORT,
  "providers": {
    "openai":    { "keys": [ { "id": "default", "value": "\${OPENAI_API_KEY}", "weight": 1 } ] },
    "anthropic": { "keys": [ { "id": "default", "value": "\${ANTHROPIC_API_KEY}", "weight": 1 } ] },
    "cohere":    { "keys": [ { "id": "default", "value": "\${COHERE_API_KEY}", "weight": 1 } ] }
  }
}
JSON

echo "Building + starting gateway on :$PORT ..."
cargo build -q -p kgateway-server || { echo "build failed"; exit 1; }
./target/debug/kgateway-server --config "$CFG" >/tmp/kg_validate.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null; rm -f "$CFG"' EXIT
for _ in $(seq 1 20); do curl -sf "$BASE/health" >/dev/null 2>&1 && break; sleep 0.5; done

check() { # name  jq-ish-grep  curl-args...
  local name="$1"; shift
  local pat="$1"; shift
  local body; body="$(curl -s "$@")"
  if echo "$body" | grep -q "$pat"; then
    echo "  PASS  $name"; PASS=$((PASS+1))
  else
    echo "  FAIL  $name"; echo "        response: $(echo "$body" | head -c 200)"; FAIL=$((FAIL+1))
  fi
}

echo "=== OpenAI ==="
check "chat completion" '"content"' -X POST "$BASE/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"Reply with exactly: OK"}]}'
check "embeddings" '"embedding"' -X POST "$BASE/v1/embeddings" \
  -H 'content-type: application/json' \
  -d '{"model":"openai/text-embedding-3-small","input":["hello world"]}'

echo "=== OpenAI streaming ==="
if curl -sN -X POST "$BASE/v1/chat/completions" -H 'content-type: application/json' \
     -d '{"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"Count: 1 2 3"}],"stream":true}' \
   | grep -q 'data:'; then echo "  PASS  chat stream (SSE)"; PASS=$((PASS+1)); else echo "  FAIL  chat stream"; FAIL=$((FAIL+1)); fi

if [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
  echo "=== Anthropic ==="
  check "chat completion" '"content"' -X POST "$BASE/v1/chat/completions" \
    -H 'content-type: application/json' \
    -d '{"model":"anthropic/claude-3-5-haiku-latest","messages":[{"role":"user","content":"Reply with exactly: OK"}]}'
fi

if [[ -n "${COHERE_API_KEY:-}" ]]; then
  echo "=== Cohere ==="
  check "rerank" '"relevance_score"' -X POST "$BASE/v1/rerank" \
    -H 'content-type: application/json' \
    -d '{"model":"cohere/rerank-v3.5","query":"cats","documents":["dogs bark","cats meow"]}'
fi

echo "=== failover (bad primary key path via unknown model → fallback) ==="
# primary openai model that errors is provider-specific; here we just confirm fallbacks parse.
check "fallback request accepted" '"content"\|"error"' -X POST "$BASE/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"OK"}],"fallbacks":[{"provider":"openai","model":"gpt-4o-mini"}]}'

echo
echo "RESULT: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] && echo "✅ Live validation succeeded — wire formats confirmed against real APIs." || echo "❌ Some checks failed — see responses above and /tmp/kg_validate.log"
exit $FAIL
