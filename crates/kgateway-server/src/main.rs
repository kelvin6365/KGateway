//! kgateway-server — the HTTP gateway binary. The implementation lives in the library
//! crate (`lib.rs`) so integration tests can drive the real router.

use kgateway_server::{app, banner, config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kgateway=debug".into()),
        )
        .init();

    let config_path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .unwrap_or_else(|| "config.json".to_string());

    let config = match config::Config::from_file(&config_path) {
        Ok(c) => {
            tracing::info!(path = %config_path, providers = c.providers.len(), "loaded config");
            c
        }
        Err(e) => {
            tracing::warn!(path = %config_path, error = %e, "no config loaded; starting empty");
            config::Config::default()
        }
    };

    let port = config.port;
    // Snapshot for the startup banner before `config` is moved into the engine.
    let banner_config = config.clone();
    let state = app::build_state(config, config_path.clone()).await;
    let router = app::build_router(state.clone());

    // Background log retention: prunes logs older than `log_retention_days` (no-op while
    // unset). Runs for the process lifetime; picks up config changes on hot-reload.
    app::spawn_retention(state.clone());

    // Hot-reload: on SIGHUP, re-read the config file and swap the engine (providers,
    // observers, virtual keys, cache, MCP) without dropping traffic. Port + admin_token
    // still require a restart.
    #[cfg(unix)]
    {
        let state = state.clone();
        let config_path = config_path.clone();
        tokio::spawn(async move {
            let mut sighup = match tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::hangup(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "could not install SIGHUP handler; hot-reload disabled");
                    return;
                }
            };
            while sighup.recv().await.is_some() {
                tracing::info!("SIGHUP received; reloading config");
                app::reload_engine(&state, &config_path).await;
            }
        });
    }

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "kgateway listening");
    // Cosmetic startup banner (ASCII wordmark + version + config summary) to stdout.
    banner::print(&banner_config, &addr);
    // Graceful shutdown: stop accepting new connections and drain in-flight requests
    // on SIGINT/SIGTERM (so k8s rollouts and Ctrl-C don't drop live requests).
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // Server drained; flush the async log writer so buffered logs are persisted, and flush
    // + shut down the OTLP exporters so buffered spans/metrics are sent.
    app::flush_logs(&state).await;
    state.otel_providers.shutdown();
    tracing::info!("shutdown complete");
    Ok(())
}

/// Resolve when the process receives SIGINT (Ctrl-C) or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl-C, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
