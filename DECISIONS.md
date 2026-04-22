# Décisions de conception

Chaque entrée datée, motivée, et placée ici pour éviter de rejouer les mêmes arbitrages à froid.

## 2026-04-17

### Cargo workspace layout : lib + bin dans un seul crate
Motif : `cargo build` avec `-D warnings` traite les fonctions non utilisées par `main.rs` comme du dead code. Exposer les modules en `pub mod` via `src/lib.rs` les rend API publique de la bibliothèque, ce qui stoppe les warnings et garde l'architecture propre (la bibliothèque sera aussi utilisable dans les tests d'intégration de `tests/`).

### Box sur `figment::Error` dans l'enum `Error`
Motif : clippy déclenche `result_large_err` dès qu'un `Result<_, Error>` est retourné. `figment::Error` pèse ~200 octets à cause de son `Vec` de traces de contexte, ce qui fait gonfler tout appelant. Le Box limite l'overhead à 8 octets. Perdu : un déréférencement supplémentaire sur le chemin d'erreur (négligeable, c'est le chemin d'exception).

### tonic 0.14 vs 0.13 et découplage prost
Motif : tonic 0.14 a scindé le support prost en deux crates — `tonic-prost` (runtime) et `tonic-prost-build` (codegen). On cible 0.14.5 pour profiter des améliorations et rester sur la génération la plus récente. L'utilisation : `tonic-prost-build::configure().compile_protos(...)` dans `build.rs`.

### `target-cpu=native` dans `.cargo/config.toml`
Motif : autorise l'auto-vectorisation SIMD via AVX2/AVX512 selon la CPU de build. Inconvénient : le binaire n'est plus portable entre CPU families. Acceptable pour dev local ; pour Docker, on devra fixer une target explicite à l'étape 11.

## 2026-04-18

### `rcu` (read-copy-update) plutôt que `Mutex<HashMap>` pour les updates du registre
Motif : `ArcSwap::rcu` donne des updates lock-free avec retry implicite sur contention. Pour des mises à jour rares (admin, quelques par heure au plus) et des lectures fréquentes (chaque requête gRPC), c'est strictement supérieur à un `RwLock` — zéro attente côté lecteurs, contention nulle en régime nominal. Prix payé : la closure d'update peut être appelée plusieurs fois si un autre thread update en parallèle, mais c'est bénin (même résultat final).

### Test loom découplé de la primitive ArcSwap
Motif : `arc-swap` a sa propre couverture loom interne. Reproduire cette couverture dans notre crate est coûteux et redondant. À la place, le test loom valide le *pattern* concurrent (snapshot via clone d'Arc + update via remplacement atomique) sur un modèle simplifié `Mutex<Arc<T>>`. Ce qu'on teste : notre logique de registre n'introduit pas de race au-delà de ce que la primitive garantit.

### `AlignedBuffer` via `Box<[u32]>` + `bytemuck::cast_slice`
Motif : garantir un alignement 4 sur un buffer `u8` nécessite soit `unsafe` (interdit), soit un `repr(align(4))` wrapper, soit un stockage sur un type naturellement aligné (u32, f32, etc.). Le stockage u32 est le plus simple, sans unsafe, et la conversion vers `&[u8]` / `&[f32]` passe par `bytemuck` qui est safe. Coût : rien (u32 et u8 ont la même représentation mémoire).

### `PooledBuffer` : `mem::take` avec sentinelle `AlignedBuffer::default()` au lieu de `Option`
Motif : l'invariant « PooledBuffer possède toujours un buffer valide jusqu'à Drop » s'exprime naturellement avec un champ `buffer: AlignedBuffer` (pas `Option<...>`). Dans Drop, `mem::take` échange le buffer avec un `AlignedBuffer::default()` (empty slice, zéro alloc). Ça évite un `Option::unwrap` / `.expect` dans `Deref`, qui serait interdit par les règles du projet.

### Miri + alignement stack
Motif (trace d'un bug rencontré) : miri ne garantit pas les alignements stack au-delà de ce que le type demande. Un `[u8; 17]` n'est aligné qu'à 1 octet sous miri, même si stack-allocator natif l'alignerait à 16. Les tests de désalignement doivent donc forcer un alignement de base via un type u32 (ou repr(align(4))), puis prendre un offset de 1 à l'intérieur.

### Benches étendus comme « tests » pour la règle unwrap/expect
Motif : le brief interdit `unwrap`/`expect` hors `main.rs`, `tests/`, `build.rs`. Les fichiers `benches/` sont sémantiquement des tests (de performance). On étend la règle à benches/, ce qui permet l'usage idiomatique de `unwrap` pour les préconditions de bench et l'assertion de sanité.

### Réécriture de `l2_norm_squared` avec huit accumulateurs parallèles (branchless + déroulage manuel)
Motif : la version initiale fusionnait validation NaN/Inf et somme des carrés dans une boucle unique avec branche (`if !x.is_finite() return Err`). Cette branche empêchait l'auto-vectorisation SIMD par LLVM, donnant ~4.4 µs pour 1536 dims (vitesse scalaire pure, ~3 % du pic AVX2). Deux itérations successives : (1) passage branchless en exploitant la propagation IEEE 754 (NaN/Inf propagent à travers `*` et `+`), vérification de la finitude de la somme en sortie → gain ×2.1. (2) huit accumulateurs indépendants via `chunks_exact(8)` et déroulage manuel, pour briser la chaîne de dépendance séquentielle de la réduction sans violer l'associativité stricte IEEE 754 → gain supplémentaire ×3, total ×6.3. Résultat : 702 ns médiane pour 1536 dims, débit ~2.2 Gélém/s (voir `BENCHES.md`). Aucun `unsafe`, aucun recours à `-C fast-math`, miri vert, correction numérique préservée.

### Dockerfile multi-stage avec `gcr.io/distroless/cc-debian12:nonroot`
Motif : image de base minimale (~27 Mo), pas de shell, pas de package manager — surface d'attaque réduite. Variante `:nonroot` pour respecter le principe du moindre privilège en conteneur. Alternative envisagée (distroless/static-debian12) rejetée : demanderait de compiler en musl avec target `-musl`, lourd et bénéfice marginal vu qu'on a déjà besoin de `libc` pour les déps transitives.

### Désactivation de `target-cpu=native` dans le build Docker
Motif : `.cargo/config.toml` local utilise `-C target-cpu=native` pour exploiter les features CPU du builder (AVX2/FMA). Un binaire produit avec ce flag ne tournerait que sur une CPU identique. Dans le Dockerfile, on remplace explicitement ce fichier par un équivalent `-C target-cpu=x86-64-v3` (Haswell+, couvre la quasi-totalité des CPU serveurs depuis 2013). Compromis : on perd potentiellement 10-20 % de perf SIMD vs native sur un serveur récent, pour la portabilité du binaire.

### Shutdown via `tokio::sync::broadcast` plutôt que `Notify` ou `CancellationToken`
Motif : trois tâches indépendantes (gRPC, HTTP, gauge updater) doivent recevoir le signal d'arrêt simultanément. `broadcast::Sender<()>` offre ça nativement : un `.send(())` réveille tous les `.subscribe()`. `Notify::notify_waiters()` pose un problème de fenêtre (les subscribers qui s'abonnent après notify ne voient rien), `tokio_util::sync::CancellationToken` ajouterait une dépendance pour un usage trivial. `broadcast` coche toutes les cases : natif tokio, multi-consumer, un seul send.

Variantes testées : `service_starts_and_shuts_down_cleanly` (flux nominal), `shutdown_propagates_to_all_tasks_in_order` (drain < 1s alors que le gauge updater tick à 5s — prouve que `tokio::select!` interrompt bien), `drain_timeout_is_reported` (force un scénario tâche-qui-hang via `shutdown_tx.clone()` gardé alive, vérifie que le timeout remonte une erreur explicite).

### `start_service` vs `start_service_with_vdb` — factorisation pour testabilité
Motif : la version prod instancie `QdrantVdbClient` à partir de la config, ce qui rend le code non testable sans une vraie instance Qdrant. Extraction d'une variante `start_service_with_vdb(…, vdb: Arc<dyn VectorDbClient>, …)` qui accepte un VDB préconstruit. Les tests injectent `MockVdbClient` ; la prod passe par la première variante qui compose les deux. Pas de logique dupliquée, pas de generics inutiles, aucune régression sur l'API publique.

### Métrique de saturation VDB via `inflight()` sur le trait (pas d'accès aux pools internes)
Motif : `qdrant-client` n'expose pas l'état de son pool de connexions (géré par hyper/tonic en interne, pas d'API publique). Pour obtenir une métrique de saturation utilisable en prod, ajout d'une méthode `fn inflight(&self) -> u64` au trait `VectorDbClient` avec implémentation par défaut (retourne 0 pour backends qui ne tracent pas). `QdrantVdbClient` la surcharge via son `AtomicU64` incrémenté/décrémenté par `InflightGuard` (RAII). C'est une métrique applicative ("combien d'appels le middleware a envoyé et attend"), pas le vrai nombre de connexions gRPC — mais suffisant pour détecter la pression downstream dans Grafana.

### Gauges mis à jour par tâche périodique (5s) plutôt qu'à chaque événement
Motif : `registered_models`, `pool_available`, `vdb_inflight` sont des valeurs lues depuis l'état applicatif (`Registry::len`, `BufferPool::available`, `VectorDbClient::inflight`). Les émettre à chaque changement multiplierait le coût sans bénéfice (Prometheus scrape au mieux toutes les 10-15s). Choix : tâche background `telemetry::run_gauge_updater` qui lit et pousse toutes les 5s. Latence d'observation max ~5s, largement suffisante pour un dashboard.

### Fix : routage `point_id` → `Num` ou `Uuid` selon le format (bug révélé par stress test 2026-04-20)
**Bug** : dans l'implémentation Qdrant initiale, `params.point_id` (String) était systématiquement enveloppé dans `PointIdOptions::Uuid(...)`. Qdrant n'accepte que deux formats d'ID : `u64` ou UUID en string. Un `point_id` arbitraire type `"doc-bench"` faisait échouer la requête côté Qdrant avec `"Unable to parse UUID"`, remonté en `Error::Vdb(...)` avec status gRPC `Unavailable`.

**Non détecté par les tests** : le `MockVdbClient` accepte n'importe quelle String comme `point_id`. Aucune validation du format côté mock. Les 72 tests (unit + intégration + spike + miri) étaient tous verts malgré ce bug.

**Révélé par** : un stress test avec `ghz` contre un vrai Qdrant en Docker local. 2000 requêtes, 100 % `Unavailable`, avec le message Qdrant explicite en clair dans la réponse.

**Fix** : fonction `point_id_from_string(s: String) -> PointId` qui tente `s.parse::<u64>()` d'abord. Si ça passe → `PointIdOptions::Num(n)`. Sinon → `PointIdOptions::Uuid(s)`. Qdrant valide ensuite lui-même si c'est un UUID valide, et remonte son erreur au client si ce n'est ni u64 ni UUID — on ne masque rien côté middleware.

**Leçon documentée** : les tests avec mock ne couvrent que la logique applicative, pas les contrats externes. Tout composant qui parle à une dépendance externe critique (VDB, LLM, broker) doit avoir au moins un test d'intégration contre la vraie chose en CI. À ajouter dans une itération future : un test `#[cfg(feature = "integration-qdrant")]` qui spin-up Qdrant en testcontainers et exerce les cas limites (ID numérique, UUID valide, string arbitraire, metadata complexe).

**Tests ajoutés** : `point_id_from_numeric_string_becomes_num`, `point_id_from_uuid_string_becomes_uuid`, `point_id_from_arbitrary_string_becomes_uuid_then_rejected_by_qdrant` dans `src/client/qdrant.rs`.

### Dashboard HTML minimal embarqué à `/dashboard` — révision du 2026-04-20
Décision initiale (voir ci-dessous) : pas de dashboard HTML embarqué, Grafana comme seul outil de visualisation. Cette règle tenait tant que le but était la production.

**Révision** : après validation du projet, le besoin commercial est apparu — un artefact de démo visuel pour les appels prospect de 30 min, où Grafana demande un setup trop lourd (Prometheus + datasource + import JSON). Ajout d'une page HTML statique servie à `GET /dashboard`, embarquée via `include_str!("../../static/dashboard.html")`.

Garde-fous pour éviter de retomber dans les pièges de la décision initiale :
- **Contenu 100 % truthful** : métriques RED temps réel scrapées depuis `/metrics`, chiffres de tests et benches hardcodés à jour. Zéro KPI inventé type "Bypass ROI".
- **Pas d'input utilisateur affiché** : surface XSS nulle, la page ne lit que des noms de métriques Prometheus déjà produits par l'application elle-même.
- **Impact taille binaire** : +11 Ko dans l'image Docker. Zéro impact pratique.
- **Positionnement explicite** : le footer de la page indique « Tableau de bord local pour démonstration · Observabilité production via Grafana ». Jamais présenté comme un remplacement de Grafana.

Fichier source : `static/dashboard.html` (HTML + CSS + JS inline, zéro dépendance externe). Endpoint : `GET /dashboard` dans `src/server/http.rs`. Test d'intégration : `dashboard_returns_html_with_expected_sections`.

### Pas de dashboard HTML embarqué — décision initiale (annulée le 2026-04-20)
Motif initial : proposition (dashboard UI Tailwind embarqué) rejetée — doublerait Grafana, ajoute une surface d'attaque XSS, augmente la taille binaire, s'éloigne du standard SRE. Conservée tant que le scope est strictement production. Cf. révision ci-dessus pour le besoin démo qui a justifié l'ajout d'un dashboard minimal.

### Double `tonic` dans l'arbre de dépendances (accepté)
Motif : `qdrant-client` 1.12 (et 1.17 latest) embarque `tonic 0.12 + prost 0.13` en interne. Notre serveur gRPC utilise `tonic 0.14.5 + prost 0.14.3`. Les deux versions coexistent dans le binaire sans interop au niveau des types (qdrant-client encapsule sa couche gRPC, notre service expose sa propre API). Choix retenu : garder les deux en parallèle plutôt que downgrade massif (coût : +1-2 Mo binaire, +temps de compilation ; alternative : 1-3 j de rework sur les fondations validées). À revisiter si `qdrant-client` migre en 0.14 dans une version future.

### `VectorDbClient` comme trait générique plutôt que type concret
Motif : une interface stable entre le handler gRPC et le backend VDB facilite (1) les tests du handler via `MockVdbClient` (hand-rolled dans `client::mock`, pas de dépendance mockall), (2) l'ajout ultérieur de backends (Pinecone, Weaviate, pgvector) sans toucher au code applicatif. Risque YAGNI accepté : l'abstraction coûte ~30 lignes, le bénéfice de testabilité est immédiat.

### Mock VDB hand-rolled plutôt que wiremock pour les tests
Motif : `wiremock-rs` est orienté HTTP/REST. `qdrant-client` 1.12 utilise gRPC en premier. Faire un mock gRPC complet du proto Qdrant aurait coûté 1-2 jours et introduit une deuxième dépendance au schéma Qdrant. À la place, `MockVdbClient` dans `client::mock` (gated `#[cfg(test)]`, ~40 lignes) couvre les handler tests de l'étape 7 avec une API contrôlable. Le vrai client Qdrant est testé par construction + timeout + RAII `inflight_guard`, sans appel réseau réel.

### `InflightGuard` pour la métrique de saturation VDB
Motif : `qdrant-client` n'expose pas l'état interne de son pool de connexions (géré par hyper/tonic). Pour mesurer la saturation applicative, on incrémente/décrémente un `AtomicU64::inflight` via une garde RAII autour de chaque appel. Cohérent même sur chemin d'erreur (le `Drop` est exécuté). Plan A du brief initial.

### Pas de retry exponentiel dans le client VDB (remonté au handler)
Motif : un retry qui duplique les params (`UpsertParams` contient `Vec<f32>` de 1536 × 4 = 6 Ko) au niveau client forcerait des clones coûteux sur chaque retry, même en régime nominal où la majorité des appels réussissent au premier essai. Mieux : laisser le handler gRPC (étape 7) construire les params une fois et piloter sa propre politique de retry en appelant le client plusieurs fois avec les mêmes params réutilisés. Le client reste donc "one-shot" : un appel, une réponse, un timeout strict.

### Ajout du RPC Search au proto (hors scope initial du brief)
Motif : le brief initial couvrait uniquement `Upsert`, mais la cohérence du système impose que le vecteur de requête subisse exactement la même transformation (validation + normalisation) que les vecteurs stockés. Si le client appelle Qdrant directement pour la recherche sans passer par le middleware, il saute la normalisation et introduit un biais systématique sur les scores. Ajout d'un RPC `Search` qui partage le pipeline avec `Upsert`, diffère uniquement à l'étape 7 (appel `search_points` au lieu de `upsert_points`). Coût additionnel estimé : 0,5 jour dans l'étape 7. La métrique `requests_total` gagne un label `op ∈ {upsert, search}`.

### 2026-04-21 — `producer_id` + journal de rejet structuré + licence Ed25519
Motif : trois chantiers en un sprint pour rendre le middleware distribuable commercialement avec traçabilité forensique et protection offline.

**Producer attribution** (`producer_id` sur `UpsertRequest`/`SearchRequest`) : champ proto3 optionnel (champs 6 et 7) ajouté en append backward-compatible. Vide => "unknown" côté métriques. Devient un label de `requests_total` et `request_duration_seconds` aux côtés de `model_id`, `op`, `status`. Cardinalité bornée contractuellement par le client (nom de service, pas d'UUID) — documenté dans le proto. Permet d'identifier quel producteur envoie des vecteurs mal formés sans tracing distribué.

**Journal de rejet structuré (JSON Lines sur stderr)** : émis uniquement en cas de rejet à la validation (`unknown_model`, `invalid_dim`, `invalid_numeric`). Contient `{event, op, producer_id, model_id, status, reason}`. Choix de stderr pour ne pas polluer une sortie structurée applicative éventuelle. Séparé des métriques : agrégation vs granularité unitaire pour post-mortem `grep | jq`. Pas de trace ID : c'est un outil de debug, pas un audit trail.

**Licence Ed25519 + mode éval 45 j** : format JWT-like `<b64url(claims)>.<b64url(sig)>` avec claims `{customer, issued_at, expires_at, features}`. Clé publique embarquée via `include_bytes!("../keys/license.pub")` — point d'ancrage de la chaîne de confiance, aucun override runtime. Clé privée `keys/license.sec` gitignored (chmod 600). Mode éval : `LicenseConfig.path = None` => démarrage en évaluation pour `max_eval_days` (45 par défaut), premier boot persisté dans `eval_state_path`. Pas de phone-home. Vérification au démarrage uniquement (jamais sur chemin chaud). Binaire interne `license-gen` pour keypair + sign ; exposé comme `[[bin]]` séparé dans `Cargo.toml`. Warning à J-7 avant expiration (licence ou éval).

**Tests** : +17 unitaires (licence), +4 intégration (round-trip signé avec la clé embarquée, rejet d'une licence expirée, fallback évaluation), +1 unitaire (`normalize_producer`). Miri 16/16 vert sur `license::*` avec `-Zmiri-disable-isolation`. Clippy `-D warnings` vert. Format signature (signature du `b64(claims)`, pas du JSON brut) aligné sur JWS EdDSA pour qu'un auditeur puisse reconnaître le format sans spec custom.
