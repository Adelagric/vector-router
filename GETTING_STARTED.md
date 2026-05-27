# Vector Router — Prise en main

**La couche de confiance entre vos agents IA et votre base vectorielle.**
Vector Router s'intercale entre les producteurs d'embeddings (agents LLM,
pipelines RAG, jobs d'indexation) et votre base vectorielle. Il valide,
normalise et route chaque vecteur avant qu'il n'entre en base — pour empêcher
la corruption silencieuse de la mémoire de l'entreprise et rendre visibles
les incohérences de pipeline (mauvais modèle, NaN/Inf, vecteurs non
normalisés, producteur fautif).

Guide destiné à l'opérateur qui déploie le binaire en production. Temps de
mise en route visé : **30 minutes** depuis le build jusqu'à la première
requête routée.

Ce document assume que vous avez accès au repo (`git clone` + `make build` /
`make docker`) ou à une image Docker pré-construite.

---

## 1. Prérequis

**Plateforme.** Linux x86-64, glibc ≥ 2.31 (Ubuntu 20.04+, Debian 11+, RHEL 9+).
Le binaire est compilé pour `x86-64-v3` (Haswell 2013 et plus récent). Toute
CPU serveur achetée après 2014 convient.

**Backend vectoriel.** Un Qdrant joignable. Testé avec Qdrant 1.12+. L'URL
va dans `config.toml` — aucun autre backend dans cette version.

**Ports.**

| Port     | Rôle        | Qui y accède                        |
|----------|-------------|--------------------------------------|
| `50051`  | gRPC        | Vos producteurs d'embeddings         |
| `9090`   | HTTP        | Prometheus, Grafana, sondes k8s      |

Les deux sont configurables. Le binaire ne sort pas en dehors de ces ports
plus la connexion sortante vers Qdrant.

**Permissions.** Aucun chemin disque writable n'est requis par défaut — le
binaire fonctionne stateless tant que le backend vectoriel est joignable.

---

## 2. Installation

```bash
# 1. Build (à partir des sources)
git clone https://github.com/Adelagric/vector-router.git
cd vector-router
make build               # produit target/release/vector-router

# 2. Copie du binaire
sudo install -m 755 target/release/vector-router /usr/local/bin/

# 3. Config
sudo mkdir -p /etc/vector-router
sudo cp config.example.toml /etc/vector-router/config.toml
sudo chmod 640 /etc/vector-router/config.toml
# Éditez config.toml — voir section 3.
```

Pour un déploiement Docker, sauter cette section et aller directement à 4.b.

---

## 3. Configuration minimale

Ouvrez `/etc/vector-router/config.toml`. Les **quatre** champs à vérifier en
priorité :

```toml
[server]
grpc_bind = "0.0.0.0:50051"
http_bind = "0.0.0.0:9090"

[vdb]
url = "http://votre-qdrant.interne:6334"
timeout_ms = 500

[admin]
# À remplacer impérativement avant mise en prod. Idéalement injecté par
# votre gestionnaire de secrets (Vault, AWS Secrets Manager, etc.).
bearer_token = "CHANGE-ME"

[models."openai-text-embedding-3-small"]
dim = 1536
normalize = true
vdb_namespace = "prod-openai-small"
```

**Modèles.** Chaque modèle que vos producteurs utilisent doit être déclaré
ici. Un modèle non déclaré = requête rejetée avec `UnknownModel`. C'est
délibéré — pas de découverte automatique, pour que la base ne se remplisse
jamais d'un modèle non validé par vous.

**Variables d'environnement.** Toute valeur de la config peut être
surchargée par une variable d'env préfixée `VR_`, avec `__` comme séparateur
de sous-champ :

```bash
VR_ADMIN__BEARER_TOKEN="$(vault kv get -field=token ...)" \
VR_VDB__URL="http://qdrant.prod:6334" \
/usr/local/bin/vector-router
```

Pratique pour ne pas committer de secrets dans le TOML.

---

## 4. Premier démarrage

### 4.a. Binaire natif

```bash
VR_CONFIG_PATH=/etc/vector-router/config.toml /usr/local/bin/vector-router
```

### 4.b. Image Docker (recommandé pour k8s / CI)

Si vous avez reçu également l'archive `vector-router-X.Y.Z-docker.tar.gz` :

```bash
docker load -i vector-router-X.Y.Z-docker.tar.gz
# => Loaded image: vector-router:X.Y.Z
```

**Piège à éviter** — pour les mounts : montez `config.toml` **au niveau
fichier**, pas au niveau répertoire, pour éviter qu'un autre mount masque
le fichier.

```bash
docker run -d --name vector-router \
  -p 50051:50051 \
  -p 9090:9090 \
  -v /etc/vector-router/config.toml:/etc/vector-router/config.toml:ro \
  -e VR_CONFIG_PATH=/etc/vector-router/config.toml \
  vector-router:X.Y.Z
```

Pour que le container puisse joindre un Qdrant qui tourne sur l'hôte (dev
local), remplacez `url = "http://qdrant:6334"` par
`url = "http://host.docker.internal:6334"` dans `config.toml`.

En Kubernetes, les ConfigMap et Secret se mappent naturellement : chacun
produit un fichier individuel, donc le piège du directory-mount
n'apparaît pas.

Sortie attendue sur stderr :

```
vector-router : chargement config depuis /etc/vector-router/config.toml
vector-router : recorder Prometheus installé
vector-router : serveurs démarrés (gRPC 0.0.0.0:50051, HTTP 0.0.0.0:9090)
```

---

## 5. Vérification end-to-end

### 5.1 Sondes HTTP

```bash
curl http://localhost:9090/health
# => "ok"  (200)

curl http://localhost:9090/ready
# => "ready"  (200) si Qdrant joignable
# => "vdb indisponible : ..." (503) sinon
```

### 5.2 Premier appel gRPC

Installez `grpcurl` (`brew install grpcurl`, `apt install grpcurl`, ou
<https://github.com/fullstorydev/grpcurl/releases>).

Encodage du vecteur : les octets `f32` little-endian doivent être encodés en
base64 pour le champ `bytes`. Le vecteur 4-dim `[1.0, 0.0, 0.0, 0.0]` donne
`AACAPwAAAAAAAAAAAAAAAA==`.

```bash
grpcurl -plaintext \
  -proto docs/router.proto \
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

Réponse attendue (timing indicatif) :
```json
{
  "pointId": "hello-world",
  "processingUs": "87",
  "wasNormalized": true,
  "vdbNamespace": "prod-openai-small"
}
```

### 5.3 Métriques Prometheus

```bash
curl -s http://localhost:9090/metrics | grep requests_total
```

Doit contenir au moins une ligne du type :
```
requests_total{model_id="openai-text-embedding-3-small",op="upsert",status="ok",producer_id="install-test"} 1
```

Si c'est vert, le pipeline est opérationnel.

---

## 6. Exploitation en prod

### 6.1 systemd

Fichier `/etc/systemd/system/vector-router.service` :

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

# Durcissement
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
NoNewPrivileges=yes
ReadWritePaths=/var/lib/vector-router

[Install]
WantedBy=multi-user.target
```

`secrets.env` (permissions 600) contient typiquement
`VR_ADMIN__BEARER_TOKEN=...` et, si applicable, `VR_VDB__API_KEY=...`.

```bash
sudo useradd --system --no-create-home vector-router
sudo chown -R vector-router:vector-router /var/lib/vector-router
sudo systemctl daemon-reload
sudo systemctl enable --now vector-router
sudo journalctl -u vector-router -f
```

### 6.2 Kubernetes

Exposez les sondes standard :

```yaml
livenessProbe:
  httpGet: { path: /health, port: 9090 }
  periodSeconds: 10
readinessProbe:
  httpGet: { path: /ready, port: 9090 }
  periodSeconds: 5
```

`/ready` renvoie 503 si Qdrant devient injoignable — le load balancer
retire alors le pod du pool automatiquement.

### 6.3 Shutdown propre

Le binaire gère SIGTERM. La séquence :
1. Cesse d'accepter de nouvelles connexions gRPC.
2. Draine les requêtes en cours (timeout 30 s).
3. Arrêt propre.

Ne l'interrompez jamais par `kill -9` en prod — les requêtes en vol
perdent leur réponse.

---

## 7. Observabilité

### 7.1 Dashboard Grafana

Importez `docs/grafana-dashboard.json` dans Grafana 10+.
Source Prometheus pointée sur `http://<host>:9090/metrics` (ou votre
scrape job existant).

Panneaux clés :
- **Taux de requêtes RED** par modèle et par `producer_id`
- **Taux d'erreurs** par status (unknown_model, invalid_dim, vdb_error, etc.)
- **Latence p50/p95/p99** par modèle
- **Saturation VDB** (`vdb_inflight`)
- **Pool mémoire** (`pool_available`, `pool_exhausted_total`)
- **Désalignement vecteurs** (`misaligned_copies_total`)

### 7.2 Journal de rejet

Toute requête rejetée à la validation produit une ligne JSON sur **stderr**
(distincte des métriques Prometheus, qui agrègent) :

```json
{"event":"rejection","op":"upsert","producer_id":"batch-nightly","model_id":"openai-small","status":"invalid_dim","reason":"dimension invalide : attendu 6144, reçu 4096"}
```

Pour retrouver le producteur fautif quand une métrique Prometheus monte :

```bash
journalctl -u vector-router --since "10 min ago" \
  | grep '"event":"rejection"' \
  | jq -r 'select(.status=="invalid_dim") | .producer_id' \
  | sort | uniq -c | sort -rn
```

### 7.3 Qu'est-ce qui doit vous inquiéter ?

| Signal                              | Seuil typique | Action                        |
|-------------------------------------|---------------|-------------------------------|
| `misaligned_copies_total` / total   | > 1 %         | Producteur envoie des buffers mal alignés — investiguer côté client |
| `pool_exhausted_total`              | > 0           | Sous-dimensionné — augmenter `pool.buffers_per_worker` |
| Latence p99 `request_duration`      | > 1 ms        | Réseau Qdrant ou saturation VDB — vérifier `vdb_inflight` |
| `requests_total{status="vdb_error"}`| toute valeur >0 soutenue | Qdrant instable ou timeout trop bas |

---

## 8. Dépannage

### « vector-router : chargement config depuis config.toml » puis crash

Le binaire cherche `config.toml` dans le répertoire courant par défaut.
Fixez `VR_CONFIG_PATH` ou lancez-le depuis le bon répertoire.

### `/ready` renvoie 503

Qdrant n'est pas joignable depuis le container / host. Vérifiez :

```bash
curl -v http://<qdrant-host>:6334/readyz
```

Si c'est un problème de latence, augmenter `vdb.timeout_ms` dans la
config. Par défaut 500 ms — suffisant sur LAN, à relever pour du
cross-region.

### Logs `pool_exhausted_total` qui grimpe

Le pool mémoire est dimensionné via `pool.buffers_per_worker` (défaut 2).
Si votre charge envoie plus de 2N requêtes concurrentes en burst (N =
nombre de workers tokio), allonger le pool. Le fallback est transparent
(allocation ad-hoc), mais coûte plus cher.

### Erreur `DeadlineExceeded` côté client gRPC

Votre client a un timeout plus court que le temps de traitement en queue.
Augmenter le timeout côté client, ou dimensionner plus grand
`server.max_concurrent_requests`.

### « modèle inconnu : xxx »

Le modèle n'est pas déclaré dans la config. Ajoutez une section
`[models."xxx"]` et redémarrez. Pas de découverte automatique — c'est
volontaire, pour éviter qu'un producteur n'enregistre n'importe quoi.

---

## 9. Support

Pour tout problème non couvert ici :

- **Logs utiles** : `journalctl -u vector-router --since "1 hour ago"`
- **État métriques** : `curl -s http://localhost:9090/metrics > /tmp/metrics.txt`
- **Version** : `/usr/local/bin/vector-router --version` (si compilé
  avec l'option) ou inspection du `sha256sum` du binaire

Contact : Adel Kaleche — <kaleche@gmail.com> — +33 7 80 76 06 71
