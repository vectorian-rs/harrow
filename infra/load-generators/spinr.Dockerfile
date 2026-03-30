# Spinr HTTP load testing tool
# Multi-arch support: linux/arm64, linux/amd64

FROM --platform=$BUILDPLATFORM alpine:3.19 AS downloader
ARG TARGETARCH
ARG SPINR_VERSION=0.5.1

RUN set -ex \
 && apk add --no-cache wget \
 && ARCH=$(echo $TARGETARCH | sed 's/amd64/x86_64/;s/arm64/arm64/') \
 && wget -q "https://github.com/l1x/spinr/releases/download/v${SPINR_VERSION}/spinr-v${SPINR_VERSION}-linux-${ARCH}.tar.gz" -O /tmp/spinr.tar.gz \
 && tar xzf /tmp/spinr.tar.gz -C /tmp \
 && mv /tmp/spinr-v${SPINR_VERSION}-linux-${ARCH}/spinr /tmp/spinr

FROM debian:testing-slim
COPY --from=downloader /tmp/spinr /usr/local/bin/spinr
ENTRYPOINT ["/usr/local/bin/spinr"]
CMD ["--help"]
