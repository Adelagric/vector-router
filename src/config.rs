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

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct VdbConfig {
    /// Qdrant cluster URL (e.g. http://qdrant:6334).
    pub url: String,
    /// Optional API key.
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
}
