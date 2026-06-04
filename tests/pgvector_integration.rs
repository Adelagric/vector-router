//! Live integration tests for the pgvector backend.
//!
//! The database-backed tests run only when `VR_TEST_PG_URL` points at a
//! reachable Postgres + pgvector instance (CI provides one as a service
//! container — see `.github/workflows/ci.yml`). When the variable is unset
//! they print a notice and return, so `cargo test --features pgvector` stays
//! green on a machine without a database.
//!
//! The whole file is gated on the `pgvector` feature, so it compiles out of
//! the default build entirely.
#![cfg(feature = "pgvector")]

use std::collections::HashMap;

use vector_router::client::{PgVectorClient, SearchParams, UpsertParams, VectorDbClient};
use vector_router::config::{ModelSpec, VdbBackend, VdbConfig};

/// Returns the live test database URL, or `None` if the suite should skip.
fn pg_url() -> Option<String> {
    std::env::var("VR_TEST_PG_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Skips the test (early return) when no live database is configured.
macro_rules! pg_url_or_skip {
    () => {
        match pg_url() {
            Some(url) => url,
            None => {
                eprintln!("skipping live pgvector test: VR_TEST_PG_URL not set");
                return;
            }
        }
    };
}

fn cfg(url: &str, ef_search: Option<u32>) -> VdbConfig {
    VdbConfig {
        backend: VdbBackend::Pgvector,
        url: url.to_string(),
        api_key: None,
        timeout_ms: 5000,
        max_retries: 1,
        retry_base_delay_ms: 1,
        ef_search,
        max_connections: Some(4),
    }
}

/// A one-model registry mapping the lone model to `namespace` with `dim`.
fn one_model(namespace: &str, dim: usize) -> HashMap<String, ModelSpec> {
    let mut models = HashMap::new();
    models.insert(
        "m".to_string(),
        ModelSpec {
            dim,
            normalize: true,
            vdb_namespace: namespace.to_string(),
        },
    );
    models
}

fn meta(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn upsert(
    namespace: &str,
    point_id: &str,
    vector: Vec<f32>,
    metadata: &[(&str, &str)],
) -> UpsertParams {
    UpsertParams {
        namespace: namespace.to_string(),
        point_id: point_id.to_string(),
        vector,
        metadata: meta(metadata),
    }
}

fn search(namespace: &str, vector: Vec<f32>, limit: u32) -> SearchParams {
    SearchParams {
        namespace: namespace.to_string(),
        vector,
        limit,
        score_threshold: None,
        metadata_filter: HashMap::new(),
    }
}

// --- Failure path: no live DB required --------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn connect_to_unreachable_host_errors() {
    // Port 1 refuses connections: connect() must fail rather than hang.
    // Short acquire timeout so the failure is fast.
    let mut conf = cfg("postgres://postgres:postgres@127.0.0.1:1/postgres", None);
    conf.timeout_ms = 800;
    let res = PgVectorClient::connect(&conf, &one_model("vr_it_unreachable", 3)).await;
    assert!(res.is_err(), "connect to refused port must error");
}

// --- Live tests (require VR_TEST_PG_URL) ------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ensure_schema_is_idempotent() {
    let url = pg_url_or_skip!();
    let models = one_model("vr_it_idempotent", 4);
    // Two consecutive connects must both succeed (CREATE ... IF NOT EXISTS).
    let _c1 = PgVectorClient::connect(&cfg(&url, None), &models)
        .await
        .expect("first connect provisions schema");
    let _c2 = PgVectorClient::connect(&cfg(&url, None), &models)
        .await
        .expect("second connect is a no-op");
}

#[tokio::test(flavor = "multi_thread")]
async fn health_ok_against_live_db() {
    let url = pg_url_or_skip!();
    let client = PgVectorClient::connect(&cfg(&url, None), &one_model("vr_it_health", 3))
        .await
        .expect("connect");
    client.health().await.expect("health ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn upsert_then_search_round_trip() {
    let url = pg_url_or_skip!();
    let ns = "vr_it_roundtrip";
    let client = PgVectorClient::connect(&cfg(&url, None), &one_model(ns, 3))
        .await
        .expect("connect");

    // Unit vectors: inner product == cosine, so the returned score is the
    // cosine similarity.
    client
        .upsert(upsert(ns, "a", vec![1.0, 0.0, 0.0], &[("tenant", "acme")]))
        .await
        .expect("upsert a");
    client
        .upsert(upsert(
            ns,
            "b",
            vec![0.0, 1.0, 0.0],
            &[("tenant", "globex")],
        ))
        .await
        .expect("upsert b");

    let hits = client
        .search(search(ns, vec![1.0, 0.0, 0.0], 10))
        .await
        .expect("search");

    assert!(hits.len() >= 2, "both points should be returned");
    assert_eq!(hits[0].point_id, "a", "closest point first");
    assert!(
        (hits[0].score - 1.0).abs() < 1e-4,
        "self inner product should be ~1.0, got {}",
        hits[0].score
    );
    assert_eq!(hits[0].metadata.get("tenant"), Some(&"acme".to_string()));
    let b = hits.iter().find(|h| h.point_id == "b").expect("b present");
    assert!(b.score.abs() < 1e-4, "orthogonal score ~0, got {}", b.score);
}

#[tokio::test(flavor = "multi_thread")]
async fn upsert_is_idempotent_on_point_id() {
    let url = pg_url_or_skip!();
    let ns = "vr_it_upsert_idem";
    let client = PgVectorClient::connect(&cfg(&url, None), &one_model(ns, 3))
        .await
        .expect("connect");

    client
        .upsert(upsert(ns, "dup", vec![1.0, 0.0, 0.0], &[("v", "1")]))
        .await
        .expect("first upsert");
    // Second upsert of the same point_id replaces the row (ON CONFLICT).
    client
        .upsert(upsert(ns, "dup", vec![0.0, 1.0, 0.0], &[("v", "2")]))
        .await
        .expect("second upsert overwrites");

    let hits = client
        .search(search(ns, vec![0.0, 1.0, 0.0], 10))
        .await
        .expect("search");
    let dup = hits
        .iter()
        .find(|h| h.point_id == "dup")
        .expect("dup found");
    assert_eq!(
        dup.metadata.get("v"),
        Some(&"2".to_string()),
        "metadata should reflect the latest upsert"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_applies_metadata_filter() {
    let url = pg_url_or_skip!();
    let ns = "vr_it_filter";
    let client = PgVectorClient::connect(&cfg(&url, None), &one_model(ns, 3))
        .await
        .expect("connect");

    client
        .upsert(upsert(
            ns,
            "acme-1",
            vec![1.0, 0.0, 0.0],
            &[("tenant", "acme")],
        ))
        .await
        .expect("upsert acme");
    client
        .upsert(upsert(
            ns,
            "globex-1",
            vec![1.0, 0.0, 0.0],
            &[("tenant", "globex")],
        ))
        .await
        .expect("upsert globex");

    let mut params = search(ns, vec![1.0, 0.0, 0.0], 10);
    params.metadata_filter = meta(&[("tenant", "acme")]);
    let hits = client.search(params).await.expect("filtered search");

    assert!(
        hits.iter()
            .all(|h| h.metadata.get("tenant") == Some(&"acme".to_string())),
        "filter must exclude other tenants"
    );
    assert!(hits.iter().any(|h| h.point_id == "acme-1"));
    assert!(!hits.iter().any(|h| h.point_id == "globex-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn search_applies_score_threshold() {
    let url = pg_url_or_skip!();
    let ns = "vr_it_threshold";
    let client = PgVectorClient::connect(&cfg(&url, None), &one_model(ns, 3))
        .await
        .expect("connect");

    client
        .upsert(upsert(ns, "near", vec![1.0, 0.0, 0.0], &[]))
        .await
        .expect("upsert near");
    client
        .upsert(upsert(ns, "far", vec![0.0, 0.0, 1.0], &[]))
        .await
        .expect("upsert far");

    let mut params = search(ns, vec![1.0, 0.0, 0.0], 10);
    params.score_threshold = Some(0.5);
    let hits = client.search(params).await.expect("threshold search");

    assert!(
        hits.iter().any(|h| h.point_id == "near"),
        "near passes threshold"
    );
    assert!(
        !hits.iter().any(|h| h.point_id == "far"),
        "orthogonal point (score ~0) must be filtered out by threshold 0.5"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_with_ef_search_set() {
    let url = pg_url_or_skip!();
    let ns = "vr_it_ef";
    // ef_search forces the transaction + SET LOCAL path.
    let client = PgVectorClient::connect(&cfg(&url, Some(40)), &one_model(ns, 3))
        .await
        .expect("connect");

    client
        .upsert(upsert(ns, "x", vec![1.0, 0.0, 0.0], &[]))
        .await
        .expect("upsert");
    let hits = client
        .search(search(ns, vec![1.0, 0.0, 0.0], 5))
        .await
        .expect("search with ef_search");
    assert!(hits.iter().any(|h| h.point_id == "x"));
}

#[tokio::test(flavor = "multi_thread")]
async fn upsert_rejects_dimension_mismatch_at_table_level() {
    let url = pg_url_or_skip!();
    let ns = "vr_it_dim";
    // Table is vector(3); the router normally guards dim, but the database
    // is the last line of defense.
    let client = PgVectorClient::connect(&cfg(&url, None), &one_model(ns, 3))
        .await
        .expect("connect");

    let err = client
        .upsert(upsert(ns, "wrong", vec![1.0, 0.0, 0.0, 0.0], &[]))
        .await
        .expect_err("4-dim vector into a vector(3) column must fail");
    assert!(
        matches!(err, vector_router::error::Error::Vdb(_)),
        "expected a Vdb error, got {err:?}"
    );
}
