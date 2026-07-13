//! kgateway-core — the engine: schemas, provider/plugin traits, routing, and the
//! request pipeline. This crate has **no HTTP dependency**, so KGateway is embeddable
//! as a library. See `docs/01-architecture.md`.

pub mod context;
pub mod engine;
pub mod error;
pub mod keyselect;
pub mod mcp;
pub mod observer;
pub mod plugin;
pub mod provider;
pub mod router;
pub mod schema;

pub use context::Ctx;
pub use error::{KgError, KgErrorKind};
pub use plugin::{LlmPlugin, Plugin, PreOutcome};
pub use provider::{ApiKey, Provider, ProviderKey};
pub use schema::{ChatRequest, ChatResponse, Message, Role, StreamChunk, Usage};
