"""Offline tests for scripts/cargo-audit-report-guard.sh.

The guard is pure: it takes a captured cargo-audit terminal log, the numeric
scanner exit code, and the lockfile path the gate expected the scan to cover, and
returns a verdict. None of these tests touch the network or install anything.

Provenance: the logs under fixtures/logs are captured runtime output from the
real pinned cargo-audit 0.22.2 binary (see README.md), recorded
with a relative `--file Cargo.lock`, so `Cargo.lock` is their expected path. The
SYNTHETIC variants are contract fixtures derived in memory from the clean log to
isolate single rejection reasons; they are not scanner output. The runtime proof
for the absolute-path Scanning line is the opt-in network_smoke.py suite.
"""
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
GUARD = HERE.parents[1] / "cargo-audit-report-guard.sh"
LOGS = HERE / "fixtures" / "logs"
CLEAN_LOG = LOGS / "clean-success.log"
CLEAN = CLEAN_LOG.read_text()
CAPTURED_LOCKFILE = "Cargo.lock"

# (captured log, scanner exit code it was captured with, expected guard exit code)
CAPTURED = [
    ("clean-success", 0, 0),
    ("warning-only-success", 0, 0),
    ("vulnerable-failure", 1, 1),
    ("index-error-rc0", 0, 1),
    ("quiet-output-rc0", 0, 1),
    ("disabled-yanked-rc0", 0, 1),
    ("disabled-fetch-rc0", 0, 1),
]


def without(marker):
    kept = [line for line in CLEAN.splitlines() if marker not in line]
    if len(kept) == len(CLEAN.splitlines()):
        raise LookupError(f"clean log has no '{marker}' line")
    return "\n".join(kept) + "\n"


def replaced(old, new):
    if old not in CLEAN:
        raise LookupError(f"clean log does not contain '{old}'")
    return CLEAN.replace(old, new)


def reordered():
    fetch, loaded, *rest = CLEAN.splitlines()
    if "Fetching" not in fetch or "Loaded" not in loaded:
        raise LookupError("clean log no longer starts with Fetching then Loaded")
    return "\n".join([loaded, fetch, *rest]) + "\n"


ABSOLUTE_SCAN = "Scanning /tmp/absolute/path/Cargo.lock for vulnerabilities"

# (name, log text, expected lockfile, expected guard exit code); scanner exit is 0.
SYNTHETIC = [
    *[(f"missing-{m}", without(m), CAPTURED_LOCKFILE, 1) for m in ("Fetching", "Loaded", "Updating", "Scanning")],
    ("update-index-diagnostic", CLEAN + "warning: couldn't update crates.io index: fixture\n", CAPTURED_LOCKFILE, 1),
    ("open-index-diagnostic", CLEAN + "warning: couldn't open crates.io index: fixture\n", CAPTURED_LOCKFILE, 1),
    ("yanked-diagnostic", CLEAN + "error: couldn't check if the package is yanked: fixture\n", CAPTURED_LOCKFILE, 1),
    ("zero-advisories", replaced("Loaded 1380 security", "Loaded 0 security"), CAPTURED_LOCKFILE, 1),
    ("missing-advisory-count", replaced("Loaded 1380 security", "Loaded some security"), CAPTURED_LOCKFILE, 1),
    ("zero-dependencies", replaced("(1 crate dependencies)", "(0 crate dependencies)"), CAPTURED_LOCKFILE, 1),
    ("missing-dependency-count", replaced("(1 crate", "(several crate"), CAPTURED_LOCKFILE, 1),
    ("empty-log", "", CAPTURED_LOCKFILE, 1),
    ("bare-marker-words", "Fetching x\nLoaded a note\nUpdating x\nScanning x\n", CAPTURED_LOCKFILE, 1),
    ("fetch-after-loaded", reordered(), CAPTURED_LOCKFILE, 1),
    ("exact-wrong-path", CLEAN, "/expected/elsewhere/Cargo.lock", 1),
    ("suffix-wrong-path", replaced("Scanning Cargo.lock", "Scanning /a/b/Cargo.lock"), "/b/Cargo.lock", 1),
    ("absolute-valid-path", replaced("Scanning Cargo.lock for vulnerabilities", ABSOLUTE_SCAN),
     "/tmp/absolute/path/Cargo.lock", 0),
]

# (name, argv after the guard path); every case is a usage error (exit 2).
USAGE_ERRORS = [
    *[(f"status-{rc or 'empty'}", [str(CLEAN_LOG), rc, CAPTURED_LOCKFILE]) for rc in ("abc", "", "-1", "0x0", "1.0")],
    ("missing-log", [str(LOGS / "does-not-exist.log"), "0", CAPTURED_LOCKFILE]),
    ("empty-expected-path", [str(CLEAN_LOG), "0", ""]),
    ("zero-args", []),
    ("one-arg", [str(CLEAN_LOG)]),
    ("two-args", [str(CLEAN_LOG), "0"]),
    ("four-args", ["a", "b", "c", "d"]),
]


def guard(*args):
    return subprocess.run(["bash", str(GUARD), *map(str, args)], capture_output=True, text=True, check=False)


class ReportGuardTests(unittest.TestCase):
    def assert_exit(self, result, expected):
        self.assertEqual(result.returncode, expected, result.stdout + result.stderr)

    def test_captured_logs(self):
        for name, scanner_rc, expected in CAPTURED:
            with self.subTest(name):
                log = LOGS / f"{name}.log"
                if name == "warning-only-success":
                    self.assertIn("allowed warning found", log.read_text())
                self.assert_exit(guard(log, scanner_rc, CAPTURED_LOCKFILE), expected)

    def test_synthetic_variants(self):
        with tempfile.TemporaryDirectory() as tmp:
            for name, text, expected_lockfile, expected in SYNTHETIC:
                with self.subTest(name):
                    log = Path(tmp) / f"{name}.log"
                    log.write_text(text)
                    self.assert_exit(guard(log, 0, expected_lockfile), expected)

    def test_nonzero_scanner_status_rejects_clean_log(self):
        # The exit code is authoritative: a clean log cannot rescue a failed scan,
        # including the 124/137 statuses of the bounded scanner timeout.
        for rc in ("1", "2", "124", "137"):
            with self.subTest(rc):
                result = guard(CLEAN_LOG, rc, CAPTURED_LOCKFILE)
                self.assert_exit(result, 1)
                self.assertIn("release blocked", result.stderr)

    def test_usage_errors(self):
        for name, args in USAGE_ERRORS:
            with self.subTest(name):
                self.assert_exit(guard(*args), 2)


if __name__ == "__main__":
    unittest.main()
