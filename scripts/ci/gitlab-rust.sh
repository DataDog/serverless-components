#!/usr/bin/env bash

set -euo pipefail

mode="${1:?mode is required}"
platform="${2:?platform is required}"
features=""
if [[ "$platform" == windows ]]; then
    features="--features datadog-serverless-compat/windows-pipes,datadog-serverless-compat/windows-enhanced-metrics"
fi

case "$mode" in
    check)
        # shellcheck disable=SC2086
        cargo check --workspace $features
        ;;
    fmt)
        cargo fmt --all -- --check
        ;;
    clippy)
        if [[ "$platform" == windows ]]; then
            export AWS_LC_FIPS_SYS_NO_ASM=1
        fi
        cargo clippy --workspace --all-features -- -D warnings
        ;;
    build)
        # shellcheck disable=SC2086
        cargo build --all $features
        ;;
    compat-release)
        case "$platform" in
            linux-amd64)
                rustup target add x86_64-unknown-linux-musl
                cargo build --release -p datadog-serverless-compat --target x86_64-unknown-linux-musl
                ;;
            linux-arm64)
                rustup target add aarch64-unknown-linux-musl
                cargo build --release -p datadog-serverless-compat --target aarch64-unknown-linux-musl
                ;;
            windows)
                cargo build --release -p datadog-serverless-compat --features windows-pipes,windows-enhanced-metrics
                rustup target add i686-pc-windows-msvc
                cargo build --release -p datadog-serverless-compat \
                    --target i686-pc-windows-msvc \
                    --features windows-pipes,windows-enhanced-metrics
                ;;
            *) echo "unsupported platform: $platform" >&2; exit 2 ;;
        esac
        ;;
    test)
        # shellcheck disable=SC2086
        cargo nextest run --workspace $features
        ;;
    *) echo "unsupported mode: $mode" >&2; exit 2 ;;
esac
