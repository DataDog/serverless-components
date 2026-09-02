#!/usr/bin/env bash
# Build the serverless-compat binary for x86_64-unknown-linux-musl.
# Called by scripts/svls9604/runner.py before packaging.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# Ensure rustup-managed toolchain is on PATH (needed when invoked by runner.py)
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

CARGO="${CARGO:-$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo}"
if [[ ! -x "$CARGO" ]]; then
    CARGO="$(command -v cargo)"
fi

echo "Building datadog-serverless-compat for x86_64-unknown-linux-musl..."
"$CARGO" build \
    --manifest-path "$REPO_ROOT/Cargo.toml" \
    --package datadog-serverless-compat \
    --target x86_64-unknown-linux-musl \
    --release

echo "Binary: $REPO_ROOT/target/x86_64-unknown-linux-musl/release/datadog-serverless-compat"
