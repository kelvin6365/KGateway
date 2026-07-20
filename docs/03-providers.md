# 03 — Providers (connectors)

> **Implemented (20 connectors):** OpenAI, Anthropic, Cohere (native); Groq, OpenRouter, xAI, DeepSeek, Cerebras, Perplexity, Together, Ollama, Mistral, Nebius, HuggingFace, z.ai GLM (`zai` pay-as-you-go + `zai-coding` Coding Plan), Moonshot (Kimi), MiniMax, vLLM, SGLang (OpenAI-compatible via `openai_compat`); **Bedrock** (Converse + SigV4), **Google Gemini** (native), **Azure OpenAI** (deployment routing). Anthropic-compatible custom providers (e.g. **z.ai GLM Coding Plan**, Moonshot `/anthropic`, MiniMax `/anthropic`) via `kind: "anthropic"`. Register any of the wire-format-specific ones under a custom name with `kind: "openai" | "anthropic" | "bedrock" | "gemini" | "azure"`. See [Verification status](#verification-status) for what has been exercised against live upstreams.

## Verification status

What each provider has actually been exercised against, beyond the unit/e2e test suite
(which mocks upstreams with wiremock and always runs in CI). Last live verification:
**2026-07-20**.

| Provider (route prefix) | Wire(s) | Status |
|---|---|---|
| **z.ai GLM Coding Plan** — `zai` (`kind: "anthropic"`), `zai-coding` | Anthropic + OpenAI-compat | ✅ **Fully tested live** — unary + streaming + tool use through the gateway; **Claude Code, OMP CLI, and Pi CLI** end-to-end; `/v1/models` aggregation against both official list APIs |
| **Moonshot (Kimi)** — `moonshot` | OpenAI-compat (+ Anthropic at `/anthropic`) | 🟡 **Prepared — pending a real key.** Keyless verification done: official docs cross-checked (base `https://api.moonshot.ai/v1`, Bearer auth, kimi-k3 / kimi-k2.x / moonshot-v1-\* models); live 401 probes confirm both wires and `GET /v1/models` exist; gateway routing, scrubbed error mapping, and `/v1/models` graceful skip verified |
| **MiniMax** — `minimax` | OpenAI-compat (+ Anthropic at `/anthropic`) | 🟡 **Prepared — pending a real key.** Same keyless verification; official docs confirm both wires (`https://api.minimax.io/v1`, `/anthropic`) and models MiniMax-M2 → MiniMax-M3 |
| **OpenAI** — `openai` | OpenAI native | 🧪 **Unit-tested (mocked)** — full connector suite (chat, stream, embeddings, images, audio, errors); pending live key |
| **Anthropic (Claude)** — `anthropic` | Anthropic native | 🧪 **Unit-tested (mocked)** — incl. tools + streaming; the same connector code path is what `zai` runs on, which **is** live-verified; pending live key |
| Cohere, Bedrock, Gemini, Azure | native | 🧪 Unit-tested (mocked); pending live key |
| Groq, OpenRouter, xAI, DeepSeek, Cerebras, Perplexity, Together, Fireworks, Parasail, Mistral, Nebius, HuggingFace, Ollama, vLLM, SGLang | OpenAI-compat | 🧪 Shared OpenAI connector (live-verified via `zai-coding`); per-vendor base URLs unit-tested; pending live key |

To promote a 🟡/🧪 provider to ✅: export its `${ENV}` key, send one unary + one streamed
chat through the gateway, and hit `/v1/models` — then update this table.

## The core lesson

A single **100+ method** provider contract that every provider must satisfy — where unsupported operations return a runtime "not supported" error — is a design we explicitly reject.

**KGateway strategy:** a slim required `Provider` trait (chat + chat_stream) plus **opt-in capability traits**. A provider implements only what it supports; the engine consults a capability registry and returns `Unsupported` *before* dispatch.

```rust
// Required of every provider
trait Provider { key; chat; chat_stream }

// Opt-in, implemented only where supported
trait Embeddings / Images / Audio / Rerank / Responses / Batch / Files / Video / OCR / CachedContent / Containers
```

## The OpenAI-compatible shortcut

The OpenAI provider is our reference implementation, and **9+ providers delegate to it** (Groq, Cerebras, Ollama, Perplexity, OpenRouter, …):

```rust
pub struct OpenAiCompatible {
    key: ProviderKey,
    base_url: String,
    quirks: Quirks,   // header names, path overrides, unsupported params to strip
}
impl Provider for OpenAiCompatible { /* reuse OpenAI encode/decode */ }
```

This makes ~9 connectors nearly free once OpenAI works.

## Full connector matrix (target: 23 connectors)

| Provider | Wire family | Auth | Notes / port strategy | Milestone |
|---|---|---|---|---|
| **OpenAI** | OpenAI (native) | Bearer | Reference impl. chat, responses, embeddings, images, audio, batch, files | M1 |
| **Anthropic** | Anthropic native | `x-api-key` | Distinct message/tool schema; own converter | M2 |
| **Groq** | OpenAI-compat | Bearer | via `OpenAiCompatible` | M2 |
| **Ollama** | OpenAI-compat | none/local | local base_url | M2 |
| **OpenRouter** | OpenAI-compat | Bearer | extra routing headers | M2 |
| **xAI (Grok)** | OpenAI-compat | Bearer | | M2 |
| **DeepSeek** | OpenAI-compat | Bearer | | M2 |
| **Cerebras** | OpenAI-compat | Bearer | | M2 |
| **Perplexity** | OpenAI-compat | Bearer | | M2 |
| **Together** | OpenAI-compat | Bearer | | M2 |
| **Mistral** | OpenAI-compat (mostly) | Bearer | some param diffs | M3/M7 |
| **Cohere** | Cohere native | Bearer | embeddings + rerank first-class | M3 (embed), M7 |
| **AWS Bedrock** | Bedrock Converse | **SigV4** | needs `aws-sigv4` + `aws-config`; per-model families | M7 |
| **Google Vertex / Gemini** | Gemini native | OAuth2 / API key | own converter; safety settings | M7 |
| **Azure OpenAI** | OpenAI + deployment routing | api-key / AAD | deployment-name path scheme | M7 |
| **HuggingFace** | OpenAI-compat / TGI | Bearer | | M7 |
| **Nebius** | OpenAI-compat | Bearer | | M7 |
| **Replicate** | Replicate native | Bearer | async prediction polling | M7 |
| **vLLM** | OpenAI-compat | none/Bearer | self-hosted | M7 |
| **SGLang** | OpenAI-compat | none/Bearer | self-hosted | M7 |
| **Parasail** | OpenAI-compat | Bearer | | M7 |
| **z.ai GLM** | OpenAI-compat | Bearer | `zai` = pay-as-you-go (`/api/paas/v4`); `zai-coding` = Coding Plan (`/api/coding/paas/v4`); the Coding Plan is also Anthropic-compatible via `kind: "anthropic"` + `https://api.z.ai/api/anthropic` | M22 |
| **Moonshot (Kimi)** | OpenAI-compat | Bearer | `https://api.moonshot.ai/v1` (intl; override base_url with `api.moonshot.cn` for China); also Anthropic-compatible via `kind: "anthropic"` + `https://api.moonshot.ai/anthropic` | M23 |
| **MiniMax** | OpenAI-compat | Bearer | `https://api.minimax.io/v1`; also Anthropic-compatible via `kind: "anthropic"` + `https://api.minimax.io/anthropic` | M23 |
| **ElevenLabs** | ElevenLabs native | `xi-api-key` | audio (speech) only | M7 |
| **Bedrock Mantle** | Bedrock variant | SigV4 | Bedrock-family variant | M7 |

> When implementing each connector, work from the vendor's API reference for the exact wire quirks (header casing, param stripping, streaming delta shapes, error mapping). Enumerate the edge cases as tests.

## Per-connector definition of done

1. `chat` + `chat_stream` round-trip against the internal schema.
2. Error mapping → `KgError` with correct `retryable` flag (drives failover).
3. Streaming delta accumulation matches non-streaming output.
4. Unit tests: request encode, response decode, one streaming case, one error case.
5. Registered in the capability registry.

## Capability registry

```rust
pub struct ProviderEntry {
    provider: Arc<dyn Provider>,
    caps: Capabilities,  // bitflags: CHAT | EMBEDDINGS | IMAGES | AUDIO | RERANK | BATCH | ...
}
```
The router checks `caps` before dispatch and returns a clean `Unsupported` (never a mid-flight provider error), rather than surfacing an unsupported operation as a runtime error mid-request.
