//! Concrete `VectorDbClient` implementation for PostgreSQL + `pgvector`,
//! built on `sqlx` 0.8.
//!
//! Design notes (full rationale in `DECISIONS.md`):
//! - **Namespace = table.** A `pgvector` column is `vector(N)` — fixed
//!   dimension — so each model dimension needs its own table. The existing
//!   `model_id -> vdb_namespace` mapping becomes `namespace -> table`. The
//!   namespace is validated at config load and double-quoted here, because a
//!   table name is interpolated into SQL (it cannot be a bind parameter).
//! - **HNSW + inner product.** Indexes use `vector_ip_ops` (`<#>`), not
//!   `vector_cosine_ops`. Vector Router L2-normalizes in the hot path, so on
//!   unit vectors inner product is cosine — and `<#>` is slightly cheaper.
//!   pgvector returns `<#>` as the *negative* inner product, so the similarity
//!   score we hand back is `(embedding <#> $query) * -1`, landing in the same
//!   [-1, 1] range as Qdrant's cosine score.
//! - **Same client contract as Qdrant.** One-shot calls wrapped in
//!   `tokio::time::timeout`; a timeout maps to `Error::Vdb("timeout {op}")`
//!   (the only error class the gRPC handler retries). No retry lives here.
//! - **`ef_search`** is applied per query via `SET LOCAL hnsw.ef_search`,
//!   which is transaction-scoped — so the search runs inside a transaction
//!   when configured.
//! - **Schema is auto-provisioned** idempotently at startup from the model
//!   registry (`CREATE EXTENSION/TABLE/INDEX IF NOT EXISTS`); `sql/pgvector_schema.sql`
//!   ships the same DDL for least-privilege operators who pre-provision.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use pgvector::Vector;
use sqlx::PgPool;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;

use crate::config::{ModelSpec, VdbConfig, is_valid_pg_namespace};
use crate::error::Error;

use super::{SearchHit, SearchParams, UpsertParams, VectorDbClient};

/// Client backed by a PostgreSQL connection pool with the `pgvector`
/// extension.
///
/// `inflight` mirrors the Qdrant client: it counts the calls the middleware
/// currently has in flight (RAII-guarded), feeding the `vdb_inflight` gauge.
pub struct PgVectorClient {
    pool: PgPool,
    timeout: Duration,
    ef_search: Option<u32>,
    inflight: AtomicU64,
}

impl PgVectorClient {
    /// Connects the pool and provisions the schema for every namespace in the
    /// model registry. The connection string and tuning come from
    /// `VdbConfig`; the per-namespace dimensions come from `models`.
    pub async fn connect(
        cfg: &VdbConfig,
        models: &HashMap<String, ModelSpec>,
    ) -> Result<Self, Error> {
        let mut opts = PgPoolOptions::new().acquire_timeout(Duration::from_millis(cfg.timeout_ms));
        if let Some(max) = cfg.max_connections {
            opts = opts.max_connections(max);
        }
        let pool = opts
            .connect(&cfg.url)
            .await
            .map_err(|e| Error::Vdb(format!("connect: {e}")))?;

        let client = Self {
            pool,
            timeout: Duration::from_millis(cfg.timeout_ms),
            ef_search: cfg.ef_search,
            inflight: AtomicU64::new(0),
        };
        client.ensure_schema(models).await?;
        Ok(client)
    }

    /// Idempotently creates the extension and, for each distinct namespace,
    /// the table and its HNSW index. Safe to run on every boot. Config
    /// validation guarantees one dimension per namespace.
    async fn ensure_schema(&self, models: &HashMap<String, ModelSpec>) -> Result<(), Error> {
        sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&self.pool)
            .await
            .map_err(|e| {
                Error::Vdb(format!(
                    "ensure extension 'vector' (is it installed and may this role create it?): {e}"
                ))
            })?;

        let mut dim_by_namespace: HashMap<&str, usize> = HashMap::new();
        for spec in models.values() {
            dim_by_namespace
                .entry(spec.vdb_namespace.as_str())
                .or_insert(spec.dim);
        }

        for (namespace, dim) in dim_by_namespace {
            let table = quote_ident(namespace)?;
            sqlx::query(&create_table_sql(&table, dim))
                .execute(&self.pool)
                .await
                .map_err(|e| Error::Vdb(format!("ensure table {namespace}: {e}")))?;
            sqlx::query(&create_index_sql(&table, namespace))
                .execute(&self.pool)
                .await
                .map_err(|e| Error::Vdb(format!("ensure index {namespace}: {e}")))?;
        }
        Ok(())
    }

    /// RAII guard: increments `inflight` on entry, decrements on drop (even on
    /// the error path).
    fn track(&self) -> InflightGuard<'_> {
        self.inflight.fetch_add(1, Ordering::Relaxed);
        InflightGuard { owner: self }
    }

    /// Runs the search, optionally inside a transaction so `SET LOCAL
    /// hnsw.ef_search` scopes to this query. Wrapped by `search` in the
    /// per-call timeout.
    async fn run_search(&self, params: SearchParams) -> Result<Vec<SearchHit>, Error> {
        let table = quote_ident(&params.namespace)?;
        let has_filter = !params.metadata_filter.is_empty();
        let has_threshold = params.score_threshold.is_some();
        let sql = search_sql(&table, has_filter, has_threshold);

        let filter_json = if has_filter {
            Some(
                serde_json::to_string(&params.metadata_filter)
                    .map_err(|e| Error::Vdb(format!("search filter encode: {e}")))?,
            )
        } else {
            None
        };

        let rows = if let Some(ef) = self.ef_search {
            let mut tx = self
                .pool
                .begin()
                .await
                .map_err(|e| Error::Vdb(format!("search begin: {e}")))?;
            // ef_search is a validated u32, safe to interpolate (SET takes no
            // bind parameters).
            sqlx::query(&format!("SET LOCAL hnsw.ef_search = {ef}"))
                .execute(&mut *tx)
                .await
                .map_err(|e| Error::Vdb(format!("search set ef_search: {e}")))?;
            let mut q = sqlx::query(&sql)
                .bind(Vector::from(params.vector))
                .bind(i64::from(params.limit));
            if let Some(f) = filter_json {
                q = q.bind(f);
            }
            if let Some(th) = params.score_threshold {
                q = q.bind(f64::from(th));
            }
            let rows = q
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| Error::Vdb(format!("search: {e}")))?;
            tx.commit()
                .await
                .map_err(|e| Error::Vdb(format!("search commit: {e}")))?;
            rows
        } else {
            let mut q = sqlx::query(&sql)
                .bind(Vector::from(params.vector))
                .bind(i64::from(params.limit));
            if let Some(f) = filter_json {
                q = q.bind(f);
            }
            if let Some(th) = params.score_threshold {
                q = q.bind(f64::from(th));
            }
            q.fetch_all(&self.pool)
                .await
                .map_err(|e| Error::Vdb(format!("search: {e}")))?
        };

        let mut hits = Vec::with_capacity(rows.len());
        for row in rows {
            let point_id: String = row
                .try_get("point_id")
                .map_err(|e| Error::Vdb(format!("search decode point_id: {e}")))?;
            let score: f64 = row
                .try_get("score")
                .map_err(|e| Error::Vdb(format!("search decode score: {e}")))?;
            let metadata: String = row
                .try_get("metadata")
                .map_err(|e| Error::Vdb(format!("search decode metadata: {e}")))?;
            let metadata = serde_json::from_str::<serde_json::Value>(&metadata)
                .map_err(|e| Error::Vdb(format!("search parse metadata: {e}")))?;
            hits.push(SearchHit {
                point_id,
                score: score as f32,
                metadata: json_to_metadata(metadata),
            });
        }
        Ok(hits)
    }
}

struct InflightGuard<'a> {
    owner: &'a PgVectorClient,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.owner.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

// --- Pure SQL / conversion helpers (unit-tested without a database) ---------

/// Validates and double-quotes a namespace for use as a table name. The
/// namespace is interpolated into SQL, so this is the injection boundary;
/// config validation enforces the same charset at load time.
fn quote_ident(namespace: &str) -> Result<String, Error> {
    if !is_valid_pg_namespace(namespace) {
        return Err(Error::Vdb(format!(
            "invalid pgvector namespace '{namespace}'"
        )));
    }
    Ok(format!("\"{namespace}\""))
}

fn create_table_sql(quoted_table: &str, dim: usize) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {quoted_table} (\
         point_id TEXT PRIMARY KEY, \
         embedding vector({dim}) NOT NULL, \
         metadata JSONB NOT NULL DEFAULT '{{}}'::jsonb)"
    )
}

fn create_index_sql(quoted_table: &str, namespace: &str) -> String {
    // Index name is quoted; `namespace` is already a validated identifier.
    // Inner-product opclass: valid because vectors are L2-normalized upstream.
    format!(
        "CREATE INDEX IF NOT EXISTS \"{namespace}_embedding_hnsw\" \
         ON {quoted_table} USING hnsw (embedding vector_ip_ops)"
    )
}

fn upsert_sql(quoted_table: &str) -> String {
    // Idempotent like the Qdrant path: a second upsert of the same point_id
    // replaces the row.
    format!(
        "INSERT INTO {quoted_table} (point_id, embedding, metadata) \
         VALUES ($1, $2, $3::jsonb) \
         ON CONFLICT (point_id) DO UPDATE SET \
         embedding = EXCLUDED.embedding, metadata = EXCLUDED.metadata"
    )
}

/// Builds the kNN query. `$1` = query vector, `$2` = limit; the metadata
/// filter and score threshold take the next slots when present. The score is
/// the inner product (`<#>` negated), matching Qdrant's cosine score range.
fn search_sql(quoted_table: &str, has_filter: bool, has_threshold: bool) -> String {
    // `metadata::text` so we can decode it as a String and parse with
    // serde_json — avoids sqlx's `json` feature (and its extra drivers).
    let mut sql = format!(
        "SELECT point_id, (embedding <#> $1) * -1 AS score, metadata::text AS metadata FROM {quoted_table}"
    );
    let mut next = 3;
    let mut conds: Vec<String> = Vec::new();
    if has_filter {
        conds.push(format!("metadata @> ${next}::jsonb"));
        next += 1;
    }
    if has_threshold {
        conds.push(format!("(embedding <#> $1) * -1 >= ${next}"));
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY embedding <#> $1 LIMIT $2");
    sql
}

/// Extracts a `String -> String` map from a JSONB object, keeping only string
/// values. Mirrors the Qdrant client, which can only represent string
/// metadata; non-string entries are ignored.
fn json_to_metadata(value: serde_json::Value) -> HashMap<String, String> {
    match value {
        serde_json::Value::Object(map) => map
            .into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::String(s) => Some((k, s)),
                _ => None,
            })
            .collect(),
        _ => HashMap::new(),
    }
}

// --- VectorDbClient impl ----------------------------------------------------

#[async_trait]
impl VectorDbClient for PgVectorClient {
    fn inflight(&self) -> u64 {
        self.inflight.load(Ordering::Relaxed)
    }

    async fn upsert(&self, params: UpsertParams) -> Result<(), Error> {
        let _guard = self.track();
        let table = quote_ident(&params.namespace)?;
        let sql = upsert_sql(&table);
        // Metadata travels as a JSON text literal and is cast to jsonb in SQL
        // (`$3::jsonb`). This keeps the sqlx feature set minimal — the facade's
        // `json` feature would drag in the MySQL and SQLite drivers.
        let metadata = serde_json::to_string(&params.metadata)
            .map_err(|e| Error::Vdb(format!("upsert metadata encode: {e}")))?;

        let fut = sqlx::query(&sql)
            .bind(params.point_id)
            .bind(Vector::from(params.vector))
            .bind(metadata)
            .execute(&self.pool);

        tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| Error::Vdb("timeout upsert".to_string()))?
            .map_err(|e| Error::Vdb(format!("upsert: {e}")))?;
        Ok(())
    }

    async fn search(&self, params: SearchParams) -> Result<Vec<SearchHit>, Error> {
        let _guard = self.track();
        tokio::time::timeout(self.timeout, self.run_search(params))
            .await
            .map_err(|_| Error::Vdb("timeout search".to_string()))?
    }

    async fn health(&self) -> Result<(), Error> {
        let _guard = self.track();
        let fut = sqlx::query("SELECT 1").execute(&self.pool);
        tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| Error::Vdb("timeout health".to_string()))?
            .map_err(|e| Error::Vdb(format!("health: {e}")))?;
        Ok(())
    }
}

// --- Tests (pure helpers; no database) --------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_accepts_valid_and_quotes() {
        assert_eq!(quote_ident("prod_openai").unwrap(), "\"prod_openai\"");
        assert_eq!(quote_ident("prod-cohere-en").unwrap(), "\"prod-cohere-en\"");
    }

    #[test]
    fn quote_ident_rejects_injection() {
        for bad in ["a\"; DROP TABLE x;--", "a b", "tablé", "", "a;b"] {
            assert!(quote_ident(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn create_table_sql_has_fixed_dim_and_jsonb() {
        let sql = create_table_sql("\"ns\"", 1536);
        assert!(sql.contains("IF NOT EXISTS \"ns\""));
        assert!(sql.contains("embedding vector(1536) NOT NULL"));
        assert!(sql.contains("metadata JSONB"));
        assert!(sql.contains("point_id TEXT PRIMARY KEY"));
    }

    #[test]
    fn create_index_sql_uses_hnsw_inner_product() {
        let sql = create_index_sql("\"ns\"", "ns");
        assert!(sql.contains("USING hnsw"));
        assert!(sql.contains("vector_ip_ops"));
        assert!(sql.contains("IF NOT EXISTS \"ns_embedding_hnsw\""));
    }

    #[test]
    fn upsert_sql_is_idempotent() {
        let sql = upsert_sql("\"ns\"");
        assert!(sql.contains("INSERT INTO \"ns\""));
        assert!(sql.contains("$3::jsonb"));
        assert!(sql.contains("ON CONFLICT (point_id) DO UPDATE"));
    }

    #[test]
    fn search_sql_no_filter_no_threshold() {
        let sql = search_sql("\"ns\"", false, false);
        assert!(sql.contains("(embedding <#> $1) * -1 AS score"));
        assert!(!sql.contains("WHERE"));
        assert!(sql.contains("ORDER BY embedding <#> $1 LIMIT $2"));
    }

    #[test]
    fn search_sql_filter_only_uses_param_3() {
        let sql = search_sql("\"ns\"", true, false);
        assert!(sql.contains("WHERE metadata @> $3::jsonb"));
        assert!(!sql.contains("$4"));
        assert!(!sql.contains(">= $"));
    }

    #[test]
    fn search_sql_threshold_only_uses_param_3() {
        let sql = search_sql("\"ns\"", false, true);
        assert!(sql.contains("WHERE (embedding <#> $1) * -1 >= $3"));
        assert!(!sql.contains("@>"));
    }

    #[test]
    fn search_sql_filter_and_threshold_order_params() {
        let sql = search_sql("\"ns\"", true, true);
        assert!(sql.contains("metadata @> $3::jsonb"));
        assert!(sql.contains("(embedding <#> $1) * -1 >= $4"));
        assert!(sql.contains(" AND "));
    }

    #[test]
    fn json_to_metadata_keeps_only_strings() {
        let v = serde_json::json!({
            "tenant": "acme",
            "lang": "en",
            "count": 7,
            "nested": {"a": "b"},
        });
        let m = json_to_metadata(v);
        assert_eq!(m.len(), 2);
        assert_eq!(m.get("tenant"), Some(&"acme".to_string()));
        assert_eq!(m.get("lang"), Some(&"en".to_string()));
        assert!(!m.contains_key("count"));
        assert!(!m.contains_key("nested"));
    }

    #[test]
    fn metadata_round_trips_through_json() {
        let mut m = HashMap::new();
        m.insert("tenant".to_string(), "acme".to_string());
        m.insert("source".to_string(), "rag-nightly".to_string());
        let value = serde_json::to_value(&m).unwrap();
        assert_eq!(json_to_metadata(value), m);
    }

    #[test]
    fn json_to_metadata_on_non_object_is_empty() {
        assert!(json_to_metadata(serde_json::json!("scalar")).is_empty());
        assert!(json_to_metadata(serde_json::json!([1, 2, 3])).is_empty());
    }
}
