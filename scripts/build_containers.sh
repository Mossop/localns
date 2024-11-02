#! /bin/bash

set -e
cd $(dirname "${BASH_SOURCE[0]:-$0}")/..

docker build -t localns_test_empty:latest \
  --label localns.hostname=test1.home.local \
  --label localns.network=bridge \
  test_resources/containers/empty
