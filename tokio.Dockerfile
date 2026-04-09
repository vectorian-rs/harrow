# Build stage — aarch64 only
FROM --platform=$BUILDPLATFORM rust:1 AS build-env
WORKDIR /app

# Copy manifests first so dependency resolution survives unrelated source edits.
COPY Cargo.toml Cargo.lock ./
COPY harrow/Cargo.toml harrow/Cargo.toml

COPY harrow-core/Cargo.toml harrow-core/Cargo.toml

COPY harrow-middleware/Cargo.toml harrow-middleware/Cargo.toml

COPY harrow-o11y/Cargo.toml harrow-o11y/Cargo.toml

COPY harrow-serde/Cargo.toml harrow-serde/Cargo.toml

COPY harrow-server-tokio/Cargo.toml harrow-server-tokio/Cargo.toml

COPY harrow-server-monoio/Cargo.toml harrow-server-monoio/Cargo.toml

COPY harrow-server-meguri/Cargo.toml harrow-server-meguri/Cargo.toml

COPY meguri/Cargo.toml meguri/Cargo.toml

COPY harrow-bench/Cargo.toml harrow-bench/Cargo.toml

# Cargo needs target entrypoints present to resolve the workspace during fetch.
COPY harrow/examples harrow/examples
COPY harrow/src/lib.rs harrow/src/lib.rs
COPY harrow-core/src/lib.rs harrow-core/src/lib.rs
COPY harrow-middleware/src/lib.rs harrow-middleware/src/lib.rs
COPY harrow-o11y/src/lib.rs harrow-o11y/src/lib.rs
COPY harrow-serde/src/lib.rs harrow-serde/src/lib.rs
COPY harrow-server-tokio/src/lib.rs harrow-server-tokio/src/lib.rs
COPY harrow-server-meguri/src/lib.rs harrow-server-meguri/src/lib.rs
COPY meguri/src/lib.rs meguri/src/lib.rs
COPY harrow-bench/benches harrow-bench/benches
COPY harrow-bench/src/lib.rs harrow-bench/src/lib.rs
COPY harrow-bench/src/bin harrow-bench/src/bin

RUN rustup target add aarch64-unknown-linux-gnu && \
    cargo fetch --locked --target=aarch64-unknown-linux-gnu

# Copy only the workspace source trees needed for the server binaries.
COPY harrow/src harrow/src

COPY harrow-core/src harrow-core/src

COPY harrow-middleware/src harrow-middleware/src

COPY harrow-o11y/src harrow-o11y/src

COPY harrow-serde/src harrow-serde/src

COPY harrow-server-tokio/src harrow-server-tokio/src

COPY harrow-server-monoio/src harrow-server-monoio/src

COPY harrow-server-meguri/src harrow-server-meguri/src

COPY meguri/src meguri/src

COPY harrow-bench/src harrow-bench/src

RUN cargo build --locked --release --target=aarch64-unknown-linux-gnu \
        -p harrow-bench \
        --bin harrow-server-tokio --bin axum-server \
        --bin harrow-perf-server --bin axum-perf-server

# --- harrow-server-tokio ---
FROM gcr.io/distroless/cc-debian13:latest-arm64 AS harrow-server-tokio
COPY --from=build-env /app/target/aarch64-unknown-linux-gnu/release/harrow-server-tokio /
CMD ["/harrow-server-tokio", "--bind", "0.0.0.0"]

# --- axum-server ---
FROM gcr.io/distroless/cc-debian13:latest-arm64 AS axum-server
COPY --from=build-env /app/target/aarch64-unknown-linux-gnu/release/axum-server /
CMD ["/axum-server", "--bind", "0.0.0.0"]

# --- harrow-perf-server ---
FROM gcr.io/distroless/cc-debian13:latest-arm64 AS harrow-perf-server
COPY --from=build-env /app/target/aarch64-unknown-linux-gnu/release/harrow-perf-server /
CMD ["/harrow-perf-server", "--bind", "0.0.0.0"]

# --- axum-perf-server ---
FROM gcr.io/distroless/cc-debian13:latest-arm64 AS axum-perf-server
COPY --from=build-env /app/target/aarch64-unknown-linux-gnu/release/axum-perf-server /
CMD ["/axum-perf-server", "--bind", "0.0.0.0"]
