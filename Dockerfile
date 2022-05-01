FROM rust:alpine as rust-build
RUN apk add musl-dev
WORKDIR /rust
COPY Cargo.* /rust/
RUN \
  mkdir src && \
  echo "fn main() {}" > src/main.rs && \
  cargo build --release && \
  rm -rf src
COPY src /rust/src
RUN cargo build --release

FROM alpine
ARG S6_OVERLAY_VERSION=3.1.0.1
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz /tmp
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-x86_64.tar.xz /tmp
RUN \
  tar -C / -Jxpf /tmp/s6-overlay-noarch.tar.xz && \
  tar -C / -Jxpf /tmp/s6-overlay-x86_64.tar.xz && \
  rm /tmp/*.tar.xz && \
  apk add --no-cache coredns

COPY --from=rust-build /rust/target/*/localns /bin/localns
COPY etc /etc/

ENV LOCALNS_CONFIG=/etc/localns/config.yaml
EXPOSE 53/udp
EXPOSE 53/tcp

ENTRYPOINT ["/init"]
