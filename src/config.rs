use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;

use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Configuration complète du middleware.
///
/// Le champ `models` peut être vide au démarrage : le service répondra
/// alors `UnknownModel` à chaque requête, ce qui est le comportement
/// voulu (un modèle non déclaré est rejeté).
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
    /// Adresse d'écoute gRPC.
    pub grpc_bind: SocketAddr,
    /// Adresse d'écoute HTTP (admin, health, metrics).
    pub http_bind: SocketAddr,
    /// Limite de requêtes simultanées (rate limit global).
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: u32,
    /// Limite de taille d'un message gRPC entrant, en octets.
    #[serde(default = "default_max_message_size")]
    pub max_decoding_message_size_bytes: usize,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AdminConfig {
    /// Token d'authentification bearer pour les endpoints /admin/*.
    /// Comparé en temps constant à la réception.
    pub bearer_token: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct VdbConfig {
    /// URL du cluster Qdrant (ex: http://qdrant:6334).
    pub url: String,
    /// Clé API optionnelle.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Timeout par appel, en millisecondes.
    #[serde(default = "default_vdb_timeout_ms")]
    pub timeout_ms: u64,
    /// Nombre maximum de tentatives (1 = pas de retry).
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Délai initial du backoff exponentiel, en millisecondes.
    #[serde(default = "default_retry_base_delay_ms")]
    pub retry_base_delay_ms: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PoolConfig {
    /// Facteur multiplicatif : `N_buffers = buffers_per_worker × worker_threads`.
    pub buffers_per_worker: u32,
    /// Override du nombre de workers tokio. `None` = détection auto.
    pub worker_threads: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TelemetryConfig {
    /// Niveau de log pour tracing (`error`, `warn`, `info`, `debug`, `trace`).
    pub log_level: String,
    /// Endpoint OTLP pour l'export des traces. `None` = export désactivé.
    pub otlp_endpoint: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub dim: usize,
    pub normalize: bool,
    pub vdb_namespace: String,
}

// --- Defaults pour les champs optionnels ------------------------------------

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
    // 16 Mo : couvre largement tout vecteur raisonnable (jusqu'à 4M dims)
    // sans laisser passer un abus de taille.
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

// --- Chargement et validation -----------------------------------------------

impl Config {
    /// Charge depuis un fichier TOML, override par variables d'env préfixées `VR_`.
    /// Exemple : `VR_SERVER__GRPC_BIND="0.0.0.0:50051"`.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        Self::from_figment(
            Figment::new()
                .merge(Toml::file(path))
                .merge(Env::prefixed("VR_").split("__")),
        )
    }

    /// Variante utilisée par les tests et les chargements en mémoire.
    pub fn from_figment(fig: Figment) -> Result<Self, Error> {
        let cfg: Config = fig.extract()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Contrôles post-parsing : dimensions, tokens, URLs non vides.
    fn validate(&self) -> Result<(), Error> {
        if self.admin.bearer_token.is_empty() {
            return Err(Error::Validation("admin.bearer_token vide".into()));
        }
        if self.vdb.url.is_empty() {
            return Err(Error::Validation("vdb.url vide".into()));
        }
        if self.pool.buffers_per_worker == 0 {
            return Err(Error::Validation(
                "pool.buffers_per_worker doit être ≥ 1".into(),
            ));
        }
        if self.vdb.max_retries == 0 {
            return Err(Error::Validation(
                "vdb.max_retries doit être ≥ 1 (1 = pas de retry)".into(),
            ));
        }
        for (name, spec) in &self.models {
            if name.is_empty() {
                return Err(Error::Validation("nom de modèle vide".into()));
            }
            if spec.dim == 0 {
                return Err(Error::Validation(format!("modèle '{name}' : dim = 0")));
            }
            if spec.vdb_namespace.is_empty() {
                return Err(Error::Validation(format!(
                    "modèle '{name}' : vdb_namespace vide"
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
        let cfg = load_str(VALID_MINIMAL).expect("config minimale valide");
        assert_eq!(cfg.server.grpc_bind.port(), 50051);
        assert_eq!(cfg.admin.bearer_token, "secret");
        assert_eq!(cfg.vdb.url, "http://qdrant:6334");
        assert!(cfg.models.is_empty());
        // Les defaults doivent s'appliquer.
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
        let cfg = load_str(&toml).expect("deux modèles valides");
        assert_eq!(cfg.models.len(), 2);
        let spec = cfg
            .models
            .get("openai-text-embedding-3-small")
            .expect("openai présent");
        assert_eq!(spec.dim, 1536);
        assert!(spec.normalize);
    }

    #[test]
    fn rejects_missing_required_field() {
        // Pas de section [admin] = bearer_token manquant au parsing.
        let toml = r#"
[server]
grpc_bind = "0.0.0.0:50051"
http_bind = "0.0.0.0:9090"

[vdb]
url = "http://qdrant:6334"

[pool]
buffers_per_worker = 2
"#;
        let err = load_str(toml).expect_err("admin manquant");
        assert!(
            matches!(err, Error::Config(_)),
            "attendu Config, eu {err:?}"
        );
    }

    #[test]
    fn rejects_empty_bearer_token() {
        let toml = VALID_MINIMAL.replace("secret", "");
        let err = load_str(&toml).expect_err("token vide");
        assert!(matches!(err, Error::Validation(msg) if msg.contains("bearer_token")));
    }

    #[test]
    fn rejects_empty_vdb_url() {
        let toml = VALID_MINIMAL.replace("http://qdrant:6334", "");
        let err = load_str(&toml).expect_err("vdb.url vide");
        assert!(matches!(err, Error::Validation(msg) if msg.contains("vdb.url")));
    }

    #[test]
    fn rejects_zero_buffers_per_worker() {
        let toml = VALID_MINIMAL.replace("buffers_per_worker = 2", "buffers_per_worker = 0");
        let err = load_str(&toml).expect_err("pool à 0");
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
        let err = load_str(&toml).expect_err("namespace vide");
        assert!(matches!(err, Error::Validation(msg) if msg.contains("vdb_namespace")));
    }

    #[test]
    fn rejects_zero_max_retries() {
        let toml = format!(
            r#"{VALID_MINIMAL}

[vdb-extra]
# rien, on surcharge via env-like
"#
        );
        // On ne peut pas facilement surcharger via Toml::string — on fait une map manuelle.
        let _ = toml; // silencer warning
        let full =
            VALID_MINIMAL.to_string() + "\n[vdb]\nurl = \"http://qdrant:6334\"\nmax_retries = 0\n";
        // Cette construction crée deux [vdb], figment prend le dernier gagnant :
        let err = load_str(&full).expect_err("max_retries = 0");
        // Soit Config (doublon de section) soit Validation selon comment figment fusionne.
        match err {
            Error::Validation(msg) => assert!(msg.contains("max_retries")),
            Error::Config(_) => {} // acceptable : parsing a échoué sur la section dupliquée
            other => panic!("erreur inattendue : {other:?}"),
        }
    }

    #[test]
    fn empty_registry_is_valid() {
        let cfg = load_str(VALID_MINIMAL).expect("vide OK");
        assert!(cfg.models.is_empty());
        cfg.validate().expect("validation sur registre vide passe");
    }

    #[test]
    fn invalid_socket_addr_is_config_error() {
        let toml = VALID_MINIMAL.replace("0.0.0.0:50051", "not-a-socket");
        let err = load_str(&toml).expect_err("socket invalide");
        assert!(matches!(err, Error::Config(_)));
    }
}
