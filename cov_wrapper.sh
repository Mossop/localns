#! /bin/sh

BINARIES=$(for file in \
  $( \
    RUSTFLAGS="-C instrument-coverage" \
      cargo test --all-features --no-run --message-format=json \
        | jq -r "select(.profile.test == true) | .filenames[]" \
        | grep -v dSYM - \
  ); do \
  printf "%s %s " -object $file; \
done)

COMMAND=$1
shift
rust-cov $COMMAND $BINARIES $@
