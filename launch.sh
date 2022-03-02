#! /bin/sh

docker build -t docker-dns .
docker run --rm -e RUST_LOG=docker_dns=trace -v ${PWD}:/etc/docker-dns -p 5333:53/udp docker-dns
