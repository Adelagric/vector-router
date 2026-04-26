# Clients multi-langages

Le service expose une API gRPC standard ([`router.proto`](../../proto/vector_router/v1/router.proto)) — n'importe quel langage avec un runtime gRPC peut s'y brancher. Deux clients de référence sont fournis ici, en Python et en TypeScript, pour montrer concrètement comment intégrer vector-router dans une stack existante.

| Langage | Dossier | Runtime gRPC | Codegen |
|---|---|---|---|
| Python 3.10+ | [`python/`](python/) | `grpcio` | runtime via `grpcio-tools` (pas de stubs versionnés) |
| TypeScript / Node 22+ | [`typescript/`](typescript/) | `@grpc/grpc-js` | runtime via `@grpc/proto-loader` |

Les deux clients exécutent la même séquence de démo contre un vector-router en écoute sur `localhost:50051` :

1. `Upsert` d'un vecteur 1536-dim valide → succès, retour du namespace VDB et flag `was_normalized`.
2. `Upsert` du même vecteur avec un `NaN` injecté → rejeté avec `INVALID_ARGUMENT` côté router, jamais écrit en base.
3. `Search` avec le même pipeline de validation et de normalisation → garantie de cohérence des scores.

Chaque requête porte un champ `producer_id` (`python-client`, `ts-client`) qui devient un label Prometheus côté router. Sur le dashboard Grafana, vous voyez immédiatement la répartition des appels par langage et la part de rejets par client.

## Quickstart

Stack minimale pour reproduire localement (Qdrant + vector-router en compose, voir [README principal](../../README.md#quickstart--vector-router--qdrant-en-5-minutes)) :

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

## Pourquoi gRPC plutôt que REST

- **Schéma fort** — le `.proto` est la source de vérité, codegen automatique dans tous les langages.
- **Bytes natifs pour les vecteurs** — `bytes` protobuf transporte le `Float32Array` brut, pas du JSON base64 verbeux. Sur un vecteur 1536-dim, gain ~3× sur la taille du payload et zéro coût de parsing JSON.
- **Streaming** — non utilisé en V1, mais ouvre la porte à un mode batch streaming sans rupture d'API.
- **Compatible service mesh** — Istio, Linkerd, Envoy ont tous un support gRPC de premier ordre (mTLS, retry budget, load balancing par requête).
