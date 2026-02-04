# Building Cross-Platform

## Prerequisites

Before building, ensure you have:
- **Node.js**: Version 16 or higher
- **npm**: Version 10 or higher
- **Rust**: Latest stable toolchain (install via [rustup](https://rustup.rs/))
- **Platform-specific tools**:
  - macOS: Xcode Command Line Tools
  - Linux: `build-essential` or equivalent
  - Windows: Visual Studio Build Tools

## Local Development

Build for your current platform:
```bash
npm run build
```

## CI/CD

Cross-platform builds run automatically on GitHub Actions for:
- macOS (x64 + ARM64 universal binary)
- Linux (x64 + ARM64)
- Windows (x64)

## Manual Cross-Compilation

### macOS ARM64 (from macOS x64)
```bash
rustup target add aarch64-apple-darwin
npm run build -- --target aarch64-apple-darwin
```

### Linux x64 (requires Docker)
```bash
# From repository root
docker run --rm -v $(pwd):/build -w /build/crates/datadog-serverless-node \
  ghcr.io/napi-rs/napi-rs/nodejs-rust:lts-debian \
  npm run build -- --target x86_64-unknown-linux-gnu
```

### Linux ARM64 (requires Docker)
```bash
# From repository root
docker run --rm -v $(pwd):/build -w /build/crates/datadog-serverless-node \
  ghcr.io/napi-rs/napi-rs/nodejs-rust:lts-debian-aarch64 \
  npm run build -- --target aarch64-unknown-linux-gnu
```

## Publishing

Publishing happens automatically via GitHub Actions when you push a tag.

### Steps:

1. **Update version**: Edit `package.json` and increment the version
2. **Commit changes**: `git commit -am "chore: bump version to 0.2.0"`
3. **Create git tag**: `git tag nodejs-bridge-v0.2.0`
4. **Push tag**: `git push origin nodejs-bridge-v0.2.0`

GitHub Actions will:
- Build all platform binaries
- Run tests on macOS, Linux, and Windows
- Create universal macOS binary
- Publish to npm registry (requires `NPM_TOKEN` secret)

**Note**: Only tags trigger publishing. Regular pushes only run tests.

## Troubleshooting

### Build Failures

**Rust not found**:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

**Clean build**:
```bash
rm -rf target/
rm -rf npm/*/
npm run build
```

**Platform-specific issues**:
- macOS: Install Xcode Command Line Tools: `xcode-select --install`
- Linux: Install build tools: `sudo apt-get install build-essential`
- Windows: Set `AWS_LC_FIPS_SYS_NO_ASM=1` environment variable if build fails

## Artifacts

After build, artifacts are organized by platform:
```
npm/
├── darwin-x64/
│   └── datadog-serverless-node.darwin-x64.node
├── darwin-arm64/
│   └── datadog-serverless-node.darwin-arm64.node
└── ...
```
