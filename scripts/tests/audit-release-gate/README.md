# Audit release-gate tests

Exercise the real release gate offline with disposable git repositories and
fixture-only scanner/download doubles. No test publishes, signs or uploads.

## Run

Activate a disposable virtual environment outside the checkout and install
`PyYAML==6.0.2`. Use that environment's Python for the offline suite:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -v -s scripts/tests/audit-release-gate -p 'test_*.py'
# Opt-in: requires cargo and network access, uses isolated fixture lockfiles.
PYTHONDONTWRITEBYTECODE=1 python3 scripts/tests/audit-release-gate/network_smoke.py
```

| File | Coverage |
|---|---|
| `test_release_gate.py` | Identity, installation, verdicts, sentinel blocking, fresh state/retry, cleanup, pins and command bounds |
| `test_report_guard.py` | Captured verdicts, synthetic incomplete scans, ordered markers, counts, exact paths and status/usage errors |
| `test_workflows.py` | Inline ordering, approvals, permissions, source/artifact binding and locked builds, with representative mutations |
| `network_smoke.py` | Real clean/retry, findings/committed ignore, informational warning, and scanner/database identity |

## Captured log provenance

`fixtures/logs/` contains terminal output from the official cargo-audit 0.22.2
Linux x86_64 musl archive, SHA-256
`7fb9497f8594b389e5fce5ef9b92db08432996895b2e0c5a0167a69ed445c428`.
Capture combined stdout/stderr from a temporary fixture using `cargo-audit audit
--file Cargo.lock --db <fresh> --url https://github.com/RustSec/advisory-db.git
--format terminal --color never`. Index failure was induced with a directory at
`CARGO_HOME/.package-cache`; configuration cases disabled fetching or yanked checks.
The relative captured path is expected as `Cargo.lock`; embedded scratch paths
are not assertions. Retained logs are unchanged from the original captures.

| Log | Scanner status | Guard verdict |
|---|---|---|
| `clean-success.log` | 0 | pass |
| `warning-only-success.log` | 0 | pass |
| `vulnerable-failure.log` | 1 | reject |
| `index-error-rc0.log` | 0 | reject |
| `quiet-output-rc0.log` | 0 | reject |
| `disabled-yanked-rc0.log` | 0 | reject |
| `disabled-fetch-rc0.log` | 0 | reject |

## Lint and limits

```sh
shellcheck scripts/cargo-audit-release-gate.sh scripts/cargo-audit-report-guard.sh \
  scripts/tests/audit-release-gate/fixtures/fake-scanner/cargo-audit \
  scripts/tests/audit-release-gate/fixtures/stubs/{cargo,curl,sha256sum}
```

Captured logs are real output; in-memory variants are synthetic contract fixtures.
Only the opt-in smoke executes the real scanner and proves current output shapes,
nonzero counts and absolute scan paths. The guard remains coupled to 0.22.2.
Timeout statuses are replayed, not actual timer/signal tests; bounds are checked
on command tokens with comments excluded. Workflow checks cover publication and
build wiring, not the tests workflow itself or GitHub-runner integration.
The repository lockfile need not pass: the smoke suite uses fixtures, not the
current unresolved advisories.
