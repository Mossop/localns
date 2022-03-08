FROM golang:alpine as go-build
ARG COREDNS_VERSION=1.9.0

RUN \
  apk update && \
  apk add git make && \
  git clone -b v${COREDNS_VERSION} https://github.com/coredns/coredns.git

RUN \
  cd coredns && \
  make

FROM rust:alpine as rust-build
WORKDIR /rust
COPY Cargo.* .
COPY src /rust/src
COPY bollard-stubs /rust/bollard-stubs
RUN apk add \
    musl-dev \
    openssl-dev && \
  cargo build

FROM alpine
ARG S6_OVERLAY_VERSION=3.0.0.2-2
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch-${S6_OVERLAY_VERSION}.tar.xz /tmp
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-x86_64-${S6_OVERLAY_VERSION}.tar.xz /tmp
RUN \
  tar -C / -Jxpf /tmp/s6-overlay-noarch-${S6_OVERLAY_VERSION}.tar.xz && \
  tar -C / -Jxpf /tmp/s6-overlay-x86_64-${S6_OVERLAY_VERSION}.tar.xz && \
  rm /tmp/*.tar.xz && \
  apk add openssl libc6-compat

COPY --from=go-build /go/bin/coredns /bin/coredns
COPY --from=rust-build /rust/target/debug/localns /bin/localns
COPY etc /etc/

ENV LOCALNS_CONFIG=/etc/localns/config.yaml
EXPOSE 53/udp

ENTRYPOINT ["/init"]
