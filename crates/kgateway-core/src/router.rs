//! Provider registry with per-provider isolation.
//!
//! Each provider owns a `Semaphore` bounding its in-flight requests, so one slow or
//! failing provider cannot exhaust global capacity (a "provider isolation"
//! principle). Weighted key selection lives in `keyselect.rs`; the fallback loop lives
//! in `engine.rs`.

use crate::provider::{ApiKey, Provider, ProviderKey};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Default max concurrent in-flight requests per provider.
pub const DEFAULT_PROVIDER_CONCURRENCY: usize = 256;

/// A read-only summary of a registered provider (control-plane / dashboard).
#[derive(Debug, Clone, Serialize)]
pub struct ProviderSummary {
    pub name: String,
    /// Capabilities this provider supports: always "chat", plus any opt-in ones.
    pub capabilities: Vec<&'static str>,
    pub key_count: usize,
}

/// A registered provider with its keys and an isolation semaphore.
pub struct ProviderEntry {
    pub provider: Arc<dyn Provider>,
    pub keys: Vec<ApiKey>,
    /// Bounds concurrent calls to this provider. Acquire a permit before dispatch.
    pub concurrency: Arc<Semaphore>,
}

/// Registry of all configured providers.
#[derive(Default)]
pub struct Registry {
    providers: HashMap<ProviderKey, ProviderEntry>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider with the default per-provider concurrency cap.
    pub fn register(&mut self, provider: Arc<dyn Provider>, keys: Vec<ApiKey>) {
        self.register_with_concurrency(provider, keys, DEFAULT_PROVIDER_CONCURRENCY);
    }

    /// Register a provider with an explicit concurrency cap.
    pub fn register_with_concurrency(
        &mut self,
        provider: Arc<dyn Provider>,
        keys: Vec<ApiKey>,
        concurrency: usize,
    ) {
        let entry = ProviderEntry {
            provider: provider.clone(),
            keys,
            concurrency: Arc::new(Semaphore::new(concurrency.max(1))),
        };
        self.providers.insert(provider.key(), entry);
    }

    pub fn get(&self, key: &ProviderKey) -> Option<&ProviderEntry> {
        self.providers.get(key)
    }

    /// Read-only summaries of every registered provider + its capabilities, for the
    /// control-plane/dashboard. Sorted by name for stable output.
    pub fn summaries(&self) -> Vec<ProviderSummary> {
        let mut out: Vec<ProviderSummary> = self
            .providers
            .iter()
            .map(|(key, entry)| {
                let p = &entry.provider;
                let mut capabilities = vec!["chat"];
                if p.as_embeddings().is_some() {
                    capabilities.push("embeddings");
                }
                if p.as_images().is_some() {
                    capabilities.push("images");
                }
                if p.as_audio().is_some() {
                    capabilities.push("audio");
                }
                if p.as_rerank().is_some() {
                    capabilities.push("rerank");
                }
                ProviderSummary {
                    name: key.to_string(),
                    capabilities,
                    key_count: entry.keys.len(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}
