# Multi-stage build for cmdock-server
FROM rust:1.88-bookworm AS builder

WORKDIR /build

# Cache dependency builds
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && \
    mkdir -p src/bin && \
    echo "fn main() {}" > src/bin/test_harness.rs && \
    echo "fn main() {}" > src/bin/load_test.rs && \
    echo "" > src/lib.rs && \
    cargo build --release --bin cmdock-server 2>/dev/null || true && \
    rm -rf src

# Build the actual binary (touch lib.rs to invalidate the dummy build)
COPY src/ src/
COPY migrations/ migrations/
RUN touch src/lib.rs && cargo build --release --bin cmdock-server

# Runtime image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/cmdock-server /usr/local/bin/
COPY migrations/ /app/migrations/

WORKDIR /app

# Data volume — must be persisted
VOLUME ["/app/data"]

EXPOSE 8080

# Default entrypoint supports both server and admin CLI:
#   docker run cmdock/server                           → starts server
#   docker run cmdock/server admin user create ...      → runs admin command
#   docker run cmdock/server --data-dir /app/data admin user list
ENTRYPOINT ["cmdock-server"]
CMD ["--config", "/app/config.toml"]
