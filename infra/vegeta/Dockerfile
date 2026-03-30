# Multi-arch Vegeta image for ARM64/AMD64
# Build with: docker build --platform linux/arm64 -t vegeta:arm64 .

FROM --platform=$BUILDPLATFORM golang:1.23-alpine AS builder
ARG TARGETARCH
ARG TARGETOS
ARG VEGETA_VERSION=v12.12.0

# Install vegeta for the target architecture
RUN GOOS=${TARGETOS} GOARCH=${TARGETARCH} go install github.com/tsenart/vegeta@${VEGETA_VERSION}

FROM alpine:3.19
RUN apk add --no-cache ca-certificates curl jq bash
COPY --from=builder /go/bin/vegeta /usr/local/bin/vegeta
WORKDIR /vegeta
CMD ["vegeta", "-version"]
