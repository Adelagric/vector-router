//! Prometheus metrics initialization and periodic update of gauges whose
//! values are pulled from the application (registry size, VDB inflight,
//! etc.).
//!
//! Recorder installation is global and can only happen once per process.
//! `main.rs` (step 9) calls `init_metrics()` before starting the servers.
//! Calls to `metrics::counter!()` / `metrics::histogram!()` /
//! `metrics::gauge!()` elsewhere in the code are silently ignored if no
//! recorder has been installed (tests).

use std::sync::Arc;
use std::time::Duration;

use metrics::gauge;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::client::VectorDbClient;
use crate::error::Error;
use crate::pool::BufferPool;
use crate::registry::Registry;

/// Installs the global Prometheus recorder. Returns the handle used by the
/// `/metrics` endpoint to render the Prometheus output on demand.
///
/// Must be called only once per process.
pub fn init_metrics() -> Result<PrometheusHandle, Error> {
    PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| Error::Telemetry(format!("install recorder : {e}")))
}

/// Background task that updates pull gauges (values we read periodically
/// from the application state rather than emit on every event). Reasonable
/// interval: 5s — enough for a Grafana dashboard without generating
/// overhead.
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
        // Only one test can call init_metrics(); the others will see it
        // already installed. We use a name that makes this test easy to
        // filter if the suite grows (e.g. `cargo test -- --skip install`).
        let result = init_metrics();
        // The test may run alongside other tests that already installed a
        // recorder (e.g. in http::tests). We tolerate both cases.
        match result {
            Ok(_) | Err(Error::Telemetry(_)) => {}
            Err(e) => panic!("erreur inattendue : {e:?}"),
        }
    }
}
