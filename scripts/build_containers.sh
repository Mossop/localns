#! /bin/bash

set -e
cd $(dirname "${BASH_SOURCE[0]:-$0}")/..

docker build -t localns_test_empty:latest \
  --label localns.testcontainer=1 \
  --label localns.hostname=test1.home.local \
  --label localns.network=bridge \
  test_resources/containers/empty

docker build -t localns_test_coredns:latest \
  --label localns.testcontainer=1 \
  test_resources/containers/coredns

docker build -t localns_test_traefik:latest \
  --label localns.testcontainer=1 \
  test_resources/containers/traefik
