# wrk3 HTTP load testing tool — ARM64-only
# Fork of wrk2 with Coordinated Omission correction

FROM --platform=linux/arm64 alpine:3.19 AS downloader
ARG WRK3_VERSION=0.2.0

RUN set -ex \
 && apk add --no-cache wget \
 && wget -q "https://github.com/vectorian-rs/wrk3/releases/download/v${WRK3_VERSION}/wrk3-v${WRK3_VERSION}-linux-arm64.tar.gz" -O /tmp/wrk3.tar.gz \
 && tar xzf /tmp/wrk3.tar.gz -C /tmp \
 && mv /tmp/wrk3-v${WRK3_VERSION}-linux-arm64/wrk /tmp/wrk

FROM debian:testing-slim
COPY --from=downloader /tmp/wrk /usr/local/bin/wrk
ENTRYPOINT ["/usr/local/bin/wrk"]
CMD ["--help"]
