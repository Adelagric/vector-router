# syntax=docker/dockerfile:1.7
#
# Build multi-stage pour obtenir une image distroless portable.
#
# Note : `.cargo/config.toml` local fixe `-C target-cpu=native` pour le dev.
# Dans le container on force une target portable (x86-64-v3 = Haswell+, couvre
# la quasi-totalité des CPU serveurs déployés depuis 2013). Sans cette étape
# le binaire produit ici ne tournerait que sur le CPU du builder.

FROM rust:1.95-slim-bookworm AS builder

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      protobuf-compiler \
      pkg-config \
      libssl-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Recette classique de cache : on copie les manifestes, on construit un
# squelette, cargo télécharge et compile les deps. La copie du vrai code
# vient après, les deps restent dans le cache tant que Cargo.toml/Cargo.lock
# ne changent pas.
COPY Cargo.toml Cargo.lock rust-toolchain.toml build.rs ./
COPY proto ./proto
# Stubs minimaux pour que cargo puisse valider Cargo.toml (benches + main + lib).
# Les vrais fichiers viennent après, une fois les deps compilées en cache.
RUN mkdir -p src benches \
 && echo "fn main() {}" > src/main.rs \
 && echo "" > src/lib.rs \
 && echo "fn main() {}" > benches/normalize.rs \
 && echo "fn main() {}" > benches/alignment.rs

# On remplace la config `.cargo/config.toml` par une version portable AVANT
# le premier build (sinon le cache de deps serait incohérent).
RUN mkdir -p .cargo && printf '[build]\nrustflags = ["-C", "target-cpu=x86-64-v3"]\n' > .cargo/config.toml

# Pré-compilation des dépendances seules, pour bénéficier du cache Docker
# layers sur les builds suivants quand seul le code applicatif change.
RUN cargo build --release --locked --bin vector-router \
 && rm -rf src benches

# Vrai code. Les deps sont déjà compilées et en cache ; on ne rebuild que
# notre crate. `cargo clean -p vector-router` supprime les artefacts de la
# version dummy de notre crate, indispensable sinon cargo réutilise le
# fingerprint d'une lib.rs vide et main.rs ne trouve plus les modules.
COPY src ./src
COPY benches ./benches
COPY tests ./tests
COPY static ./static

RUN cargo clean -p vector-router --release \
 && cargo build --release --locked --bin vector-router \
 && strip target/release/vector-router

# Image finale : distroless cc (libc + libstdc++), pas de shell, pas de package
# manager, réduit drastiquement la surface d'attaque. 26 Mo de base, le reste
# c'est le binaire.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /build/target/release/vector-router /usr/local/bin/vector-router

ENV VR_CONFIG_PATH=/etc/vector-router/config.toml
USER nonroot:nonroot
EXPOSE 50051 9090

ENTRYPOINT ["/usr/local/bin/vector-router"]
