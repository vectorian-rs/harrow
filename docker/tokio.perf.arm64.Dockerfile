# Build stage — aarch64 only, profile=perf (full DWARF + frame pointers)
FROM --platform=linux/arm64 rust:1 AS build-env
WORKDIR /app

ENV RUSTFLAGS="-g -Cforce-frame-pointers=on"

# Copy manifests first so dependency resolution survives unrelated source edits.
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

# Cargo needs target entrypoints present to resolve the workspace during fetch.
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

RUN rustup target add aarch64-unknown-linux-gnu && \
    cargo fetch --locked --target=aarch64-unknown-linux-gnu

# Copy all workspace source trees.
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

ARG PERF_BINS="--bin harrow-perf-server --bin harrow-server-monoio --bin harrow-server-meguri --bin ntex-perf-server"
ARG TARGET=aarch64-unknown-linux-gnu
ARG PERF_DIR=/app/target/aarch64-unknown-linux-gnu/perf

# Build all perf binaries with mimalloc (default features)
RUN cargo build --locked --profile perf --target=${TARGET} \
        -p harrow-bench ${PERF_BINS} && \
    mkdir -p /out && \
    cp ${PERF_DIR}/harrow-perf-server /out/ && \
    cp ${PERF_DIR}/harrow-server-monoio /out/ && \
    cp ${PERF_DIR}/harrow-server-meguri /out/ && \
    cp ${PERF_DIR}/ntex-perf-server /out/

# ---------------------------------------------------------------------------
# Runtime images — distroless, profiling tools run on the host
# (perf record -g -p <pid>, strace -c -f -p <pid>)
# ---------------------------------------------------------------------------

FROM gcr.io/distroless/cc-debian13:latest-arm64 AS harrow-perf-server
COPY --from=build-env /out/harrow-perf-server /
CMD ["/harrow-perf-server", "--bind", "0.0.0.0"]

FROM gcr.io/distroless/cc-debian13:latest-arm64 AS harrow-monoio-perf
COPY --from=build-env /out/harrow-server-monoio /
CMD ["/harrow-server-monoio", "--bind", "0.0.0.0"]

FROM gcr.io/distroless/cc-debian13:latest-arm64 AS harrow-meguri-perf
COPY --from=build-env /out/harrow-server-meguri /
CMD ["/harrow-server-meguri", "--bind", "0.0.0.0"]

FROM gcr.io/distroless/cc-debian13:latest-arm64 AS ntex-perf-server
COPY --from=build-env /out/ntex-perf-server /
CMD ["/ntex-perf-server", "--bind", "0.0.0.0"]
