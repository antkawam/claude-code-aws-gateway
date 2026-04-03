#!/bin/bash -eu

cd $SRC/ccag
export SQLX_OFFLINE=true

# Build each fuzz target and copy to $OUT
for target in fuzz/fuzz_targets/*.rs; do
    name=$(basename "$target" .rs)
    cargo +nightly fuzz build "$name"
    cp fuzz/target/x86_64-unknown-linux-gnu/release/"$name" "$OUT/"
done
