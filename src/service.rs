//! Lifecycle orchestration: starts the three tasks (gRPC, HTTP, gauge
//! updater), coordinates shutdown via `tokio::sync::broadcast`, drains
//! bounded by timeout.
//!
//! Shutdown architecture:
//! - A single `broadcast::Sender<()>` owned by `main` is cloned for every
//!   task via `.subscribe()`. One signal, multiple listeners, no wheel
//!   reinvention.
//! - `serve_with_shutdown` (tonic) and `with_graceful_shutdown` (axum) both
//!   take a future that completes on `recv()`. When main sends `()`, both
//!   servers stop accepting new connections and wait for in-flight requests
//!   to finish.
//! - The gauge updater listens to the same broadcast via `tokio::select!`
//!   between `shutdown.recv()` and `sleep(interval)`.
//! - `ServiceHandles::drain` waits for the three tasks to finish with a
//!   configurable global timeout.
//!
//! The `start_service_with_vdb` factoring lets tests inject a mock;
//! `start_service` is the prod variant that builds `QdrantVdbClient` from
//! config.

use std::sync::Arc;
use std::time::Duration;

use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::client::{QdrantVdbClient, VectorDbClient};
use crate::config::Config;
use crate::error::Error;
use crate::pool::BufferPool;
use crate::registry::Registry;
use crate::server::grpc::{VectorRouterService, build_grpc_server};
use crate::server::http::build_http_router;

pub struct ServiceHandles {
    grpc: JoinHandle<Result<(), tonic::transport::Error>>,
    http: JoinHandle<Result<(), std::io::Error>>,
    gauge: JoinHandle<()>,
}

impl ServiceHandles {
    /// Waits for the three tasks to finish, bounded by `timeout`. Returns
    /// `Error::Service` on timeout (signals that some tasks are not
    /// terminating — to be logged and escalated by the orchestrator).
    pub async fn drain(self, timeout: Duration) -> Result<(), Error> {
        let joined = tokio::time::timeout(timeout, async move {
            let _ = self.grpc.await;
            let _ = self.http.await;
            let _ = self.gauge.await;
        })
        .await;
        match joined {
            Ok(()) => Ok(()),
            Err(_) => Err(Error::Service(format!(
                "drain non complété dans {timeout:?}"
            ))),
        }
    }
}

/// Starts the full service with a `QdrantVdbClient` built from config.
/// Variant used by `main.rs`.
pub async fn start_service(
    config: &Config,
    metrics_handle: PrometheusHandle,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<ServiceHandles, Error> {
    let vdb: Arc<dyn VectorDbClient> = Arc::new(QdrantVdbClient::new(&config.vdb)?);
    start_service_with_vdb(config, metrics_handle, vdb, shutdown_tx).await
}

/// Variant that accepts an already-built VDB. Used by tests to inject a
/// mock, and reused internally by `start_service`.
pub async fn start_service_with_vdb(
    config: &Config,
    metrics_handle: PrometheusHandle,
    vdb: Arc<dyn VectorDbClient>,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<ServiceHandles, Error> {
    let registry = Arc::new(Registry::new(config.models.clone()));

    // Pool sizing from config. Formula: buffers_per_worker × n_workers,
    // size per buffer = max known dim × 4 bytes.
    let worker_count = config.pool.worker_threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });
    let pool_size = (config.pool.buffers_per_worker as usize).max(1) * worker_count;
    let max_dim = config.models.values().map(|m| m.dim).max().unwrap_or(1536);
    let pool = Arc::new(BufferPool::new(pool_size, max_dim * 4));

    let grpc_service = VectorRouterService::new(
        Arc::clone(&registry),
        Arc::clone(&pool),
        Arc::clone(&vdb),
        &config.vdb,
    );
    let grpc_server = build_grpc_server(grpc_service, &config.server);
    let http_router = build_http_router(Arc::clone(&vdb), metrics_handle);

    // --- Gauge updater task ---
    let gauge_shutdown = shutdown_tx.subscribe();
    let gauge = tokio::spawn(gauge_loop(
        Arc::clone(&registry),
        Arc::clone(&pool),
        Arc::clone(&vdb),
        gauge_shutdown,
    ));

    // --- gRPC task ---
    let grpc_shutdown = shutdown_tx.subscribe();
    let grpc_addr = config.server.grpc_bind;
    let grpc = tokio::spawn(async move {
        grpc_server
            .serve_with_shutdown(grpc_addr, wait_broadcast(grpc_shutdown))
            .await
    });

    // --- HTTP task ---
    let http_shutdown = shutdown_tx.subscribe();
    let http_addr = config.server.http_bind;
    let http = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(http_addr).await?;
        axum::serve(listener, http_router)
            .with_graceful_shutdown(wait_broadcast(http_shutdown))
            .await
    });

    Ok(ServiceHandles { grpc, http, gauge })
}

async fn wait_broadcast(mut rx: broadcast::Receiver<()>) {
    let _ = rx.recv().await;
}

async fn gauge_loop(
    registry: Arc<Registry>,
    pool: Arc<BufferPool>,
    vdb: Arc<dyn VectorDbClient>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let interval = Duration::from_secs(5);
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            () = tokio::time::sleep(interval) => {
                metrics::gauge!("registered_models").set(registry.len() as f64);
                metrics::gauge!("pool_available").set(pool.available() as f64);
                metrics::gauge!("vdb_inflight").set(vdb.inflight() as f64);
            }
        }
    }
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use metrics_exporter_prometheus::PrometheusBuilder;

    use crate::client::mock::MockVdbClient;
    use crate::config::{
        AdminConfig, ModelSpec, PoolConfig, ServerConfig, TelemetryConfig, VdbConfig,
    };

    fn test_config() -> Config {
        let mut models = HashMap::new();
        models.insert(
            "m1".to_string(),
            ModelSpec {
                dim: 4,
                normalize: true,
                vdb_namespace: "ns".to_string(),
            },
        );
        Config {
            server: ServerConfig {
                // Port 0: the system picks a free port; we don't need to
                // know it here (no client in this test).
                grpc_bind: "127.0.0.1:0".parse().unwrap(),
                http_bind: "127.0.0.1:0".parse().unwrap(),
                max_concurrent_requests: 64,
                max_decoding_message_size_bytes: 1024 * 1024,
            },
            admin: AdminConfig {
                bearer_token: "secret-test".to_string(),
            },
            vdb: VdbConfig {
                url: "http://localhost:6334".to_string(),
                api_key: None,
                timeout_ms: 100,
                max_retries: 3,
                retry_base_delay_ms: 10,
            },
            pool: PoolConfig {
                buffers_per_worker: 2,
                worker_threads: Some(1),
            },
            telemetry: TelemetryConfig {
                log_level: "info".to_string(),
                otlp_endpoint: None,
            },
            models,
        }
    }

    fn test_metrics_handle() -> PrometheusHandle {
        PrometheusBuilder::new().build_recorder().handle()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn service_starts_and_shuts_down_cleanly() {
        let config = test_config();
        let vdb: Arc<dyn VectorDbClient> = Arc::new(MockVdbClient::new());
        let metrics = test_metrics_handle();
        let (shutdown_tx, _) = broadcast::channel(1);

        let handles = start_service_with_vdb(&config, metrics, vdb, shutdown_tx.clone())
            .await
            .expect("start_service");

        // Let the servers bind before signaling shutdown.
        tokio::time::sleep(Duration::from_millis(50)).await;

        shutdown_tx.send(()).expect("broadcast send");

        handles
            .drain(Duration::from_secs(5))
            .await
            .expect("drain dans la fenêtre");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_propagates_to_all_tasks_in_order() {
        // Verify that a drain completes within the window even when the
        // gauge updater is sleeping (5s interval). The tokio::select! must
        // break out of the loop on shutdown, not wait for the next tick.
        let config = test_config();
        let vdb: Arc<dyn VectorDbClient> = Arc::new(MockVdbClient::new());
        let metrics = test_metrics_handle();
        let (shutdown_tx, _) = broadcast::channel(1);

        let handles = start_service_with_vdb(&config, metrics, vdb, shutdown_tx.clone())
            .await
            .expect("start_service");

        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown_tx.send(()).expect("broadcast send");

        let start = std::time::Instant::now();
        handles.drain(Duration::from_secs(5)).await.expect("drain");
        let elapsed = start.elapsed();

        // Drain must be immediate (< 1s), not blocked on the gauge
        // updater's tick (5s). If > 1s, the select! is not interrupting.
        assert!(
            elapsed < Duration::from_secs(1),
            "drain aurait dû être immédiat, eu {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_timeout_is_reported() {
        // Safety test: if drain exceeds its timeout, we return an explicit
        // error. We keep shutdown_tx alive (clone) so subscribers don't
        // receive `Closed` — tasks then wait for a real signal we won't
        // send, which forces the timeout.
        let config = test_config();
        let vdb: Arc<dyn VectorDbClient> = Arc::new(MockVdbClient::new());
        let metrics = test_metrics_handle();
        let (shutdown_tx, _) = broadcast::channel(1);

        let handles = start_service_with_vdb(&config, metrics, vdb, shutdown_tx.clone())
            .await
            .expect("start_service");

        tokio::time::sleep(Duration::from_millis(50)).await;
        // Do NOT send shutdown (shutdown_tx stays alive) → drain must time out.
        let err = handles
            .drain(Duration::from_millis(200))
            .await
            .expect_err("drain sans signal doit timeout");
        assert!(matches!(err, Error::Service(_)));

        // Cleanup: send shutdown now so background tasks can release and
        // the tokio runtime can shut down cleanly.
        let _ = shutdown_tx.send(());
    }
}
