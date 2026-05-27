# vector-router

[![CI](https://github.com/Adelagric/vector-router/actions/workflows/ci.yml/badge.svg)](https://github.com/Adelagric/vector-router/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-stable-orange?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache_2.0-blue)](LICENSE)

# Stop silent embedding corruption.

**The trust layer between your AI agents and your vector database.** A gRPC choke-point that validates, normalizes and routes every vector before it lands in the database — so that enterprise memory stays intact and similarity scores are coherent by construction.

Open-source Rust middleware (Apache 2.0). No inference, no models. Bounded scope, by design.

---

## Before / after

```
WITHOUT vector-router                      WITH vector-router
─────────────────────                      ────────────────────
✗ silently wrong dimensions                ✓ immediate reject, explicit gRPC error
✗ NaN / Inf polluting the ANN index        ✓ rejected before write, 0 contamination
✗ biased similarity scores                 ✓ uniform L2 normalization ingest + search
✗ "which agent pushed this?"               ✓ Prometheus label per producer_id
✗ blind debugging                          ✓ turnkey Grafana RED dashboard
```

## Demo (90 s, recorded live)

![Vector Router — live demo](docs/media/demo.svg)

Three cases, real stack (Qdrant + vector-router in docker-compose):

1. **Valid 1536-dim vector** → accepted, routed to Qdrant, `wasNormalized: true`.
2. **Same vector, NaN at index 42** → `InvalidArgument: vecteur contient NaN ou Inf`. Never written to the database.
3. **Agent `rag-nightly` sends a 512-dim instead of 1536** → `InvalidArgument: dimension invalide`. Visible in Prometheus with the correct `producer_id`.

## Scene of the crime (true story, rounded numbers)

> A nightly RAG pipeline switches — uncoordinated internal change — from `text-embedding-3-large` (3072 dims) to `text-embedding-3-small` (1536 dims). The production vector database is configured for 3072. Writes fail, the agent logs an error, no one has an alert on it.
>
> For **three weeks**, every night, ~8,000 documents are not indexed. Detected through customer support: "the new docs don't show up in search". Post-mortem + forced reindex of 3 months of history: **~€47,000 in OpenAI calls** + 2 engineer-weeks of remediation.
>
> With vector-router in the path: the request would have been rejected at the first batch (model_id incoherent with announced dim), a Prometheus counter `requests_total{status="invalid_dim",producer_id="rag-nightly"}` would have spiked instantly, PagerDuty alert at t0.

---

## Why

Three systematic failure modes in stacks that let embedding producers write directly to a vector database — invisible for months:

1. **Silent corruption** — an agent uses the wrong model (1536 dims instead of 3072), or a provider returns `NaN` / `Inf` on its batch endpoints. Those points permanently contaminate the ANN index.
2. **Biased search scores** — stored vectors are normalized by one agent, the query vector is produced by another agent that doesn't normalize. Cosine similarity returns systematically wrong results, with no visible error.
3. **No attribution** — impossible to know which agent pushed which vector. Months of potential reindex at tens of thousands of euros in model calls when the problem is finally discovered.

The middleware handles all three at a single control point. Each request carries a `producer_id` that becomes a Prometheus label — you see precisely which agent ships malformed vectors.

---

## Quickstart — Vector Router + Qdrant in 5 minutes

```yaml
# docker-compose.yml
services:
  qdrant:
    image: qdrant/qdrant:latest
    ports: ["6333:6333", "6334:6334"]

  vector-router:
    image: vector-router:0.1.0          # build via: make docker
    ports: ["50051:50051", "9090:9090"]
    volumes:
      - ./config.toml:/etc/vector-router/config.toml:ro
    depends_on: [qdrant]
```

```toml
# config.toml — see config.example.toml for full options
[server]
grpc_bind = "0.0.0.0:50051"
http_bind = "0.0.0.0:9090"

[vdb]
url = "http://qdrant:6334"
timeout_ms = 500

[admin]
bearer_token = "change-me-in-production"

[models."openai-text-embedding-3-small"]
dim = 1536
normalize = true
vdb_namespace = "demo"
```

```bash
docker compose up -d
curl -s http://localhost:9090/ready           # => "ready"
curl -s http://localhost:9090/metrics | head  # live Prometheus metrics
```

From this point on, any `Upsert` / `Search` request to `localhost:50051` goes through validation + normalization + routing. Rejections show up in `/metrics` and on stderr as structured JSON.

Full operator walkthrough in [`GETTING_STARTED.md`](GETTING_STARTED.md).

---

## Architecture

```
[Embedding producers] ── gRPC ─▶ [vector-router] ── gRPC ─▶ [Qdrant]
                                       │
                                       ▼
                              Prometheus /metrics
                              Axum HTTP :9090
```

Two RPCs exposed (see [`proto/vector_router/v1/router.proto`](proto/vector_router/v1/router.proto)):

- `Upsert` — ingest a vector with validation, normalization, namespace routing.
- `Search` — k-NN search with **the same validation/normalization pipeline** as ingestion. Guarantees score coherence by construction.

Module-by-module code tour: [`CODE_WALKTHROUGH.md`](CODE_WALKTHROUGH.md). Design trade-offs argued: [`DECISIONS.md`](DECISIONS.md). Performance numbers: [`BENCHES.md`](BENCHES.md).

---

## Performance

- **Hot path**: ~330 ns for 1536 dims (validation + L2 norm² + normalization) on Mac Studio M4 Max; ~1.4 µs on a 2017 Intel Kaby Lake laptop. Methodology and reproducibility in [`BENCHES.md`](BENCHES.md).
- **`l2_norm_squared` throughput**: ~11 Gelem/s on M4 Max, ~2.2 Gelem/s on Kaby Lake. Branchless + 8 parallel accumulators, no `unsafe`, no `-C fast-math`.
- **Tests**: 71 unit + integration + loom concurrency, all green. Zero `unsafe`, zero `unwrap`/`expect` outside `main.rs`, clippy `-D warnings` green, miri green on `math` and `pool`.
- **Docker image**: ~46 MB (distroless/cc `nonroot`, CPU target `x86-64-v3`).

---

## Multi-language clients

The API being standard gRPC, it plugs into any stack. Two reference clients are provided:

- [`samples/clients/python/`](samples/clients/python/) — `grpcio` + `grpcio-tools`. Runtime codegen.
- [`samples/clients/typescript/`](samples/clients/typescript/) — `@grpc/grpc-js` + `@grpc/proto-loader`. Node 22+.

Both run the same sequence (valid Upsert → NaN Upsert rejected → Search), with a distinct `producer_id` that becomes a Prometheus label on the router side. Details in [`samples/clients/README.md`](samples/clients/README.md).

---

## Observability

### HTTP endpoints (port `http_bind`, default 9090)

- `GET /health` — liveness (200 as soon as the process responds).
- `GET /ready` — readiness, queries `VectorDbClient::health()`; 503 if VDB unreachable.
- `GET /metrics` — Prometheus format.

### Exposed metrics (RED + operational)

| Metric | Type | Labels |
|---|---|---|
| `requests_total` | counter | `model_id`, `op` (upsert\|search), `status` (ok\|unknown_model\|invalid_dim\|invalid_numeric\|vdb_error\|internal_error), `producer_id` |
| `request_duration_seconds` | histogram | `model_id`, `op`, `producer_id` |
| `normalizations_performed_total` | counter | `model_id` |
| `misaligned_copies_total` | counter | — |
| `pool_exhausted_total` | counter | — |
| `registered_models` | gauge | — |
| `pool_available` | gauge | — |
| `vdb_inflight` | gauge | — |

Ready-to-import Grafana dashboard: [`docs/grafana-dashboard.json`](docs/grafana-dashboard.json). Panels: RED request rate, error rate, p50/p95/p99 latency, VDB saturation, pool, misaligned copies, normalizations per model.

---

## Build from source

```bash
git clone https://github.com/Adelagric/vector-router.git
cd vector-router

# Native (uses .cargo/config.toml → target-cpu=native)
make build               # release binary in target/release/vector-router
make test                # 71+ tests
make check               # clippy -D warnings + fmt --check
make bench               # reproducible Criterion benchmarks

# Portable Docker image (target-cpu=x86-64-v3)
make docker              # vector-router:<version>

# Advanced verification
make miri                # math + pool modules under miri
make loom                # registry under loom (concurrency)
```

Toolchain pinned via [`rust-toolchain.toml`](rust-toolchain.toml). MSRV: 1.94.

---

## Contributing

Issues, PRs, bug repros and design proposals welcome. No CLA required; by submitting a contribution you license it under Apache 2.0 per clause 5 of the license.

Before opening a PR:
- `make check` must pass (clippy `-D warnings` + fmt).
- `make test` must pass.
- If the PR touches `math.rs` or `pool.rs`: `make miri` must also pass.

---

## License

Vector Router is distributed under the **[Apache License 2.0](LICENSE)**. You can use, modify, embed it in a commercial product, self-host it in production — at no cost.

## Commercial support

If you want production support with an SLA, custom integrations (pgvector, Pinecone, Weaviate, OTLP, dynamic admin endpoints), deployment consulting or configurations specific to your stack, that's the separate paid offering:

**Contact**: [kaleche@gmail.com](mailto:kaleche@gmail.com)

---

*Copyright 2026 Adel Kaleche. Distributed under Apache License 2.0 — see [LICENSE](LICENSE).*
