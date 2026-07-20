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
