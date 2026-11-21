#!/bin/sh
set -eu
cd "$(dirname "$0")"
exec docker run --rm --init --user "$(id -u):$(id -g)" -v "$PWD:/volume" \
    -e CARGO_HOME=/volume/target/.muslrust-cargo -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}" \
    clux/muslrust:stable cargo build --release --locked "$@"
