//! kgateway-providers — connector implementations.
//!
//! M1: `openai`. M2 (build agents): `anthropic`, `openai_compat` (groq/ollama/...).
//! See `docs/03-providers.md`.

pub mod anthropic;
pub mod azure;
pub mod bedrock;
pub mod bedrock_mantle;
pub mod cohere;
pub mod elevenlabs;
pub mod gemini;
pub(crate) mod http;
pub mod model_listing;
pub mod openai;
pub mod openai_compat;
pub mod replicate;
pub mod runware;
pub mod runway;
pub mod sarvam;
pub mod vertex;

pub use anthropic::AnthropicProvider;
pub use azure::AzureProvider;
pub use bedrock::BedrockProvider;
pub use bedrock_mantle::BedrockMantleProvider;
pub use cohere::CohereProvider;
pub use elevenlabs::ElevenLabsProvider;
pub use gemini::GeminiProvider;
pub use openai::OpenAiProvider;
pub use replicate::ReplicateProvider;
pub use runware::RunwareProvider;
pub use runway::RunwayProvider;
pub use sarvam::SarvamProvider;
pub use vertex::VertexProvider;
