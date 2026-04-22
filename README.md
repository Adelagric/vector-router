# vector-router — vitrine technique

> **Note.** Ce repo est un **extrait public** du projet vector-router : documentation d'architecture, benchmarks, contrat d'API gRPC et deux fichiers source représentatifs du style. Le code complet (routage, client VDB, licence, registre, serveur) est distribué sous licence commerciale. Voir [Licensing](#licensing) en bas.

Middleware Rust qui s'intercale entre les producteurs d'embeddings et une base vectorielle (Qdrant aujourd'hui). Il **valide, normalise et route** des vecteurs `f32` déjà calculés, avec des garanties de performance (chemin chaud ~1,4 µs hors réseau, zéro allocation) et d'observabilité (Prometheus + RED).

Le service **ne fait pas d'inférence**. Il ne touche pas aux modèles. Il protège l'intégrité des vecteurs qui entrent dans la base et garantit que les recherches sont effectuées dans les mêmes conditions que l'ingestion.

## Pourquoi

Trois problèmes récurrents dans les pipelines d'embeddings en production :

1. **Vecteurs corrompus en base** — un producteur envoie des NaN, des Inf, des dimensions fausses, qui polluent les index pendant des mois sans qu'on s'en aperçoive.
2. **Scores de recherche biaisés** — la base contient des vecteurs normalisés, mais le vecteur de requête ne l'est pas (ou inversement). Les résultats sont systématiquement faux.
3. **Aucune visibilité centrale** — chaque producteur gère sa propre logique, impossible de savoir ce qui entre dans la base sans auditer chaque service amont.

Le middleware traite les trois en un seul point de contrôle.

## Résultats mesurés

- **Chemin chaud** : ~1,4 µs pour 1536 dims (validation + L2 norm² + normalisation). Détails et méthodo dans [`BENCHES.md`](BENCHES.md).
- **Débit `l2_norm_squared`** : ~2,2 Gélém/s (×6,3 vs la version naïve, par refactor branchless + 8 accumulateurs parallèles, sans `unsafe` ni `-C fast-math`).
- **Tests** : 96 unitaires + intégration + concurrence loom, tous verts. Zéro `unsafe`, zéro `unwrap`/`expect` hors `main.rs`, clippy `-D warnings` vert, miri vert sur modules sensibles.
- **Image Docker** : ~46 Mo (distroless/cc `nonroot`, CPU target `x86-64-v3` pour compatibilité serveurs ≥ 2013).

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
