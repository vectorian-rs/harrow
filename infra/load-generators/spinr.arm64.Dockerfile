FROM --platform=linux/arm64 alpine:3.19 AS downloader
ARG TARGETARCH
ARG SPINR_VERSION=0.5.1

RUN set -ex \
 && apk add --no-cache wget \
 && wget -q "https://github.com/l1x/spinr/releases/download/v${SPINR_VERSION}/spinr-v${SPINR_VERSION}-linux-arm64.tar.gz" -O /tmp/spinr.tar.gz \
 && tar xzf /tmp/spinr.tar.gz -C /tmp \
 && mv /tmp/spinr-v${SPINR_VERSION}-linux-arm64/spinr /tmp/spinr

# Use distroless cc-debian13 (has newer glibc that spinr needs)
FROM gcr.io/distroless/cc-debian13:latest-arm64
COPY --from=downloader /tmp/spinr /usr/local/bin/spinr
ENTRYPOINT ["/usr/local/bin/spinr"]
CMD ["--help"]
