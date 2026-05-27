//! Secondary HTTP server for SRE observability.
//!
//! Endpoints:
//! - `GET /health`: liveness. Returns 200 OK as soon as the service responds.
//! - `GET /ready`: readiness. Returns 200 if the VDB is reachable via
//!   `VectorDbClient::health()`, 503 otherwise. Used by Kubernetes /
//!   load balancer to route traffic.
//! - `GET /metrics`: Prometheus format, rendered by the global handle.
//! - `GET /dashboard`: static HTML page (embedded via `include_str!`)
//!   that aggregates real-time metrics + technical evidence (tests,
//!   benches) for local demos. Does NOT replace Grafana in production.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use metrics_exporter_prometheus::PrometheusHandle;

use crate::client::VectorDbClient;

/// Static HTML embedded at build time via `include_str!`. Avoids any
/// runtime dependency on an external file and keeps the Docker image
/// self-contained.
const DASHBOARD_HTML: &str = include_str!("../../static/dashboard.html");

#[derive(Clone)]
struct AppState {
    vdb: Arc<dyn VectorDbClient>,
    metrics: PrometheusHandle,
}

/// Builds the supervision HTTP router.
pub fn build_http_router(vdb: Arc<dyn VectorDbClient>, metrics: PrometheusHandle) -> Router {
    let state = AppState { vdb, metrics };
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics_handler))
        .route("/dashboard", get(dashboard))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    match state.vdb.health().await {
        Ok(()) => (StatusCode::OK, "ready").into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("vdb indisponible : {e}"),
        )
            .into_response(),
    }
}

async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

async fn dashboard() -> impl IntoResponse {
    Html(DASHBOARD_HTML)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::mock::MockVdbClient;
    use axum::body::Body;
    use axum::http::Request;
    use metrics_exporter_prometheus::PrometheusBuilder;
    use tower::ServiceExt;

    /// Returns an isolated Prometheus handle for tests. Uses
    /// `build_recorder` (not `install_recorder`) to avoid global
    /// installation conflicts across tests.
    fn test_metrics_handle() -> PrometheusHandle {
        PrometheusBuilder::new().build_recorder().handle()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_returns_200() {
        let vdb = Arc::new(MockVdbClient::new());
        let app = build_http_router(vdb, test_metrics_handle());

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ready_returns_200_when_vdb_healthy() {
        let vdb = Arc::new(MockVdbClient::new());
        let app = build_http_router(vdb, test_metrics_handle());

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ready_returns_503_when_vdb_down() {
        let vdb = Arc::new(MockVdbClient::new());
        vdb.set_failure("timeout health");
        let app = build_http_router(vdb, test_metrics_handle());

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dashboard_returns_html_with_expected_sections() {
        let vdb = Arc::new(MockVdbClient::new());
        let app = build_http_router(vdb, test_metrics_handle());

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("text/html"),
            "expected HTML content-type, got {ct}"
        );

        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();

        // Verify the page structure: the three sections are all present.
        assert!(body.contains("Vector Router"), "title missing");
        assert!(body.contains("Service status"), "live section missing");
        assert!(body.contains("Code quality"), "evidence section missing");
        assert!(
            body.contains("Measured performance"),
            "bench section missing"
        );
        // The JS must point to /metrics for live refresh.
        assert!(body.contains("fetch(\"/metrics\""), "fetch metrics missing");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn metrics_endpoint_returns_prometheus_format() {
        let vdb = Arc::new(MockVdbClient::new());
        let app = build_http_router(vdb, test_metrics_handle());

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("text/plain"),
            "expected Prometheus content-type, got {ct}"
        );
    }
}
