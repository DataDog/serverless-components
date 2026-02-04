# Platform-Specific Binaries

This directory contains platform-specific NPM packages for @datadog/serverless-node.

Each subdirectory contains a minimal package.json that will be published separately
with the native binary for that platform.

## Platforms

- `darwin-x64` - macOS Intel (x86_64)
- `darwin-arm64` - macOS Apple Silicon (ARM64)
- `linux-x64-gnu` - Linux Intel/AMD (x86_64)
- `linux-arm64-gnu` - Linux ARM64
- `win32-x64-msvc` - Windows (x86_64)

The main package has optional dependencies on these platform-specific packages,
and npm will automatically install the correct one based on the user's platform.
