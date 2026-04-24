# vector-router

# Stop silent embedding corruption.

**La couche de confiance entre vos agents IA et votre base vectorielle.** Un point de contrôle gRPC qui valide, normalise et route chaque vecteur avant qu'il n'entre en base — pour que la mémoire de l'entreprise reste intègre et que les scores de recherche soient cohérents par construction.

> Ce repo est un **extrait public** : architecture, benchmarks, contrat d'API gRPC, 2 fichiers source illustratifs. Le code complet (routage, client VDB, licence, registre, serveur) est en licence commerciale. Voir [Licensing](#licensing).

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

## Scène de crime (histoire vraie, chiffres arrondis)

> Un pipeline RAG nightly bascule — changement interne non coordonné — de `text-embedding-3-large` (3072 dims) vers `text-embedding-3-small` (1536 dims). La base vectorielle en production est configurée pour 3072. Les écritures échouent, l'agent logge une erreur, personne n'a d'alerte dessus.
>
> Pendant **3 semaines**, chaque nuit, ~8 000 documents ne sont pas indexés. Détecté au support client : "les nouveaux docs n'apparaissent pas dans la recherche". Post-mortem + ré-indexation forcée sur 3 mois d'historique : **~47 000 € d'appels OpenAI** + 2 semaines-ingé de remédiation.
>
> Avec vector-router en coupure : la requête aurait été rejetée au premier batch (model_id incohérent avec la dim annoncée), un compteur Prometheus `requests_total{status="invalid_dim",producer_id="rag-nightly"}` serait monté instantanément, alerte PagerDuty à t0.

## Voir la démo (90 s, enregistrée en live)

![Vector Router — démo live](docs/media/demo.svg)

Trois cas, stack réelle (Qdrant + vector-router en docker-compose, mode évaluation 45 jours) :

1. **Vecteur 1536-dim valide** → accepté, routé vers Qdrant, `wasNormalized: true`.
2. **Même vecteur, NaN à l'index 42** → `InvalidArgument: vecteur contient NaN ou Inf`. Jamais écrit en base.
3. **Agent `rag-nightly` envoie un 512-dim au lieu de 1536** → `InvalidArgument: dimension invalide`. Visible dans Prometheus avec le bon `producer_id`.

La démo est reproductible — voir [quickstart](#quickstart--vector-router--qdrant-en-5-minutes) ci-dessous.

---

## Pourquoi

Trois modes d'échec systématiques dans les stacks qui laissent des agents IA écrire directement en base vectorielle — invisibles pendant des mois :

1. **Corruption silencieuse** — un agent utilise le mauvais modèle (1536 dims au lieu de 3072), ou un provider renvoie des `NaN` / `Inf` sur ses batch endpoints. Ces points contaminent l'index ANN de façon permanente.
2. **Scores de recherche biaisés** — les vecteurs stockés sont normalisés par un agent, le vecteur de requête est produit par un autre agent qui ne normalise pas. La similarité cosinus retourne des résultats faux, sans erreur visible.
3. **Aucune attribution** — impossible de savoir quel agent a poussé quel vecteur. Des mois de ré-indexation potentielle à des dizaines de milliers d'euros en appels modèle quand le problème est finalement découvert.

Le middleware traite les trois en un seul point de contrôle. Chaque requête porte un `producer_id` qui devient un label Prometheus — vous voyez précisément quel agent envoie des vecteurs mal formés.

Ce que le service **ne fait pas** : pas d'inférence, pas de cache, pas de transformation de dimension. Il ne touche pas à vos modèles ni à vos agents. Périmètre borné, volontairement.

## Résultats mesurés

- **Chemin chaud** : ~1,4 µs pour 1536 dims (validation + L2 norm² + normalisation). Détails et méthodo dans [`BENCHES.md`](BENCHES.md).
- **Débit `l2_norm_squared`** : ~2,2 Gélém/s (×6,3 vs la version naïve, par refactor branchless + 8 accumulateurs parallèles, sans `unsafe` ni `-C fast-math`).
- **Tests** : 101 unitaires + intégration + concurrence loom + anti-abus licence, tous verts. Zéro `unsafe`, zéro `unwrap`/`expect` hors `main.rs`, clippy `-D warnings` vert, miri vert sur modules sensibles.
- **Image Docker** : ~46 Mo (distroless/cc `nonroot`, CPU target `x86-64-v3` pour compatibilité serveurs ≥ 2013).

## Quickstart — Vector Router + Qdrant en 5 minutes

Vous avez reçu le tarball Tier 1 (binaire Linux) ou Tier 2 (image Docker). Stack minimale pour valider le pipeline :

```yaml
# docker-compose.yml
services:
  qdrant:
    image: qdrant/qdrant:latest
    ports: ["6333:6333", "6334:6334"]

  vector-router:
    image: vector-router:0.1.0          # chargée via: docker load -i vector-router-*.tar.gz
    ports: ["50051:50051", "9090:9090"]
    volumes:
      - ./config.toml:/etc/vector-router/config.toml:ro
      - ./acme.lic:/etc/vector-router/licenses/acme.lic:ro
    depends_on: [qdrant]
```

```toml
# config.toml (minimal)
[server]
grpc_bind = "0.0.0.0:50051"
http_bind = "0.0.0.0:9090"

[vdb]
url = "http://qdrant:6334"
timeout_ms = 500

[license]
path = "/etc/vector-router/licenses/acme.lic"

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

À ce stade le router est en coupure devant Qdrant. Toute requête `Upsert` / `Search` vers `localhost:50051` passe par validation + normalisation + routage. Les rejets apparaissent dans `/metrics` et sur stderr en JSON structuré.

Pas de licence ? Le binaire démarre en **évaluation 45 jours** automatiquement, double ancrage anti-reset — aucun champ à remplir.

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

Les deux requêtes portent un champ `producer_id` optionnel (nom de service, cardinalité bornée) qui devient un label Prometheus — permet d'identifier précisément quel producteur envoie des vecteurs mal formés.

Tour de code module par module : [`CODE_WALKTHROUGH.md`](CODE_WALKTHROUGH.md). Arbitrages de design argumentés : [`DECISIONS.md`](DECISIONS.md).

## Extraits de code

Deux modules représentatifs du style sont fournis dans [`samples/`](samples/) :

- [`samples/pool.rs`](samples/pool.rs) — pool de buffers alignés lock-free avec RAII (`PooledBuffer`), stratégie non-bloquante. `Box<[u32]>` + `bytemuck::cast_slice` pour l'alignement 4 sans `unsafe`.
- [`samples/math.rs`](samples/math.rs) — noyau numérique : `validate_and_align` (zero-copy si aligné, copie dans le pool sinon), `l2_norm_squared` optimisée, `normalize_in_place`. Branchless, 8 accumulateurs parallèles, IEEE 754 stricte.

Ces fichiers ne se compilent pas seuls — ils dépendent du crate complet. Ils sont là pour illustrer :
- le niveau de commentaires des décisions non triviales,
- la structure des tests unitaires (miri, désalignement forcé, propriétés numériques),
- l'usage d'`enum Error` centralisé plutôt que `anyhow`,
- la discipline « pas d'`unsafe`, pas d'`unwrap`/`expect` hors tests/`main.rs` ».

## Observabilité

### Endpoints HTTP

- `GET /health` — liveness.
- `GET /ready` — readiness, interroge `VectorDbClient::health()` ; 503 si VDB indisponible.
- `GET /metrics` — format Prometheus.

### Métriques exposées

Convention RED (Rate / Errors / Duration) + opérationnelles.

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

## Licence d'exécution (runtime)

Le binaire refuse de démarrer sans licence valide ou période d'évaluation active.

### Mode évaluation

Sans section `[license]` ou sans `.lic`, démarrage automatique en évaluation pour **45 jours**. Premier boot horodaté dans `/var/lib/vector-router/eval.json` (double ancrage anti-reset). Warning à J-7, refus de démarrer à J+45.

### Mode licence (production)

`.lic` au format `<base64url(claims_json)>.<base64url(signature)>`. Claims : `customer`, `issued_at`, `expires_at`, `features`. Signature Ed25519 sur les octets ASCII de la partie claims (convention JWS EdDSA). Clé publique de vérification **embarquée dans le binaire** (`include_bytes!`) ; aucun paramètre runtime ne permet d'en changer. Offline-first, zéro phone-home.

Détails des arbitrages (format, anti-abus, chaîne de confiance) : [`DECISIONS.md`](DECISIONS.md) section « Licence Ed25519 ».

## Limites connues

- **Pas de retry configurable par client** : politique globale au niveau handler.
- **OTLP non câblé** : le champ `telemetry.otlp_endpoint` existe en config mais n'est pas branché dans cette version.
- **Pas d'endpoints admin dynamiques** : modèles déclarés en config au démarrage.
- **Benchmarks laptop Intel 2017** : chiffres indicatifs, à rebencher sur hardware de prod avec fréquence CPU fixe pour tout SLA contractuel.

## Licensing

Le code complet est distribué commercialement sous trois formats :

1. **Tier 1 — binaire** : archive `vector-router-x.y.z-linux-x86_64.tar.gz` + `.lic` client.
2. **Tier 1 alternatif — image Docker** : tarball `docker save`, import via `docker load`.
3. **Tier 3 — code source** : archive complète avec droit de recompiler, modifier, signer vos propres licences clients. Paire de clés Ed25519 générée par l'acheteur.

Ce repo public ne contient **pas** de code fonctionnel permettant de reconstruire le binaire. Les fichiers dans `samples/` illustrent le style mais dépendent du crate complet.

**Contact commercial** : [kaleche@gmail.com](mailto:kaleche@gmail.com)

---

*© 2026 Adel Kaleche. Tous droits réservés sur les fichiers de ce repo. Voir [LICENSE](LICENSE).*
