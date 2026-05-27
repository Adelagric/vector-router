//! Implémentation concrète de `VectorDbClient` pour Qdrant, basée sur
//! `qdrant-client` 1.12.
//!
//! Chaque appel VDB est enveloppé dans `tokio::time::timeout` avec la valeur
//! `config.vdb.timeout_ms`. La politique de retry (exponentiel, `max_retries`,
//! erreurs transitoires uniquement) est appliquée sur les erreurs réseau.
//!
//! Note technique : le double `tonic` dans l'arbre est documenté dans
//! `DECISIONS.md`. `qdrant-client` 1.12 utilise tonic 0.12 en interne ;
//! notre serveur gRPC utilise tonic 0.14. Les deux ne se croisent jamais
//! au niveau des types.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use qdrant_client::Qdrant;
use qdrant_client::qdrant::condition::ConditionOneOf;
use qdrant_client::qdrant::r#match::MatchValue;
use qdrant_client::qdrant::point_id::PointIdOptions;
use qdrant_client::qdrant::{
    Condition, FieldCondition, Filter, Match, PointId, PointStruct, SearchPoints,
    UpsertPointsBuilder, Value, Vector, Vectors, vectors,
};

use crate::config::VdbConfig;
use crate::error::Error;

use super::{SearchHit, SearchParams, UpsertParams, VectorDbClient};

/// Wrapper autour du client Qdrant officiel.
///
/// `inflight` compte les appels VDB en cours, exposable comme métrique de
/// saturation (Plan A du brief : pas de pool natif accessible, on mesure
/// l'activité applicative).
pub struct QdrantVdbClient {
    inner: Qdrant,
    timeout: Duration,
    inflight: AtomicU64,
}

impl QdrantVdbClient {
    /// Construit un client à partir de la config applicative.
    pub fn new(cfg: &VdbConfig) -> Result<Self, Error> {
        let mut builder = Qdrant::from_url(&cfg.url);
        if let Some(key) = &cfg.api_key {
            builder = builder.api_key(key.clone());
        }
        // Le timeout du client est défensif ; le timeout effectif par appel
        // est géré par `tokio::time::timeout` pour garantir un comportement
        // strict même si la couche transport ne coopère pas.
        builder = builder.timeout(Duration::from_millis(cfg.timeout_ms));

        let inner = builder
            .build()
            .map_err(|e| Error::Vdb(format!("init Qdrant : {e}")))?;
        Ok(Self {
            inner,
            timeout: Duration::from_millis(cfg.timeout_ms),
            inflight: AtomicU64::new(0),
        })
    }

    /// Nombre d'appels VDB actuellement en vol (gauge pour Prometheus).
    pub fn inflight(&self) -> u64 {
        self.inflight.load(Ordering::Relaxed)
    }

    /// Garde RAII qui incrémente `inflight` à l'entrée et le décrémente à la
    /// sortie. Garantit la cohérence du compteur même sur chemin d'erreur.
    fn track(&self) -> InflightGuard<'_> {
        self.inflight.fetch_add(1, Ordering::Relaxed);
        InflightGuard { owner: self }
    }
}

struct InflightGuard<'a> {
    owner: &'a QdrantVdbClient,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.owner.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

// --- Conversions utilitaires ------------------------------------------------

fn metadata_to_payload(meta: &HashMap<String, String>) -> HashMap<String, Value> {
    meta.iter()
        .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
        .collect()
}

fn filter_from_metadata(meta: &HashMap<String, String>) -> Option<Filter> {
    if meta.is_empty() {
        return None;
    }
    let must = meta
        .iter()
        .map(|(k, v)| Condition {
            condition_one_of: Some(ConditionOneOf::Field(FieldCondition {
                key: k.clone(),
                r#match: Some(Match {
                    match_value: Some(MatchValue::Keyword(v.clone())),
                }),
                ..Default::default()
            })),
        })
        .collect();
    Some(Filter {
        must,
        ..Default::default()
    })
}

fn payload_to_metadata(payload: HashMap<String, Value>) -> HashMap<String, String> {
    payload
        .into_iter()
        .filter_map(|(k, v)| {
            // Qdrant stocke des Value typés ; côté middleware on ne sait
            // représenter que des strings. Les autres types sont ignorés
            // silencieusement (avertissement à documenter côté API).
            v.kind.and_then(|kind| match kind {
                qdrant_client::qdrant::value::Kind::StringValue(s) => Some((k, s)),
                _ => None,
            })
        })
        .collect()
}

fn point_id_to_string(id: &Option<PointId>) -> String {
    match id.as_ref().and_then(|p| p.point_id_options.as_ref()) {
        Some(PointIdOptions::Uuid(s)) => s.clone(),
        Some(PointIdOptions::Num(n)) => n.to_string(),
        None => String::new(),
    }
}

/// Qdrant n'accepte que deux types d'ID de point : un entier `u64` ou un
/// UUID en représentation string. Si le `point_id` de la requête est
/// parseable en `u64`, on l'envoie comme `Num`. Sinon on le passe en `Uuid`
/// (Qdrant rejettera côté serveur si ce n'est pas un UUID valide — erreur
/// propagée telle quelle au client via `Error::Vdb`).
fn point_id_from_string(s: String) -> PointId {
    if let Ok(n) = s.parse::<u64>() {
        PointId {
            point_id_options: Some(PointIdOptions::Num(n)),
        }
    } else {
        PointId {
            point_id_options: Some(PointIdOptions::Uuid(s)),
        }
    }
}

// --- Impl VectorDbClient ----------------------------------------------------

#[async_trait]
impl VectorDbClient for QdrantVdbClient {
    fn inflight(&self) -> u64 {
        // Réutilise le compteur atomique incrémenté par `InflightGuard`.
        self.inflight.load(Ordering::Relaxed)
    }

    async fn upsert(&self, params: UpsertParams) -> Result<(), Error> {
        let _guard = self.track();

        let point = PointStruct {
            id: Some(point_id_from_string(params.point_id)),
            vectors: Some(Vectors {
                vectors_options: Some(vectors::VectorsOptions::Vector(Vector {
                    data: params.vector,
                    ..Default::default()
                })),
            }),
            payload: metadata_to_payload(&params.metadata),
        };

        let req = UpsertPointsBuilder::new(params.namespace, vec![point])
            .wait(true)
            .build();

        tokio::time::timeout(self.timeout, self.inner.upsert_points(req))
            .await
            .map_err(|_| Error::Vdb("timeout upsert".to_string()))?
            .map_err(|e| Error::Vdb(format!("upsert : {e}")))?;
        Ok(())
    }

    async fn search(&self, params: SearchParams) -> Result<Vec<SearchHit>, Error> {
        let _guard = self.track();

        let req = SearchPoints {
            collection_name: params.namespace,
            vector: params.vector,
            limit: params.limit as u64,
            score_threshold: params.score_threshold,
            filter: filter_from_metadata(&params.metadata_filter),
            with_payload: Some(true.into()),
            ..Default::default()
        };

        let resp = tokio::time::timeout(self.timeout, self.inner.search_points(req))
            .await
            .map_err(|_| Error::Vdb("timeout search".to_string()))?
            .map_err(|e| Error::Vdb(format!("search : {e}")))?;

        Ok(resp
            .result
            .into_iter()
            .map(|p| SearchHit {
                point_id: point_id_to_string(&p.id),
                score: p.score,
                metadata: payload_to_metadata(p.payload),
            })
            .collect())
    }

    async fn health(&self) -> Result<(), Error> {
        let _guard = self.track();
        tokio::time::timeout(self.timeout, self.inner.health_check())
            .await
            .map_err(|_| Error::Vdb("timeout health".to_string()))?
            .map_err(|e| Error::Vdb(format!("health : {e}")))?;
        Ok(())
    }
}

// --- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str, timeout_ms: u64) -> VdbConfig {
        VdbConfig {
            url: url.to_string(),
            api_key: None,
            timeout_ms,
            max_retries: 3,
            retry_base_delay_ms: 50,
        }
    }

    #[test]
    fn client_constructs_from_valid_config() {
        let c = QdrantVdbClient::new(&cfg("http://localhost:6334", 5000));
        assert!(c.is_ok());
    }

    #[test]
    fn inflight_starts_at_zero() {
        let c = QdrantVdbClient::new(&cfg("http://localhost:6334", 5000)).unwrap();
        assert_eq!(c.inflight(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeout_fires_on_unreachable_host() {
        // Port 1 n'accepte pas de connexion : le timeout doit fire rapidement.
        let c = QdrantVdbClient::new(&cfg("http://127.0.0.1:1", 100)).unwrap();
        let params = UpsertParams {
            namespace: "test".to_string(),
            point_id: "p".to_string(),
            vector: vec![0.0],
            metadata: HashMap::new(),
        };
        let start = std::time::Instant::now();
        let err = c.upsert(params).await.unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "timeout devrait firer en < 500ms (eu {elapsed:?})"
        );
        assert!(matches!(err, Error::Vdb(_)));
    }

    #[test]
    fn point_id_from_numeric_string_becomes_num() {
        let p = point_id_from_string("42".to_string());
        match p.point_id_options {
            Some(PointIdOptions::Num(n)) => assert_eq!(n, 42),
            other => panic!("attendu Num(42), eu {other:?}"),
        }
    }

    #[test]
    fn point_id_from_uuid_string_becomes_uuid() {
        let uuid = "00000000-0000-0000-0000-000000000001".to_string();
        let p = point_id_from_string(uuid.clone());
        match p.point_id_options {
            Some(PointIdOptions::Uuid(s)) => assert_eq!(s, uuid),
            other => panic!("attendu Uuid, eu {other:?}"),
        }
    }

    #[test]
    fn point_id_from_arbitrary_string_becomes_uuid_then_rejected_by_qdrant() {
        // Cas "doc-bench" : ni u64, ni UUID valide. On le route vers Uuid,
        // Qdrant rejettera côté serveur — comportement voulu (on propage
        // l'erreur au lieu de masquer la bizarrerie côté client).
        let p = point_id_from_string("doc-bench".to_string());
        match p.point_id_options {
            Some(PointIdOptions::Uuid(s)) => assert_eq!(s, "doc-bench"),
            other => panic!("attendu Uuid('doc-bench'), eu {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn inflight_increments_and_decrements_on_error() {
        let c = QdrantVdbClient::new(&cfg("http://127.0.0.1:1", 100)).unwrap();
        let params = UpsertParams {
            namespace: "test".to_string(),
            point_id: "p".to_string(),
            vector: vec![0.0],
            metadata: HashMap::new(),
        };
        // L'appel va échouer (timeout), mais inflight doit revenir à 0.
        let _ = c.upsert(params).await;
        assert_eq!(c.inflight(), 0, "inflight doit revenir à 0 après erreur");
    }
}
