//! Structured error type. The `retryable` flag drives router failover.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum KgErrorKind {
    /// Upstream provider returned an error status.
    Provider,
    /// Auth / credential failure.
    Auth,
    /// Rate limit / quota (usually retryable on another key/provider).
    RateLimit,
    /// Request was malformed / invalid.
    BadRequest,
    /// The requested operation/capability is not supported by the provider.
    Unsupported,
    /// Network / timeout / connection error.
    Network,
    /// Internal gateway error.
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct KgError {
    pub kind: KgErrorKind,
    pub message: String,
    /// HTTP status from the upstream provider, if any.
    pub status: Option<u16>,
    /// Provider identifier that produced the error, if any.
    pub provider: Option<String>,
    /// Whether the router may retry on another key/provider/fallback.
    pub retryable: bool,
}

impl KgError {
    pub fn new(kind: KgErrorKind, message: impl Into<String>) -> Self {
        let retryable = matches!(kind, KgErrorKind::RateLimit | KgErrorKind::Network);
        Self {
            kind,
            message: message.into(),
            status: None,
            provider: None,
            retryable,
        }
    }

    pub fn provider(msg: impl Into<String>, status: u16) -> Self {
        Self {
            kind: KgErrorKind::Provider,
            message: msg.into(),
            status: Some(status),
            provider: None,
            // 5xx and 429 are generally retryable; 4xx (except 429) are not.
            retryable: status == 429 || status >= 500,
        }
    }

    pub fn unsupported(op: impl Into<String>) -> Self {
        Self::new(
            KgErrorKind::Unsupported,
            format!("operation not supported: {}", op.into()),
        )
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(KgErrorKind::Internal, msg)
    }

    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    /// Whether the router should try a DIFFERENT KEY within the same provider. Per-key
    /// failures — auth/payment/forbidden on THIS key (401/402/403) — rotate to the next
    /// eligible key instead of failing the whole provider; transient errors (5xx/429) rotate
    /// too (as `is_retryable`). A different-*provider* fallback is governed separately by
    /// `is_retryable`, so an all-keys-401 provider surfaces the auth error rather than
    /// silently failing over. Mirrors the standard dead-key/used-key split.
    pub fn is_key_rotatable(&self) -> bool {
        self.retryable || matches!(self.status, Some(401..=403))
    }

    /// The HTTP status this error maps to: the explicit upstream `status` if present,
    /// otherwise derived from `kind`. Single source of truth shared by the HTTP handler
    /// (response status) and the logging plugin (audit status) so they never disagree.
    pub fn http_status(&self) -> u16 {
        if let Some(s) = self.status {
            return s;
        }
        match self.kind {
            KgErrorKind::Auth => 401,
            KgErrorKind::RateLimit => 429,
            KgErrorKind::BadRequest => 400,
            KgErrorKind::Unsupported => 501,
            KgErrorKind::Network | KgErrorKind::Provider => 502,
            KgErrorKind::Internal => 500,
        }
    }
}
