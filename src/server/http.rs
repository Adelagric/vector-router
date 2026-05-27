//! Serveur HTTP secondaire pour l'observabilité SRE.
//!
//! Endpoints :
//! - `GET /health` : liveness. Retourne 200 OK dès que le service répond.
//! - `GET /ready`  : readiness. Retourne 200 si le VDB est joignable via
//!   `VectorDbClient::health()`, 503 sinon. Utilisé par Kubernetes /
//!   load balancer pour router le trafic.
//! - `GET /metrics` : format Prometheus, rendu par le handle global.
//! - `GET /dashboard` : page HTML statique (embarquée via `include_str!`)
//!   qui agrège métriques temps réel + preuves techniques (tests, benches)
//!   pour les démonstrations locales. Ne remplace PAS Grafana en production.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use metrics_exporter_prometheus::PrometheusHandle;

use crate::client::VectorDbClient;

/// HTML statique embarqué au build via `include_str!`. Évite toute
/// dépendance runtime sur un fichier externe, et garde l'image Docker
/// auto-suffisante.
const DASHBOARD_HTML: &str = include_str!("../../static/dashboard.html");

#[derive(Clone)]
struct AppState {
    vdb: Arc<dyn VectorDbClient>,
    metrics: PrometheusHandle,
}

/// Construit le router HTTP de supervision.
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

    /// Retourne un handle Prometheus isolé pour les tests. Utilise
    /// `build_recorder` (pas `install_recorder`) pour éviter le conflit
    /// d'installation globale entre tests.
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
            "content-type HTML attendu, eu {ct}"
        );

        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();

        // Vérifie la structure de la page : les trois sections sont bien présentes.
        assert!(body.contains("Vector Router"), "titre manquant");
        assert!(body.contains("État du service"), "section live manquante");
        assert!(
            body.contains("Qualité du code"),
            "section preuves manquante"
        );
        assert!(
            body.contains("Performance mesurée"),
            "section bench manquante"
        );
        // Le JS doit pointer vers /metrics pour le live refresh.
        assert!(
            body.contains("fetch(\"/metrics\""),
            "fetch metrics manquant"
        );
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
            "content-type Prometheus attendu, eu {ct}"
        );
    }
}
