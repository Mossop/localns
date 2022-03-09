#! /bin/sh

set -e
docker build -t localns .
docker run --rm -e RUST_LOG=localns=trace -v ${PWD}:/etc/localns --dns 127.0.0.1 -p 53:53/udp localns
