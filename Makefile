# Makefile vector-router — build, test, lint, packaging.
#
# Targets:
#   build         Build release binary (native target)
#   test          Run unit + integration tests
#   bench         Run Criterion benchmarks
#   miri          Run miri on math + pool modules
#   loom          Run loom model-checking on registry
#   check         clippy -D warnings + fmt --check
#   docker        Build distroless Docker image
#   clean         Remove build artifacts
#
# Conventions:
#   - All commands assume rustc 1.94+, set via rust-toolchain.toml.
#   - Native builds use .cargo/config.toml (target-cpu=native).
#   - Docker builds force target-cpu=x86-64-v3 for portable Linux x86-64.

BINARY_NAME    := vector-router
VERSION        := $(shell grep -m1 '^version' Cargo.toml | cut -d'"' -f2)

.PHONY: help
help:
	@echo "vector-router $(VERSION) — targets:"
	@echo "  build       Compile release binary"
	@echo "  test        Unit + integration tests (default + pgvector)"
	@echo "  test-pgvector  pgvector tests (set VR_TEST_PG_URL for the live suite)"
	@echo "  bench       Criterion benchmarks"
	@echo "  miri        Memory safety check on sensitive modules"
	@echo "  loom        Concurrency model check"
	@echo "  check       clippy (default + pgvector) + fmt"
	@echo "  docker      Build distroless image $(BINARY_NAME):$(VERSION)"
	@echo "  clean       Remove target/"

.PHONY: build
build:
	cargo build --release --locked --bin $(BINARY_NAME)

.PHONY: test
test:
	cargo test --all-targets --locked
	cargo test --all-targets --locked --features pgvector

.PHONY: test-pgvector
test-pgvector:
	cargo test --all-targets --locked --features pgvector

.PHONY: bench
bench:
	cargo bench --bench normalize
	cargo bench --bench alignment

.PHONY: miri
miri:
	cargo +nightly miri test --lib math
	cargo +nightly miri test --lib pool

.PHONY: loom
loom:
	RUSTFLAGS='--cfg loom' cargo test --release --lib registry_loom

.PHONY: check
check:
	cargo fmt --all -- --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo clippy --all-targets --locked --features pgvector -- -D warnings

.PHONY: docker
docker:
	docker build --platform linux/amd64 -t $(BINARY_NAME):$(VERSION) .
	@echo "Image: $(BINARY_NAME):$(VERSION)"
	@echo "Run:   docker run --rm -p 50051:50051 -p 9090:9090 \\"
	@echo "         -v \$$(pwd)/config.toml:/etc/vector-router/config.toml:ro \\"
	@echo "         $(BINARY_NAME):$(VERSION)"

.PHONY: clean
clean:
	cargo clean
