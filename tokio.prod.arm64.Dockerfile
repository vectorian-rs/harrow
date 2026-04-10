FROM --platform=linux/arm64 rust:1 AS build-env
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
COPY harrow-server-monoio/src/lib.rs harrow-server-monoio/src/lib.rs
COPY harrow-server-meguri/src/lib.rs harrow-server-meguri/src/lib.rs
COPY meguri/src/lib.rs meguri/src/lib.rs
COPY harrow-bench/benches harrow-bench/benches
COPY harrow-bench/src/lib.rs harrow-bench/src/lib.rs
COPY harrow-bench/src/bin harrow-bench/src/bin

RUN rustup target add aarch64-unknown-linux-gnu && \
    cargo fetch --locked --target=aarch64-unknown-linux-gnu

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

# All perf server binaries
ARG PERF_BINS="--bin harrow-perf-server --bin axum-perf-server --bin tako-perf-server --bin salvo-perf-server --bin warp-perf-server --bin ntex-perf-server"
ARG TARGET=aarch64-unknown-linux-gnu
ARG REL=/app/target/aarch64-unknown-linux-gnu/release

# Build with mimalloc (default features)
RUN cargo build --locked --release --target=${TARGET} -p harrow-bench ${PERF_BINS} && \
    mkdir -p /stage/mimalloc && \
    for bin in harrow-perf-server axum-perf-server tako-perf-server salvo-perf-server warp-perf-server ntex-perf-server; do \
        cp ${REL}/${bin} /stage/mimalloc/${bin}; \
    done

# Build with jemalloc
RUN cargo build --locked --release --target=${TARGET} -p harrow-bench \
        --no-default-features --features jemalloc ${PERF_BINS} && \
    mkdir -p /stage/jemalloc && \
    for bin in harrow-perf-server axum-perf-server tako-perf-server salvo-perf-server warp-perf-server ntex-perf-server; do \
        cp ${REL}/${bin} /stage/jemalloc/${bin}; \
    done

# Build with system allocator
RUN cargo build --locked --release --target=${TARGET} -p harrow-bench \
        --no-default-features ${PERF_BINS} && \
    mkdir -p /stage/system && \
    for bin in harrow-perf-server axum-perf-server tako-perf-server salvo-perf-server warp-perf-server ntex-perf-server; do \
        cp ${REL}/${bin} /stage/system/${bin}; \
    done

# ---------------------------------------------------------------------------
# One image per allocator — all 6 framework binaries inside each
# ---------------------------------------------------------------------------

FROM gcr.io/distroless/cc-debian13:latest-arm64 AS prod-mimalloc
ENV MIMALLOC_LARGE_OS_PAGES=1 MIMALLOC_ALLOW_DECOMMIT=0 MIMALLOC_EAGER_COMMIT=1
COPY --from=build-env /stage/mimalloc/ /usr/local/bin/

FROM gcr.io/distroless/cc-debian13:latest-arm64 AS prod-jemalloc
COPY --from=build-env /stage/jemalloc/ /usr/local/bin/

FROM gcr.io/distroless/cc-debian13:latest-arm64 AS prod-sysalloc
COPY --from=build-env /stage/system/ /usr/local/bin/
