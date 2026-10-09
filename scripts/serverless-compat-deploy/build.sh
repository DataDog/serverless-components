#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Builds the serverless-compat binary for x86_64-unknown-linux-musl (Azure
# Linux Consumption + GCP Cloud Functions 1st gen both run on linux/amd64).
#
# Also packs the @datadog/serverless-compat npm package so the resulting
# package.tgz can be dropped into any Node.js test function directory.
#
# Output:
#   target/x86_64-unknown-linux-musl/release/datadog-serverless-compat
#   npm/package.tgz  (created by `npm pack` in the npm/ directory)
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Keep the load-test build independent from the user's global npm cache. Some
# development machines have an older, root-owned ~/.npm cache, which otherwise
# makes the binary build succeed and the package step fail with EPERM.
NPM_CONFIG_CACHE="${NPM_CONFIG_CACHE:-${REPO_ROOT}/target/npm-cache}"
export NPM_CONFIG_CACHE
mkdir -p "${NPM_CONFIG_CACHE}"

# Add rustup toolchain to PATH so cargo + rustc can find each other.
RUSTUP_BIN="${HOME}/.rustup/toolchains/stable-aarch64-apple-darwin/bin"
if [[ -d "${RUSTUP_BIN}" ]]; then
  export PATH="${RUSTUP_BIN}:${PATH}"
fi
CARGO="${CARGO:-$(which cargo 2>/dev/null || true)}"
if [[ -z "${CARGO}" || ! -x "${CARGO}" ]]; then
  echo "ERROR: cargo not found. Set CARGO=/path/to/cargo or add rustup to PATH."
  exit 1
fi
TARGET="x86_64-unknown-linux-musl"

log() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*"; }

# ---------------------------------------------------------------------------
# Build Rust binary
# ---------------------------------------------------------------------------
log "Building datadog-serverless-compat (${TARGET})..."
"${CARGO}" build \
  --release \
  --package datadog-serverless-compat \
  --target "${TARGET}" \
  --manifest-path "${REPO_ROOT}/Cargo.toml"

BINARY="${REPO_ROOT}/target/${TARGET}/release/datadog-serverless-compat"
log "Binary: ${BINARY} ($(du -sh "${BINARY}" | cut -f1))"

# ---------------------------------------------------------------------------
# Copy binary into npm/datadog-serverless-compat-linux-x64 package
# ---------------------------------------------------------------------------
NPM_LINUX_X64="${REPO_ROOT}/npm/datadog-serverless-compat-linux-x64"
PACKAGE_TGZ=""
if [[ -d "${NPM_LINUX_X64}" ]]; then
  log "Copying binary into ${NPM_LINUX_X64}/bin/..."
  mkdir -p "${NPM_LINUX_X64}/bin"
  cp "${BINARY}" "${NPM_LINUX_X64}/bin/datadog-serverless-compat"
  chmod +x "${NPM_LINUX_X64}/bin/datadog-serverless-compat"

  log "Packing npm package..."
  (cd "${NPM_LINUX_X64}" && npm pack --pack-destination "${NPM_LINUX_X64}")
  PACKAGE_TGZ="$(ls -t "${NPM_LINUX_X64}"/*.tgz 2>/dev/null | head -1 || true)"
  [[ -n "${PACKAGE_TGZ}" ]] && log "Package: ${PACKAGE_TGZ}"
else
  log "No npm/datadog-serverless-compat-linux-x64 found — skipping npm pack (binary only)"
fi

log "Done."
log "  Binary : ${BINARY}"
[[ -n "${PACKAGE_TGZ:-}" ]] && log "  Package: ${PACKAGE_TGZ}"
