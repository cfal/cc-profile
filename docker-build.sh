#!/bin/sh
set -eu
cd "$(dirname "$0")"
exec docker run --rm --init --user "$(id -u):$(id -g)" -v "$PWD:/volume" \
    -e CARGO_HOME=/volume/target/.muslrust-cargo -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}" \
    clux/muslrust:1.100.0-stable@sha256:7f48348bb38fb6f42737c1888de6eb1e3af2ef70e5e0df58717b88dea93d96bd \
    cargo build --release --locked "$@"
