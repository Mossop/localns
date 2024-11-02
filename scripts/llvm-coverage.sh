#! /bin/bash

export LLVM_PROFILE_FILE="target/cov/default_%m_%p.profraw"

cargo clean
mkdir -p target/cov
CARGO_INCREMENTAL=0 RUSTFLAGS="-C link-dead-code -C instrument-coverage" cargo test --tests --all-features

echo -n "" > target/cov/objects
for file in target/debug/deps/localns-*; do
  [ -d "${file}" ] && continue
  [ -x "${file}" ] || continue
  echo -n " -object ${file}" >> target/cov/objects
done

rust-profdata merge -sparse target/cov/default_*.profraw -o target/cov/localns.profdata
rm -f target/cov/default_*.profraw
