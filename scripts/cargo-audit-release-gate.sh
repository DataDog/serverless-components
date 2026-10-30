#!/usr/bin/env bash
#
# Strict cargo-audit release gate for datadog-serverless-compat publication.
#
# Usage: scripts/cargo-audit-release-gate.sh <expected-git-sha>
#
# Run this gate inside a protected publication job, immediately before its first
# publication side effect. It performs downloads and a dependency scan only:
# it never publishes, signs, uploads, or pushes anything.
#
# The gate fails closed unless every one of these holds:
#   * the repository root is the expected commit (usually $GITHUB_SHA);
#   * the working-tree Cargo.lock and .cargo/audit.toml byte-match that commit,
#     so the audited lockfile and advisory policy cannot be swapped out;
#   * a working Rust toolchain is present, because the pinned prebuilt
#     cargo-audit still shells out to `cargo -V`;
#   * the pinned official cargo-audit 0.22.2 (Linux x86_64 musl) archive
#     downloads (bounded by curl's --connect-timeout 15 and --max-time 180) and
#     matches its reviewed SHA-256;
#   * the installed binary reports exactly the expected version;
#   * a fresh RustSec advisory database and a fresh Cargo cache are created in a
#     private temporary directory, so a warm cache can never turn a failed
#     advisory refresh into an offline success;
#   * the scanner runs under coreutils timeout (--kill-after=10s, 5m) and exits
#     zero, the terminal log shows the full Fetching/Loaded/Updating/Scanning
#     sequence with nonzero advisory and dependency counts for the expected
#     lockfile, and the log carries none of the crates.io index diagnostics the
#     pinned binary can otherwise hide behind a zero exit status. See
#     scripts/cargo-audit-report-guard.sh.
#
# The archive URL, digest, scanner version, and advisory database URL are fixed
# reviewed constants. This script deliberately honors no environment variable
# that could point it at a different archive, digest, or advisory source, and it
# passes no --no-fetch, --stale, or extra ignore flags. All exceptions come from
# the repository's reviewed .cargo/audit.toml.
set -euo pipefail

readonly SCANNER_VERSION_LINE="cargo-audit-audit 0.22.2"
readonly ARCHIVE_URL="https://github.com/rustsec/rustsec/releases/download/cargo-audit/v0.22.2/cargo-audit-x86_64-unknown-linux-musl-v0.22.2.tgz"
readonly ARCHIVE_SHA256="7fb9497f8594b389e5fce5ef9b92db08432996895b2e0c5a0167a69ed445c428"
readonly ADVISORY_DB_URL="https://github.com/RustSec/advisory-db.git"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly script_dir
readonly GUARD_SCRIPT="${script_dir}/cargo-audit-report-guard.sh"

die() {
  printf 'cargo-audit release gate: ERROR: %s\n' "$*" >&2
  exit 2
}

fail() {
  printf 'cargo-audit release gate: FAILED: %s\n' "$*" >&2
  exit 1
}

if [ "$#" -ne 1 ]; then
  die "usage: $0 <expected-git-sha>"
fi

expected_sha="$1"
if ! [[ "$expected_sha" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]]; then
  die "expected commit must be a 40- or 64-character lowercase hex id"
fi

[ -x "$GUARD_SCRIPT" ] || fail "report guard is missing or not executable: $GUARD_SCRIPT"

for tool in git cargo curl tar sha256sum cut timeout; do
  command -v "$tool" >/dev/null 2>&1 || die "required tool not found on PATH: $tool"
done

cargo_version="$(cargo -V 2>&1)" || die "cargo -V failed; a working Rust toolchain must be installed before this gate"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not inside a git work tree"
cd "$repo_root"

head_sha="$(git rev-parse HEAD)" || die "cannot resolve HEAD"

lockfile="$repo_root/Cargo.lock"
policy="$repo_root/.cargo/audit.toml"
[ -f "$lockfile" ] || fail "missing Cargo.lock at repository root"
[ -f "$policy" ] || fail "missing .cargo/audit.toml at repository root"

if [ "$head_sha" != "$expected_sha" ]; then
  fail "HEAD ($head_sha) does not match expected release commit ($expected_sha)"
fi

sha256_of() {
  sha256sum "$@" | cut -d' ' -f1
}

# Bind an audited input to the expected commit, not to the working tree alone,
# and print its verified SHA-256. <path> is relative to the repository root.
verified_sha256() {
  local path="$1" committed actual
  committed="$(git show "${expected_sha}:${path}" | sha256_of)" ||
    fail "cannot read $path from expected commit $expected_sha"
  actual="$(sha256_of "$path")"
  [ "$committed" = "$actual" ] || fail "working-tree $path does not match the expected commit"
  printf '%s\n' "$actual"
}

# Command substitution runs in a subshell, so propagate a failure explicitly.
lock_hash="$(verified_sha256 Cargo.lock)" || exit "$?"
policy_hash="$(verified_sha256 .cargo/audit.toml)" || exit "$?"

work="$(mktemp -d "${TMPDIR:-/tmp}/cargo-audit-release-gate.XXXXXXXX")" || die "cannot create a temporary directory"
readonly work
# Single quotes defer expansion to EXIT so the cleanup always targets this run's
# directory without interpolating it into a fragile command string.
trap 'rm -rf -- "$work"' EXIT

archive="$work/cargo-audit.tgz"
install_dir="$work/install"
mkdir -p "$install_dir"

if ! curl --fail --silent --show-error --location --proto '=https' \
  --connect-timeout 15 --max-time 180 --output "$archive" "$ARCHIVE_URL"; then
  fail "failed to download the pinned cargo-audit archive"
fi

if [ "$(sha256_of "$archive")" != "$ARCHIVE_SHA256" ]; then
  fail "pinned cargo-audit archive checksum mismatch"
fi

if ! tar -xzf "$archive" -C "$install_dir" --strip-components=1; then
  fail "failed to extract the pinned cargo-audit archive"
fi

scanner="$install_dir/cargo-audit"
[ -x "$scanner" ] || fail "extracted archive does not contain an executable cargo-audit"

scanner_version="$("$scanner" audit --version 2>&1)" || fail "cannot execute the installed cargo-audit"
if [ "$scanner_version" != "$SCANNER_VERSION_LINE" ]; then
  fail "unexpected cargo-audit version: '$scanner_version' (expected '$SCANNER_VERSION_LINE')"
fi

# Fresh advisory database and fresh Cargo cache on every execution. Nothing from
# a previous run, job, or retry is reused, so a stale cache cannot authorize a
# release when the required refresh fails.
cargo_home="$work/cargo-home"
mkdir -p "$cargo_home"
db="$work/rustsec-db"
scan_log="$work/cargo-audit.log"

# The scan is bounded by coreutils timeout: 5 minutes of runtime, then a 10
# second grace before SIGKILL. A killed scan exits 124 (SIGTERM) or 137
# (SIGKILL); both are nonzero and are rejected by the report guard below.
scan_rc=0
CARGO_HOME="$cargo_home" timeout --kill-after=10s 5m "$scanner" audit \
  --file "$lockfile" \
  --db "$db" \
  --url "$ADVISORY_DB_URL" \
  --format terminal \
  --color never >"$scan_log" 2>&1 || scan_rc=$?

db_origin="unavailable"
db_revision="unavailable"
if [ -d "$db/.git" ]; then
  db_origin="$(git -C "$db" remote get-url origin 2>/dev/null || printf 'unavailable')"
  db_revision="$(git -C "$db" rev-parse HEAD 2>/dev/null || printf 'unavailable')"
fi

printf 'cargo-audit release gate\n'
printf 'expected-sha: %s\n' "$expected_sha"
printf 'head-sha: %s\n' "$head_sha"
printf 'cargo-lock-sha256: %s\n' "$lock_hash"
printf 'audit-toml-sha256: %s\n' "$policy_hash"
printf 'cargo-version: %s\n' "$cargo_version"
printf 'scanner-version: %s\n' "$scanner_version"
printf 'advisory-db-url: %s\n' "$ADVISORY_DB_URL"
printf 'advisory-db-origin: %s\n' "$db_origin"
printf 'advisory-db-revision: %s\n' "$db_revision"
printf 'scanner-exit: %s\n' "$scan_rc"

printf '%s\n' '----- begin cargo-audit output -----'
if ! cat "$scan_log"; then
  fail "cannot read the cargo-audit scan log: $scan_log"
fi
printf '%s\n' '----- end cargo-audit output -----'

guard_rc=0
"$GUARD_SCRIPT" "$scan_log" "$scan_rc" "$lockfile" || guard_rc=$?
case "$guard_rc" in
  0) ;;
  1)
    printf 'cargo-audit release gate: result: FAIL (scanner-exit=%s)\n' "$scan_rc" >&2
    # Preserve the scanner's own status for rejected findings and errors.
    if [ "$scan_rc" -ne 0 ]; then
      exit "$scan_rc"
    fi
    exit 1
    ;;
  *) fail "report guard returned an unexpected status ($guard_rc); treating as failure" ;;
esac

if [ "$db_origin" != "$ADVISORY_DB_URL" ]; then
  fail "unexpected advisory database origin: '$db_origin'"
fi

printf 'cargo-audit release gate: result: PASS\n'
exit 0
