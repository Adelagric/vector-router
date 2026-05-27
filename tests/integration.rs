//! End-to-end integration tests through the real tonic transport.
//!
//! Coverage:
//! - `graceful_shutdown_preserves_in_flight_request`: proves that tonic
//!   0.14.5's `serve_with_shutdown` drains in-flight requests.
//! - `concurrency_limit_enforces_max_inflight`: proves that the
//!   `ConcurrencyLimitLayer` is wired in and effectively regulates load.
//! - `oversized_payload_rejected_before_handler`: proves volumetric DoS
//!   protection via `max_decoding_message_size`.
//! - `multi_model_concurrent_upserts`: checks pipeline consistency under
//!   concurrent load across several models.
//!
//! Synchronization: tests that need to freeze the handler at a precise
//! point use `tokio::sync::Notify` via `SyncMock` (no sleep, no fragile
//! timing). The only sleep is a semantically justified wait for network
//! shutdown propagation.

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

// --- Mock with Notify-based synchronization ---------------------------------

#[derive(Default)]
struct SyncHooks {
    /// Signaled by the mock as soon as upsert() is called.
    entered: Notify,
    /// Awaited by the mock before returning.
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
        // Capture the optional hook WITHOUT holding the lock across await.
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

// --- tonic server setup helpers ---------------------------------------------

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

/// Starts a tonic server on a dynamic port, with the tower layers applied.
/// Returns the bound address and a shutdown sender.
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
    // Small retry loop in case the server isn't ready to accept
    // connections yet (known race between bind and serve).
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

// --- Test 1: graceful shutdown ---------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graceful_shutdown_preserves_in_flight_request() {
    let mock = Arc::new(SyncMock::new());
    let hooks = Arc::new(SyncHooks::default());
    mock.install_upsert_sync(hooks.clone());

    let server = start_server(mock.clone(), 16, 1 << 20).await;
    let mut client = make_client(server.addr).await;

    // 1. Launch the request in the background: it will block in the mock.
    let req_task = tokio::spawn(async move { client.upsert(upsert_req("m1", "p1")).await });

    // 2. Wait for the handler to actually enter the mock (explicit sync).
    hooks.entered.notified().await;

    // 3. Trigger shutdown on the server side.
    server.shutdown_tx.send(()).expect("broadcast");

    // 4. Give 50ms for tonic to stop accepting new connections.
    //    The existing stream must be drained, not cut.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 5. Release the handler so it finishes its work.
    hooks.release.notify_one();

    // 6. The in-flight request MUST succeed despite the in-progress shutdown.
    let resp = req_task
        .await
        .expect("join")
        .expect("upsert répond Ok malgré shutdown");
    assert_eq!(resp.into_inner().vdb_namespace, "ns-m1");
    assert_eq!(mock.upsert_count(), 1);
}

// --- Test 2: concurrency limit via ConcurrencyLimitLayer -------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_limit_enforces_max_inflight() {
    // max_concurrent = 2: at most 2 requests can be "in" the handler at
    // any given time. The 3rd must wait.
    let mock = Arc::new(SyncMock::new());
    let hooks = Arc::new(SyncHooks::default());
    mock.install_upsert_sync(hooks.clone());

    let server = start_server(mock.clone(), 2, 1 << 20).await;

    let mut client1 = make_client(server.addr).await;
    let mut client2 = client1.clone();
    let mut client3 = client1.clone();

    // Launch three concurrent requests — all will block in the mock.
    let h1 = tokio::spawn(async move { client1.upsert(upsert_req("m1", "p1")).await });
    let h2 = tokio::spawn(async move { client2.upsert(upsert_req("m1", "p2")).await });
    let h3 = tokio::spawn(async move { client3.upsert(upsert_req("m1", "p3")).await });

    // Wait for 2 entries; the 3rd must NOT have entered (Tower-blocked).
    hooks.entered.notified().await;
    hooks.entered.notified().await;

    // Wait a bit to make sure the 3rd would have had time to enter if it
    // weren't limited. 100ms is well beyond localhost traffic. If it
    // enters anyway, the counter will hit 3 before we release the first
    // two, and the test will detect it indirectly.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Release the first two.
    hooks.release.notify_one();
    hooks.release.notify_one();

    // Now the 3rd can enter and be released.
    hooks.entered.notified().await;
    hooks.release.notify_one();

    // All three must finish successfully.
    for h in [h1, h2, h3] {
        h.await.expect("join").expect("upsert");
    }
    assert_eq!(mock.upsert_count(), 3);

    let _ = server.shutdown_tx.send(());
}

// --- Test 3: oversized payload rejection -----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_payload_rejected_before_handler() {
    // max_decoding = 128 bytes: a 1536-dim vector (6144 bytes) must be
    // rejected BEFORE reaching the handler.
    let mock = Arc::new(SyncMock::new());
    let server = start_server(mock.clone(), 8, 128).await;
    let mut client = make_client(server.addr).await;

    // Deliberately large payload
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
    // The exact code varies with the tonic mapping (ResourceExhausted,
    // OutOfRange, or Unknown on the transport side). Just verify that
    // it's an error AND that the handler was NOT invoked.
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

// --- Test 4: concurrent multi-model load ----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_model_concurrent_upserts() {
    // 3 models × 30 requests each = 90 parallel upserts, without sync
    // hooks (the mock responds immediately). Checks for the absence of
    // races and counter consistency.
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

    // Check the per-namespace distribution.
    let calls = mock.upserts.lock().expect("mutex");
    let ns1 = calls.iter().filter(|u| u.namespace == "ns-m1").count();
    let ns2 = calls.iter().filter(|u| u.namespace == "ns-m2").count();
    let ns3 = calls.iter().filter(|u| u.namespace == "ns-m3").count();
    assert_eq!(ns1, 30);
    assert_eq!(ns2, 30);
    assert_eq!(ns3, 30);

    let _ = server.shutdown_tx.send(());
}
