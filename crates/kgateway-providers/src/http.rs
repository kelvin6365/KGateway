//! Shared HTTP client construction for providers. Centralizes timeout/pool policy so
//! every connector gets the same robustness baseline.

use std::time::Duration;

/// Connection-establishment timeout. Applied to the client, so it is safe for
/// long-lived streaming requests (it only bounds the connect phase, not the body).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Total request timeout for NON-streaming calls. Do NOT apply this to the client
/// (it would abort long SSE streams); apply it per-request via `RequestBuilder::timeout`.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Build the shared provider HTTP client. Falls back to the default client if the
/// builder ever fails (it won't with these options).
pub fn default_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .unwrap_or_default()
}
