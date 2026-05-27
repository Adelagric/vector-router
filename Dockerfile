# syntax=docker/dockerfile:1.7
#
# Multi-stage build to obtain a portable distroless image.
#
# Note: the local `.cargo/config.toml` sets `-C target-cpu=native` for dev.
# In the container we force a portable target (x86-64-v3 = Haswell+, covers
# nearly all server CPUs deployed since 2013). Without this step the binary
# produced here would only run on the builder's CPU.

FROM rust:1.95-slim-bookworm AS builder

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      protobuf-compiler \
      pkg-config \
      libssl-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Classic cache recipe: copy the manifests, build a skeleton, cargo
# downloads and compiles the deps. Copying the real code comes later;
# deps stay in cache as long as Cargo.toml/Cargo.lock don't change.
COPY Cargo.toml Cargo.lock rust-toolchain.toml build.rs ./
COPY proto ./proto
# Minimal stubs so cargo can validate Cargo.toml (benches + main + lib).
# The real files come later, once deps are compiled into the cache.
RUN mkdir -p src benches \
 && echo "fn main() {}" > src/main.rs \
 && echo "" > src/lib.rs \
 && echo "fn main() {}" > benches/normalize.rs \
 && echo "fn main() {}" > benches/alignment.rs

# Replace the `.cargo/config.toml` with a portable version BEFORE the
# first build (otherwise the dep cache would be inconsistent).
RUN mkdir -p .cargo && printf '[build]\nrustflags = ["-C", "target-cpu=x86-64-v3"]\n' > .cargo/config.toml

# Pre-compile dependencies alone so we benefit from the Docker layer
# cache on subsequent builds when only the application code changes.
RUN cargo build --release --locked --bin vector-router \
 && rm -rf src benches

# Real code. Deps are already compiled and cached; only our crate is
# rebuilt. `cargo clean -p vector-router` removes artifacts of our
# crate's dummy version, indispensable otherwise cargo reuses the
# fingerprint of an empty lib.rs and main.rs can't find the modules.
COPY src ./src
COPY benches ./benches
COPY tests ./tests
COPY static ./static

RUN cargo clean -p vector-router --release \
 && cargo build --release --locked --bin vector-router \
 && strip target/release/vector-router

# Final image: distroless cc (libc + libstdc++), no shell, no package
# manager, drastically reduces the attack surface. 26 MB base, the rest
# is the binary.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /build/target/release/vector-router /usr/local/bin/vector-router

ENV VR_CONFIG_PATH=/etc/vector-router/config.toml
USER nonroot:nonroot
EXPOSE 50051 9090

ENTRYPOINT ["/usr/local/bin/vector-router"]
