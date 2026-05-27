//! Orchestration du cycle de vie : démarrage des trois tâches (gRPC, HTTP,
//! gauge updater), coordination du shutdown via `tokio::sync::broadcast`,
//! drain borné par timeout.
//!
//! Architecture du shutdown :
//! - Un unique `broadcast::Sender<()>` possédé par `main` est cloné pour
//!   chaque tâche via `.subscribe()`. Signal unique, écoute multiple, pas de
//!   réinvention de la roue.
//! - `serve_with_shutdown` (tonic) et `with_graceful_shutdown` (axum) prennent
//!   une future qui termine sur `recv()`. Quand main envoie `()`, les deux
//!   serveurs arrêtent d'accepter de nouvelles connexions et attendent la fin
//!   des requêtes en cours.
//! - Le gauge updater écoute la même broadcast via `tokio::select!` entre
//!   `shutdown.recv()` et `sleep(interval)`.
//! - `ServiceHandles::drain` attend la fin des trois tâches avec un timeout
//!   global configurable.
//!
//! La factorisation `start_service_with_vdb` permet d'injecter un mock en
//! test ; `start_service` est la variante prod qui construit `QdrantVdbClient`
//! à partir de la config.

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
    /// Attend la fin des trois tâches, borné par `timeout`. Retourne
    /// `Error::Service` si le timeout est atteint (signal que certaines
    /// tâches ne se terminent pas — à logger et escalader côté orchestrateur).
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

/// Démarre tout le service avec un `QdrantVdbClient` construit depuis la config.
/// Variante utilisée par `main.rs`.
pub async fn start_service(
    config: &Config,
    metrics_handle: PrometheusHandle,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<ServiceHandles, Error> {
    let vdb: Arc<dyn VectorDbClient> = Arc::new(QdrantVdbClient::new(&config.vdb)?);
    start_service_with_vdb(config, metrics_handle, vdb, shutdown_tx).await
}

/// Variante qui accepte un VDB déjà construit. Utilisée par les tests pour
/// injecter un mock, et réutilisée en interne par `start_service`.
pub async fn start_service_with_vdb(
    config: &Config,
    metrics_handle: PrometheusHandle,
    vdb: Arc<dyn VectorDbClient>,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<ServiceHandles, Error> {
    let registry = Arc::new(Registry::new(config.models.clone()));

    // Sizing du pool depuis la config. Calcul : buffers_per_worker × nb_workers,
    // taille par buffer = dim max connue × 4 octets.
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

    // --- Tâche gauge updater ---
    let gauge_shutdown = shutdown_tx.subscribe();
    let gauge = tokio::spawn(gauge_loop(
        Arc::clone(&registry),
        Arc::clone(&pool),
        Arc::clone(&vdb),
        gauge_shutdown,
    ));

    // --- Tâche gRPC ---
    let grpc_shutdown = shutdown_tx.subscribe();
    let grpc_addr = config.server.grpc_bind;
    let grpc = tokio::spawn(async move {
        grpc_server
            .serve_with_shutdown(grpc_addr, wait_broadcast(grpc_shutdown))
            .await
    });

    // --- Tâche HTTP ---
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
                // Port 0 : le système assigne un port libre, on n'a pas besoin
                // de le connaître ici (pas de client dans ce test).
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

        // Laisser les serveurs bind avant de signaler shutdown.
        tokio::time::sleep(Duration::from_millis(50)).await;

        shutdown_tx.send(()).expect("broadcast send");

        handles
            .drain(Duration::from_secs(5))
            .await
            .expect("drain dans la fenêtre");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_propagates_to_all_tasks_in_order() {
        // On vérifie qu'un drain s'achève bien dans les temps même si le
        // gauge updater dort (intervalle 5s). Le tokio::select! doit sortir
        // de la boucle sur shutdown, pas attendre le tick suivant.
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

        // Le drain doit être immédiat (< 1s), pas bloqué sur le tick du
        // gauge updater (5s). Si > 1s, c'est que le select! n'interrompt pas.
        assert!(
            elapsed < Duration::from_secs(1),
            "drain aurait dû être immédiat, eu {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_timeout_is_reported() {
        // Test de sécurité : si drain dépasse son timeout, on retourne une
        // erreur explicite. On garde shutdown_tx alive (clone) pour empêcher
        // les subscribers de recevoir `Closed` — les tâches attendent donc
        // un vrai signal qu'on ne va pas envoyer, ce qui force le timeout.
        let config = test_config();
        let vdb: Arc<dyn VectorDbClient> = Arc::new(MockVdbClient::new());
        let metrics = test_metrics_handle();
        let (shutdown_tx, _) = broadcast::channel(1);

        let handles = start_service_with_vdb(&config, metrics, vdb, shutdown_tx.clone())
            .await
            .expect("start_service");

        tokio::time::sleep(Duration::from_millis(50)).await;
        // Ne PAS envoyer shutdown (shutdown_tx reste alive) → drain doit timeout.
        let err = handles
            .drain(Duration::from_millis(200))
            .await
            .expect_err("drain sans signal doit timeout");
        assert!(matches!(err, Error::Service(_)));

        // Cleanup : on envoie shutdown maintenant pour libérer les tâches
        // background et permettre au runtime tokio de terminer proprement.
        let _ = shutdown_tx.send(());
    }
}
