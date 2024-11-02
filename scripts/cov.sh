#! /bin/bash

COMMAND=$1
shift
BINARIES=$(cat coverage/objects)

rust-cov $COMMAND $BINARIES --ignore-filename-regex="/.cargo/registry" --instr-profile=coverage/localns.profdata $@
