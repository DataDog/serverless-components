# Building Cross-Platform

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

### Linux (requires Docker)
```bash
docker run --rm -v $(pwd):/build -w /build/crates/datadog-serverless-node \
  ghcr.io/napi-rs/napi-rs/nodejs-rust:lts-debian \
  npm run build
```

## Publishing

1. Update version in package.json
2. Create git tag: `git tag nodejs-bridge-v0.1.0`
3. Push tag: `git push origin nodejs-bridge-v0.1.0`
4. GitHub Actions will build and publish automatically

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
