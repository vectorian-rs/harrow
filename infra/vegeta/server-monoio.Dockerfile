# Build stage
FROM rust:1.86-slim-bookworm AS builder

WORKDIR /build

# Install dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy entire workspace
COPY . .

# Build just the monoio example
RUN cargo build --release --example vegeta_target_monoio \
    --features monoio,json --no-default-features \
    -p harrow

# Runtime stage - using debian:bookworm-slim for healthcheck support
# NOTE: For production, consider gcr.io/distroless/cc-debian12 with external healthcheck
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binary
COPY --from=builder /build/target/release/examples/vegeta_target_monoio /app/server

EXPOSE 3000

# Note: For io_uring support, run container with --privileged
CMD ["/app/server"]
