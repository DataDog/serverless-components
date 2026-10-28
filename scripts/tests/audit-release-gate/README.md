# Audit release gate tests

Tests for the strict cargo-audit release gate in
[`scripts/cargo-audit-release-gate.sh`](../../cargo-audit-release-gate.sh) and its
pure verdict guard
[`scripts/cargo-audit-report-guard.sh`](../../cargo-audit-report-guard.sh), plus
structural checks that the gate is wired into the protected publication jobs
correctly.

## Offline suite (default)

Deterministic, no network, no real scanner, no publishers. It runs the real
production helper against temporary git repositories, a fixture scanner archive,
and local PATH test doubles, and it checks the real workflow YAML structure.

```sh
python3 -m pip install 'PyYAML==6.0.2'
python3 -m unittest discover -v -s scripts/tests/audit-release-gate -p 'test_*.py'
```

PyYAML is a test-only dependency used by `test_workflows.py`.

| File | Coverage |
|---|---|
| `test_report_guard.py` | Guard verdicts for captured real-scanner logs plus synthetic contract fixtures: nonzero advisory/dependency counts, exact expected-lockfile matching, marker order, hidden crates.io diagnostics, exit-code validation, shell sentinel behavior |
| `test_release_gate.py` | Helper control flow: download/checksum/version/prerequisite failures, hidden index errors, missing markers, fetch failure, source and lockfile identity, bounded network and scanner invocations, an inherited warm cache, missing `timeout`, scanner timeout statuses, substituted lockfiles, fresh per-execution directories, retry, guard robustness |
| `test_workflows.py` | Gate placement, exact command, unconditional step, bash shell, and pinning in `publish.yml`, `release-datadog-serverless-compat.yml`, `build-datadog-serverless-compat.yml`, and `audit-release-gate-tests.yml`, plus negative mutations for command masking, conditions, job-level error ignoring, cross-run artifact downloads, and checkout refs |

The test doubles shape only the helper's inputs (via `PATH` and fixture files).
The production helper reads no bypass environment variable, and the tests add
none.

### Production time bounds

The gate bounds every potentially hanging step. These values are production
constants with no environment override, so a bypass cannot shorten them:

- `curl --connect-timeout 15 --max-time 180` for the pinned archive download.
- `timeout --kill-after=10s 5m <scanner> audit ...` for the dependency scan.
  `timeout` is a required tool; a killed scan exits 124 (SIGTERM) or 137
  (SIGKILL), both of which the guard rejects before the publication sentinel.

## Opt-in network smoke

Downloads the real pinned cargo-audit archive, verifies its reviewed SHA-256,
clones a fresh RustSec advisory database, updates the crates.io index, and scans
small fixture lockfiles (clean, vulnerable, informational warning, and an
existing-ignore policy). It requires network access and a working Rust
toolchain, and it never publishes, signs, or uploads anything.

```sh
python3 scripts/tests/audit-release-gate/network_smoke.py
```

This suite is the runtime proof for the guard contract: it asserts that the real
scanner prints a nonzero `Loaded ... security advisories` line and an absolute
`Scanning /.../Cargo.lock for vulnerabilities ([1-9][0-9]* crate dependencies)`
line on the current machine. The captured logs under `fixtures/logs` are real
scanner output but were captured with a relative `--file Cargo.lock`; the
synthetic variants in `test_report_guard.py` are contract fixtures, not runtime
proof. See `fixtures/logs/README.md`.

The smoke suite never expects the repository's own `Cargo.lock` to pass; it uses
isolated fixtures only. Run it from a normal checkout with `cargo` on `PATH`.

## Shell lint

```sh
shellcheck scripts/cargo-audit-release-gate.sh scripts/cargo-audit-report-guard.sh \
  scripts/tests/audit-release-gate/fixtures/fake-scanner/cargo-audit \
  scripts/tests/audit-release-gate/fixtures/stubs/*
```

## Limits

- The guard is deliberately coupled to the pinned scanner's human-readable
  terminal format. Re-review `cargo-audit-report-guard.sh` and its tests whenever
  the pinned cargo-audit version changes.
- The offline workflow checks are structural. They prove the gate step exists in
  the right job at the right position with the exact reviewed command; they do
  not execute GitHub Actions.
- The opt-in smoke suite validates the scanner contract on the current machine.
  It does not validate GitHub runner images, npm/OIDC publishing, or
  action-gh-release.
