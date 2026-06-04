# Design decisions

Each entry dated, motivated, and recorded here to avoid replaying the same trade-offs cold.

## 2026-04-17

### Cargo workspace layout: lib + bin in a single crate
Rationale: `cargo build` with `-D warnings` treats functions not used by `main.rs` as dead code. Exposing modules as `pub mod` via `src/lib.rs` makes them the public API of the library, which stops the warnings and keeps the architecture clean (the library will also be usable in integration tests under `tests/`).

### Box on `figment::Error` in the `Error` enum
Rationale: clippy fires `result_large_err` as soon as a `Result<_, Error>` is returned. `figment::Error` weighs ~200 bytes due to its `Vec` of context traces, which inflates every caller. The Box limits the overhead to 8 bytes. Lost: one extra dereference on the error path (negligible, this is the exception path).

### tonic 0.14 vs 0.13 and prost decoupling
Rationale: tonic 0.14 split prost support into two crates — `tonic-prost` (runtime) and `tonic-prost-build` (codegen). We target 0.14.5 to benefit from the improvements and stay on the latest codegen. Usage: `tonic-prost-build::configure().compile_protos(...)` in `build.rs`.

### `target-cpu=native` in `.cargo/config.toml`
Rationale: enables SIMD auto-vectorization via AVX2/AVX512 depending on the build CPU. Downside: the binary is no longer portable across CPU families. Acceptable for local dev; for Docker, we will pin an explicit target at step 11.

## 2026-04-18

### `rcu` (read-copy-update) rather than `Mutex<HashMap>` for registry updates
Rationale: `ArcSwap::rcu` provides lock-free updates with implicit retry on contention. For rare updates (admin, a few per hour at most) and frequent reads (every gRPC request), it is strictly superior to a `RwLock` — zero wait on the reader side, near-zero contention under nominal traffic. Price paid: the update closure can be called multiple times if another thread updates concurrently, but that is benign (same final result).

### loom test decoupled from the ArcSwap primitive
Rationale: `arc-swap` has its own internal loom coverage. Reproducing that coverage in our crate is costly and redundant. Instead, the loom test validates the *pattern* (snapshot via Arc clone + update via atomic replacement) on a simplified `Mutex<Arc<T>>` model. What we test: our registry logic introduces no race beyond what the primitive guarantees.

### `AlignedBuffer` via `Box<[u32]>` + `bytemuck::cast_slice`
Rationale: guaranteeing alignment 4 on a `u8` buffer requires either `unsafe` (forbidden), a `repr(align(4))` wrapper, or storage on a naturally aligned type (u32, f32, etc.). u32 storage is the simplest, without unsafe, and conversion to `&[u8]` / `&[f32]` goes through `bytemuck` which is safe. Cost: nothing (u32 and u8 share the same memory representation).

### `PooledBuffer`: `mem::take` with `AlignedBuffer::default()` sentinel instead of `Option`
Rationale: the invariant "PooledBuffer always owns a valid buffer until Drop" expresses itself naturally with a `buffer: AlignedBuffer` field (not `Option<...>`). In Drop, `mem::take` swaps the buffer with an `AlignedBuffer::default()` (empty slice, zero alloc). This avoids an `Option::unwrap` / `.expect` in `Deref`, which would be forbidden by project rules.

### Miri + stack alignment
Rationale (trace of a bug encountered): miri does not guarantee stack alignments beyond what the type requests. A `[u8; 17]` is only 1-byte aligned under miri, even though a native stack allocator would align it to 16. Misalignment tests must therefore force a base alignment via a u32 type (or `repr(align(4))`), then take an offset of 1 inside it.

### Benches extended as "tests" for the unwrap/expect rule
Rationale: the brief forbids `unwrap`/`expect` outside `main.rs`, `tests/`, `build.rs`. Files under `benches/` are semantically tests (of performance). We extend the rule to `benches/`, which allows idiomatic `unwrap` use for bench preconditions and sanity assertions.

### Rewriting `l2_norm_squared` with eight parallel accumulators (branchless + manual unroll)
Rationale: the initial version fused NaN/Inf validation and sum of squares in a single branched loop (`if !x.is_finite() return Err`). That branch prevented LLVM's SIMD auto-vectorization, yielding ~4.4 µs for 1536 dims (pure scalar speed, ~3 % of the AVX2 peak). Two successive iterations: (1) branchless conversion exploiting IEEE 754 propagation (NaN/Inf propagate through `*` and `+`), checking the finiteness of the sum at the end → ×2.1 gain. (2) eight independent accumulators via `chunks_exact(8)` and manual unroll, to break the sequential reduction dependency chain without violating strict IEEE 754 associativity → an extra ×3 gain, total ×6.3. Result: 702 ns median for 1536 dims, throughput ~2.2 Gelem/s (see `BENCHES.md`). No `unsafe`, no `-C fast-math`, miri green, numerical correctness preserved.

### Multi-stage Dockerfile with `gcr.io/distroless/cc-debian12:nonroot`
Rationale: minimal base image (~27 MB), no shell, no package manager — reduced attack surface. `:nonroot` variant to respect the principle of least privilege inside the container. Alternative considered (distroless/static-debian12) rejected: would require compiling against musl with target `-musl`, heavy and marginal benefit since we already need `libc` for transitive deps.

### Disabling `target-cpu=native` in the Docker build
Rationale: local `.cargo/config.toml` uses `-C target-cpu=native` to exploit the builder's CPU features (AVX2/FMA). A binary produced with that flag would only run on an identical CPU. In the Dockerfile we explicitly replace this file with an equivalent using `-C target-cpu=x86-64-v3` (Haswell+, covers almost all server CPUs since 2013). Trade-off: we potentially lose 10–20 % SIMD perf vs native on a recent server, in exchange for binary portability.

### Shutdown via `tokio::sync::broadcast` rather than `Notify` or `CancellationToken`
Rationale: three independent tasks (gRPC, HTTP, gauge updater) need to receive the shutdown signal simultaneously. `broadcast::Sender<()>` provides that natively: a `.send(())` wakes every `.subscribe()`. `Notify::notify_waiters()` has a window problem (subscribers who subscribe after the notify see nothing), `tokio_util::sync::CancellationToken` would add a dependency for a trivial use case. `broadcast` ticks every box: native to tokio, multi-consumer, a single send.

Variants tested: `service_starts_and_shuts_down_cleanly` (nominal flow), `shutdown_propagates_to_all_tasks_in_order` (drain < 1s even though the gauge updater ticks every 5s — proves that `tokio::select!` interrupts correctly), `drain_timeout_is_reported` (forces a stuck-task scenario via `shutdown_tx.clone()` kept alive, verifies that the timeout surfaces an explicit error).

### `start_service` vs `start_service_with_vdb` — refactor for testability
Rationale: the prod version instantiates `QdrantVdbClient` from the config, which makes the code untestable without a real Qdrant instance. Extracted a `start_service_with_vdb(…, vdb: Arc<dyn VectorDbClient>, …)` variant that accepts a pre-built VDB. Tests inject `MockVdbClient`; prod goes through the first variant which composes the two. No duplicated logic, no useless generics, no regression on the public API.

### VDB saturation metric via `inflight()` on the trait (no access to internal pools)
Rationale: `qdrant-client` does not expose its connection pool state (managed by hyper/tonic internally, no public API). To produce a saturation metric usable in prod, we added an `fn inflight(&self) -> u64` method to the `VectorDbClient` trait with a default implementation (returns 0 for backends that do not track this). `QdrantVdbClient` overrides it via its `AtomicU64` incremented/decremented by `InflightGuard` (RAII). It is an application metric ("how many calls the middleware has sent and is waiting on"), not the true number of gRPC connections — but enough to detect downstream pressure in Grafana.

### Gauges updated by periodic task (5s) rather than on every event
Rationale: `registered_models`, `pool_available`, `vdb_inflight` are values read from application state (`Registry::len`, `BufferPool::available`, `VectorDbClient::inflight`). Emitting them on every change would multiply the cost without benefit (Prometheus scrapes every 10–15s at best). Choice: a background task `telemetry::run_gauge_updater` that reads and pushes every 5s. Max observation latency ~5s, more than enough for a dashboard.

### Fix: `point_id` routing → `Num` or `Uuid` depending on format (bug surfaced by 2026-04-20 stress test)
**Bug**: in the initial Qdrant implementation, `params.point_id` (String) was systematically wrapped in `PointIdOptions::Uuid(...)`. Qdrant only accepts two ID formats: `u64` or UUID-as-string. An arbitrary `point_id` like `"doc-bench"` made the Qdrant request fail with `"Unable to parse UUID"`, surfaced as `Error::Vdb(...)` with gRPC status `Unavailable`.

**Not caught by tests**: `MockVdbClient` accepts any String as `point_id`. No format validation on the mock side. All 72 tests (unit + integration + spike + miri) were green despite this bug.

**Surfaced by**: a stress test with `ghz` against a real Qdrant running locally in Docker. 2000 requests, 100 % `Unavailable`, with Qdrant's explicit message in plain text in the response.

**Fix**: a `point_id_from_string(s: String) -> PointId` function that tries `s.parse::<u64>()` first. If it succeeds → `PointIdOptions::Num(n)`. Otherwise → `PointIdOptions::Uuid(s)`. Qdrant then validates itself whether it is a valid UUID, and surfaces its error to the client if it is neither u64 nor UUID — we hide nothing on the middleware side.

**Lesson documented**: mock-based tests only cover application logic, not external contracts. Any component that talks to a critical external dependency (VDB, LLM, broker) must have at least one integration test against the real thing in CI. To add in a future iteration: a `#[cfg(feature = "integration-qdrant")]` test that spins up Qdrant in testcontainers and exercises the edge cases (numeric ID, valid UUID, arbitrary string, complex metadata).

**Tests added**: `point_id_from_numeric_string_becomes_num`, `point_id_from_uuid_string_becomes_uuid`, `point_id_from_arbitrary_string_becomes_uuid_then_rejected_by_qdrant` in `src/client/qdrant.rs`.

### Minimal embedded HTML dashboard at `/dashboard` — revised on 2026-04-20
Initial decision (see below): no embedded HTML dashboard, Grafana as the only visualization tool. That rule held as long as the goal was production.

**Revision**: after the project was validated, a commercial need emerged — a visual demo artifact for 30-minute prospect calls, where Grafana requires too heavy a setup (Prometheus + datasource + JSON import). Added a static HTML page served at `GET /dashboard`, embedded via `include_str!("../../static/dashboard.html")`.

Guardrails to avoid falling back into the traps of the initial decision:
- **100 % truthful content**: real-time RED metrics scraped from `/metrics`, hardcoded test and bench numbers kept current. Zero made-up KPI like "Bypass ROI".
- **No user input displayed**: XSS surface zero, the page only reads Prometheus metric names already produced by the application itself.
- **Binary size impact**: +11 KB in the Docker image. Zero practical impact.
- **Explicit positioning**: the page footer states "Local dashboard for demonstration · Production observability via Grafana". Never presented as a replacement for Grafana.

Source file: `static/dashboard.html` (HTML + CSS + JS inline, zero external dependencies). Endpoint: `GET /dashboard` in `src/server/http.rs`. Integration test: `dashboard_returns_html_with_expected_sections`.

### No embedded HTML dashboard — initial decision (overturned on 2026-04-20)
Initial rationale: proposal (embedded Tailwind UI dashboard) rejected — would duplicate Grafana, add an XSS attack surface, increase binary size, deviate from the SRE standard. Kept as long as scope is strictly production. See the revision above for the demo need that justified adding a minimal dashboard.

### Double `tonic` in the dependency tree (accepted)
Rationale: `qdrant-client` 1.12 (and the latest 1.17) ships `tonic 0.12 + prost 0.13` internally. Our gRPC server uses `tonic 0.14.5 + prost 0.14.3`. Both versions coexist in the binary without type-level interop (qdrant-client encapsulates its gRPC layer, our service exposes its own API). Choice retained: keep both in parallel rather than a massive downgrade (cost: +1–2 MB binary, +compile time; alternative: 1–3 days of rework on validated foundations). To revisit if `qdrant-client` migrates to 0.14 in a future release.

### `VectorDbClient` as a generic trait rather than a concrete type
Rationale: a stable interface between the gRPC handler and the VDB backend makes it easier (1) to test the handler via `MockVdbClient` (hand-rolled in `client::mock`, no mockall dependency), (2) to add later backends (Pinecone, Weaviate, pgvector) without touching application code. YAGNI risk accepted: the abstraction costs ~30 lines, the testability benefit is immediate.

### Hand-rolled mock VDB rather than wiremock for tests
Rationale: `wiremock-rs` is HTTP/REST-oriented. `qdrant-client` 1.12 uses gRPC first. Building a complete gRPC mock of the Qdrant proto would have cost 1–2 days and introduced a second dependency on the Qdrant schema. Instead, `MockVdbClient` in `client::mock` (gated `#[cfg(test)]`, ~40 lines) covers the step-7 handler tests with a controllable API. The real Qdrant client is tested by construction + timeout + RAII `inflight_guard`, without any real network call.

### `InflightGuard` for the VDB saturation metric
Rationale: `qdrant-client` does not expose the internal state of its connection pool (managed by hyper/tonic). To measure application-level saturation, we increment/decrement an `AtomicU64::inflight` via a RAII guard around each call. Consistent even on the error path (the `Drop` runs). Plan A from the initial brief.

### No exponential retry in the VDB client (pushed up to the handler)
Rationale: a retry that duplicates the params (`UpsertParams` holds a `Vec<f32>` of 1536 × 4 = 6 KB) at the client level would force expensive clones on every retry, even under nominal traffic where most calls succeed on the first try. Better: let the gRPC handler (step 7) build the params once and drive its own retry policy by calling the client multiple times with the same reused params. The client therefore stays "one-shot": one call, one response, one strict timeout.

### Adding the Search RPC to the proto (out of initial brief scope)
Rationale: the initial brief only covered `Upsert`, but system consistency requires the query vector to undergo the same transformation (validation + normalization) as the stored vectors. If the client calls Qdrant directly for search without going through the middleware, it skips normalization and introduces a systematic bias on the scores. Added a `Search` RPC that shares the pipeline with `Upsert`, diverging only at step 7 (calling `search_points` instead of `upsert_points`). Estimated additional cost: 0.5 day in step 7. The `requests_total` metric gains an `op ∈ {upsert, search}` label.

### 2026-04-21 — `producer_id` + structured rejection journal + Ed25519 license
Rationale: three pieces in one sprint to make the middleware commercially distributable with forensic traceability and offline protection.

**Producer attribution** (`producer_id` on `UpsertRequest`/`SearchRequest`): optional proto3 field (fields 6 and 7) added as a backward-compatible append. Empty => "unknown" on the metrics side. Becomes a label on `requests_total` and `request_duration_seconds` alongside `model_id`, `op`, `status`. Cardinality bounded contractually by the client (service name, no UUID) — documented in the proto. Allows identifying which producer is sending malformed vectors without distributed tracing.

**Structured rejection journal (JSON Lines on stderr)**: emitted only on validation rejection (`unknown_model`, `invalid_dim`, `invalid_numeric`). Contains `{event, op, producer_id, model_id, status, reason}`. Chose stderr so as not to pollute a potential structured application stdout. Separated from metrics: aggregation vs unit granularity for post-mortem `grep | jq`. No trace ID: this is a debug tool, not an audit trail.

**Tests**: unit additions (`normalize_producer` on the gRPC side) + integration. Clippy `-D warnings` green.

> **Historical note**: a runtime license layer signed with Ed25519 (signed claims, public key embedded via `include_bytes!`, 45-day evaluation mode) was developed then removed when moving to Apache 2.0. The code remains accessible in the git history for anyone who wants to rebuild gating for their enterprise fork.

## 2026-06-03 — pgvector backend (v0.2.0)

The v0.2 goal is adoption: strict validation plus the most widely deployed vector store, pgvector, at zero install friction. Everything below is, and stays, Apache 2.0.

### sqlx rather than Diesel or SeaORM
Rationale: the data layer is async-native and tokio-first (matching tonic 0.14), with a thin abstraction that leaves the pgvector operators and index tuning in plain sight — exactly what an ORM hides. The choice is **not** about query-construction speed (the bottleneck is the Postgres round trip, not building the SQL string) but about control, dependency discipline, and fit with the project's ethos. sqlx is pinned with `default-features = false` and only `postgres` + `runtime-tokio-rustls`; the `pgvector` crate's `sqlx` feature provides `pgvector::Vector: Encode/Decode`, so a `Vec<f32>` binds straight onto a `vector` column.

### sqlx 0.8, not the freshly released 0.9
Rationale: 0.9 shipped days before this work and reorganized its runtime/TLS features (the `runtime-tokio-rustls` alias the ecosystem documents is 0.8-shaped). For an adoption-focused release the mature 0.8.6 is the lower-risk pin; `pgvector` 0.4.2 allows `>=0.8, <0.10`, so the upgrade path stays open. On 0.8.6 `runtime-tokio-rustls` resolves to rustls + ring (not aws-lc-rs), so no cmake / C toolchain enters the build — which matters for the distroless image.

### Backends are Cargo features; pgvector is opt-in, Qdrant stays the default
Rationale: `default = ["qdrant"]` keeps existing builds bit-for-bit identical and sqlx out of their dependency tree. pgvector lives behind `--features pgvector`; a `--no-default-features --features pgvector` build drops qdrant-client and its transitive tonic 0.12 tree entirely. A `compile_error!` guards the no-backend build, and `start_service` rejects a config that names a backend whose feature was not compiled in (clear error, no silent fallback).

### Namespace = table (imposed by pgvector, not a preference)
Rationale: a `vector(N)` column is fixed-dimension. Since each model has its own dimension, each needs its own table. The existing `model_id -> vdb_namespace` mapping becomes `namespace -> table`. Config validation enforces that all models sharing a namespace agree on dimension, and that the namespace is a safe identifier: it is interpolated into SQL (a table name cannot be a bind parameter), so it is validated against `[A-Za-z0-9_-]`, capped at 63 bytes, and double-quoted. Bonus: per-model index isolation, and a natural seam for the v0.3 multi-tenant layer.

### HNSW + inner product, leaning on guaranteed normalization
Rationale: indexes use HNSW (pgvector 0.5+) rather than IVFFlat — better recall/latency, no training step. The opclass is `vector_ip_ops` (inner product, `<#>`), not `vector_cosine_ops`: Vector Router L2-normalizes in its hot path, so on unit vectors inner product **is** cosine, and `<#>` is marginally cheaper. pgvector returns `<#>` as the negative inner product, so the score handed back is `(embedding <#> $query) * -1`, in the same `[-1, 1]` range as the Qdrant cosine score — `score_threshold` semantics carry over unchanged.

### `ef_search` per query via `SET LOCAL`
Rationale: `vdb.ef_search` trades recall for latency. It is applied with `SET LOCAL hnsw.ef_search = N`, which is transaction-scoped, so a configured search runs inside a transaction. Left unset, the server default applies.

### Client contract identical to Qdrant: one-shot + strict timeout, retry in the handler
Rationale: matching the existing decision ("No exponential retry in the VDB client"), `PgVectorClient` makes one call per operation wrapped in `tokio::time::timeout`. A timeout becomes `Error::Vdb("timeout ...")`, the only class `is_transient()` retries — so the gRPC handler's existing backoff drives retries for both backends with no handler change. `inflight()` reuses the `AtomicU64` + RAII-guard pattern.

### Metadata as JSON text cast to `jsonb` (no sqlx `json` feature)
Rationale: enabling the sqlx facade's `json` feature drags in the MySQL and SQLite drivers (and an RSA/crypto subtree), because that feature activates json across **all** drivers. Instead metadata is serialized with serde_json (already a dependency) to a text literal and cast `$n::jsonb` in SQL; reads select `metadata::text` and parse in Rust. The equality filter uses jsonb containment (`metadata @> $n::jsonb`), mirroring Qdrant's match-all semantics. Net: the sqlx feature set stays minimal.

### Schema auto-provisioned at startup (idempotent); SQL also shipped
Rationale: zero install friction is the v0.2 headline. The router knows each namespace and dimension from config, so on boot it runs `CREATE EXTENSION / TABLE / INDEX IF NOT EXISTS`. `sql/pgvector_schema.sql` ships the same DDL for operators who run under a least-privilege role and prefer to pre-provision with a privileged role, then point the router at a restricted one. Unlike Qdrant (where a point id must be `u64` or UUID), the pgvector table uses a `TEXT` primary key, so arbitrary `point_id`s just work — the Qdrant `point_id` quirk has no analogue here.

### CI without a live database at build time
Rationale: queries use the runtime-checked sqlx API (`query` / `fetch_all`), not the compile-time macros, so no `.sqlx` cache is committed and the build never needs a database. The live tests (`tests/pgvector_integration.rs`) run in a dedicated CI job against a `pgvector/pgvector:pg16` service container via `VR_TEST_PG_URL`, and skip when it is unset so default and local runs stay green.

### Where an additive Enterprise layer would graft (NOT built here)
Note for v0.3+: the Enterprise tier must remain **purely additive** — no free-core feature ever moves behind a paywall. The natural seams, none of which exist in v0.2: a per-tenant predicate injected into the search `WHERE` clause (or a tenant column / table-per-tenant, which namespace=table already foreshadows); a quota check around `UpsertParams` keyed by `producer_id`; RBAC enforced before `validate_and_prepare` in the gRPC handler. Each would wrap, not modify, the Apache-2.0 core.
