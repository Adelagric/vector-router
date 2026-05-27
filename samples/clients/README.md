# Multi-language clients

The service exposes a standard gRPC API ([`router.proto`](../../proto/vector_router/v1/router.proto)) — any language with a gRPC runtime can plug in. Two reference clients are provided here, in Python and TypeScript, to show concretely how to integrate vector-router into an existing stack.

| Language | Directory | gRPC runtime | Codegen |
|---|---|---|---|
| Python 3.10+ | [`python/`](python/) | `grpcio` | runtime via `grpcio-tools` (no versioned stubs) |
| TypeScript / Node 22+ | [`typescript/`](typescript/) | `@grpc/grpc-js` | runtime via `@grpc/proto-loader` |

Both clients run the same demo sequence against a vector-router listening on `localhost:50051`:

1. `Upsert` of a valid 1536-dim vector → success, returns the VDB namespace and `was_normalized` flag.
2. `Upsert` of the same vector with an injected `NaN` → rejected with `INVALID_ARGUMENT` by the router, never written to the database.
3. `Search` using the same validation and normalization pipeline → guaranteed score coherence.

Each request carries a `producer_id` field (`python-client`, `ts-client`) that becomes a Prometheus label on the router side. On the Grafana dashboard you immediately see the split of calls by language and the share of rejects per client.

## Quickstart

Minimal stack to reproduce locally (Qdrant + vector-router via compose, see [main README](../../README.md#quickstart--vector-router--qdrant-in-5-minutes)):

### Python

```bash
cd samples/clients/python
pip install -r requirements.txt
python client.py
```

### TypeScript

```bash
cd samples/clients/typescript
npm install
npm run demo
```

## Why gRPC rather than REST

- **Strong schema** — the `.proto` is the source of truth, automatic codegen in every language.
- **Native bytes for vectors** — protobuf `bytes` carries the raw `Float32Array`, not verbose base64-encoded JSON. On a 1536-dim vector, ~3× saving on payload size and zero JSON parsing cost.
- **Streaming** — not used in V1, but opens the door to a streaming batch mode with no breaking change to the API.
- **Service mesh compatible** — Istio, Linkerd and Envoy all have first-class gRPC support (mTLS, retry budget, per-request load balancing).
