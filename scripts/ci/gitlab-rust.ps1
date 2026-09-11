$ErrorActionPreference = "Stop"

$Mode = $args[0]
if (-not $Mode) {
    throw "mode is required"
}

$Features = "datadog-serverless-compat/windows-pipes,datadog-serverless-compat/windows-enhanced-metrics"

switch ($Mode) {
    "check" {
        cargo check --workspace --features $Features
    }
    "fmt" {
        cargo fmt --all -- --check
    }
    "clippy" {
        $env:AWS_LC_FIPS_SYS_NO_ASM = "1"
        cargo clippy --workspace --all-features -- -D warnings
    }
    "build" {
        cargo build --all --features $Features
    }
    "compat-release" {
        cargo build --release -p datadog-serverless-compat --features $Features
        rustup target add i686-pc-windows-msvc
        cargo build --release -p datadog-serverless-compat --target i686-pc-windows-msvc --features $Features
    }
    "test" {
        cargo nextest run --workspace --features $Features
    }
    default {
        throw "unsupported mode: $Mode"
    }
}

if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
