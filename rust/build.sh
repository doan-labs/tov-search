#!/bin/sh
# Build the search engine into bin/engine (see bench/PROTOCOL.md).
# Editable: any language and toolchain, as long as this leaves bin/engine.
set -e
cd "$(dirname "$0")"
export RUSTFLAGS="-C target-cpu=native"
cargo build --release --quiet --bin engine
mkdir -p bin
cp target/release/engine bin/engine
