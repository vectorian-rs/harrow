# Monoio/io_uring perf server for ARM64 production deployments

FROM --platform=linux/arm64 rust:1 AS builder
WORKDIR /app

ARG BUILD_PROFILE=release
ARG BINARY_DIR=release

# Install dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first for better layer caching
COPY Cargo.toml Cargo.lock ./
COPY harrow/Cargo.toml harrow/Cargo.toml
COPY harrow-codec-h1/Cargo.toml harrow-codec-h1/Cargo.toml
COPY harrow-core/Cargo.toml harrow-core/Cargo.toml
COPY harrow-io/Cargo.toml harrow-io/Cargo.toml
COPY harrow-middleware/Cargo.toml harrow-middleware/Cargo.toml
COPY harrow-o11y/Cargo.toml harrow-o11y/Cargo.toml
COPY harrow-serde/Cargo.toml harrow-serde/Cargo.toml
COPY harrow-server/Cargo.toml harrow-server/Cargo.toml
COPY harrow-server-tokio/Cargo.toml harrow-server-tokio/Cargo.toml
COPY harrow-server-monoio/Cargo.toml harrow-server-monoio/Cargo.toml
COPY harrow-server-meguri/Cargo.toml harrow-server-meguri/Cargo.toml
COPY meguri/Cargo.toml meguri/Cargo.toml
COPY harrow-bench/Cargo.toml harrow-bench/Cargo.toml

# Copy source files needed for cargo fetch
COPY harrow/examples harrow/examples
COPY harrow/src/lib.rs harrow/src/lib.rs
COPY harrow-codec-h1/src/lib.rs harrow-codec-h1/src/lib.rs
COPY harrow-core/src/lib.rs harrow-core/src/lib.rs
COPY harrow-io/src/lib.rs harrow-io/src/lib.rs
COPY harrow-middleware/src/lib.rs harrow-middleware/src/lib.rs
COPY harrow-o11y/src/lib.rs harrow-o11y/src/lib.rs
COPY harrow-serde/src/lib.rs harrow-serde/src/lib.rs
COPY harrow-server/src/lib.rs harrow-server/src/lib.rs
COPY harrow-server-tokio/src/lib.rs harrow-server-tokio/src/lib.rs
COPY harrow-server-monoio/src/lib.rs harrow-server-monoio/src/lib.rs
COPY harrow-server-meguri/src/lib.rs harrow-server-meguri/src/lib.rs
COPY meguri/src/lib.rs meguri/src/lib.rs
COPY harrow-bench/benches harrow-bench/benches
COPY harrow-bench/src/lib.rs harrow-bench/src/lib.rs
COPY harrow-bench/src/bin harrow-bench/src/bin

# Fetch dependencies
RUN rustup target add aarch64-unknown-linux-gnu && \
    cargo fetch --locked --target=aarch64-unknown-linux-gnu

# Copy full source
COPY harrow/src harrow/src
COPY harrow-codec-h1/src harrow-codec-h1/src
COPY harrow-core/src harrow-core/src
COPY harrow-io/src harrow-io/src
COPY harrow-middleware/src harrow-middleware/src
COPY harrow-o11y/src harrow-o11y/src
COPY harrow-serde/src harrow-serde/src
COPY harrow-server/src harrow-server/src
COPY harrow-server-tokio/src harrow-server-tokio/src
COPY harrow-server-monoio/src harrow-server-monoio/src
COPY harrow-server-meguri/src harrow-server-meguri/src
COPY meguri/src meguri/src
COPY harrow-bench/src harrow-bench/src

# Build the monoio perf server
RUN set -eu; \
    if [ "${BUILD_PROFILE}" = "dev" ]; then \
        cargo build --locked --target=aarch64-unknown-linux-gnu -p harrow-bench --bin harrow-server-monoio; \
    elif [ "${BUILD_PROFILE}" = "perf" ]; then \
        RUSTFLAGS="-g -Cforce-frame-pointers=on" \
        cargo build --locked --profile perf --target=aarch64-unknown-linux-gnu -p harrow-bench --bin harrow-server-monoio; \
    else \
        cargo build --locked --release --target=aarch64-unknown-linux-gnu -p harrow-bench --bin harrow-server-monoio; \
    fi

FROM gcr.io/distroless/cc-debian13:latest-arm64

ARG BINARY_DIR=release

# Note: For io_uring support, run container with --privileged
COPY --from=builder /app/target/aarch64-unknown-linux-gnu/${BINARY_DIR}/harrow-server-monoio /harrow-server-monoio

CMD ["/harrow-server-monoio"]
