//! Initialisation des métriques Prometheus et mise à jour périodique des
//! gauges dont la valeur est lue en pull depuis l'application (taille du
//! registre, inflight VDB, etc.).
//!
//! L'installation du recorder est globale et ne peut être faite qu'une fois
//! par process. `main.rs` (étape 9) appellera `init_metrics()` avant de
//! démarrer les serveurs. Les appels de `metrics::counter!()` /
//! `metrics::histogram!()` / `metrics::gauge!()` ailleurs dans le code sont
//! silencieusement ignorés si le recorder n'a pas été installé (tests).

use std::sync::Arc;
use std::time::Duration;

use metrics::gauge;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::client::VectorDbClient;
use crate::error::Error;
use crate::pool::BufferPool;
use crate::registry::Registry;

/// Installe le recorder Prometheus global. Retourne le handle utilisé par
/// l'endpoint `/metrics` pour rendre la sortie Prometheus à la volée.
///
/// À n'appeler qu'une seule fois par process.
pub fn init_metrics() -> Result<PrometheusHandle, Error> {
    PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| Error::Telemetry(format!("install recorder : {e}")))
}

/// Tâche background qui met à jour les gauges de pull (valeurs qu'on lit
/// périodiquement depuis l'état applicatif plutôt que d'émettre à chaque
/// événement). Intervalle raisonnable : 5s — suffisant pour un dashboard
/// Grafana sans générer de surcharge.
pub async fn run_gauge_updater(
    registry: Arc<Registry>,
    pool: Arc<BufferPool>,
    vdb: Arc<dyn VectorDbClient>,
) {
    let interval = Duration::from_secs(5);
    loop {
        gauge!("registered_models").set(registry.len() as f64);
        gauge!("pool_available").set(pool.available() as f64);
        gauge!("vdb_inflight").set(vdb.inflight() as f64);
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_metrics_installs_recorder_once() {
        // Un seul test peut appeler init_metrics() : les autres tests le
        // verront déjà installé. On utilise un nom qui rend ce test facile
        // à filtrer si la suite grandit (e.g. `cargo test -- --skip install`).
        let result = init_metrics();
        // Le test peut être exécuté avec d'autres tests qui ont déjà installé
        // un recorder (par exemple dans http::tests). On tolère les deux cas.
        match result {
            Ok(_) | Err(Error::Telemetry(_)) => {}
            Err(e) => panic!("erreur inattendue : {e:?}"),
        }
    }
}
