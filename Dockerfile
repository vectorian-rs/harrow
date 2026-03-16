# Build stage — aarch64 only
FROM --platform=linux/arm64 rust:1 AS build-env
WORKDIR /app
COPY . /app
RUN rustup target add aarch64-unknown-linux-gnu && \
    cargo build --release --target=aarch64-unknown-linux-gnu \
        --bin harrow-server --bin axum-server \
        --bin serde-bench-server --bin axum-serde-server

# --- harrow-server ---
FROM gcr.io/distroless/cc-debian13:latest-arm64 AS harrow-server
COPY --from=build-env /app/target/aarch64-unknown-linux-gnu/release/harrow-server /
CMD ["/harrow-server", "--bind", "0.0.0.0"]

# --- axum-server ---
FROM gcr.io/distroless/cc-debian13:latest-arm64 AS axum-server
COPY --from=build-env /app/target/aarch64-unknown-linux-gnu/release/axum-server /
CMD ["/axum-server", "--bind", "0.0.0.0"]

# --- serde-bench-server ---
FROM gcr.io/distroless/cc-debian13:latest-arm64 AS serde-bench-server
COPY --from=build-env /app/target/aarch64-unknown-linux-gnu/release/serde-bench-server /
CMD ["/serde-bench-server", "--bind", "0.0.0.0"]

# --- axum-serde-server ---
FROM gcr.io/distroless/cc-debian13:latest-arm64 AS axum-serde-server
COPY --from=build-env /app/target/aarch64-unknown-linux-gnu/release/axum-serde-server /
CMD ["/axum-serde-server", "--bind", "0.0.0.0"]
