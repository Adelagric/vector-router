# Vector Router — Getting started

**The trust layer between your AI agents and your vector database.**
Vector Router sits between embedding producers (LLM agents, RAG pipelines,
indexing jobs) and your vector database. It validates, normalizes and
routes every vector before it enters the database — preventing silent
corruption of enterprise memory and surfacing pipeline inconsistencies
(wrong model, NaN/Inf, unnormalized vectors, offending producer).

Operator-oriented guide for production deployment. Target time-to-first-
request: **30 minutes** from build to the first routed request.

This document assumes you have access to the repo (`git clone` + `make build`
/ `make docker`) or a pre-built Docker image.

---

## 1. Prerequisites

**Platform.** Linux x86-64, glibc ≥ 2.31 (Ubuntu 20.04+, Debian 11+, RHEL 9+).
The binary is compiled for `x86-64-v3` (Haswell 2013 and newer). Any server
CPU bought after 2014 will work.

**Vector backend.** One of:

- **Qdrant** (default) — a reachable Qdrant 1.12+ instance; its URL goes into `config.toml`.
- **PostgreSQL + pgvector** — Postgres with the `pgvector` extension, with the router built `--features pgvector`. The router auto-creates one table and HNSW index per model namespace on startup, so the connecting role needs `CREATE` on the target database (and the privilege to `CREATE EXTENSION vector` the first time, unless `vector` is already installed). For a least-privilege setup, pre-provision once with [`sql/pgvector_schema.sql`](sql/pgvector_schema.sql) using an admin role, then run the router with a restricted one.

**Ports.**

| Port     | Role        | Who accesses it                     |
|----------|-------------|--------------------------------------|
| `50051`  | gRPC        | Your embedding producers             |
| `9090`   | HTTP        | Prometheus, Grafana, k8s probes      |

Both are configurable. The binary makes no outbound traffic other than the
Qdrant connection.

**Permissions.** No writable disk path is required by default — the binary
runs stateless as long as the vector backend is reachable.

---

## 2. Installation

```bash
# 1. Build (from source)
git clone https://github.com/Adelagric/vector-router.git
cd vector-router
make build               # produces target/release/vector-router (Qdrant backend)
# For the pgvector backend instead:
#   cargo build --release --no-default-features --features pgvector

# 2. Install the binary
sudo install -m 755 target/release/vector-router /usr/local/bin/

# 3. Config
sudo mkdir -p /etc/vector-router
sudo cp config.example.toml /etc/vector-router/config.toml
sudo chmod 640 /etc/vector-router/config.toml
# Edit config.toml — see section 3.
```

For a Docker deployment, skip this section and jump straight to 4.b.

---

## 3. Minimal configuration

Open `/etc/vector-router/config.toml`. The **four** fields to verify first:

```toml
[server]
grpc_bind = "0.0.0.0:50051"
http_bind = "0.0.0.0:9090"

[vdb]
url = "http://your-qdrant.internal:6334"
timeout_ms = 500

[admin]
# Must be replaced before going to production. Ideally injected by your
# secrets manager (Vault, AWS Secrets Manager, etc.).
bearer_token = "CHANGE-ME"

[models."openai-text-embedding-3-small"]
dim = 1536
normalize = true
vdb_namespace = "prod-openai-small"
```

**Models.** Every model that your producers use must be declared here. An
undeclared model = request rejected with `UnknownModel`. This is deliberate
— no auto-discovery, so the database can never be filled with a model you
have not validated.

**Environment variables.** Any config value can be overridden via a `VR_`-
prefixed env var, with `__` as sub-field separator:

```bash
VR_ADMIN__BEARER_TOKEN="$(vault kv get -field=token ...)" \
VR_VDB__URL="http://qdrant.prod:6334" \
/usr/local/bin/vector-router
```

Useful for keeping secrets out of the committed TOML.

**Using pgvector.** Set `backend = "pgvector"` and put a Postgres connection
string in `url`; each model's `vdb_namespace` then names a table (one table per
model dimension, auto-created on boot). Optional knobs: `ef_search` (HNSW
recall/latency, applied per query via `SET LOCAL`) and `max_connections` (sqlx
pool size). Full reference in `config.example.toml`.

```toml
[vdb]
backend = "pgvector"
url = "postgres://vr:secret@postgres.internal:5432/vectors"
ef_search = 80
```

---

## 4. First boot

### 4.a. Native binary

```bash
VR_CONFIG_PATH=/etc/vector-router/config.toml /usr/local/bin/vector-router
```

### 4.b. Docker image (recommended for k8s / CI)

If you have built the Docker image via `make docker`:

```bash
docker run -d --name vector-router \
  -p 50051:50051 \
  -p 9090:9090 \
  -v /etc/vector-router/config.toml:/etc/vector-router/config.toml:ro \
  -e VR_CONFIG_PATH=/etc/vector-router/config.toml \
  vector-router:X.Y.Z
```

**Pitfall to avoid** — mount `config.toml` **as a file**, not as a
directory, so that no later mount can mask the file.

To let the container reach a Qdrant running on the host (local dev),
replace `url = "http://qdrant:6334"` with `url = "http://host.docker.internal:6334"`
in `config.toml`.

In Kubernetes, ConfigMap and Secret map naturally: each produces an
individual file, so the directory-mount pitfall does not show up.

Expected stderr output:

```
vector-router : chargement config depuis /etc/vector-router/config.toml
vector-router : recorder Prometheus installé
vector-router : serveurs démarrés (gRPC 0.0.0.0:50051, HTTP 0.0.0.0:9090)
```

---

## 5. End-to-end verification

### 5.1 HTTP probes

```bash
curl http://localhost:9090/health
# => "ok"  (200)

curl http://localhost:9090/ready
# => "ready"  (200) if Qdrant is reachable
# => "vdb indisponible : ..." (503) otherwise
```

### 5.2 First gRPC call

Install `grpcurl` (`brew install grpcurl`, `apt install grpcurl`, or
<https://github.com/fullstorydev/grpcurl/releases>).

Vector encoding: `f32` little-endian bytes must be base64-encoded for the
`bytes` protobuf field. The 4-dim vector `[1.0, 0.0, 0.0, 0.0]` becomes
`AACAPwAAAAAAAAAAAAAAAA==`.

```bash
grpcurl -plaintext \
  -proto proto/vector_router/v1/router.proto \
  -d '{
        "model_id": "openai-text-embedding-3-small",
        "point_id": "hello-world",
        "vector": "'"$(python3 -c 'import struct,base64; print(base64.b64encode(struct.pack("<1536f", *([1.0]+[0.0]*1535))).decode())')"'",
        "dim": 1536,
        "producer_id": "install-test"
      }' \
  localhost:50051 \
  vector_router.v1.VectorRouter/Upsert
```

Expected response (indicative timing):
```json
{
  "pointId": "hello-world",
  "processingUs": "87",
  "wasNormalized": true,
  "vdbNamespace": "prod-openai-small"
}
```

### 5.3 Prometheus metrics

```bash
curl -s http://localhost:9090/metrics | grep requests_total
```

Should contain at least one line of the form:
```
requests_total{model_id="openai-text-embedding-3-small",op="upsert",status="ok",producer_id="install-test"} 1
```

If this is green, the pipeline is operational.

---

## 6. Running in production

### 6.1 systemd

File `/etc/systemd/system/vector-router.service`:

```ini
[Unit]
Description=Vector Router
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=vector-router
Group=vector-router
Environment=VR_CONFIG_PATH=/etc/vector-router/config.toml
EnvironmentFile=-/etc/vector-router/secrets.env
ExecStart=/usr/local/bin/vector-router
Restart=on-failure
RestartSec=5
KillSignal=SIGTERM
TimeoutStopSec=35

# Hardening
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
NoNewPrivileges=yes
ReadWritePaths=/var/lib/vector-router

[Install]
WantedBy=multi-user.target
```

`secrets.env` (mode 600) typically contains `VR_ADMIN__BEARER_TOKEN=...`
and, if applicable, `VR_VDB__API_KEY=...`.

```bash
sudo useradd --system --no-create-home vector-router
sudo chown -R vector-router:vector-router /var/lib/vector-router
sudo systemctl daemon-reload
sudo systemctl enable --now vector-router
sudo journalctl -u vector-router -f
```

### 6.2 Kubernetes

Expose the standard probes:

```yaml
livenessProbe:
  httpGet: { path: /health, port: 9090 }
  periodSeconds: 10
readinessProbe:
  httpGet: { path: /ready, port: 9090 }
  periodSeconds: 5
```

`/ready` returns 503 if Qdrant becomes unreachable — the load balancer
automatically removes the pod from rotation.

### 6.3 Graceful shutdown

The binary handles SIGTERM. The sequence:
1. Stops accepting new gRPC connections.
2. Drains in-flight requests (30 s timeout).
3. Clean exit.

Never interrupt it via `kill -9` in production — in-flight requests lose
their response.

---

## 7. Observability

### 7.1 Grafana dashboard

Import `docs/grafana-dashboard.json` into Grafana 10+.
Prometheus source pointed at `http://<host>:9090/metrics` (or your
existing scrape job).

Key panels:
- **RED request rate** per model and per `producer_id`
- **Error rate** by status (unknown_model, invalid_dim, vdb_error, etc.)
- **p50/p95/p99 latency** per model
- **VDB saturation** (`vdb_inflight`)
- **Memory pool** (`pool_available`, `pool_exhausted_total`)
- **Vector misalignment** (`misaligned_copies_total`)

### 7.2 Rejection journal

Every request rejected at validation produces a JSON line on **stderr**
(separate from Prometheus metrics, which aggregate):

```json
{"event":"rejection","op":"upsert","producer_id":"batch-nightly","model_id":"openai-small","status":"invalid_dim","reason":"dimension invalide : attendu 6144, reçu 4096"}
```

To pinpoint the offending producer when a Prometheus metric rises:

```bash
journalctl -u vector-router --since "10 min ago" \
  | grep '"event":"rejection"' \
  | jq -r 'select(.status=="invalid_dim") | .producer_id' \
  | sort | uniq -c | sort -rn
```

### 7.3 What should worry you

| Signal                              | Typical threshold | Action                        |
|-------------------------------------|-------------------|-------------------------------|
| `misaligned_copies_total` / total   | > 1 %             | Producer is sending misaligned buffers — investigate client side |
| `pool_exhausted_total`              | > 0               | Under-dimensioned — increase `pool.buffers_per_worker` |
| `request_duration` p99              | > 1 ms            | Qdrant network or VDB saturation — check `vdb_inflight` |
| `requests_total{status="vdb_error"}`| any sustained > 0 | Qdrant unstable or timeout too low |

---

## 8. Troubleshooting

### "vector-router : chargement config depuis config.toml" then crash

The binary looks for `config.toml` in the current directory by default.
Set `VR_CONFIG_PATH` or launch it from the right directory.

### `/ready` returns 503

Qdrant is not reachable from the container / host. Verify:

```bash
curl -v http://<qdrant-host>:6334/readyz
```

If it's a latency issue, raise `vdb.timeout_ms` in the config. Default is
500 ms — fine on LAN, raise it for cross-region.

### `pool_exhausted_total` climbing

The memory pool is sized via `pool.buffers_per_worker` (default 2). If
your load sends more than 2N concurrent requests in burst (N = number of
tokio workers), grow the pool. The fallback is transparent (ad-hoc
allocation) but more expensive.

### `DeadlineExceeded` error on the gRPC client

Your client has a shorter timeout than the queue-processing time. Raise
the client timeout, or grow `server.max_concurrent_requests`.

### "modèle inconnu : xxx"

The model is not declared in the config. Add a `[models."xxx"]` section
and restart. No auto-discovery — by design, to prevent any producer from
registering arbitrary models.

---

## 9. Support

For anything not covered above:

- **Useful logs**: `journalctl -u vector-router --since "1 hour ago"`
- **Metric snapshot**: `curl -s http://localhost:9090/metrics > /tmp/metrics.txt`
- **Version**: `/usr/local/bin/vector-router --version` (if compiled with
  the flag) or inspect the binary's `sha256sum`

Contact: Adel Kaleche — <kaleche@gmail.com> — +33 7 80 76 06 71
