# Build stage — aarch64 only
FROM --platform=linux/arm64 rust:1 AS build-env
WORKDIR /app

# Copy manifests first so dependency resolution survives unrelated source edits.
COPY Cargo.toml Cargo.lock ./
COPY harrow/Cargo.toml harrow/Cargo.toml
COPY harrow-core/Cargo.toml harrow-core/Cargo.toml
COPY harrow-middleware/Cargo.toml harrow-middleware/Cargo.toml
COPY harrow-o11y/Cargo.toml harrow-o11y/Cargo.toml
COPY harrow-serde/Cargo.toml harrow-serde/Cargo.toml
COPY harrow-server/Cargo.toml harrow-server/Cargo.toml
COPY harrow-bench/Cargo.toml harrow-bench/Cargo.toml

RUN rustup target add aarch64-unknown-linux-gnu && \
    cargo fetch --locked --target=aarch64-unknown-linux-gnu

# Copy only the workspace source trees needed for the server binaries.
COPY harrow/src harrow/src
COPY harrow-core/src harrow-core/src
COPY harrow-middleware/src harrow-middleware/src
COPY harrow-o11y/src harrow-o11y/src
COPY harrow-serde/src harrow-serde/src
COPY harrow-server/src harrow-server/src
COPY harrow-bench/src harrow-bench/src

RUN cargo build --locked --release --target=aarch64-unknown-linux-gnu \
        -p harrow-bench \
        --bin harrow-server --bin axum-server \
        --bin harrow-perf-server --bin axum-perf-server

# --- harrow-server ---
FROM gcr.io/distroless/cc-debian13:latest-arm64 AS harrow-server
COPY --from=build-env /app/target/aarch64-unknown-linux-gnu/release/harrow-server /
CMD ["/harrow-server", "--bind", "0.0.0.0"]

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
