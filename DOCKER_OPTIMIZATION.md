# Docker Optimization for Harrow

## Current State Analysis

### Main Dockerfile Issues:
1. **Overly Specific Copies**: Copies individual Cargo.toml and src/lib.rs files instead of entire directories
2. **Redundant Stages**: Multiple similar build stages for different binaries
3. **ARM64 Only**: Hard-coded to aarch64, limiting portability
4. **No Multi-stage Optimization**: Could better leverage build caching
5. **Vegeta Dockerfile**: Marked as deprecated but still maintained

### Docker Compose Issues:
1. **Complex Service Definitions**: Separate tokio-server and monoio-server services with duplication
2. **Privileged Mode Required**: For Monoio/io_uring, requires --privileged which is a security concern
3. **Healthcheck Complexity**: Custom healthcheck script when simple TCP check would suffice
4. **Vegeta Overhead**: Complex script-based test runner when simpler approaches exist

## Recommendations for Simplification

### 1. Unified, Portable Dockerfile

```dockerfile
# Use build arguments for flexibility
ARG PLATFORM=linux/amd64
ARG RUST_TOOLCHAIN=stable
ARG PROFILE=release

# Build stage
FROM --platform=${PLATFORM} rust:${RUST_TOOLCHAIN} AS build
WORKDIR /app

# Cache dependencies
COPY Cargo.toml Cargo.lock ./
# Create dummy src to cache dependencies
RUN mkdir src && echo "fn main() {}" > src/lib.rs
RUN cargo fetch --locked

# Copy actual source
COPY . .
# Build only what we need
RUN cargo build --locked --${PROFILE}

# Runtime stage - use distroless for security
FROM gcr.io/distroless/cc-debian12
COPY --from=build /app/target/${PROFILE}/harrow-server-tokio /harrow-server-tokio
EXPOSE 3000
ENTRYPOINT ["/harrow-server-tokio"]
CMD ["--bind", "0.0.0.0:3000"]
```

### 2. Simplified Docker Compose

```yaml
version: "3.8"
services:
  harrow:
    build:
      context: .
      dockerfile: Dockerfile
      args:
        PLATFORM: linux/amd64  # or linux/arm64 for ARM
        PROFILE: release
    ports:
      - "3000:3000"
    environment:
      - RUST_LOG=info
    healthcheck:
      test: ["CMD", "wget", "-qO-", "http://localhost:3000/health || exit 1"]
      interval: 5s
      timeout: 3s
      retries: 5
```

### 3. Monoio/Optimized Variant (Optional)

For users who want io_uring performance:

```yaml
  harrow-monoio:
    build:
      context: .
      dockerfile: Dockerfile
      args:
        PLATFORM: linux/amd64
        PROFILE: release
        # Features would be set via build-args or multi-target approach
    cap_add:
      - SYS_ADMIN  # More specific than privileged
    security_opt:
      - no-new-privileges:true
    environment:
      - HARROW_FEATURES=monoio
    # Note: Requires Linux 6.1+ host kernel
```

### 4. Simplified Testing Approach

Instead of complex vegeta test harness, recommend:

```yaml
version: "3.8"
services:
  app:
    build: .
    ports: ["3000:3000"]
    
  vegeta:
    image: peterevans/vegeta:latest
    depends_on:
      - app
    volumes:
      - ./targets:/targets
    command: >
      sh -c "
      vegeta attack -duration=10s -rate=1000 \
        -targets=/targets/get.txt |
      vegeta report
      "
```

With simple target file:
```
GET http://app:3000/
```

### 5. Multi-Platform Build Recommendations

Add to Makefile or CI:

```makefile
docker-build-all:
	docker buildx build \
	  --platform linux/amd64,linux/arm64 \
	  -t harrow-server-tokio:latest \
	  --load .

docker-push-all:
	docker buildx build \
	  --platform linux/amd64,linux/arm64 \
	  -t harrow-server-tokio:latest \
	  --push .
```

## Benefits of Simplification

1. **Reduced Complexity**: Single Dockerfile instead of multiple similar ones
2. **Better Caching**: Dependency caching works more effectively
3. **Portability**: Easy to switch between ARM/x86 via build args
4. **Security**: Distroless base image, minimal capabilities
5. **Maintainability**: Less duplication, easier to update
6. **Developer Experience**: Simpler local testing workflow
7. **CI/CD Friendly**: Standardized build and test processes

## Implementation Strategy

### Phase 1: Immediate Wins
- Consolidate Dockerfile copying strategy
- Simplify healthcheck to basic TCP/HTTP check
- Remove redundant example copying in Dockerfile

### Phase 2: Structural Improvements
- Implement unified Dockerfile with build args
- Consolidate docker-compose services
- Update documentation to reflect simpler approach

### Phase 3: Advanced Optimizations
- Multi-platform build support in CI
- Optional monoio variant with proper capability dropping
- Integrated testing targets in repository

## Backward Compatibility

To maintain compatibility:
1. Keep existing Dockerfile as `Dockerfile.legacy` temporarily
2. Provide migration guide in documentation
3. Ensure new approach produces functionally identical images
4. Gradually deprecate complex vegeta test harness

## Security Improvements

1. **Drop Privileges**: Use distroless images by default
2. **Minimal Capabilities**: Only grant what's absolutely needed (NET_RAW for ping, etc.)
3. **Non-root User**: Consider adding non-root user in future iterations
4. **Read-only Root FS**: Where applicable for added security
5. **Resource Limits**: Add memory/CPU constraints in compose files

## Performance Considerations

1. **Multi-stage Builds**: Already well-implemented, just needs simplification
2. **Layer Caching**: Optimize copy order for better cache hits
3. **Binary Stripping**: Ensure release builds are stripped
4. **Dynamic Linking**: Consider musl target for smaller static binaries
5. **Profile Guided Optimization**: Optional for performance-critical deployments

## Testing Validation

Ensure simplified approach maintains:
1. Same functionality as current setup
2. Equivalent performance characteristics
3. Identical API surface and behavior
4. Proper health checking
5. Correct signal handling for graceful shutdown

## Documentation Updates Needed

1. Update README.md with simplified Docker instructions
2. Create Docker-specific getting started guide
3. Update contributing documentation
4. Provide examples for common deployment scenarios (dev, staging, prod)
5. Document multi-platform build and deployment process