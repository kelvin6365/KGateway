//! kgateway-providers — connector implementations.
//!
//! M1: `openai`. M2 (build agents): `anthropic`, `openai_compat` (groq/ollama/...).
//! See `docs/03-providers.md`.

pub mod anthropic;
pub mod azure;
pub mod bedrock;
pub mod cohere;
pub mod gemini;
pub(crate) mod http;
pub mod openai;
pub mod openai_compat;

pub use anthropic::AnthropicProvider;
pub use azure::AzureProvider;
pub use bedrock::BedrockProvider;
pub use cohere::CohereProvider;
pub use gemini::GeminiProvider;
pub use openai::OpenAiProvider;
