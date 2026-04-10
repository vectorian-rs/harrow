# wrk3 HTTP load testing tool (fork of wrk2 with Coordinated Omission correction)
# Multi-arch support: linux/arm64, linux/amd64

FROM --platform=$BUILDPLATFORM alpine:3.19 AS downloader
ARG TARGETARCH
ARG WRK3_VERSION=0.2.0

RUN set -ex \
 && apk add --no-cache wget \
 && ARCH=$(echo $TARGETARCH | sed 's/amd64/x86_64/;s/arm64/arm64/') \
 && wget -q "https://github.com/vectorian-rs/wrk3/releases/download/v${WRK3_VERSION}/wrk3-v${WRK3_VERSION}-linux-${ARCH}.tar.gz" -O /tmp/wrk3.tar.gz \
 && tar xzf /tmp/wrk3.tar.gz -C /tmp \
 && mv /tmp/wrk3-v${WRK3_VERSION}-linux-${ARCH}/wrk /tmp/wrk

FROM gcr.io/distroless/cc-debian13
COPY --from=downloader /tmp/wrk /usr/local/bin/wrk
ENTRYPOINT ["/usr/local/bin/wrk"]
CMD ["--help"]
