#!/usr/bin/env bash
#
# Pure verdict guard for a cargo-audit release scan.
#
# Usage: scripts/cargo-audit-report-guard.sh <scan-log> <scanner-exit-code> <expected-lockfile-path>
#
# The guard exits 0 only when all of the following hold:
#   * the scanner exit code is a non-negative integer and equals zero;
#   * the scan log contains the complete Fetching/Loaded/Updating/Scanning
#     progress sequence emitted by the pinned cargo-audit 0.22.2 binary, in that
#     order, with a nonzero loaded-advisory count and a nonzero crate-dependency
#     count for the expected lockfile;
#   * the lockfile named on the Scanning line is exactly the expected lockfile
#     path the gate passed to the scanner, so a scan of a different or
#     substituted file cannot authorize a release;
#   * the scan log contains none of the pinned binary's crates.io index
#     diagnostics ("couldn't update crates.io index", "couldn't open
#     crates.io index", "couldn't check if the package is yanked").
#
# cargo-audit 0.22.2 can exit zero while hiding an internal crates.io index
# failure behind JSON output. Exit status and JSON alone are therefore not
# enough to authorize a release, which is why the human-readable terminal log
# is parsed here.
#
# The marker shapes this guard requires are pinned to the scanner version's
# terminal output. Structured lines are matched rather than bare marker words, so
# a log that merely mentions "Loaded" or "Scanning" without the substantive
# payload cannot pass. Re-review this guard and its regression tests whenever the
# pinned version changes.
#
# This script performs no network, download, installation, or publication work,
# so it can be exercised entirely offline.
set -euo pipefail

usage_error() {
  printf 'cargo-audit report guard: %s\n' "$*" >&2
  exit 2
}

reject() {
  printf 'cargo-audit report guard: %s\n' "$*" >&2
  exit 1
}

if [ "$#" -ne 3 ]; then
  usage_error "usage: $0 <scan-log> <scanner-exit-code> <expected-lockfile-path>"
fi

log="$1"
rc="$2"
expected_lockfile="$3"

[ -n "$expected_lockfile" ] || usage_error "expected lockfile path must not be empty"
[ -f "$log" ] || usage_error "scan log not found: $log"

case "$rc" in
  '' | *[!0-9]*) usage_error "scanner exit code is not a non-negative integer: $rc" ;;
esac

[ "$rc" -eq 0 ] || reject "scanner exited $rc; release blocked"

if grep -Eq "couldn't (update|open) crates\.io index|couldn't check if the package is yanked" "$log"; then
  reject "scan log shows an incomplete crates.io index check; release blocked"
fi

fetch_line=0
loaded_line=0
updating_line=0
scanning_line=0
scanned_lockfile=""
line_number=0
while IFS= read -r line || [ -n "$line" ]; do
  line_number=$((line_number + 1))

  if [ "$fetch_line" -eq 0 ] && [[ "$line" =~ ^[[:space:]]*Fetching[[:space:]]+advisory[[:space:]]+database[[:space:]]+from[[:space:]] ]]; then
    fetch_line=$line_number
    continue
  fi

  if [ "$loaded_line" -eq 0 ] && [[ "$line" =~ ^[[:space:]]*Loaded[[:space:]]+([1-9][0-9]*)[[:space:]]+security[[:space:]]+advisories ]]; then
    loaded_line=$line_number
    continue
  fi

  if [ "$updating_line" -eq 0 ] && [[ "$line" =~ ^[[:space:]]*Updating[[:space:]]+crates\.io[[:space:]]+index[[:space:]]*$ ]]; then
    updating_line=$line_number
    continue
  fi

  if [ "$scanning_line" -eq 0 ] && [[ "$line" =~ ^[[:space:]]*Scanning[[:space:]]+(.+)[[:space:]]+for[[:space:]]+vulnerabilities[[:space:]]+\(([1-9][0-9]*)[[:space:]]+crate[[:space:]]+dependencies\)[[:space:]]*$ ]]; then
    scanning_line=$line_number
    scanned_lockfile="${BASH_REMATCH[1]}"
    continue
  fi
done <"$log"

[ "$fetch_line" -ne 0 ] || reject "scan log is missing the advisory database fetch progress line"
[ "$loaded_line" -ne 0 ] || reject "scan log does not report a nonzero count of loaded security advisories"
[ "$updating_line" -ne 0 ] || reject "scan log is missing the crates.io index update progress line"
[ "$scanning_line" -ne 0 ] || reject "scan log does not report scanning a lockfile with a nonzero crate dependency count"

if [ "$fetch_line" -ge "$loaded_line" ] ||
  [ "$loaded_line" -ge "$updating_line" ] ||
  [ "$updating_line" -ge "$scanning_line" ]; then
  reject "scan log progress lines are out of order"
fi

if [ "$scanned_lockfile" != "$expected_lockfile" ]; then
  reject "scan log scanned '$scanned_lockfile' but the gate expected '$expected_lockfile'"
fi

exit 0
