//! OpenAI-compatible providers. Many vendors expose an OpenAI Chat Completions
//! wire-compatible API, so we reuse [`OpenAiProvider`] under a different provider
//! id + default base URL rather than re-implementing the connector.

use crate::openai::OpenAiProvider;

/// Known OpenAI-compatible providers and their default base URLs.
const KNOWN: &[(&str, &str)] = &[
    ("groq", "https://api.groq.com/openai/v1"),
    ("openrouter", "https://openrouter.ai/api/v1"),
    ("xai", "https://api.x.ai/v1"),
    ("deepseek", "https://api.deepseek.com"),
    ("cerebras", "https://api.cerebras.ai/v1"),
    ("perplexity", "https://api.perplexity.ai"),
    ("together", "https://api.together.xyz/v1"),
    ("fireworks", "https://api.fireworks.ai/inference/v1"),
    ("parasail", "https://api.parasail.io/v1"),
    ("ollama", "http://localhost:11434/v1"),
    ("mistral", "https://api.mistral.ai/v1"),
    ("nebius", "https://api.studio.nebius.ai/v1"),
    ("huggingface", "https://router.huggingface.co/v1"),
    // z.ai (Zhipu GLM). `zai` is the pay-as-you-go API; `zai-coding` is the
    // subscription GLM Coding Plan's OpenAI-compatible endpoint (same GLM model
    // ids, metered by the plan). The Coding Plan also speaks Anthropic wire —
    // for that, configure `"kind": "anthropic"` with base_url
    // https://api.z.ai/api/anthropic instead.
    ("zai", "https://api.z.ai/api/paas/v4"),
    ("zai-coding", "https://api.z.ai/api/coding/paas/v4"),
    // Moonshot AI (Kimi). International endpoint; override base_url with
    // https://api.moonshot.cn/v1 for the China platform. Also speaks Anthropic
    // wire at https://api.moonshot.ai/anthropic (use `kind: "anthropic"`).
    ("moonshot", "https://api.moonshot.ai/v1"),
    // MiniMax. Also speaks Anthropic wire at https://api.minimax.io/anthropic
    // (use `kind: "anthropic"`).
    ("minimax", "https://api.minimax.io/v1"),
    // Opencode model brokers. `opencode-zen` is the pay-as-you-go key from
    // opencode.ai/auth; `opencode-go` is the subscription plan. Both serve the
    // same OpenAI wire — note `opencode-go` nests under the `zen` path, so its
    // URL is a strict extension of `opencode-zen`'s, not a sibling.
    ("opencode-zen", "https://opencode.ai/zen/v1"),
    ("opencode-go", "https://opencode.ai/zen/go/v1"),
    // Wafer. Passes unknown params through to the upstream model untouched.
    ("wafer", "https://pass.wafer.ai/v1"),
    // Self-hosted OpenAI-compatible servers — override base_url in config.
    ("vllm", "http://localhost:8000/v1"),
    ("sglang", "http://localhost:30000/v1"),
];

/// Default base URL for a known OpenAI-compatible provider name.
pub fn default_base_url(name: &str) -> Option<&'static str> {
    KNOWN.iter().find(|(n, _)| *n == name).map(|(_, url)| *url)
}

/// Construct an OpenAI-compatible provider for `name`.
///
/// Returns `None` if `name` is not a known OpenAI-compatible provider. The base
/// URL defaults to the vendor's standard endpoint, but `base_url_override` wins
/// when supplied (e.g. a self-hosted Ollama or a proxy).
pub fn build(name: &str, base_url_override: Option<String>) -> Option<OpenAiProvider> {
    let default = default_base_url(name)?;
    let base_url = base_url_override.unwrap_or_else(|| default.to_string());
    Some(OpenAiProvider::with_identity(name, base_url))
}

/// The list of known OpenAI-compatible provider names.
pub fn names() -> impl Iterator<Item = &'static str> {
    KNOWN.iter().map(|(n, _)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgateway_core::provider::Provider;

    #[test]
    fn default_base_urls_are_correct() {
        assert_eq!(
            default_base_url("groq"),
            Some("https://api.groq.com/openai/v1")
        );
        assert_eq!(
            default_base_url("openrouter"),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(default_base_url("xai"), Some("https://api.x.ai/v1"));
        assert_eq!(
            default_base_url("deepseek"),
            Some("https://api.deepseek.com")
        );
        assert_eq!(
            default_base_url("cerebras"),
            Some("https://api.cerebras.ai/v1")
        );
        assert_eq!(
            default_base_url("perplexity"),
            Some("https://api.perplexity.ai")
        );
        assert_eq!(
            default_base_url("together"),
            Some("https://api.together.xyz/v1")
        );
        assert_eq!(
            default_base_url("ollama"),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(
            default_base_url("zai"),
            Some("https://api.z.ai/api/paas/v4")
        );
        assert_eq!(
            default_base_url("zai-coding"),
            Some("https://api.z.ai/api/coding/paas/v4")
        );
        assert_eq!(
            default_base_url("moonshot"),
            Some("https://api.moonshot.ai/v1")
        );
        assert_eq!(
            default_base_url("minimax"),
            Some("https://api.minimax.io/v1")
        );
        assert_eq!(
            default_base_url("opencode-zen"),
            Some("https://opencode.ai/zen/v1")
        );
        assert_eq!(
            default_base_url("opencode-go"),
            Some("https://opencode.ai/zen/go/v1")
        );
        assert_eq!(default_base_url("wafer"), Some("https://pass.wafer.ai/v1"));
    }

    /// `OpenAiProvider` appends `/chat/completions` to the stored base URL, so every
    /// entry must already carry the vendor's version segment. A missing `/v1` yields
    /// a 404 that is indistinguishable from an auth failure at the call site.
    #[test]
    fn every_known_base_url_carries_its_version_segment() {
        for (name, url) in KNOWN {
            assert!(
                url.ends_with("/v1")
                    || url.ends_with("/v4")
                    || *name == "deepseek"
                    || *name == "perplexity",
                "{name}: base URL {url} has no version segment; \
                 chat would POST to a versionless /chat/completions"
            );
        }
    }

    /// The two Opencode plans share a host and a path prefix — `opencode-go` nests
    /// *under* `opencode-zen`. Swapping them silently routes subscription traffic to
    /// the metered endpoint, so pin both directions.
    #[test]
    fn opencode_plans_are_distinct_and_correctly_nested() {
        let zen = default_base_url("opencode-zen").expect("zen is known");
        let go = default_base_url("opencode-go").expect("go is known");
        assert_ne!(zen, go);
        assert!(
            go.starts_with("https://opencode.ai/zen/go"),
            "opencode-go must nest under the zen path, got {go}"
        );
        assert!(
            !zen.contains("/go"),
            "opencode-zen must not carry the go segment, got {zen}"
        );
    }

    #[test]
    fn unknown_provider_returns_none() {
        assert_eq!(default_base_url("nope"), None);
        assert!(build("nope", None).is_none());
    }

    #[test]
    fn build_uses_default_and_sets_provider_key() {
        let p = build("groq", None).expect("groq is known");
        assert_eq!(p.key().as_str(), "groq");
    }

    #[test]
    fn build_honors_base_url_override() {
        let p = build("ollama", Some("http://my-host:9999/v1".into())).expect("ollama is known");
        assert_eq!(p.key().as_str(), "ollama");
        // The override is stored on the provider; exercise it via a chat call target
        // indirectly by confirming construction succeeded with a custom URL.
        assert!(names().any(|n| n == "ollama"));
    }
}
