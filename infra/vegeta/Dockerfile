# Deprecated: Use peterevans/vegeta instead
# 
# This Dockerfile is kept for reference only.
# The docker-compose.yml now uses the official vegeta image:
#   image: peterevans/vegeta:latest
#
# To use this custom image instead, update docker-compose.yml:
#   vegeta:
#     build:
#       context: .
#       dockerfile: Dockerfile

FROM golang:1.23-alpine AS builder
RUN go install github.com/tsenart/vegeta@latest

FROM alpine:3.19
RUN apk add --no-cache ca-certificates curl jq bash
COPY --from=builder /go/bin/vegeta /usr/local/bin/vegeta
WORKDIR /vegeta
CMD ["vegeta", "-version"]
