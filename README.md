# vector-router

[![CI](https://github.com/Adelagric/vector-router/actions/workflows/ci.yml/badge.svg)](https://github.com/Adelagric/vector-router/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-stable-orange?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache_2.0-blue)](LICENSE)

# Stop silent embedding corruption.

**La couche de confiance entre vos agents IA et votre base vectorielle.** Un point de contrôle gRPC qui valide, normalise et route chaque vecteur avant qu'il n'entre en base — pour que la mémoire d'entreprise reste intègre et que les scores de recherche soient cohérents par construction.

Middleware Rust open source (Apache 2.0). Pas d'inférence, pas de modèles. Périmètre borné, volontairement.

---

## Avant / après

```
SANS vector-router                         AVEC vector-router
────────────────────                       ────────────────────
✗ dimensions silencieusement fausses       ✓ rejet immédiat, erreur gRPC explicite
✗ NaN / Inf qui polluent l'index ANN       ✓ rejet avant écriture, 0 contamination
✗ scores de similarité biaisés             ✓ normalisation L2 uniforme ingest + search
✗ "c'est quel agent qui push ?"            ✓ label Prometheus par producer_id
✗ debugging à l'aveugle                    ✓ dashboard Grafana RED clé en main
```

## Démo (90 s, enregistrée en live)

![Vector Router — démo live](docs/media/demo.svg)

Trois cas, stack réelle (Qdrant + vector-router en docker-compose) :

1. **Vecteur 1536-dim valide** → accepté, routé vers Qdrant, `wasNormalized: true`.
2. **Même vecteur, NaN à l'index 42** → `InvalidArgument: vecteur contient NaN ou Inf`. Jamais écrit en base.
3. **Agent `rag-nightly` envoie un 512-dim au lieu de 1536** → `InvalidArgument: dimension invalide`. Visible dans Prometheus avec le bon `producer_id`.

## Scène de crime (histoire vraie, chiffres arrondis)

> Un pipeline RAG nightly bascule — changement interne non coordonné — de `text-embedding-3-large` (3072 dims) vers `text-embedding-3-small` (1536 dims). La base vectorielle en production est configurée pour 3072. Les écritures échouent, l'agent logge une erreur, personne n'a d'alerte dessus.
>
> Pendant **3 semaines**, chaque nuit, ~8 000 documents ne sont pas indexés. Détecté au support client : "les nouveaux docs n'apparaissent pas dans la recherche". Post-mortem + ré-indexation forcée sur 3 mois d'historique : **~47 000 € d'appels OpenAI** + 2 semaines-ingé de remédiation.
>
> Avec vector-router en coupure : la requête aurait été rejetée au premier batch (model_id incohérent avec la dim annoncée), un compteur Prometheus `requests_total{status="invalid_dim",producer_id="rag-nightly"}` serait monté instantanément, alerte PagerDuty à t0.

---

## Pourquoi

Trois modes d'échec systématiques dans les stacks qui laissent des producteurs d'embeddings écrire directement en base vectorielle — invisibles pendant des mois :

1. **Corruption silencieuse** — un agent utilise le mauvais modèle (1536 dims au lieu de 3072), ou un provider renvoie des `NaN` / `Inf` sur ses batch endpoints. Ces points contaminent l'index ANN de façon permanente.
2. **Scores de recherche biaisés** — les vecteurs stockés sont normalisés par un agent, le vecteur de requête est produit par un autre agent qui ne normalise pas. La similarité cosinus retourne des résultats faux, sans erreur visible.
3. **Aucune attribution** — impossible de savoir quel agent a poussé quel vecteur. Des mois de ré-indexation potentielle à des dizaines de milliers d'euros en appels modèle quand le problème est finalement découvert.

Le middleware traite les trois en un seul point de contrôle. Chaque requête porte un `producer_id` qui devient un label Prometheus — vous voyez précisément quel agent envoie des vecteurs mal formés.

---

## Quickstart — Vector Router + Qdrant en 5 minutes

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
# config.toml — voir config.example.toml pour les options complètes
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
curl -s http://localhost:9090/metrics | head  # métriques Prometheus live
```

À ce stade, toute requête `Upsert` / `Search` vers `localhost:50051` passe par validation + normalisation + routage. Les rejets apparaissent dans `/metrics` et sur stderr en JSON structuré.

Tour d'horizon plus complet de l'opérateur dans [`GETTING_STARTED.md`](GETTING_STARTED.md).

---

## Architecture

```
[Producteurs d'embeddings] ── gRPC ─▶ [vector-router] ── gRPC ─▶ [Qdrant]
                                           │
                                           ▼
                                  Prometheus /metrics
                                  Axum HTTP :9090
```

Deux RPC exposés (voir [`proto/vector_router/v1/router.proto`](proto/vector_router/v1/router.proto)) :

- `Upsert` — ingestion d'un vecteur avec validation, normalisation, routage namespace.
- `Search` — recherche k-NN avec **le même pipeline de validation/normalisation** que l'ingestion. Garantie de cohérence des scores.

Tour de code module par module : [`CODE_WALKTHROUGH.md`](CODE_WALKTHROUGH.md). Arbitrages de design argumentés : [`DECISIONS.md`](DECISIONS.md). Chiffres de performance : [`BENCHES.md`](BENCHES.md).

---

## Performance

- **Chemin chaud** : ~330 ns pour 1536 dims (validation + L2 norm² + normalisation) sur Mac Studio M4 Max ; ~1,4 µs sur laptop Intel Kaby Lake 2017. Méthodo et reproductibilité dans [`BENCHES.md`](BENCHES.md).
- **Débit `l2_norm_squared`** : ~11 Gélém/s sur M4 Max, ~2,2 Gélém/s sur Kaby Lake. Branchless + 8 accumulateurs parallèles, sans `unsafe` ni `-C fast-math`.
- **Tests** : 71 unitaires + intégration + concurrence loom, tous verts. Zéro `unsafe`, zéro `unwrap`/`expect` hors `main.rs`, clippy `-D warnings` vert, miri vert sur `math` et `pool`.
- **Image Docker** : ~46 Mo (distroless/cc `nonroot`, CPU target `x86-64-v3`).

---

## Clients multi-langages

L'API étant gRPC standard, elle s'intègre dans n'importe quelle stack. Deux clients de référence sont fournis :

- [`samples/clients/python/`](samples/clients/python/) — `grpcio` + `grpcio-tools`. Codegen runtime.
- [`samples/clients/typescript/`](samples/clients/typescript/) — `@grpc/grpc-js` + `@grpc/proto-loader`. Node 22+.

Les deux exécutent la même séquence (Upsert valide → Upsert NaN rejeté → Search), avec un `producer_id` distinct qui devient un label Prometheus côté router. Détails dans [`samples/clients/README.md`](samples/clients/README.md).

---

## Observabilité

### Endpoints HTTP (port `http_bind`, défaut 9090)

- `GET /health` — liveness (200 dès que le process répond).
- `GET /ready` — readiness, interroge `VectorDbClient::health()` ; 503 si VDB indisponible.
- `GET /metrics` — format Prometheus.

### Métriques exposées (RED + opérationnelles)

| Métrique | Type | Labels |
|---|---|---|
| `requests_total` | counter | `model_id`, `op` (upsert\|search), `status` (ok\|unknown_model\|invalid_dim\|invalid_numeric\|vdb_error\|internal_error), `producer_id` |
| `request_duration_seconds` | histogram | `model_id`, `op`, `producer_id` |
| `normalizations_performed_total` | counter | `model_id` |
| `misaligned_copies_total` | counter | — |
| `pool_exhausted_total` | counter | — |
| `registered_models` | gauge | — |
| `pool_available` | gauge | — |
| `vdb_inflight` | gauge | — |

Dashboard Grafana prêt à importer : [`docs/grafana-dashboard.json`](docs/grafana-dashboard.json). Panneaux : taux de requêtes RED, taux d'erreurs, latences p50/p95/p99, saturation VDB, pool, copies désalignées, normalisations par modèle.

---

## Build depuis les sources

```bash
git clone https://github.com/Adelagric/vector-router.git
cd vector-router

# Native (utilise .cargo/config.toml → target-cpu=native)
make build               # release binary dans target/release/vector-router
make test                # 71+ tests
make check               # clippy -D warnings + fmt --check
make bench               # Criterion benchmarks reproductibles

# Image Docker portable (target-cpu=x86-64-v3)
make docker              # vector-router:<version>

# Vérifications avancées
make miri                # modules math + pool sous miri
make loom                # registry sous loom (concurrence)
```

Toolchain pinnée via [`rust-toolchain.toml`](rust-toolchain.toml). MSRV : 1.94.

---

## Contribuer

Issues, PRs, repros de bugs et propositions de design bienvenues. Aucun CLA requis ; en soumettant une contribution vous l'autorisez sous Apache 2.0 conformément à la clause 5 de la licence.

Avant un PR :
- `make check` doit passer (clippy `-D warnings` + fmt).
- `make test` doit passer.
- Si la PR touche `math.rs` ou `pool.rs` : `make miri` doit aussi passer.

---

## Licence

Vector Router est distribué sous **[Apache License 2.0](LICENSE)**. Vous pouvez l'utiliser, le modifier, l'embarquer dans un produit commercial, l'auto-héberger en production — sans contrepartie financière.

## Support commercial

Si vous voulez du support production avec SLA, des intégrations sur mesure (pgvector, Pinecone, Weaviate, OTLP, endpoints admin dynamiques), du conseil en déploiement ou des configurations spécifiques à votre stack, c'est l'objet de l'offre séparée :

**Contact** : [kaleche@gmail.com](mailto:kaleche@gmail.com)

---

*Copyright 2026 Adel Kaleche. Distribué sous Apache License 2.0 — voir [LICENSE](LICENSE).*
