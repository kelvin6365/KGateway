//! Static per-model pricing table for cost estimation (M10 logs).
//!
//! Rather than pulling live pricing from a pricing service, KGateway ships a small static
//! table of public list prices (USD per 1M tokens) as a pragmatic MVP. Prices drift, so
//! this is a best-effort estimate — `estimate_cost` returns `None` for unknown models
//! (the UI then shows "—" rather than a wrong number). Matching is substring-based on the
//! bare model id, most-specific patterns first.

/// (pattern, input $/1M tokens, output $/1M tokens). Order matters: the first pattern the
/// model id contains wins, so more specific families precede their generic prefixes.
const PRICES: &[(&str, f64, f64)] = &[
    // OpenAI
    ("gpt-4o-mini", 0.15, 0.60),
    ("gpt-4o", 2.50, 10.00),
    ("gpt-4-turbo", 10.00, 30.00),
    ("gpt-4.1-mini", 0.40, 1.60),
    ("gpt-4.1", 2.00, 8.00),
    ("gpt-4", 30.00, 60.00),
    ("gpt-3.5-turbo", 0.50, 1.50),
    ("o1-mini", 1.10, 4.40),
    ("o3-mini", 1.10, 4.40),
    ("o1", 15.00, 60.00),
    // Anthropic
    ("claude-3-5-haiku", 0.80, 4.00),
    ("claude-3-haiku", 0.25, 1.25),
    ("claude-3-5-sonnet", 3.00, 15.00),
    ("claude-3-7-sonnet", 3.00, 15.00),
    ("claude-sonnet-4", 3.00, 15.00),
    ("claude-3-opus", 15.00, 75.00),
    ("claude-opus-4", 15.00, 75.00),
    // Google Gemini
    ("gemini-1.5-flash", 0.075, 0.30),
    ("gemini-2.0-flash", 0.10, 0.40),
    ("gemini-1.5-pro", 1.25, 5.00),
    ("gemini-2.5-pro", 1.25, 10.00),
    // Cohere
    ("command-r-plus", 2.50, 10.00),
    ("command-r", 0.15, 0.60),
    // z.ai GLM (approximate public list; coding-plan keys are subscription-metered).
    // `contains` matching means "glm-5" also prices "glm-5.1", "glm-5.2[1m]" (the official
    // 1M-context id), "glm-5-air", etc. ORDER MATTERS — `find` returns the FIRST match, so any
    // more-specific entry (e.g. a distinct "glm-5.2" rate) MUST go ABOVE "glm-5" or it's shadowed.
    // Adjust rates for your plan — these are best-effort estimates, not billing truth.
    ("glm-5", 0.60, 2.20),
    ("glm-4.6", 0.60, 2.20),
    ("glm-4.5", 0.60, 2.20),
    // DeepSeek / Mistral
    ("deepseek-chat", 0.27, 1.10),
    ("mistral-large", 2.00, 6.00),
];

/// Estimate the USD cost of a call from a static price table. Returns `None` when the
/// model isn't in the table (caller records cost as unknown).
pub fn estimate_cost(model: &str, prompt_tokens: u32, completion_tokens: u32) -> Option<f64> {
    let m = model.to_lowercase();
    // Strip an optional `provider/` prefix so "openai/gpt-4o" matches "gpt-4o".
    let bare = m.rsplit('/').next().unwrap_or(&m);
    let (_, in_price, out_price) = PRICES.iter().find(|(pat, _, _)| bare.contains(pat))?;
    let cost = (prompt_tokens as f64 / 1_000_000.0) * in_price
        + (completion_tokens as f64 / 1_000_000.0) * out_price;
    Some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_model_prices_out() {
        // 1M prompt + 1M completion of gpt-4o = 2.50 + 10.00.
        let c = estimate_cost("gpt-4o", 1_000_000, 1_000_000).unwrap();
        assert!((c - 12.50).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn strips_provider_prefix() {
        assert!(estimate_cost("openai/gpt-4o-mini", 1_000_000, 0).is_some());
    }

    #[test]
    fn specific_beats_generic() {
        // gpt-4o-mini must not be captured by the gpt-4 / gpt-4o generic rows.
        let mini = estimate_cost("gpt-4o-mini", 1_000_000, 0).unwrap();
        assert!((mini - 0.15).abs() < 1e-9, "got {mini}");
    }

    #[test]
    fn unknown_model_is_none() {
        assert!(estimate_cost("some-random-model", 100, 100).is_none());
    }

    #[test]
    fn glm_5_variants_are_priced() {
        // `contains` matching prices the whole glm-5 family, incl. a `provider/` prefix and
        // the official bracketed 1M-context id "glm-5.2[1m]" (the `[1m]` suffix must not break it).
        assert!(estimate_cost("zai/glm-5.1", 1_000, 1_000).is_some());
        assert!(estimate_cost("glm-5", 1_000, 0).is_some());
        assert!(estimate_cost("zai/glm-5.2[1m]", 1_000, 1_000).is_some());
        assert!(estimate_cost("GLM-5.2[1M]", 1_000, 0).is_some()); // case-insensitive
    }
}
