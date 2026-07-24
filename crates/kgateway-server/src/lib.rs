//! kgateway-server — the HTTP gateway, as a library so the binary (`main.rs`) and
//! integration tests both drive the SAME router/state/handlers (no reconstructed test
//! router). See `main.rs` for the binary entrypoint.

pub mod anthropic_ingress;
pub mod api_catalog;
pub mod api_docs;
pub mod app;
pub mod auth;
pub mod banner;
pub mod config;
pub mod handlers;
pub mod metrics;
pub mod otel;
