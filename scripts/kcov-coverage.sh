#! /bin/bash

cargo clean
mkdir -p target/cov/tests
CWD=$(pwd)
CARGO_INCREMENTAL=0 RUSTFLAGS="-C link-dead-code" \
  cargo test --tests --all-features --no-run --message-format json \
  | jq "select(.reason == \"compiler-artifact\" and .profile.test and .manifest_path == \"$CWD/Cargo.toml\")" \
  > target/cov/tests.json

KCOV_ARGS="--exclude-pattern=/.cargo --verify --output-interval=0"

for binary in $(cat target/cov/tests.json | jq -r 'select(.target.kind[0] == "test") | .executable'); do
  RUST_BACKTRACE=1 KCOV=`which kcov` "$binary"
done

for binary in $(cat target/cov/tests.json | jq -r 'select(.target.kind[0] != "test") | .executable'); do
  RUST_BACKTRACE=1 kcov $KCOV_ARGS "target/cov/$(basename $binary)" "$binary"
done
