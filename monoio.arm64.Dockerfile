# Monoio/io_uring server for ARM64 production deployments
# This builds the vegeta_target_monoio example for load testing

FROM --platform=linux/arm64 rust:1.86-slim-bookworm AS builder
WORKDIR /app

# Install dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first for better layer caching
COPY Cargo.toml Cargo.lock ./
COPY harrow/Cargo.toml harrow/Cargo.toml
COPY harrow-core/Cargo.toml harrow-core/Cargo.toml
COPY harrow-middleware/Cargo.toml harrow-middleware/Cargo.toml
COPY harrow-o11y/Cargo.toml harrow-o11y/Cargo.toml
COPY harrow-serde/Cargo.toml harrow-serde/Cargo.toml
COPY harrow-server/Cargo.toml harrow-server/Cargo.toml
COPY harrow-server-monoio/Cargo.toml harrow-server-monoio/Cargo.toml
COPY harrow-bench/Cargo.toml harrow-bench/Cargo.toml

# Copy source files needed for cargo fetch
COPY harrow/examples harrow/examples
COPY harrow/src/lib.rs harrow/src/lib.rs
COPY harrow-core/src/lib.rs harrow-core/src/lib.rs
COPY harrow-middleware/src/lib.rs harrow-middleware/src/lib.rs
COPY harrow-o11y/src/lib.rs harrow-o11y/src/lib.rs
COPY harrow-serde/src/lib.rs harrow-serde/src/lib.rs
COPY harrow-server/src/lib.rs harrow-server/src/lib.rs
COPY harrow-server-monoio/src/lib.rs harrow-server-monoio/src/lib.rs

# Fetch dependencies
RUN rustup target add aarch64-unknown-linux-gnu && \
    cargo fetch --locked --target=aarch64-unknown-linux-gnu

# Copy full source
COPY harrow/src harrow/src
COPY harrow-core/src harrow-core/src
COPY harrow-middleware/src harrow-middleware/src
COPY harrow-o11y/src harrow-o11y/src
COPY harrow-serde/src harrow-serde/src
COPY harrow-server/src harrow-server/src
COPY harrow-server-monoio/src harrow-server-monoio/src

# Build monoio example
RUN cargo build --locked --release --target=aarch64-unknown-linux-gnu \
        --example vegeta_target_monoio \
        --features monoio,json --no-default-features \
        -p harrow

FROM gcr.io/distroless/cc-debian13:latest-arm64

# Note: For io_uring support, run container with --privileged
COPY --from=builder /app/target/aarch64-unknown-linux-gnu/release/examples/vegeta_target_monoio /harrow-monoio-server

CMD ["/harrow-monoio-server"]
