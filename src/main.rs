//! Entry point for the `vector-router` binary.
//!
//! Startup sequence:
//! 1. Load config (TOML + `VR_*` env vars).
//! 2. Build the multi-thread tokio runtime (size configurable via
//!    `config.pool.worker_threads`).
//! 3. Install the Prometheus recorder — **fail-fast**: observability is an
//!    operational prerequisite, not an option.
//! 4. Start the service via `service::start_service`: gRPC, HTTP, gauge
//!    updater in parallel, with a broadcast shutdown signal.
//! 5. Wait for SIGTERM / SIGINT.
//! 6. Propagate shutdown and bounded drain (30 s default).
//!
//! Project rules forbid `unwrap`/`expect` outside `main.rs`. This is where
//! the remaining `expect()` calls live — on operations where a failure means
//! an environment issue (e.g. signal handler unavailable) that we want to
//! surface as a panic rather than mask.

use std::time::Duration;

use tokio::sync::broadcast;

use vector_router::config::Config;
use vector_router::service::start_service;
use vector_router::telemetry::init_metrics;

const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = std::env::var("VR_CONFIG_PATH").unwrap_or_else(|_| "config.toml".to_string());

    eprintln!("vector-router : chargement config depuis {config_path}");
    let config =
        Config::load(&config_path).map_err(|e| format!("échec chargement config : {e}"))?;

    let runtime = build_runtime(&config)?;
    runtime.block_on(async_main(config))
}

fn build_runtime(config: &Config) -> std::io::Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if let Some(n) = config.pool.worker_threads {
        builder.worker_threads(n);
    }
    builder.build()
}

async fn async_main(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    // Fail-fast: without a Prometheus recorder, refuse to start.
    let metrics_handle = init_metrics()?;
    eprintln!("vector-router : recorder Prometheus installé");

    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let handles = start_service(&config, metrics_handle, shutdown_tx.clone()).await?;
    eprintln!(
        "vector-router : serveurs démarrés (gRPC {}, HTTP {})",
        config.server.grpc_bind, config.server.http_bind
    );

    wait_for_shutdown_signal().await;
    eprintln!("vector-router : signal reçu, drain en cours (timeout {DRAIN_TIMEOUT:?})");

    // Propagation: every subscriber sees `send(())`.
    let _ = shutdown_tx.send(());
    handles.drain(DRAIN_TIMEOUT).await?;
    eprintln!("vector-router : drain terminé, arrêt propre");
    Ok(())
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).expect("installer le handler SIGTERM");
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
