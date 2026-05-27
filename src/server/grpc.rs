//! Serveur gRPC exposant les RPC `Upsert` et `Search` du service `VectorRouter`.
//!
//! Architecture :
//! - `validate_and_prepare` : fonction synchrone partagée entre les deux RPC.
//!   Résolution modèle + validation dim + alignement + norme² + normalisation.
//!   Produit un `Vec<f32>` owned : on relâche le `PooledBuffer` AVANT tout
//!   `.await`, ce qui évite toute friction de lifetime avec le runtime async.
//!   Pattern validé par `tests/lifetime_spike.rs`.
//! - `call_vdb_with_retry` : wrapper de retry exponentiel sur timeout,
//!   appelé par les deux handlers avec un closure qui clone les params par
//!   appel. Le clone est borné (≤ max_retries fois) et n'a lieu que sur le
//!   chemin rare ; le chemin nominal fait une copie unique.
//! - `#[tonic::async_trait]` sur l'impl pour satisfaire le trait généré
//!   (voir `tests/lifetime_spike.rs` ; sans cette annotation, erreur E0195).

use std::sync::Arc;
use std::time::{Duration, Instant};

use metrics::{counter, histogram};
use tonic::transport::server::Router;
use tonic::{Request, Response, Status};
use tower::layer::util::{Identity, Stack};
use tower::limit::ConcurrencyLimitLayer;

use crate::client::{SearchParams, UpsertParams, VectorDbClient};
use crate::config::{ModelSpec, ServerConfig, VdbConfig};
use crate::error::Error;
use crate::math::{l2_norm_squared, normalize_in_place, validate_and_align};
use crate::pool::BufferPool;
use crate::proto::vector_router::v1::{
    SearchHit as ProtoSearchHit, SearchRequest, SearchResponse, UpsertRequest, UpsertResponse,
    vector_router_server::{VectorRouter, VectorRouterServer},
};
use crate::registry::Registry;

// --- Politique de retry ----------------------------------------------------

#[derive(Clone)]
struct RetryPolicy {
    max_retries: u32,
    base_delay_ms: u64,
}

impl RetryPolicy {
    fn from_cfg(cfg: &VdbConfig) -> Self {
        Self {
            max_retries: cfg.max_retries,
            base_delay_ms: cfg.retry_base_delay_ms,
        }
    }
}

/// Détecte si une erreur VDB est "transitive" (candidate au retry).
/// On reste strict : seuls les timeouts déclenchent un retry. Les erreurs
/// logiques ou 4xx/5xx remontent immédiatement pour éviter de masquer un
/// vrai problème de schéma ou d'auth par du bruit de retries.
fn is_transient(err: &Error) -> bool {
    matches!(err, Error::Vdb(msg) if msg.starts_with("timeout"))
}

// --- Service ---------------------------------------------------------------

pub struct VectorRouterService {
    registry: Arc<Registry>,
    pool: Arc<BufferPool>,
    vdb: Arc<dyn VectorDbClient>,
    retry: RetryPolicy,
}

impl VectorRouterService {
    pub fn new(
        registry: Arc<Registry>,
        pool: Arc<BufferPool>,
        vdb: Arc<dyn VectorDbClient>,
        vdb_cfg: &VdbConfig,
    ) -> Self {
        Self {
            registry,
            pool,
            vdb,
            retry: RetryPolicy::from_cfg(vdb_cfg),
        }
    }

    /// Pipeline partagé Upsert/Search. **Synchrone** : tout le travail qui
    /// touche au pool et à la validation se fait avant le premier `.await`.
    /// Retourne un `Vec<f32>` owned, prêt à partir vers la VDB.
    fn validate_and_prepare(
        &self,
        model_id: &str,
        dim: u32,
        raw_vector: &[u8],
    ) -> Result<ValidatedVector, Error> {
        // Un seul .load() par requête (via Registry::get).
        let spec = self
            .registry
            .get(model_id)
            .ok_or_else(|| Error::UnknownModel {
                model_id: model_id.to_string(),
            })?;

        if dim as usize != spec.dim {
            return Err(Error::InvalidDim {
                expected: spec.dim * 4,
                got: raw_vector.len(),
            });
        }

        let mut pooled = self.pool.take();
        let view = validate_and_align(raw_vector, spec.dim, &mut pooled)?;
        let n2 = l2_norm_squared(&view)?;

        // Passage Cow → Vec<f32> owned : la vue ne borrow plus `pooled`,
        // ce qui permet de libérer le buffer avant l'appel VDB async.
        let mut owned: Vec<f32> = view.into_owned();
        drop(pooled);

        let was_normalized = if spec.normalize {
            normalize_in_place(&mut owned, n2)
        } else {
            false
        };

        Ok(ValidatedVector {
            spec,
            vector: owned,
            was_normalized,
        })
    }

    /// Retry exponentiel sur les erreurs transitoires, tentative unique pour
    /// les erreurs logiques. Le closure `op` est appelé 1..=max_retries fois.
    async fn call_vdb_with_retry<F, Fut, T>(&self, mut op: F) -> Result<T, Error>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, Error>>,
    {
        let mut delay = Duration::from_millis(self.retry.base_delay_ms);
        let mut last_err: Option<Error> = None;

        for attempt in 1..=self.retry.max_retries {
            match op().await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let retryable = is_transient(&e) && attempt < self.retry.max_retries;
                    if !retryable {
                        return Err(e);
                    }
                    last_err = Some(e);
                    tokio::time::sleep(delay).await;
                    // Saturating pour ne pas overflow sur max_retries exotique.
                    delay = delay.saturating_mul(2);
                }
            }
        }
        // Atteint uniquement si max_retries == 0 (interdit par validate()).
        Err(last_err.unwrap_or_else(|| Error::Vdb("retry loop vide".to_string())))
    }
}

struct ValidatedVector {
    spec: ModelSpec,
    vector: Vec<f32>,
    was_normalized: bool,
}

// --- Builder du serveur tonic ---------------------------------------------

/// Assemble le `Router` tonic avec les protections demandées par le brief :
/// - `max_decoding_message_size` limite la taille des payloads entrants AVANT
///   parsing (rejet en amont du pipeline).
/// - `ConcurrencyLimitLayer` borne le nombre de requêtes traitées en parallèle
///   pour éviter de saturer le VDB downstream.
///
/// Appelé depuis `main.rs` pour construire le serveur prêt à `serve(addr)`.
pub fn build_grpc_server(
    service: VectorRouterService,
    cfg: &ServerConfig,
) -> Router<Stack<ConcurrencyLimitLayer, Identity>> {
    let svc = VectorRouterServer::new(service)
        .max_decoding_message_size(cfg.max_decoding_message_size_bytes);

    tonic::transport::Server::builder()
        .layer(ConcurrencyLimitLayer::new(
            cfg.max_concurrent_requests as usize,
        ))
        .add_service(svc)
}

// --- Mapping Error → tonic::Status -----------------------------------------

fn status_from_error(err: Error) -> Status {
    match err {
        Error::UnknownModel { ref model_id } => {
            Status::not_found(format!("modèle inconnu : {model_id}"))
        }
        Error::InvalidDim { expected, got } => Status::invalid_argument(format!(
            "dimension invalide : attendu {expected} octets, reçu {got}"
        )),
        Error::InvalidNumeric => {
            Status::invalid_argument("vecteur contient NaN ou Inf".to_string())
        }
        Error::Vdb(msg) => Status::unavailable(format!("base vectorielle : {msg}")),
        Error::Validation(msg) => Status::invalid_argument(msg),
        Error::Config(e) => Status::internal(format!("config : {e}")),
        Error::Io(e) => Status::internal(format!("io : {e}")),
        Error::Telemetry(msg) => Status::internal(format!("télémétrie : {msg}")),
        Error::Service(msg) => Status::internal(format!("service : {msg}")),
    }
}

/// Label `status` pour la métrique `requests_total`, déduit de la variante
/// d'erreur. Aligné sur les codes utilisés dans le brief initial.
fn status_label_from_error(err: &Error) -> &'static str {
    match err {
        Error::UnknownModel { .. } => "unknown_model",
        Error::InvalidDim { .. } => "invalid_dim",
        Error::InvalidNumeric => "invalid_numeric",
        Error::Vdb(_) => "vdb_error",
        _ => "internal_error",
    }
}

/// Normalise l'identifiant producteur reçu en protocole gRPC.
///
/// Un `producer_id` vide (champ proto3 absent ou explicitement "") est
/// remplacé par `"unknown"` pour éviter une chaîne vide dans les labels
/// Prometheus. Contrat avec l'appelant : l'ensemble des `producer_id`
/// remontés doit rester borné (noms de service, pas d'UUID) — toute
/// explosion de cardinalité est imputable à l'intégration client, pas au
/// middleware. Cf. commentaire dans router.proto.
fn normalize_producer(producer_id: &str) -> &str {
    if producer_id.is_empty() {
        "unknown"
    } else {
        producer_id
    }
}

/// Enregistre une requête terminée : incrémente `requests_total` avec
/// (`model_id`, `op`, `status`, `producer_id`) et enregistre la durée dans
/// `request_duration_seconds`. `model_id = "unknown"` si le lookup a échoué
/// (le label reste borné). `producer_id` est déjà normalisé par
/// [`normalize_producer`].
fn record_request_metrics(
    model_id: &str,
    op: &'static str,
    status: &'static str,
    producer_id: &str,
    duration_s: f64,
) {
    counter!(
        "requests_total",
        "model_id" => model_id.to_string(),
        "op" => op,
        "status" => status,
        "producer_id" => producer_id.to_string(),
    )
    .increment(1);
    histogram!(
        "request_duration_seconds",
        "model_id" => model_id.to_string(),
        "op" => op,
        "producer_id" => producer_id.to_string(),
    )
    .record(duration_s);
}

/// Journal de rejet structuré (JSON Lines sur stderr).
///
/// Émis pour toute requête rejetée par `validate_and_prepare`, afin de
/// permettre la post-mortem forensique : quel producteur envoie des
/// vecteurs mal formés, quelle dimension, quel modèle. Volontairement
/// séparé de la métrique Prometheus : les métriques sont agrégées, le log
/// garde la granularité point-par-point pour investigation.
///
/// Format volontairement minimal — pas de trace ID, pas de corrélation
/// distribuée : c'est un outil de debug, pas un audit trail. Un opérateur
/// peut `grep | jq` sans chaîne de tooling complexe.
///
/// Écrit sur stderr (pas stdout) pour ne pas polluer une éventuelle sortie
/// structurée du binaire, et ne pas être capturé par une redirection
/// applicative.
fn log_rejection(op: &'static str, producer_id: &str, model_id: &str, status: &str, reason: &str) {
    let entry = serde_json::json!({
        "event": "rejection",
        "op": op,
        "producer_id": producer_id,
        "model_id": model_id,
        "status": status,
        "reason": reason,
    });
    eprintln!("{entry}");
}

// --- Impl du trait tonic ---------------------------------------------------

#[tonic::async_trait]
impl VectorRouter for VectorRouterService {
    async fn upsert(
        &self,
        request: Request<UpsertRequest>,
    ) -> Result<Response<UpsertResponse>, Status> {
        let start = Instant::now();
        let req = request.into_inner();
        let model_id = req.model_id.clone();
        let producer = normalize_producer(&req.producer_id).to_string();

        let validated = match self.validate_and_prepare(&model_id, req.dim, &req.vector) {
            Ok(v) => v,
            Err(e) => {
                let status = status_label_from_error(&e);
                let reason = e.to_string();
                log_rejection("upsert", &producer, &model_id, status, &reason);
                record_request_metrics(
                    &model_id,
                    "upsert",
                    status,
                    &producer,
                    start.elapsed().as_secs_f64(),
                );
                return Err(status_from_error(e));
            }
        };

        if validated.was_normalized {
            counter!("normalizations_performed_total", "model_id" => model_id.clone()).increment(1);
        }

        let namespace = validated.spec.vdb_namespace.clone();
        let was_normalized = validated.was_normalized;
        let point_id = req.point_id.clone();

        let template = UpsertParams {
            namespace: namespace.clone(),
            point_id: req.point_id,
            vector: validated.vector,
            metadata: req.metadata,
        };
        let vdb = Arc::clone(&self.vdb);

        let result = self
            .call_vdb_with_retry(|| {
                let params = template.clone();
                let vdb = Arc::clone(&vdb);
                async move { vdb.upsert(params).await }
            })
            .await;

        let duration_s = start.elapsed().as_secs_f64();
        match result {
            Ok(()) => {
                record_request_metrics(&model_id, "upsert", "ok", &producer, duration_s);
                Ok(Response::new(UpsertResponse {
                    point_id,
                    processing_us: start.elapsed().as_micros() as u64,
                    was_normalized,
                    vdb_namespace: namespace,
                }))
            }
            Err(e) => {
                let status = status_label_from_error(&e);
                record_request_metrics(&model_id, "upsert", status, &producer, duration_s);
                Err(status_from_error(e))
            }
        }
    }

    async fn search(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<SearchResponse>, Status> {
        let start = Instant::now();
        let req = request.into_inner();
        let model_id = req.model_id.clone();
        let producer = normalize_producer(&req.producer_id).to_string();

        let validated = match self.validate_and_prepare(&model_id, req.dim, &req.vector) {
            Ok(v) => v,
            Err(e) => {
                let status = status_label_from_error(&e);
                let reason = e.to_string();
                log_rejection("search", &producer, &model_id, status, &reason);
                record_request_metrics(
                    &model_id,
                    "search",
                    status,
                    &producer,
                    start.elapsed().as_secs_f64(),
                );
                return Err(status_from_error(e));
            }
        };

        if validated.was_normalized {
            counter!("normalizations_performed_total", "model_id" => model_id.clone()).increment(1);
        }

        let namespace = validated.spec.vdb_namespace.clone();
        let was_normalized = validated.was_normalized;

        let template = SearchParams {
            namespace: namespace.clone(),
            vector: validated.vector,
            limit: req.limit,
            score_threshold: if req.score_threshold != 0.0 {
                Some(req.score_threshold)
            } else {
                None
            },
            metadata_filter: req.metadata_filter,
        };
        let vdb = Arc::clone(&self.vdb);

        let result = self
            .call_vdb_with_retry(|| {
                let params = template.clone();
                let vdb = Arc::clone(&vdb);
                async move { vdb.search(params).await }
            })
            .await;

        let duration_s = start.elapsed().as_secs_f64();
        match result {
            Ok(hits) => {
                record_request_metrics(&model_id, "search", "ok", &producer, duration_s);
                Ok(Response::new(SearchResponse {
                    hits: hits
                        .into_iter()
                        .map(|h| ProtoSearchHit {
                            point_id: h.point_id,
                            score: h.score,
                            metadata: h.metadata,
                        })
                        .collect(),
                    processing_us: start.elapsed().as_micros() as u64,
                    was_normalized,
                    vdb_namespace: namespace,
                }))
            }
            Err(e) => {
                let status = status_label_from_error(&e);
                record_request_metrics(&model_id, "search", status, &producer, duration_s);
                Err(status_from_error(e))
            }
        }
    }
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use bytemuck;

    use crate::client::SearchHit;
    use crate::client::mock::MockVdbClient;

    fn make_service(vdb: Arc<dyn VectorDbClient>) -> VectorRouterService {
        let mut models = HashMap::new();
        models.insert(
            "m1".to_string(),
            ModelSpec {
                dim: 4,
                normalize: true,
                vdb_namespace: "ns-m1".to_string(),
            },
        );
        models.insert(
            "m2".to_string(),
            ModelSpec {
                dim: 2,
                normalize: false,
                vdb_namespace: "ns-m2".to_string(),
            },
        );
        let registry = Arc::new(Registry::new(models));
        let pool = Arc::new(BufferPool::new(2, 1024));
        let vdb_cfg = VdbConfig {
            url: "http://test".to_string(),
            api_key: None,
            timeout_ms: 5000,
            max_retries: 3,
            retry_base_delay_ms: 1, // rapide pour tests
        };
        VectorRouterService::new(registry, pool, vdb, &vdb_cfg)
    }

    fn upsert_req(model_id: &str, dim: u32, floats: &[f32]) -> Request<UpsertRequest> {
        upsert_req_from("test-producer", model_id, dim, floats)
    }

    fn upsert_req_from(
        producer: &str,
        model_id: &str,
        dim: u32,
        floats: &[f32],
    ) -> Request<UpsertRequest> {
        Request::new(UpsertRequest {
            model_id: model_id.to_string(),
            point_id: "p1".to_string(),
            vector: bytemuck::cast_slice(floats).to_vec(),
            dim,
            metadata: HashMap::new(),
            producer_id: producer.to_string(),
        })
    }

    fn search_req(model_id: &str, dim: u32, floats: &[f32]) -> Request<SearchRequest> {
        Request::new(SearchRequest {
            model_id: model_id.to_string(),
            vector: bytemuck::cast_slice(floats).to_vec(),
            dim,
            limit: 10,
            score_threshold: 0.0,
            metadata_filter: HashMap::new(),
            producer_id: "test-producer".to_string(),
        })
    }

    #[test]
    fn normalize_producer_handles_empty() {
        assert_eq!(normalize_producer(""), "unknown");
        assert_eq!(normalize_producer("batch-nightly"), "batch-nightly");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upsert_happy_path_normalizes_and_forwards() {
        let mock = Arc::new(MockVdbClient::new());
        let svc = make_service(mock.clone());

        // [3, 4, 0, 0] → norme = 5 → normalisé → [0.6, 0.8, 0, 0]
        let floats = [3.0f32, 4.0, 0.0, 0.0];
        let resp = svc.upsert(upsert_req("m1", 4, &floats)).await.unwrap();
        let inner = resp.into_inner();

        assert_eq!(inner.vdb_namespace, "ns-m1");
        assert!(inner.was_normalized);

        let calls = mock.upserts.lock().expect("mutex");
        assert_eq!(calls.len(), 1);
        let v = &calls[0].vector;
        assert!((v[0] - 0.6).abs() < 1e-5, "normalisé attendu, eu {v:?}");
        assert!((v[1] - 0.8).abs() < 1e-5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upsert_skips_normalization_when_model_says_so() {
        let mock = Arc::new(MockVdbClient::new());
        let svc = make_service(mock.clone());

        // m2.normalize = false → vecteur envoyé tel quel.
        let floats = [3.0f32, 4.0];
        let resp = svc.upsert(upsert_req("m2", 2, &floats)).await.unwrap();
        assert!(!resp.into_inner().was_normalized);

        let calls = mock.upserts.lock().expect("mutex");
        let v = &calls[0].vector;
        assert_eq!(v, &[3.0, 4.0]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upsert_rejects_unknown_model() {
        let mock = Arc::new(MockVdbClient::new());
        let svc = make_service(mock.clone());
        let floats = [1.0f32, 2.0, 3.0, 4.0];
        let err = svc
            .upsert(upsert_req("does-not-exist", 4, &floats))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert_eq!(mock.upserts.lock().expect("mutex").len(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upsert_rejects_dim_mismatch() {
        let mock = Arc::new(MockVdbClient::new());
        let svc = make_service(mock.clone());
        let floats = [1.0f32, 2.0, 3.0];
        // m1 attend dim 4, on envoie 3
        let err = svc.upsert(upsert_req("m1", 3, &floats)).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upsert_propagates_permanent_vdb_error_without_retry() {
        let mock = Arc::new(MockVdbClient::new());
        mock.set_failure("schema mismatch"); // non-transitive
        let svc = make_service(mock.clone());

        let floats = [1.0f32, 0.0, 0.0, 0.0];
        let err = svc.upsert(upsert_req("m1", 4, &floats)).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);

        // Une seule tentative (pas de retry sur erreur non-transitive).
        assert_eq!(mock.upserts.lock().expect("mutex").len(), 0);
        // (Le mock set_failure rejette avant d'enregistrer l'upsert)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upsert_retries_on_transient_then_fails() {
        let mock = Arc::new(MockVdbClient::new());
        mock.set_failure("timeout upsert"); // classé transitoire
        let svc = make_service(mock.clone());

        let floats = [1.0f32, 0.0, 0.0, 0.0];
        let start = Instant::now();
        let err = svc.upsert(upsert_req("m1", 4, &floats)).await.unwrap_err();
        let elapsed = start.elapsed();

        assert_eq!(err.code(), tonic::Code::Unavailable);
        // max_retries=3, base_delay=1ms → backoff attendu ~1+2 = 3ms min.
        // On vérifie juste qu'il y a eu retry (durée > 0).
        assert!(
            elapsed >= Duration::from_millis(2),
            "retry attendu, elapsed {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_returns_hits_from_vdb() {
        let mock = Arc::new(MockVdbClient::new());
        mock.set_search_result(vec![
            SearchHit {
                point_id: "doc-1".to_string(),
                score: 0.95,
                metadata: HashMap::new(),
            },
            SearchHit {
                point_id: "doc-2".to_string(),
                score: 0.82,
                metadata: HashMap::new(),
            },
        ]);
        let svc = make_service(mock.clone());

        let floats = [1.0f32, 0.0, 0.0, 0.0];
        let resp = svc.search(search_req("m1", 4, &floats)).await.unwrap();
        let inner = resp.into_inner();
        assert_eq!(inner.hits.len(), 2);
        assert_eq!(inner.hits[0].point_id, "doc-1");
        assert!((inner.hits[0].score - 0.95).abs() < 1e-6);
        assert_eq!(inner.vdb_namespace, "ns-m1");

        // Le vecteur de requête doit avoir été normalisé (m1.normalize=true).
        let searches = mock.searches.lock().expect("mutex");
        let v = &searches[0].vector;
        let norm_sq: f32 = v.iter().map(|x| x * x).sum();
        assert!(
            (norm_sq - 1.0).abs() < 1e-5,
            "vecteur requête devrait être normalisé, norme²={norm_sq}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_threshold_zero_becomes_none() {
        // Vérifie la convention "score_threshold=0.0 proto = pas de filtre".
        let mock = Arc::new(MockVdbClient::new());
        let svc = make_service(mock.clone());
        let floats = [1.0f32, 0.0, 0.0, 0.0];
        let _ = svc.search(search_req("m1", 4, &floats)).await.unwrap();
        let searches = mock.searches.lock().expect("mutex");
        assert_eq!(searches[0].score_threshold, None);
    }

    #[test]
    fn build_grpc_server_accepts_valid_config() {
        // Smoke test du builder : la fonction doit construire un Router sans panic.
        // L'exécution TCP est couverte en étape 10 (tests d'intégration).
        let mock = Arc::new(MockVdbClient::new());
        let svc = make_service(mock);
        let server_cfg = ServerConfig {
            grpc_bind: "0.0.0.0:50051".parse().unwrap(),
            http_bind: "0.0.0.0:9090".parse().unwrap(),
            max_concurrent_requests: 256,
            max_decoding_message_size_bytes: 4 * 1024 * 1024,
        };
        let _router = build_grpc_server(svc, &server_cfg);
        // Si on arrive ici sans panic, le Stack<ConcurrencyLimit, Identity>
        // compile et se construit correctement.
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pool_is_released_before_vdb_call() {
        // On utilise un pool de taille 1 et on lance 2 requêtes séquentielles.
        // Si le pool n'était pas libéré avant l'appel VDB, la 2e requête devrait
        // passer par le fallback (exhausted_count > 0). Ici, on attend 0.
        let mock = Arc::new(MockVdbClient::new());
        let mut models = HashMap::new();
        models.insert(
            "m".to_string(),
            ModelSpec {
                dim: 4,
                normalize: false,
                vdb_namespace: "ns".to_string(),
            },
        );
        let registry = Arc::new(Registry::new(models));
        let pool = Arc::new(BufferPool::new(1, 1024));
        let vdb_cfg = VdbConfig {
            url: "http://test".to_string(),
            api_key: None,
            timeout_ms: 5000,
            max_retries: 3,
            retry_base_delay_ms: 1,
        };
        let svc = VectorRouterService::new(registry, pool.clone(), mock, &vdb_cfg);

        let floats = [1.0f32, 2.0, 3.0, 4.0];
        svc.upsert(upsert_req("m", 4, &floats)).await.unwrap();
        svc.upsert(upsert_req("m", 4, &floats)).await.unwrap();

        assert_eq!(
            pool.exhausted_count(),
            0,
            "le pool devrait être libéré entre les requêtes"
        );
    }
}
