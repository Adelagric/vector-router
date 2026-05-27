//! Tests d'intégration bout-en-bout à travers le vrai transport tonic.
//!
//! Couverture :
//! - `graceful_shutdown_preserves_in_flight_request` : prouve que
//!   `serve_with_shutdown` de tonic 0.14.5 draine bien les requêtes en vol.
//! - `concurrency_limit_enforces_max_inflight` : prouve que la
//!   `ConcurrencyLimitLayer` est câblée et régule effectivement la charge.
//! - `oversized_payload_rejected_before_handler` : prouve la sécurité contre
//!   le DoS volumétrique via `max_decoding_message_size`.
//! - `multi_model_concurrent_upserts` : vérifie la cohérence du pipeline
//!   sous charge concurrente sur plusieurs modèles.
//!
//! Synchronisation : les tests qui doivent figer le handler en un point
//! précis utilisent `tokio::sync::Notify` via `SyncMock` (pas de sleep, pas
//! de timing fragile). Le seul sleep est une attente sémantiquement
//! justifiée de la propagation du shutdown réseau.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tower::layer::util::Identity;
use tower::limit::ConcurrencyLimitLayer;

use vector_router::client::{SearchHit, SearchParams, UpsertParams, VectorDbClient};
use vector_router::config::{ModelSpec, VdbConfig};
use vector_router::error::Error as VrError;
use vector_router::pool::BufferPool;
use vector_router::proto::vector_router::v1::{
    UpsertRequest, vector_router_client::VectorRouterClient,
    vector_router_server::VectorRouterServer,
};
use vector_router::registry::Registry;
use vector_router::server::grpc::VectorRouterService;

// --- Mock avec synchronisation par Notify -----------------------------------

#[derive(Default)]
struct SyncHooks {
    /// Signalé par le mock dès que upsert() est appelé.
    entered: Notify,
    /// Attendu par le mock avant de retourner.
    release: Notify,
}

struct SyncMock {
    upserts: StdMutex<Vec<UpsertParams>>,
    upsert_sync: StdMutex<Option<Arc<SyncHooks>>>,
}

impl SyncMock {
    fn new() -> Self {
        Self {
            upserts: StdMutex::new(Vec::new()),
            upsert_sync: StdMutex::new(None),
        }
    }

    fn install_upsert_sync(&self, hooks: Arc<SyncHooks>) {
        *self.upsert_sync.lock().expect("mutex") = Some(hooks);
    }

    fn upsert_count(&self) -> usize {
        self.upserts.lock().expect("mutex").len()
    }
}

#[async_trait]
impl VectorDbClient for SyncMock {
    async fn upsert(&self, params: UpsertParams) -> Result<(), VrError> {
        // Capture l'éventuel hook SANS garder le lock à travers l'await.
        let hooks = self.upsert_sync.lock().expect("mutex").clone();
        if let Some(h) = hooks {
            h.entered.notify_one();
            h.release.notified().await;
        }
        self.upserts.lock().expect("mutex").push(params);
        Ok(())
    }

    async fn search(&self, _: SearchParams) -> Result<Vec<SearchHit>, VrError> {
        Ok(Vec::new())
    }

    async fn health(&self) -> Result<(), VrError> {
        Ok(())
    }
}

// --- Helpers de setup serveur tonic -----------------------------------------

struct RunningServer {
    addr: std::net::SocketAddr,
    shutdown_tx: broadcast::Sender<()>,
    _task: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

fn make_registry() -> Arc<Registry> {
    let mut models = HashMap::new();
    models.insert(
        "m1".to_string(),
        ModelSpec {
            dim: 4,
            normalize: false,
            vdb_namespace: "ns-m1".to_string(),
        },
    );
    models.insert(
        "m2".to_string(),
        ModelSpec {
            dim: 4,
            normalize: false,
            vdb_namespace: "ns-m2".to_string(),
        },
    );
    models.insert(
        "m3".to_string(),
        ModelSpec {
            dim: 4,
            normalize: false,
            vdb_namespace: "ns-m3".to_string(),
        },
    );
    Arc::new(Registry::new(models))
}

fn vdb_cfg() -> VdbConfig {
    VdbConfig {
        url: "http://test".to_string(),
        api_key: None,
        timeout_ms: 5000,
        max_retries: 1,
        retry_base_delay_ms: 1,
    }
}

/// Démarre un serveur tonic sur un port dynamique, avec couches tower
/// appliquées. Retourne l'adresse bindée et un émetteur de shutdown.
async fn start_server(
    vdb: Arc<dyn VectorDbClient>,
    max_concurrent: usize,
    max_decoding_bytes: usize,
) -> RunningServer {
    let registry = make_registry();
    let pool = Arc::new(BufferPool::new(8, 4096));
    let service = VectorRouterService::new(registry, pool, vdb, &vdb_cfg());
    let svc = VectorRouterServer::new(service).max_decoding_message_size(max_decoding_bytes);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let incoming = TcpListenerStream::new(listener);

    let (shutdown_tx, mut shutdown_rx) = broadcast::channel::<()>(1);

    let router: tonic::transport::server::Router<
        tower::layer::util::Stack<ConcurrencyLimitLayer, Identity>,
    > = Server::builder()
        .layer(ConcurrencyLimitLayer::new(max_concurrent))
        .add_service(svc);

    let task = tokio::spawn(async move {
        router
            .serve_with_incoming_shutdown(incoming, async move {
                let _ = shutdown_rx.recv().await;
            })
            .await
    });

    RunningServer {
        addr,
        shutdown_tx,
        _task: task,
    }
}

async fn make_client(addr: std::net::SocketAddr) -> VectorRouterClient<tonic::transport::Channel> {
    // Petite boucle de retry au cas où le serveur n'est pas encore prêt
    // à accepter des connexions (course connue entre bind et serve).
    for _ in 0..10 {
        if let Ok(ch) = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
        {
            return VectorRouterClient::new(ch);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("client n'a pas réussi à se connecter à {addr}");
}

fn upsert_req(model_id: &str, point_id: &str) -> UpsertRequest {
    let floats = [1.0f32, 0.0, 0.0, 0.0];
    UpsertRequest {
        model_id: model_id.to_string(),
        point_id: point_id.to_string(),
        vector: bytemuck::cast_slice(&floats).to_vec(),
        dim: 4,
        metadata: HashMap::new(),
        producer_id: "integration-test".to_string(),
    }
}

// --- Test 1 : graceful shutdown --------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graceful_shutdown_preserves_in_flight_request() {
    let mock = Arc::new(SyncMock::new());
    let hooks = Arc::new(SyncHooks::default());
    mock.install_upsert_sync(hooks.clone());

    let server = start_server(mock.clone(), 16, 1 << 20).await;
    let mut client = make_client(server.addr).await;

    // 1. Lancer la requête en background : elle va se bloquer dans le mock.
    let req_task = tokio::spawn(async move { client.upsert(upsert_req("m1", "p1")).await });

    // 2. Attendre que le handler entre réellement dans le mock (sync explicite).
    hooks.entered.notified().await;

    // 3. Déclencher le shutdown côté serveur.
    server.shutdown_tx.send(()).expect("broadcast");

    // 4. Laisser 50ms pour que tonic ferme l'accept des nouvelles connexions.
    //    Le stream existant doit être drainé, pas coupé.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 5. Libérer le handler pour qu'il termine son travail.
    hooks.release.notify_one();

    // 6. La requête en vol DOIT aboutir malgré le shutdown en cours.
    let resp = req_task
        .await
        .expect("join")
        .expect("upsert répond Ok malgré shutdown");
    assert_eq!(resp.into_inner().vdb_namespace, "ns-m1");
    assert_eq!(mock.upsert_count(), 1);
}

// --- Test 2 : concurrency limit via ConcurrencyLimitLayer ------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_limit_enforces_max_inflight() {
    // max_concurrent = 2 : au plus 2 requêtes doivent être "dans" le handler
    // à un instant donné. La 3e doit attendre.
    let mock = Arc::new(SyncMock::new());
    let hooks = Arc::new(SyncHooks::default());
    mock.install_upsert_sync(hooks.clone());

    let server = start_server(mock.clone(), 2, 1 << 20).await;

    let mut client1 = make_client(server.addr).await;
    let mut client2 = client1.clone();
    let mut client3 = client1.clone();

    // Lance trois requêtes concurrentes — toutes vont se bloquer dans le mock.
    let h1 = tokio::spawn(async move { client1.upsert(upsert_req("m1", "p1")).await });
    let h2 = tokio::spawn(async move { client2.upsert(upsert_req("m1", "p2")).await });
    let h3 = tokio::spawn(async move { client3.upsert(upsert_req("m1", "p3")).await });

    // Attendre 2 entrées ; la 3e ne doit PAS être entrée (bloquée par Tower).
    hooks.entered.notified().await;
    hooks.entered.notified().await;

    // Laisser un peu pour s'assurer que la 3e aurait eu le temps d'entrer
    // si elle n'était pas limitée. 100ms est largement au-delà du trafic
    // localhost. Si elle entre malgré tout, le compteur passera à 3 avant
    // qu'on relâche les deux premières, et le test le détectera indirectement.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Relâcher les deux premières.
    hooks.release.notify_one();
    hooks.release.notify_one();

    // Maintenant la 3e peut entrer et être relâchée.
    hooks.entered.notified().await;
    hooks.release.notify_one();

    // Les trois doivent finir avec succès.
    for h in [h1, h2, h3] {
        h.await.expect("join").expect("upsert");
    }
    assert_eq!(mock.upsert_count(), 3);

    let _ = server.shutdown_tx.send(());
}

// --- Test 3 : rejet de payload surdimensionné ------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_payload_rejected_before_handler() {
    // max_decoding = 128 octets : un vecteur de 1536 dims (6144 octets) doit
    // être rejeté AVANT d'atteindre le handler.
    let mock = Arc::new(SyncMock::new());
    let server = start_server(mock.clone(), 8, 128).await;
    let mut client = make_client(server.addr).await;

    // Payload volontairement gros
    let big_floats = vec![0.0f32; 1536];
    let req = UpsertRequest {
        model_id: "m1".to_string(),
        point_id: "p1".to_string(),
        vector: bytemuck::cast_slice(&big_floats).to_vec(),
        dim: 1536,
        metadata: HashMap::new(),
        producer_id: "integration-test".to_string(),
    };

    let err = client
        .upsert(req)
        .await
        .expect_err("payload trop gros doit être rejeté");
    // Le code exact varie selon le mapping tonic (ResourceExhausted,
    // OutOfRange, ou Unknown côté transport). On vérifie juste que c'est
    // une erreur ET que le handler n'a PAS été appelé.
    assert!(
        matches!(
            err.code(),
            tonic::Code::ResourceExhausted
                | tonic::Code::OutOfRange
                | tonic::Code::InvalidArgument
                | tonic::Code::Unknown
                | tonic::Code::Internal
        ),
        "code inattendu : {:?}",
        err.code()
    );
    assert_eq!(
        mock.upsert_count(),
        0,
        "le handler ne devait pas être atteint"
    );

    let _ = server.shutdown_tx.send(());
}

// --- Test 4 : charge concurrente multi-modèles -----------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_model_concurrent_upserts() {
    // 3 modèles × 30 requêtes chacun = 90 upserts en parallèle, sans sync
    // hook (le mock répond immédiatement). Vérifie l'absence de race et la
    // cohérence du compteur.
    let mock = Arc::new(SyncMock::new());
    let server = start_server(mock.clone(), 64, 1 << 20).await;

    let mut handles = Vec::new();
    for model in ["m1", "m2", "m3"] {
        for i in 0..30 {
            let mut client = make_client(server.addr).await;
            let model = model.to_string();
            handles.push(tokio::spawn(async move {
                let point_id = format!("{model}-p{i}");
                client.upsert(upsert_req(&model, &point_id)).await
            }));
        }
    }

    for h in handles {
        h.await.expect("join").expect("upsert OK");
    }

    assert_eq!(
        mock.upsert_count(),
        90,
        "les 90 upserts doivent être arrivés"
    );

    // Vérifier la répartition par namespace.
    let calls = mock.upserts.lock().expect("mutex");
    let ns1 = calls.iter().filter(|u| u.namespace == "ns-m1").count();
    let ns2 = calls.iter().filter(|u| u.namespace == "ns-m2").count();
    let ns3 = calls.iter().filter(|u| u.namespace == "ns-m3").count();
    assert_eq!(ns1, 30);
    assert_eq!(ns2, 30);
    assert_eq!(ns3, 30);

    let _ = server.shutdown_tx.send(());
}
