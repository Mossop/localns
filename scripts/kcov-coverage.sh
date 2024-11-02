#! /bin/bash

cargo clean
mkdir -p target/cov
CARGO_INCREMENTAL=0 RUSTFLAGS="-C link-dead-code" cargo test --tests --all-features --no-run

for file in target/debug/deps/localns-*; do
  [ -d "${file}" ] && continue
  [ -x "${file}" ] || continue
  mkdir -p "target/cov/$(basename $file)"
  kcov --exclude-pattern=/.cargo --verify "target/cov/$(basename $file)" "$file"
done

kcov --merge target/cov/localns target/cov/localns-*
rm -rf target/cov/localns-*
