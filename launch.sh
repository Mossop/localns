#! /bin/sh

docker build -t localns .
docker run --rm -e RUST_LOG=localns=trace -v ${PWD}:/etc/localns -p 5333:53/udp localns
