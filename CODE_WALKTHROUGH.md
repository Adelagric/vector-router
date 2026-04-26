# Walkthrough du code

Ce document explique comment le code est organisé, ce que fait chaque module, et comment ils s'articulent entre eux. À lire avant de plonger dans le code, pour avoir la carte mentale.

---

## Structure générale

```
vector-router/
├── Cargo.toml                 — dépendances épinglées
├── rust-toolchain.toml        — verrou de version rustc (1.95.0)
├── .cargo/config.toml         — target-cpu=native
├── build.rs                   — compile le .proto via tonic-prost-build
├── proto/
│   └── vector_router/v1/
│       └── router.proto        — schéma gRPC
├── src/
│   ├── lib.rs                  — expose les modules publiquement
│   ├── main.rs                 — point d'entrée (étape 9, minimal pour l'instant)
│   ├── error.rs                — enum Error du crate
│   ├── config.rs               — schéma de config + parsing TOML/env
│   ├── registry.rs             — registre partagé des modèles
│   ├── pool.rs                 — pool de buffers alignés
│   ├── math.rs                 — noyau numérique (validation + normalisation)
│   └── proto/mod.rs            — inclut les types générés par tonic-prost-build
├── benches/
│   ├── normalize.rs            — bench l2_norm_squared + normalize_in_place
│   └── alignment.rs            — bench validate_and_align aligné / désaligné
├── DECISIONS.md                — arbitrages de design
├── BENCHES.md                  — chiffres de perf
├── HANDOVER.md                 — état des lieux pour la reprise
└── CODE_WALKTHROUGH.md         — ce fichier
```

---

## Module par module

### `error.rs`

Un seul enum `Error` pour tout le crate, avec 7 variants :

- **`UnknownModel { model_id }`** — le `model_id` du payload n'est pas dans le registre.
- **`InvalidDim { expected, got }`** — la taille du payload (en octets) ne correspond pas à la dimension déclarée pour ce modèle.
- **`InvalidNumeric`** — le vecteur contient un `NaN` ou un `Inf`.
- **`Vdb(String)`** — erreur remontée par le client Qdrant.
- **`Config(Box<figment::Error>)`** — erreur de parsing / chargement de config. Boxée car `figment::Error` pèse ~200 octets.
- **`Io(std::io::Error)`** — erreur I/O au démarrage.
- **`Validation(String)`** — échec des règles de validation post-parsing de config.

Conversions `From` dérivées via `#[from]` pour `io::Error` et `figment::Error`.

Les variants de chemin chaud (`UnknownModel`, `InvalidDim`, `InvalidNumeric`, `Vdb`) correspondent aux labels `status` de la métrique `requests_total`.

### `config.rs`

Schéma complet de la configuration, décomposé en sous-structs :

- **`Config`** — racine.
- **`ServerConfig`** — adresses d'écoute, limites rate / payload.
- **`AdminConfig`** — bearer token.
- **`VdbConfig`** — URL, timeout, retry.
- **`PoolConfig`** — facteur de buffers par worker.
- **`TelemetryConfig`** — niveau de log, endpoint OTLP.
- **`ModelSpec`** — `{ dim, normalize, vdb_namespace }`, stocké dans le registre.

Chargement via `figment` :
- `Config::load(path)` lit un TOML puis applique les overrides d'env (`VR_*`, séparateur `__` pour les sous-champs).
- `Config::from_figment(fig)` expose l'injection pour les tests.

Après parsing, `validate()` vérifie :
- `admin.bearer_token` non vide.
- `vdb.url` non vide.
- `pool.buffers_per_worker ≥ 1`.
- `vdb.max_retries ≥ 1`.
- Pour chaque modèle : nom non vide, `dim ≠ 0`, `vdb_namespace` non vide.

11 tests unitaires couvrent tous les cas d'erreur et la configuration minimale valide.

### `registry.rs`

Le registre est la source de vérité sur les modèles autorisés. Il est accédé à chaque requête gRPC, d'où l'optimisation pour la lecture.

**Structure interne** : `ArcSwap<HashMap<String, ModelSpec>>`. L'`ArcSwap` permet des lectures lock-free (chaque lecteur clone un `Arc`, ne prend aucun verrou).

**API** :
- `new(initial)` — crée le registre avec un contenu initial.
- `get(id)` — lookup O(1), retourne une copie du `ModelSpec`.
- `snapshot()` — retourne l'`Arc<HashMap>` pour un parcours multiple sans race.
- `upsert(id, spec)` — ajoute ou remplace, via `ArcSwap::rcu`.
- `remove(id)` — supprime, retourne si l'entrée existait.
- `list()` — retourne un Vec des entrées actuelles.

**Sur `rcu`** : `rcu` = Read-Copy-Update. Pour mettre à jour : charger le snapshot, cloner la map, modifier la copie, swaper atomiquement. Si un autre update survient entre le chargement et le swap, `rcu` retry automatiquement. Coût amorti ≈ coût d'une lecture + clone + swap. En régime de mise à jour rare (admin, quelques par heure), la contention est quasi nulle.

**Règle critique** : dans un handler gRPC, n'appeler `snapshot()` ou `get()` **qu'une seule fois** par requête. Sinon, deux lookups peuvent tomber de part et d'autre d'un update et voir des versions incohérentes du registre (ex : modèle trouvé à la première lecture, disparu à la seconde).

**Test loom** : valide le pattern "snapshot via clone d'Arc + update via swap" sur un modèle simplifié `Mutex<Arc<T>>` (pas directement `ArcSwap`, qui a sa propre couverture loom interne). Voir `DECISIONS.md`.

### `pool.rs`

Le pool sert à amortir l'allocation mémoire pour le cas où le payload protobuf arrive désaligné et doit être recopié dans un buffer aligné.

**`AlignedBuffer`** :
- Stockage en `Box<[u32]>` — garantit un alignement 4 (taille et alignement naturels de `u32` et `f32` sont identiques).
- `len: usize` — longueur utile en octets, indépendante de la capacité.
- `copy_from_slice(&[u8])` — copie dans le buffer, met à jour `len`.
- `as_f32()` — retourne `&[f32]` via `bytemuck::try_cast_slice` (qui ne peut échouer que si `len` n'est pas multiple de 4).
- `clear()` — remet `len` à 0 sans désallouer.
- `Default` — buffer vide (sentinelle pour `mem::take` dans le Drop de `PooledBuffer`).

**`BufferPool`** :
- `queue: ArrayQueue<AlignedBuffer>` — pré-alloué avec `size` buffers à la création.
- `take()` → `PooledBuffer<'_>` — dépile du pool, ou alloue en fallback si vide (incrémente `exhausted_count`).
- **Ne bloque jamais**. Un blocage corrélé à la charge dégraderait le p99 exactement sous contention.

**`PooledBuffer<'p>`** :
- Déréférence vers `AlignedBuffer` (`Deref` / `DerefMut`).
- **Drop** : `mem::take(&mut self.buffer)` échange avec un `AlignedBuffer::default()` (vide, zéro alloc), puis le buffer extrait est clear-é et repoussé dans la queue. Si la queue est pleine (cas où le pool a précédemment fallback-alloué), le buffer est simplement droppé.

**Invariant important** : un `PooledBuffer` possède toujours un `AlignedBuffer` valide de la construction au Drop. On évite ainsi le pattern `Option<AlignedBuffer>` qui aurait forcé un `.expect()` dans `Deref` (interdit par les règles du projet).

**miri vert** sur tous les tests — aucun UB dans les casts `u32 ↔ u8`.

### `math.rs`

Le noyau numérique. Trois fonctions publiques, toutes synchrones et agnostiques du reste du service.

**`validate_and_align(raw, expected_dim, scratch) → Result<Cow<[f32]>, Error>`** :

1. Calcule `expected_bytes = expected_dim × 4` (avec `checked_mul` pour éviter tout wrap).
2. Vérifie `raw.len() == expected_bytes`, sinon retourne `InvalidDim`.
3. Tente `bytemuck::try_cast_slice::<u8, f32>(raw)` :
   - **Succès** → `Cow::Borrowed(slice)`. Zero-copy. Cas majoritaire en pratique.
   - **Échec** (désalignement, la longueur est déjà validée) → copie dans `scratch`, retourne `Cow::Borrowed(scratch.as_f32()?)`. Une copie, c'est tout.

Le `Cow` retourné est `'a` : la vie est liée au paramètre le plus court de (`raw`, `scratch`), qu'on a unifiés sous `'a` dans la signature. L'appelant ne fait pas la distinction entre zero-copy et copie.

**`l2_norm_squared(v) → Result<f32, Error>`** :

Une passe sur `v`. Pour chaque élément :
1. Si non fini (`!x.is_finite()`, c-à-d NaN ou ±Inf) → `InvalidNumeric`.
2. Sinon, accumule `sum += x × x`.

Retourne `Σ xᵢ²`. On calcule le carré (pas la racine) pour éviter un `sqrt` inutile : la comparaison avec 1 pour décider d'une normalisation peut se faire sur le carré directement.

Fusion validation + somme : une seule traversée du vecteur au lieu de deux (vs faire un `iter().any(|x| !x.is_finite())` suivi d'un `iter().map(|x| x*x).sum()`).

`#[inline]` pour maximiser les chances d'auto-vectorisation côté appelant.

**`normalize_in_place(v, norm_squared) → bool`** :

1. Si `|norm² − 1.0| ≤ 2×10⁻⁶` → rien à faire, retourne `false`.
2. Si `norm² ≤ 0.0` → vecteur nul, rien à faire, retourne `false`.
3. Sinon, `norm = norm².sqrt()`, puis `v[i] /= norm` pour chaque `i`. Retourne `true`.

Le retour booléen indique si une division a effectivement eu lieu (utile pour la métrique `normalizations_performed_total`).

**Tests** : 13 cas couvrent aligned zero-copy, misaligned avec copie forcée (via backing `[u32; 5]` pour miri), wrong size, NaN, Inf, pipeline bout-en-bout. **miri vert**.

---

## Patterns avancés (traits, génériques, lifetimes)

Les sections ci-dessous pointent vers les usages non triviaux du système de types — utiles pour qui veut juger du niveau de discipline Rust avant le cycle d'achat. Tous les liens vont vers les fichiers publics du repo, lintés par la CI.

### `Cow<'a, [f32]>` pour zero-copy conditionnel — [`samples/math.rs:30-62`](samples/math.rs#L30-L62)

`validate_and_align` retourne un `Cow<'a, [f32]>` dont la vie est unifiée sur les deux paramètres `raw: &'a [u8]` et `scratch: &'a mut AlignedBuffer`. L'appelant ne distingue jamais entre **zero-copy** (le `bytes` protobuf est aligné, retour de `Cow::Borrowed(raw_as_f32)`) et **copie unique** (désaligné, retour d'une vue dans `scratch`). Le borrow-checker garantit que la vue ne survit ni à `raw` ni à `scratch`. Aucun coût runtime à l'abstraction.

```rust
pub fn validate_and_align<'a>(
    raw: &'a [u8],
    expected_dim: usize,
    scratch: &'a mut AlignedBuffer,
) -> Result<Cow<'a, [f32]>, Error>
```

### Trait objects et RAII : `PooledBuffer<'p>` — [`samples/pool.rs:157-189`](samples/pool.rs#L157-L189)

`PooledBuffer` implémente `Deref` + `DerefMut` vers `AlignedBuffer` (l'utilisateur écrit `pooled.copy_from_slice(...)` directement) et `Drop` pour rendre automatiquement le buffer au pool — à condition que sa capacité corresponde toujours au pool d'origine, sinon le buffer est jeté (cas d'un resize de config en cours de route).

```rust
impl<'p> std::ops::Deref for PooledBuffer<'p> {
    type Target = AlignedBuffer;
    fn deref(&self) -> &AlignedBuffer { &self.buffer }
}

impl<'p> Drop for PooledBuffer<'p> {
    fn drop(&mut self) {
        let mut buf = mem::take(&mut self.buffer); // sentinelle Default = vide, zéro alloc
        buf.clear();
        if buf.capacity_bytes() == self.pool.capacity_bytes {
            let _ = self.pool.queue.push(buf);
        }
    }
}
```

Le `mem::take` exploite l'`impl Default for AlignedBuffer` qui crée un buffer vide sans aucune allocation — pattern qui évite le `Option<AlignedBuffer>` et son `.expect()` corrélé (interdit par les règles du projet, voir invariants ci-dessous).

### Turbofish typé sur `bytemuck` — [`samples/math.rs:50`](samples/math.rs#L50), [`samples/pool.rs:69-83`](samples/pool.rs#L69-L83)

`bytemuck::try_cast_slice::<u8, f32>(raw)` et `bytemuck::cast_slice_mut::<u32, u8>(&mut self.storage)` exploitent les bornes `Pod`/`AnyBitPattern` du crate pour convertir des slices entre types primitifs **sans `unsafe`** côté projet. Les contraintes de taille / alignement sont vérifiées au runtime (`try_cast_slice`) ou statiquement quand c'est possible (`cast_slice_mut` entre types compatibles).

### Génériques implicites via `chunks_exact` — [`samples/math.rs:77-107`](samples/math.rs#L77-L107)

`l2_norm_squared` utilise `<[f32]>::chunks_exact(8)` pour produire un itérateur dont chaque élément est garanti `&[f32; 8]` à l'inférence — LLVM peut alors déplier en AVX2 `vmulps`/`vaddps`. Huit accumulateurs parallèles cassent la dépendance séquentielle de la réduction, sans `unsafe` ni `-C fast-math`. Détection NaN/Inf hors hot path : on exploite la propagation IEEE 754 puis on vérifie la sortie ; un second passage rare-path remonte l'erreur exacte.

### `ArcSwap<HashMap<String, ModelSpec>>` — registre lock-free (référencé section `registry.rs`)

Lecture sans verrou via `ArcSwap::load_full()` qui retourne un `Arc<HashMap>` — chaque handler gRPC clone son `Arc`, garde le snapshot pendant toute la requête, le drop libère atomiquement la version si plus aucun lecteur. Update via `rcu` (Read-Copy-Update) : retry automatique en cas de concurrence. Le crate complet expose le détail (privé), mais le pattern est classique en Rust senior et c'est ce qui permet aux deux RPC `Upsert`/`Search` de partager un registre cohérent sans `Mutex` sur le hot path.

### Invariants à préserver (rappel)

1. **Zéro `unsafe`** hors des appels à `bytemuck` (qui est safe lui-même).
2. **Zéro `unwrap` / `expect`** hors de `main.rs`, des modules de tests, des benches et de `build.rs`.
3. **Zéro allocation dans le chemin chaud hors désalignement** (et dans ce cas, une seule copie par requête via le pool).
4. **`validate_and_align` ne panique jamais** — retourne toujours un `Result`.
5. **Le pool ne bloque jamais** — épuisement = allocation ponctuelle.
6. **Le registre n'observe jamais d'état partiel** — chaque `load()` retourne un snapshot atomique complet.

Ces invariants sont **lintés en CI** (clippy `-D warnings`, fmt, tests). Voir [.github/workflows/ci.yml](.github/workflows/ci.yml).

---

## Articulation entre modules

Le service expose deux RPC gRPC : `Upsert` (écriture) et `Search` (lecture). Les deux partagent **exactement le même pipeline de validation et de normalisation** ; seule l'opération Qdrant finale et la forme de la réponse diffèrent.

### Pipeline partagé Upsert/Search

Le code du pipeline est à extraire dans une fonction helper, appelée par chaque handler :

```
1. registry.get(&request.model_id)
      → None          → retourne Error::UnknownModel
      → Some(spec)    → continue

2. if request.dim as usize != spec.dim { return Error::InvalidDim }

3. let mut pooled = pool.take();

4. let view = math::validate_and_align(
       &request.vector,
       spec.dim,
       &mut pooled,           // scratch, utilisé seulement si désaligné
   )?;

5. let n2 = math::l2_norm_squared(&view)?;

6. let vector_owned: Vec<f32> = if spec.normalize {
       let mut owned = view.into_owned();
       let changed = math::normalize_in_place(&mut owned, n2);
       // (marquer dans la réponse : was_normalized = changed)
       owned
   } else {
       view.into_owned()
   };
   // pooled peut maintenant être libéré, l'await Qdrant peut se faire
```

À ce stade, `vector_owned` est prêt à être envoyé à Qdrant, indépendamment du RPC.

### Divergence Upsert

```
7a. qdrant.upsert_points(
        collection = spec.vdb_namespace,
        point_id   = request.point_id,
        vector     = vector_owned,
        metadata   = request.metadata,
    ).await?

8a. UpsertResponse {
        point_id,
        processing_us,
        was_normalized,
        vdb_namespace: spec.vdb_namespace,
    }
```

### Divergence Search

```
7b. let hits = qdrant.search_points(
        collection       = spec.vdb_namespace,
        vector           = vector_owned,
        limit            = request.limit,
        score_threshold  = request.score_threshold,
        filter           = request.metadata_filter,
    ).await?;

8b. SearchResponse {
        hits: hits.map(|h| SearchHit {
            point_id:  h.id,
            score:     h.score,
            metadata:  h.metadata,
        }).collect(),
        processing_us,
        was_normalized,
        vdb_namespace: spec.vdb_namespace,
    }
```

### Pourquoi partager le pipeline est critique

La recherche par similarité cosinus suppose que **tous les vecteurs comparés sont normalisés à la même norme** (généralement 1). Si la base contient des vecteurs normalisés mais que le vecteur de requête ne l'est pas (ou inversement), les scores sont systématiquement biaisés.

En passant les deux flux par la même fonction qui respecte `spec.normalize`, on garantit que requête et documents sont dans le même espace. C'est l'invariant clé de la cohérence fonctionnelle.

À l'étape 7, ce pseudo-code sera raffiné pour gérer le passage `view → owned` de manière efficace (cf. Risque 1 du `HANDOVER.md`).

---

## Invariants à préserver

Ces propriétés sont assurées par le code actuel et doivent être préservées par toute modification.

1. **Aucun `unsafe`** hors des usages de `bytemuck` (qui est safe lui-même).
2. **Aucun `unwrap` / `expect`** en dehors de `main.rs`, des modules de tests, des benches et de `build.rs`.
3. **Aucune allocation dans le chemin chaud hors désalignement** (et dans ce cas, une seule copie par requête via le pool).
4. **`validate_and_align` ne panique jamais** — retourne toujours un `Result`, même en cas d'entrée bizarre.
5. **Le pool ne bloque jamais** — épuisement = allocation ponctuelle, pas attente.
6. **Le registre n'observe jamais d'état partiel** — chaque `load()` retourne un snapshot atomique complet.
7. **Les benches utilisent `black_box` sur inputs et outputs** — si un bench est ajouté, respecter cette règle ou les chiffres seront faux.

---

## Stratégie de tests

**Tests unitaires** (`#[cfg(test)] mod tests` dans chaque module) — couvrent le comportement fonctionnel de chaque fonction publique, y compris les cas limites explicites (NaN, Inf, zero vector, empty registry, oversize payload, misaligned slice).

**Test loom** (`#[cfg(loom)]`) — valide qu'un lecteur concurrent d'un update n'observe jamais d'état corrompu. Exécution manuelle via `RUSTFLAGS='--cfg loom' cargo test`.

**Miri** (`cargo +nightly miri test`) — vérifie l'absence d'UB dans les modules `math` et `pool` (ceux qui manipulent de la mémoire brute via `bytemuck`).

**Benches Criterion** — mesurent les temps d'exécution avec `black_box` sur inputs et outputs, et des assertions de sanité intégrées pour détecter une éventuelle élimination par le compilateur.

**Tests d'intégration** (à ajouter à l'étape 10, dans `tests/`) — exercent le pipeline complet à travers le serveur gRPC.
