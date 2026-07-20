//! Upstream model listing — best-effort helpers for the gateway's aggregated
//! `GET /v1/models` endpoint.
//!
//! Two wire formats cover every listable provider:
//! - **OpenAI-compatible**: `GET {base}/models`, `Authorization: Bearer` auth
//!   (OpenAI itself + every `openai_compat` vendor, incl. z.ai's GLM endpoints).
//! - **Anthropic Messages**: `GET {base}/v1/models`, `x-api-key` auth
//!   (api.anthropic.com and Anthropic-compatible endpoints like
//!   `https://api.z.ai/api/anthropic`).
//!
//! Failures are the caller's to tolerate: the aggregate endpoint skips a
//! provider that errors rather than failing the whole listing.

use std::time::Duration;

use kgateway_core::error::{KgError, KgErrorKind};

use crate::http::default_client;

/// Listing calls are interactive (a client populating a model picker) — keep the
/// bound far below the 120s chat timeout so one dead vendor can't stall the page.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// One model id as reported by an upstream provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedModel {
    pub id: String,
    /// Unix seconds, when the upstream reports it (OpenAI wire only).
    pub created: Option<i64>,
}

/// `GET {base}/models` — OpenAI-compatible model list.
pub async fn list_openai_models(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ListedModel>, KgError> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = default_client()
        .get(&url)
        .bearer_auth(api_key)
        .timeout(LIST_TIMEOUT)
        .send()
        .await
        .map_err(|e| KgError::new(KgErrorKind::Network, format!("model list error: {e}")))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;
    if !status.is_success() {
        return Err(KgError::provider("model list failed", status.as_u16()));
    }
    Ok(parse_openai_models(&body))
}

/// `GET {base}/v1/models` — Anthropic Messages model list.
pub async fn list_anthropic_models(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ListedModel>, KgError> {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let resp = default_client()
        .get(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .timeout(LIST_TIMEOUT)
        .send()
        .await
        .map_err(|e| KgError::new(KgErrorKind::Network, format!("model list error: {e}")))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| KgError::new(KgErrorKind::Internal, format!("decode error: {e}")))?;
    if !status.is_success() {
        return Err(KgError::provider("model list failed", status.as_u16()));
    }
    Ok(parse_anthropic_models(&body))
}

/// Parse an OpenAI-wire list body: `{"object":"list","data":[{"id":..,"created":..}]}`.
fn parse_openai_models(body: &serde_json::Value) -> Vec<ListedModel> {
    body["data"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|m| {
                    Some(ListedModel {
                        id: m["id"].as_str()?.to_string(),
                        created: m["created"].as_i64(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an Anthropic-wire list body: `{"data":[{"id":..,"created_at":"RFC3339"}]}`.
/// `created_at` is a timestamp string, not unix seconds — reported as `None`.
fn parse_anthropic_models(body: &serde_json::Value) -> Vec<ListedModel> {
    body["data"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|m| {
                    Some(ListedModel {
                        id: m["id"].as_str()?.to_string(),
                        created: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_list_body() {
        let body = serde_json::json!({
            "object": "list",
            "data": [
                { "id": "glm-5.2", "object": "model", "created": 1781625600, "owned_by": "z-ai" },
                { "id": "glm-4.7", "object": "model", "created": 1766332800, "owned_by": "z-ai" },
                { "object": "model" } // malformed entry without id is skipped
            ]
        });
        let models = parse_openai_models(&body);
        assert_eq!(
            models,
            vec![
                ListedModel {
                    id: "glm-5.2".into(),
                    created: Some(1781625600)
                },
                ListedModel {
                    id: "glm-4.7".into(),
                    created: Some(1766332800)
                },
            ]
        );
    }

    #[test]
    fn parses_anthropic_list_body() {
        let body = serde_json::json!({
            "data": [
                { "type": "model", "id": "glm-5.2", "display_name": "GLM-5.2",
                  "created_at": "2026-06-17T00:00:00Z" },
                { "type": "model", "id": "glm-5", "display_name": "GLM-5",
                  "created_at": "2026-02-11T00:00:00Z" }
            ],
            "hasMore": false
        });
        let models = parse_anthropic_models(&body);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "glm-5.2");
        assert_eq!(models[0].created, None);
    }

    #[test]
    fn empty_or_malformed_bodies_yield_empty_lists() {
        assert!(parse_openai_models(&serde_json::json!({})).is_empty());
        assert!(parse_anthropic_models(&serde_json::json!({"data": "nope"})).is_empty());
    }
}
