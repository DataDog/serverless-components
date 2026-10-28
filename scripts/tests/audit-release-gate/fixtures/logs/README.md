# Captured scanner logs

These logs are real terminal output captured from the pinned `cargo-audit 0.22.2`
official Linux x86_64 musl binary (`cargo-audit-audit 0.22.2`, archive SHA-256
`7fb9497f8594b389e5fce5ef9b92db08432996895b2e0c5a0167a69ed445c428`). The guard
tests feed them to `scripts/cargo-audit-report-guard.sh` together with the
recorded scanner exit code and the expected lockfile path, so guard behavior is
verified against real scanner text rather than hand-written approximations.

They are not "expected output" for a healthy network run; each one captures a
specific real scanner behavior. They were captured with a relative
`--file Cargo.lock`, so the tests pass `Cargo.lock` as the expected lockfile path
for these files. Production always passes an absolute path; the opt-in
`network_smoke.py` suite is the runtime proof for the absolute-path line.

## Provenance categories

- This directory holds **captured runtime logs**: real output from the pinned
  binary, byte-for-byte as emitted (placeholder scratch paths aside).
- `test_report_guard.py` also derives **synthetic contract fixtures** in memory
  (zero counts, missing counts, reordered markers, substituted lockfile paths).
  Those are contract tests for rejection paths, not captured scanner output.
- `network_smoke.py` is the **runtime proof** that connects the two: it runs the
  real gate end to end against the real binary and asserts the nonzero counts and
  absolute lockfile path the guard requires.

| Log | Scanner exit | Guard verdict | What it captures |
|---|---|---|---|
| `clean-success.log` | 0 | pass | Clean lockfile, full Fetching/Loaded/Updating/Scanning sequence |
| `warning-only-success.log` | 0 | pass | Informational warning is not fatal when the scan exits zero |
| `vulnerable-failure.log` | 1 | reject | Ordinary advisory rejects the scan |
| `malformed-lock-rc2.log` | 2 | reject | Malformed Cargo.lock |
| `missing-lock-rc2.log` | 2 | reject | Missing Cargo.lock |
| `index-error-rc0.log` | 0 | reject | crates.io index I/O failure hidden behind a zero exit status |
| `quiet-output-rc0.log` | 0 | reject | `[output] quiet = true` suppresses the progress sequence |
| `disabled-yanked-rc0.log` | 0 | reject | `[yanked] enabled = false` removes the Updating marker |
| `disabled-fetch-rc0.log` | 0 | reject | `[database] fetch = false` removes the Fetching marker |

Capture method: a small fixture lockfile in a temporary directory was scanned with
`cargo-audit audit --file Cargo.lock --db <fresh> --url
https://github.com/RustSec/advisory-db.git --format terminal --color never`,
redirecting combined stdout and stderr. The index-error and configuration logs
used the documented failure injections (a directory in place of
`CARGO_HOME/.package-cache`, and scanner configuration overrides). The temporary
paths embedded in the logs are scratch paths from the capture run and are not
part of the assertions.
