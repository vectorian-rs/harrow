# Vegeta HTTP load testing tool
# Multi-arch support: linux/arm64, linux/amd64

FROM --platform=$BUILDPLATFORM alpine:3.19 AS downloader
ARG TARGETARCH
ARG VEGETA_VERSION=12.12.0

RUN set -ex \
 && apk add --no-cache wget \
 && ARCH=$(echo $TARGETARCH | sed 's/amd64/x86_64/;s/arm64/arm64/') \
 && wget -q "https://github.com/tsenart/vegeta/releases/download/v${VEGETA_VERSION}/vegeta_${VEGETA_VERSION}_linux_${ARCH}.tar.gz" -O /tmp/vegeta.tar.gz \
 && tar xzf /tmp/vegeta.tar.gz -C /tmp vegeta

FROM debian:testing-slim
COPY --from=downloader /tmp/vegeta /usr/local/bin/vegeta
ENTRYPOINT ["/usr/local/bin/vegeta"]
CMD ["-help"]
