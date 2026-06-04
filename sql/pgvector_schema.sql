-- Reference schema for the Vector Router pgvector backend.
--
-- Vector Router auto-provisions this on startup (CREATE ... IF NOT EXISTS)
-- from the model registry, so you normally do NOT need to run it by hand.
-- It is provided for operators who run the router under a least-privilege
-- role that may not create extensions, tables or indexes: provision once
-- with a privileged role using this template, then point the router at the
-- database with the restricted role.
--
-- One table per namespace (the `vdb_namespace` of each model in config). A
-- pgvector column is `vector(N)` with a FIXED dimension, so every model that
-- shares a namespace must declare the same `dim`. Replace {table} with the
-- namespace and {dim} with the model dimension.
--
-- The HNSW index uses `vector_ip_ops` (inner product, operator `<#>`).
-- Vector Router L2-normalizes vectors in its hot path, so on unit vectors
-- inner product equals cosine similarity -- and `<#>` is slightly cheaper
-- than the cosine opclass.

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS "{table}" (
    point_id  TEXT PRIMARY KEY,
    embedding vector({dim}) NOT NULL,
    metadata  JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX IF NOT EXISTS "{table}_embedding_hnsw"
    ON "{table}" USING hnsw (embedding vector_ip_ops);

-- The router tunes recall/latency per query from `vdb.ef_search` via
-- `SET LOCAL hnsw.ef_search = N` inside the search transaction; no static
-- index option is required.
