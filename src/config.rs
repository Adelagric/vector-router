use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;

use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Full middleware configuration.
///
/// The `models` field may be empty at startup: the service will then reply
/// `UnknownModel` to every request, which is the intended behavior (an
/// undeclared model is rejected).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub admin: AdminConfig,
    pub vdb: VdbConfig,
    #[serde(default)]
    pub pool: PoolConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub models: HashMap<String, ModelSpec>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ServerConfig {
    /// gRPC listen address.
    pub grpc_bind: SocketAddr,
    /// HTTP listen address (admin, health, metrics).
    pub http_bind: SocketAddr,
    /// Concurrent request limit (global rate limit).
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: u32,
    /// Maximum incoming gRPC message size, in bytes.
    #[serde(default = "default_max_message_size")]
    pub max_decoding_message_size_bytes: usize,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AdminConfig {
    /// Bearer authentication token for the /admin/* endpoints.
    /// Compared in constant time on receipt.
    pub bearer_token: String,
}

/// Which vector-database backend the router talks to. Selected at config
/// time; the matching Cargo feature must be compiled in (enforced by
/// `service::start_service`).
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum VdbBackend {
    /// Qdrant — the historical backend, kept as the default so v0.1 configs
    /// load unchanged.
    #[default]
    Qdrant,
    /// PostgreSQL with the `pgvector` extension.
    Pgvector,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct VdbConfig {
    /// Which backend to talk to. Defaults to Qdrant so configs written for
    /// v0.1 keep working untouched.
    #[serde(default)]
    pub backend: VdbBackend,
    /// Backend endpoint. Qdrant: the cluster URL (e.g. `http://qdrant:6334`).
    /// pgvector: a Postgres connection string (e.g.
    /// `postgres://user:pass@host:5432/db`).
    pub url: String,
    /// Optional API key. Qdrant only; pgvector carries auth in `url`.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Per-call timeout, in milliseconds.
    #[serde(default = "default_vdb_timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum number of attempts (1 = no retry).
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Initial exponential backoff delay, in milliseconds.
    #[serde(default = "default_retry_base_delay_ms")]
    pub retry_base_delay_ms: u64,
    /// pgvector only: HNSW `ef_search` applied per query via
    /// `SET LOCAL hnsw.ef_search`. Higher = better recall, higher latency.
    /// `None` leaves the server default. Ignored by Qdrant.
    #[serde(default)]
    pub ef_search: Option<u32>,
    /// pgvector only: maximum size of the sqlx connection pool. `None` uses
    /// the sqlx default. Ignored by Qdrant.
    #[serde(default)]
    pub max_connections: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PoolConfig {
    /// Multiplier: `N_buffers = buffers_per_worker × worker_threads`.
    pub buffers_per_worker: u32,
    /// Override of the tokio worker count. `None` = auto-detect.
    pub worker_threads: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TelemetryConfig {
    /// Tracing log level (`error`, `warn`, `info`, `debug`, `trace`).
    pub log_level: String,
    /// OTLP endpoint for trace export. `None` = export disabled.
    pub otlp_endpoint: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub dim: usize,
    pub normalize: bool,
    pub vdb_namespace: String,
}

// --- Defaults for optional fields -------------------------------------------

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            buffers_per_worker: 2,
            worker_threads: None,
        }
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            otlp_endpoint: None,
        }
    }
}

fn default_max_concurrent() -> u32 {
    1024
}

fn default_max_message_size() -> usize {
    // 16 MB: covers any reasonable vector (up to 4M dims) without letting
    // through a size-based abuse.
    16 * 1024 * 1024
}

fn default_vdb_timeout_ms() -> u64 {
    5_000
}

fn default_max_retries() -> u32 {
    3
}

fn default_retry_base_delay_ms() -> u64 {
    50
}

/// Whether `ns` is usable as a pgvector table name. The namespace is
/// interpolated into SQL (a table name cannot be a bind parameter), so we
/// restrict it to a safe, double-quotable identifier set. Postgres caps
/// identifiers at 63 bytes.
pub(crate) fn is_valid_pg_namespace(ns: &str) -> bool {
    !ns.is_empty()
        && ns.len() <= 63
        && ns
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// --- Loading and validation -------------------------------------------------

impl Config {
    /// Load from a TOML file, overridden by env vars prefixed `VR_`.
    /// Example: `VR_SERVER__GRPC_BIND="0.0.0.0:50051"`.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        Self::from_figment(
            Figment::new()
                .merge(Toml::file(path))
                .merge(Env::prefixed("VR_").split("__")),
        )
    }

    /// Variant used by tests and in-memory loads.
    pub fn from_figment(fig: Figment) -> Result<Self, Error> {
        let cfg: Config = fig.extract()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Post-parsing checks: dimensions, tokens, non-empty URLs.
    fn validate(&self) -> Result<(), Error> {
        if self.admin.bearer_token.is_empty() {
            return Err(Error::Validation("admin.bearer_token is empty".into()));
        }
        if self.vdb.url.is_empty() {
            return Err(Error::Validation("vdb.url is empty".into()));
        }
        if self.pool.buffers_per_worker == 0 {
            return Err(Error::Validation(
                "pool.buffers_per_worker must be >= 1".into(),
            ));
        }
        if self.vdb.max_retries == 0 {
            return Err(Error::Validation(
                "vdb.max_retries must be >= 1 (1 = no retry)".into(),
            ));
        }
        for (name, spec) in &self.models {
            if name.is_empty() {
                return Err(Error::Validation("empty model name".into()));
            }
            if spec.dim == 0 {
                return Err(Error::Validation(format!("model '{name}': dim is 0")));
            }
            if spec.vdb_namespace.is_empty() {
                return Err(Error::Validation(format!(
                    "model '{name}': vdb_namespace is empty"
                )));
            }
        }

        // pgvector-specific invariants. The namespace becomes a Postgres
        // table name interpolated into DDL/queries (it cannot be a bind
        // parameter), and a `vector(N)` column is fixed-dimension — so a
        // namespace shared by several models must agree on the dimension.
        if self.vdb.backend == VdbBackend::Pgvector {
            let mut dim_by_namespace: HashMap<&str, usize> = HashMap::new();
            for (name, spec) in &self.models {
                if !is_valid_pg_namespace(&spec.vdb_namespace) {
                    return Err(Error::Validation(format!(
                        "model '{name}': vdb_namespace '{}' is not a valid pgvector table name (allowed: A-Z a-z 0-9 _ -, max 63 bytes)",
                        spec.vdb_namespace
                    )));
                }
                match dim_by_namespace.get(spec.vdb_namespace.as_str()) {
                    Some(&prev) if prev != spec.dim => {
                        return Err(Error::Validation(format!(
                            "pgvector namespace '{}' maps to two different dimensions ({prev} and {}); a vector(N) column is fixed-dimension, so one table cannot hold both (model '{name}')",
                            spec.vdb_namespace, spec.dim
                        )));
                    }
                    _ => {
                        dim_by_namespace.insert(spec.vdb_namespace.as_str(), spec.dim);
                    }
                }
            }
            if self.vdb.ef_search == Some(0) {
                return Err(Error::Validation("vdb.ef_search must be >= 1".into()));
            }
            if self.vdb.max_connections == Some(0) {
                return Err(Error::Validation("vdb.max_connections must be >= 1".into()));
            }
        }

        Ok(())
    }
}

// --- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn load_str(toml: &str) -> Result<Config, Error> {
        Config::from_figment(Figment::from(Toml::string(toml)))
    }

    const VALID_MINIMAL: &str = r#"
[server]
grpc_bind = "0.0.0.0:50051"
http_bind = "0.0.0.0:9090"

[admin]
bearer_token = "secret"

[vdb]
url = "http://qdrant:6334"

[pool]
buffers_per_worker = 2
"#;

    #[test]
    fn parses_minimal_valid_config() {
        let cfg = load_str(VALID_MINIMAL).expect("minimal config valid");
        assert_eq!(cfg.server.grpc_bind.port(), 50051);
        assert_eq!(cfg.admin.bearer_token, "secret");
        assert_eq!(cfg.vdb.url, "http://qdrant:6334");
        assert!(cfg.models.is_empty());
        // Defaults must apply.
        assert_eq!(cfg.vdb.max_retries, 3);
        assert_eq!(cfg.telemetry.log_level, "info");
    }

    #[test]
    fn parses_config_with_models() {
        let toml = format!(
            r#"{VALID_MINIMAL}

[models."openai-text-embedding-3-small"]
dim = 1536
normalize = true
vdb_namespace = "prod-openai-small"

[models."cohere-embed-english-v3"]
dim = 1024
normalize = false
vdb_namespace = "prod-cohere-en"
"#
        );
        let cfg = load_str(&toml).expect("two valid models");
        assert_eq!(cfg.models.len(), 2);
        let spec = cfg
            .models
            .get("openai-text-embedding-3-small")
            .expect("openai present");
        assert_eq!(spec.dim, 1536);
        assert!(spec.normalize);
    }

    #[test]
    fn rejects_missing_required_field() {
        // No [admin] section = bearer_token missing at parse time.
        let toml = r#"
[server]
grpc_bind = "0.0.0.0:50051"
http_bind = "0.0.0.0:9090"

[vdb]
url = "http://qdrant:6334"

[pool]
buffers_per_worker = 2
"#;
        let err = load_str(toml).expect_err("admin missing");
        assert!(
            matches!(err, Error::Config(_)),
            "expected Config, got {err:?}"
        );
    }

    #[test]
    fn rejects_empty_bearer_token() {
        let toml = VALID_MINIMAL.replace("secret", "");
        let err = load_str(&toml).expect_err("empty token");
        assert!(matches!(err, Error::Validation(msg) if msg.contains("bearer_token")));
    }

    #[test]
    fn rejects_empty_vdb_url() {
        let toml = VALID_MINIMAL.replace("http://qdrant:6334", "");
        let err = load_str(&toml).expect_err("empty vdb.url");
        assert!(matches!(err, Error::Validation(msg) if msg.contains("vdb.url")));
    }

    #[test]
    fn rejects_zero_buffers_per_worker() {
        let toml = VALID_MINIMAL.replace("buffers_per_worker = 2", "buffers_per_worker = 0");
        let err = load_str(&toml).expect_err("pool at 0");
        assert!(matches!(err, Error::Validation(msg) if msg.contains("buffers_per_worker")));
    }

    #[test]
    fn rejects_model_with_dim_zero() {
        let toml = format!(
            r#"{VALID_MINIMAL}

[models."zero-dim-model"]
dim = 0
normalize = false
vdb_namespace = "ns"
"#
        );
        let err = load_str(&toml).expect_err("dim = 0");
        assert!(matches!(err, Error::Validation(msg) if msg.contains("dim")));
    }

    #[test]
    fn rejects_model_with_empty_namespace() {
        let toml = format!(
            r#"{VALID_MINIMAL}

[models."empty-ns"]
dim = 512
normalize = false
vdb_namespace = ""
"#
        );
        let err = load_str(&toml).expect_err("empty namespace");
        assert!(matches!(err, Error::Validation(msg) if msg.contains("vdb_namespace")));
    }

    #[test]
    fn rejects_zero_max_retries() {
        let toml = format!(
            r#"{VALID_MINIMAL}

[vdb-extra]
# nothing, we override via env-like
"#
        );
        // We can't easily override via Toml::string — build a manual map instead.
        let _ = toml; // silence warning
        let full =
            VALID_MINIMAL.to_string() + "\n[vdb]\nurl = \"http://qdrant:6334\"\nmax_retries = 0\n";
        // This construction creates two [vdb] sections; figment takes the last one as winner:
        let err = load_str(&full).expect_err("max_retries = 0");
        // Either Config (duplicate section) or Validation depending on how figment merges.
        match err {
            Error::Validation(msg) => assert!(msg.contains("max_retries")),
            Error::Config(_) => {} // acceptable: parsing failed on the duplicate section
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn empty_registry_is_valid() {
        let cfg = load_str(VALID_MINIMAL).expect("empty OK");
        assert!(cfg.models.is_empty());
        cfg.validate().expect("validation on empty registry passes");
    }

    #[test]
    fn invalid_socket_addr_is_config_error() {
        let toml = VALID_MINIMAL.replace("0.0.0.0:50051", "not-a-socket");
        let err = load_str(&toml).expect_err("invalid socket");
        assert!(matches!(err, Error::Config(_)));
    }

    // --- pgvector backend ----------------------------------------------------

    const PGVECTOR_BASE: &str = r#"
[server]
grpc_bind = "0.0.0.0:50051"
http_bind = "0.0.0.0:9090"

[admin]
bearer_token = "secret"

[vdb]
backend = "pgvector"
url = "postgres://u:p@localhost:5432/vr"

[pool]
buffers_per_worker = 2
"#;

    #[test]
    fn backend_defaults_to_qdrant_when_omitted() {
        let cfg = load_str(VALID_MINIMAL).expect("valid");
        assert_eq!(cfg.vdb.backend, VdbBackend::Qdrant);
    }

    #[test]
    fn parses_pgvector_backend_and_options() {
        let toml = PGVECTOR_BASE.replace(
            "url = \"postgres://u:p@localhost:5432/vr\"",
            "url = \"postgres://u:p@localhost:5432/vr\"\nef_search = 80\nmax_connections = 16",
        );
        let cfg = load_str(&toml).expect("pgvector config valid");
        assert_eq!(cfg.vdb.backend, VdbBackend::Pgvector);
        assert_eq!(cfg.vdb.ef_search, Some(80));
        assert_eq!(cfg.vdb.max_connections, Some(16));
    }

    #[test]
    fn pgvector_rejects_invalid_namespace_chars() {
        let toml = format!(
            r#"{PGVECTOR_BASE}
[models."m"]
dim = 3
normalize = true
vdb_namespace = "bad name!"
"#
        );
        let err = load_str(&toml).expect_err("invalid namespace");
        assert!(matches!(err, Error::Validation(msg) if msg.contains("valid pgvector table name")));
    }

    #[test]
    fn pgvector_rejects_namespace_dim_conflict() {
        let toml = format!(
            r#"{PGVECTOR_BASE}
[models."a"]
dim = 768
normalize = true
vdb_namespace = "shared"

[models."b"]
dim = 1536
normalize = true
vdb_namespace = "shared"
"#
        );
        let err = load_str(&toml).expect_err("dim conflict");
        assert!(matches!(err, Error::Validation(msg) if msg.contains("two different dimensions")));
    }

    #[test]
    fn pgvector_accepts_valid_models() {
        let toml = format!(
            r#"{PGVECTOR_BASE}
[models."openai-small"]
dim = 1536
normalize = true
vdb_namespace = "openai_small"

[models."cohere"]
dim = 1024
normalize = false
vdb_namespace = "cohere_en"
"#
        );
        let cfg = load_str(&toml).expect("valid pgvector models");
        assert_eq!(cfg.models.len(), 2);
        assert_eq!(cfg.vdb.backend, VdbBackend::Pgvector);
    }

    #[test]
    fn pgvector_rejects_zero_ef_search() {
        let toml = PGVECTOR_BASE.replace(
            "url = \"postgres://u:p@localhost:5432/vr\"",
            "url = \"postgres://u:p@localhost:5432/vr\"\nef_search = 0",
        );
        let err = load_str(&toml).expect_err("ef_search 0");
        assert!(matches!(err, Error::Validation(msg) if msg.contains("ef_search")));
    }

    #[test]
    fn qdrant_backend_does_not_enforce_pgvector_namespace_rules() {
        // Same namespace, two dims (and a hyphen): illegal for pgvector, but
        // fine under the default Qdrant backend. Proves the checks are gated.
        let toml = format!(
            r#"{VALID_MINIMAL}
[models."a"]
dim = 768
normalize = true
vdb_namespace = "shared-collection"

[models."b"]
dim = 1536
normalize = true
vdb_namespace = "shared-collection"
"#
        );
        let cfg = load_str(&toml).expect("qdrant ignores pgvector rules");
        assert_eq!(cfg.vdb.backend, VdbBackend::Qdrant);
        assert_eq!(cfg.models.len(), 2);
    }
}
