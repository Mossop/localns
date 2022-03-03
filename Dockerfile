FROM golang:alpine as go-build

RUN \
  apk update && \
  apk add git make patch && \
  git clone -b split-horizon https://github.com/Mossop/coredns

RUN \
  cd coredns && \
  make

FROM rust:alpine as rust-build
WORKDIR /rust
COPY Cargo.* .
COPY src /rust/src
COPY bollard-stubs /rust/bollard-stubs
RUN apk add \
    musl-dev && \
  cargo build --release

FROM alpine
ARG S6_OVERLAY_VERSION=3.0.0.2-2
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch-${S6_OVERLAY_VERSION}.tar.xz /tmp
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-x86_64-${S6_OVERLAY_VERSION}.tar.xz /tmp
RUN \
  tar -C / -Jxpf /tmp/s6-overlay-noarch-${S6_OVERLAY_VERSION}.tar.xz && \
  tar -C / -Jxpf /tmp/s6-overlay-x86_64-${S6_OVERLAY_VERSION}.tar.xz && \
  rm /tmp/*.tar.xz && \
  mkdir -p /etc/zones

COPY --from=go-build /go/bin/coredns /bin/coredns
COPY --from=rust-build /rust/target/release/localns /bin/localns
COPY etc /etc/

ENV LOCALNS_CONFIG=/etc/localns/config.yml
ENV LOCALNS_ZONE_DIR=/etc/zones
EXPOSE 53/tcp
EXPOSE 53/udp

ENTRYPOINT ["/init"]
