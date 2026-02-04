# Node.js Bridge - Phase 4: Cross-Platform Builds

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Set up automated cross-platform builds for the NAPI addon, producing native binaries for Linux (x64/ARM64), macOS (x64/ARM64), and Windows (x64). Prepare NPM package structure with platform-specific optional dependencies.

**Architecture:** Use GitHub Actions with napi-rs CI templates to build on multiple platforms. Create platform-specific NPM packages that get installed as optional dependencies. Use @napi-rs/cli for artifact management.

**Tech Stack:** GitHub Actions, napi-rs, NPM workspaces, cross-compilation

**Prerequisites:** Phase 3 must be completed (graceful shutdown working)

---

## Task 1: Setup NAPI-RS Build Configuration

**Files:**
- Modify: `crates/datadog-serverless-node/package.json`
- Create: `crates/datadog-serverless-node/.npmignore`

**Step 1: Update package.json with build configuration**

Modify: `crates/datadog-serverless-node/package.json`

Update the `napi` section:

```json
{
  "name": "@datadog/serverless-node",
  "version": "0.1.0",
  "description": "Node.js bindings for Datadog serverless monitoring",
  "main": "index.js",
  "types": "index.d.ts",
  "napi": {
    "name": "datadog-serverless-node",
    "triples": {
      "defaults": true,
      "additional": [
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc"
      ]
    }
  },
  "scripts": {
    "artifacts": "napi artifacts",
    "build": "napi build --platform --release",
    "build:debug": "napi build --platform",
    "prepublishOnly": "napi prepublish -t npm",
    "test": "mocha test/**/*.test.js",
    "universal": "napi universal",
    "version": "napi version"
  },
  "packageManager": "npm@10.0.0",
  "devDependencies": {
    "@napi-rs/cli": "^2.18.0",
    "mocha": "^10.2.0"
  },
  "engines": {
    "node": ">= 16"
  },
  "publishConfig": {
    "registry": "https://registry.npmjs.org/",
    "access": "public"
  },
  "repository": {
    "type": "git",
    "url": "https://github.com/DataDog/serverless-components"
  },
  "license": "Apache-2.0",
  "keywords": [
    "datadog",
    "monitoring",
    "serverless",
    "lambda",
    "azure-functions",
    "napi-rs",
    "rust"
  ],
  "optionalDependencies": {
    "@datadog/serverless-node-darwin-x64": "0.1.0",
    "@datadog/serverless-node-darwin-arm64": "0.1.0",
    "@datadog/serverless-node-linux-x64-gnu": "0.1.0",
    "@datadog/serverless-node-linux-arm64-gnu": "0.1.0",
    "@datadog/serverless-node-win32-x64-msvc": "0.1.0"
  }
}
```

**Step 2: Create .npmignore**

Create: `crates/datadog-serverless-node/.npmignore`

```
target/
node_modules/
**/*.rs
!index.d.ts
!index.js
Cargo.toml
Cargo.lock
build.rs
.github/
.gitignore
examples/
test/
*.node
!npm/
```

**Step 3: Test artifacts command**

```bash
cd crates/datadog-serverless-node
npm run artifacts
```

Expected: Shows artifact configuration

**Step 4: Commit**

```bash
git add crates/datadog-serverless-node/
git commit -m "feat(node): configure cross-platform build support

Update package.json with:
- Target platforms (darwin/linux/win32, x64/arm64)
- Optional dependencies for platform-specific binaries
- Build scripts for artifacts and universal binaries
- NPM publish configuration

Add .npmignore to exclude build files from package.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Task 2: Create GitHub Actions Workflow

**Files:**
- Create: `.github/workflows/nodejs-bridge-ci.yml`

**Step 1: Create GitHub Actions workflow**

Create: `.github/workflows/nodejs-bridge-ci.yml`

```yaml
name: Node.js Bridge CI

on:
  push:
    branches: [main, feature/nodejs-bridge]
    paths:
      - 'crates/datadog-serverless-node/**'
      - 'crates/datadog-serverless-core/**'
      - '.github/workflows/nodejs-bridge-ci.yml'
  pull_request:
    branches: [main]
    paths:
      - 'crates/datadog-serverless-node/**'
      - 'crates/datadog-serverless-core/**'

env:
  DEBUG: napi:*
  APP_NAME: datadog-serverless-node
  MACOSX_DEPLOYMENT_TARGET: '10.13'

permissions:
  contents: write
  id-token: write

jobs:
  build:
    strategy:
      fail-fast: false
      matrix:
        settings:
          - host: macos-latest
            target: x86_64-apple-darwin
            build: |
              npm run build
              strip -x *.node
          - host: macos-latest
            target: aarch64-apple-darwin
            build: |
              npm run build -- --target aarch64-apple-darwin
              strip -x *.node
          - host: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            docker: ghcr.io/napi-rs/napi-rs/nodejs-rust:lts-debian
            build: |
              npm run build -- --target x86_64-unknown-linux-gnu
              strip *.node
          - host: ubuntu-latest
            target: aarch64-unknown-linux-gnu
            docker: ghcr.io/napi-rs/napi-rs/nodejs-rust:lts-debian-aarch64
            build: |
              npm run build -- --target aarch64-unknown-linux-gnu
              aarch64-unknown-linux-gnu-strip *.node
          - host: windows-latest
            target: x86_64-pc-windows-msvc
            build: npm run build

    name: stable - ${{ matrix.settings.target }} - node@20
    runs-on: ${{ matrix.settings.host }}

    steps:
      - uses: actions/checkout@v4

      - name: Setup node
        uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: npm
          cache-dependency-path: crates/datadog-serverless-node/package-lock.json

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          toolchain: stable
          targets: ${{ matrix.settings.target }}

      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry/index/
            ~/.cargo/registry/cache/
            ~/.cargo/git/db/
            .cargo-cache
            target/
          key: ${{ matrix.settings.target }}-cargo-${{ hashFiles('**/Cargo.lock') }}

      - name: Install dependencies
        run: |
          cd crates/datadog-serverless-node
          npm install

      - name: Build in docker
        uses: addnab/docker-run-action@v3
        if: ${{ matrix.settings.docker }}
        with:
          image: ${{ matrix.settings.docker }}
          options: '--user 0:0 -v ${{ github.workspace }}/.cargo-cache/git/db:/usr/local/cargo/git/db -v ${{ github.workspace }}/.cargo/registry/cache:/usr/local/cargo/registry/cache -v ${{ github.workspace }}/.cargo/registry/index:/usr/local/cargo/registry/index -v ${{ github.workspace }}:/build -w /build/crates/datadog-serverless-node'
          run: ${{ matrix.settings.build }}

      - name: Build
        run: |
          cd crates/datadog-serverless-node
          ${{ matrix.settings.build }}
        if: ${{ !matrix.settings.docker }}
        shell: bash

      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: bindings-${{ matrix.settings.target }}
          path: crates/datadog-serverless-node/${{ env.APP_NAME }}.*.node
          if-no-files-found: error

  test:
    name: Test bindings on ${{ matrix.settings.target }} - node@${{ matrix.node }}
    needs:
      - build
    strategy:
      fail-fast: false
      matrix:
        settings:
          - host: macos-latest
            target: x86_64-apple-darwin
          - host: windows-latest
            target: x86_64-pc-windows-msvc
          - host: ubuntu-latest
            target: x86_64-unknown-linux-gnu
        node:
          - '18'
          - '20'

    runs-on: ${{ matrix.settings.host }}

    steps:
      - uses: actions/checkout@v4

      - name: Setup node
        uses: actions/setup-node@v4
        with:
          node-version: ${{ matrix.node }}
          cache: npm
          cache-dependency-path: crates/datadog-serverless-node/package-lock.json

      - name: Install dependencies
        run: |
          cd crates/datadog-serverless-node
          npm install

      - name: Download artifacts
        uses: actions/download-artifact@v4
        with:
          name: bindings-${{ matrix.settings.target }}
          path: crates/datadog-serverless-node

      - name: List packages
        run: ls -R crates/datadog-serverless-node
        shell: bash

      - name: Test bindings
        run: |
          cd crates/datadog-serverless-node
          npm test

  universal-macOS:
    name: Build universal macOS binary
    needs:
      - build
    runs-on: macos-latest

    steps:
      - uses: actions/checkout@v4

      - name: Setup node
        uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: npm
          cache-dependency-path: crates/datadog-serverless-node/package-lock.json

      - name: Install dependencies
        run: |
          cd crates/datadog-serverless-node
          npm install

      - name: Download macOS x64 artifact
        uses: actions/download-artifact@v4
        with:
          name: bindings-x86_64-apple-darwin
          path: crates/datadog-serverless-node/artifacts

      - name: Download macOS arm64 artifact
        uses: actions/download-artifact@v4
        with:
          name: bindings-aarch64-apple-darwin
          path: crates/datadog-serverless-node/artifacts

      - name: Combine to universal binary
        run: |
          cd crates/datadog-serverless-node
          npm run universal

      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: bindings-universal-apple-darwin
          path: crates/datadog-serverless-node/${{ env.APP_NAME }}.*.node
          if-no-files-found: error

  publish:
    name: Publish
    runs-on: ubuntu-latest
    needs:
      - test
      - universal-macOS
    if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/')

    steps:
      - uses: actions/checkout@v4

      - name: Setup node
        uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: npm
          cache-dependency-path: crates/datadog-serverless-node/package-lock.json

      - name: Install dependencies
        run: |
          cd crates/datadog-serverless-node
          npm install

      - name: Download all artifacts
        uses: actions/download-artifact@v4
        with:
          path: crates/datadog-serverless-node/artifacts

      - name: Move artifacts
        run: |
          cd crates/datadog-serverless-node
          npm run artifacts

      - name: List packages
        run: ls -R crates/datadog-serverless-node/npm
        shell: bash

      - name: Publish
        run: |
          cd crates/datadog-serverless-node
          echo "//registry.npmjs.org/:_authToken=$NPM_TOKEN" >> ~/.npmrc
          npm publish --access public
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          NPM_TOKEN: ${{ secrets.NPM_TOKEN }}
```

**Step 2: Test workflow locally (optional)**

You can test the workflow syntax:

```bash
# Install act (GitHub Actions local runner) if you want to test locally
# brew install act
# act -l  # List jobs
```

**Step 3: Commit**

```bash
git add .github/workflows/nodejs-bridge-ci.yml
git commit -m "ci: add GitHub Actions workflow for cross-platform builds

Add CI workflow that:
- Builds for 5 platforms (darwin x64/arm64, linux x64/arm64, win32 x64)
- Tests on Node.js 18 and 20
- Creates universal macOS binary
- Publishes to NPM on tag push

Uses napi-rs Docker images for Linux builds and cross-compilation.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Task 3: Create Platform-Specific Package Templates

**Files:**
- Create: `crates/datadog-serverless-node/npm/` directory structure

**Step 1: Create npm directory and platform templates**

```bash
cd crates/datadog-serverless-node
mkdir -p npm/{darwin-x64,darwin-arm64,linux-x64-gnu,linux-arm64-gnu,win32-x64-msvc}
```

**Step 2: Create package.json for each platform**

For each platform, create a package.json. Example for darwin-x64:

Create: `crates/datadog-serverless-node/npm/darwin-x64/package.json`

```json
{
  "name": "@datadog/serverless-node-darwin-x64",
  "version": "0.1.0",
  "os": ["darwin"],
  "cpu": ["x64"],
  "main": "datadog-serverless-node.darwin-x64.node",
  "files": [
    "datadog-serverless-node.darwin-x64.node"
  ],
  "license": "Apache-2.0",
  "engines": {
    "node": ">= 16"
  },
  "repository": {
    "type": "git",
    "url": "https://github.com/DataDog/serverless-components"
  }
}
```

Create similar files for other platforms (darwin-arm64, linux-x64-gnu, linux-arm64-gnu, win32-x64-msvc) with appropriate values.

**Step 3: Create README for npm directory**

Create: `crates/datadog-serverless-node/npm/README.md`

```markdown
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
```

**Step 4: Commit**

```bash
git add crates/datadog-serverless-node/npm/
git commit -m "feat(node): add platform-specific package structure

Create npm/ directory with package.json templates for each platform:
- darwin-x64, darwin-arm64
- linux-x64-gnu, linux-arm64-gnu
- win32-x64-msvc

Main package uses optional dependencies to install correct binary.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Task 4: Test Local Cross-Platform Build

**Files:**
- None (testing only)

**Step 1: Test current platform build**

```bash
cd crates/datadog-serverless-node
npm run build
```

Expected: Builds successfully for current platform

**Step 2: Test artifacts command**

```bash
npm run artifacts
```

Expected: Shows how artifacts would be organized

**Step 3: Verify package structure**

```bash
ls -la npm/
cat npm/darwin-x64/package.json
```

Expected: All platform directories exist with package.json

**Step 4: Test prepublish**

```bash
npm run prepublishOnly
```

Expected: Validates package structure for publishing

**Step 5: Document testing notes**

Create: `crates/datadog-serverless-node/BUILDING.md`

```markdown
# Building Cross-Platform

## Local Development

Build for your current platform:
```bash
npm run build
\```

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
\```

### Linux (requires Docker)
```bash
docker run --rm -v $(pwd):/build -w /build/crates/datadog-serverless-node \\
  ghcr.io/napi-rs/napi-rs/nodejs-rust:lts-debian \\
  npm run build
\```

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
\```
```

**Step 6: Commit**

```bash
git add crates/datadog-serverless-node/BUILDING.md
git commit -m "docs(node): add cross-platform building guide

Document:
- Local development builds
- CI/CD cross-platform setup
- Manual cross-compilation
- Publishing process
- Artifact organization

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Task 5: Update Main README with Installation

**Files:**
- Modify: `crates/datadog-serverless-node/README.md`

**Step 1: Update installation section**

Modify the README.md Installation section:

```markdown
## Installation

```bash
npm install @datadog/serverless-node
\```

The package automatically installs the correct pre-built binary for your platform:

| Platform | Architecture | Package |
|----------|-------------|---------|
| macOS | x64 | @datadog/serverless-node-darwin-x64 |
| macOS | ARM64 (Apple Silicon) | @datadog/serverless-node-darwin-arm64 |
| Linux | x64 | @datadog/serverless-node-linux-x64-gnu |
| Linux | ARM64 | @datadog/serverless-node-linux-arm64-gnu |
| Windows | x64 | @datadog/serverless-node-win32-x64-msvc |

### Supported Platforms

- **Node.js**: 16.x, 18.x, 20.x (LTS versions)
- **Operating Systems**:
  - macOS 10.13+ (Intel and Apple Silicon)
  - Linux (glibc 2.17+)
  - Windows 10+

### Building from Source

If pre-built binaries aren't available for your platform:

```bash
git clone https://github.com/DataDog/serverless-components
cd crates/datadog-serverless-node
npm install
npm run build
\```

See [BUILDING.md](./BUILDING.md) for details.
```

**Step 2: Commit**

```bash
git add crates/datadog-serverless-node/README.md
git commit -m "docs(node): update README with platform support info

Add:
- Platform compatibility table
- Supported Node.js versions
- Building from source instructions
- Link to BUILDING.md

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Task 6: Final Verification

**Step 1: Verify all files exist**

```bash
cd crates/datadog-serverless-node
ls -la .github/workflows/nodejs-bridge-ci.yml
ls -la npm/*/package.json
ls -la BUILDING.md
ls -la .npmignore
```

Expected: All files exist

**Step 2: Validate package.json**

```bash
npm pkg get name version napi
```

Expected: Shows correct configuration

**Step 3: Test build still works**

```bash
npm run build
npm test
```

Expected: 10 tests pass

**Step 4: Verify git status**

```bash
git status
git log --oneline | head -10
```

Expected: All changes committed

**Step 5: Summary commit if needed**

If any final adjustments:

```bash
git add .
git commit -m "chore(node): finalize Phase 4 cross-platform setup

Complete cross-platform build configuration with:
- GitHub Actions workflow
- Platform-specific packages
- Build documentation
- NPM publish readiness

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Phase 4 Complete!

At this point, Phase 4 is complete. You have:

✅ Configured napi-rs for 5 platforms
✅ Created GitHub Actions CI/CD workflow
✅ Set up platform-specific NPM packages
✅ Added build and publish automation
✅ Documented cross-platform building
✅ Updated README with platform info

The Node.js addon is now ready for:
- Automated builds on GitHub Actions
- NPM publishing with platform-specific binaries
- Installation on any supported platform
- Distribution to users worldwide

## Next Steps

To publish to NPM:
1. Set up NPM_TOKEN secret in GitHub
2. Update version in package.json
3. Create git tag: `nodejs-bridge-v0.1.0`
4. Push tag - CI will build and publish

After Phase 4, you can proceed to:
- **Phase 5**: Documentation & Examples (tutorials, guides)
- **Phase 6**: Production Readiness (performance, monitoring)
- **Or**: Start using in production!
