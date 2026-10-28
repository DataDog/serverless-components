"""Offline tests for scripts/cargo-audit-report-guard.sh.

The guard is pure: it takes a captured cargo-audit terminal log, the numeric
scanner exit code, and the lockfile path the gate expected the scan to cover, and
returns a verdict. None of these tests touch the network, install anything, or
run a publisher.

The logs under fixtures/logs were captured from the real pinned cargo-audit
0.22.2 binary; see fixtures/logs/README.md for provenance. They were captured
with a relative `--file Cargo.lock`, so the tests pass `Cargo.lock` as the
expected path for them. Synthetic variants are derived in memory to cover
individual rejection reasons (zero counts, out-of-order markers, substituted
paths); those are contract fixtures, not captured runtime output.
"""
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
GUARD = REPO / "scripts" / "cargo-audit-report-guard.sh"
LOGS = HERE / "fixtures" / "logs"

# (captured log name, captured scanner exit code, expected guard exit code)
CAPTURED = [
    ("clean-success", 0, 0),
    ("warning-only-success", 0, 0),
    ("vulnerable-failure", 1, 1),
    ("malformed-lock-rc2", 2, 1),
    ("missing-lock-rc2", 2, 1),
    ("index-error-rc0", 0, 1),
    ("quiet-output-rc0", 0, 1),
    ("disabled-yanked-rc0", 0, 1),
    ("disabled-fetch-rc0", 0, 1),
]

# The capture run scanned a relative `Cargo.lock`.
CAPTURED_LOCKFILE = "Cargo.lock"
MARKERS = ("Fetching", "Loaded", "Updating", "Scanning")


def run_guard(log, rc, expected_lockfile=CAPTURED_LOCKFILE):
    return subprocess.run(
        ["bash", str(GUARD), str(log), str(rc), str(expected_lockfile)],
        capture_output=True,
        text=True,
        check=False,
    )


def clean_text():
    return (LOGS / "clean-success.log").read_text()


def write_tmp_log(tmp, text):
    path = Path(tmp) / "log.txt"
    path.write_text(text)
    return path


class ReportGuardTests(unittest.TestCase):
    def assert_rejected(self, text, expected_lockfile=CAPTURED_LOCKFILE):
        with tempfile.TemporaryDirectory() as tmp:
            result = run_guard(write_tmp_log(tmp, text), 0, expected_lockfile)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_captured_logs_guard_verdicts(self):
        for name, rc, expected in CAPTURED:
            with self.subTest(log=name, rc=rc):
                log = LOGS / f"{name}.log"
                self.assertTrue(log.is_file(), f"missing captured log: {log}")
                result = run_guard(log, rc)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)

    def test_informational_warning_is_not_a_rejection(self):
        # A zero-exit scan whose only output is informational warnings must pass,
        # preserving the existing scanner warning policy.
        log = LOGS / "warning-only-success.log"
        self.assertIn("allowed warning found", log.read_text())
        self.assertEqual(run_guard(log, 0).returncode, 0)

    def test_clean_zero_exit_requires_every_marker(self):
        text = clean_text()
        for marker in MARKERS:
            self.assertIn(marker, text)
        for marker in MARKERS:
            with self.subTest(removed=marker):
                mutated = "\n".join(
                    line for line in text.splitlines() if marker not in line
                )
                self.assert_rejected(mutated + "\n")

    def test_zero_exit_with_hidden_index_diagnostics_is_rejected(self):
        for diagnostic in (
            "warning: couldn't update crates.io index: fixture failure\n",
            "warning: couldn't open crates.io index: fixture failure\n",
            "error: couldn't check if the package is yanked: fixture miss\n",
        ):
            with self.subTest(diagnostic=diagnostic):
                self.assert_rejected(clean_text() + diagnostic)

    def test_empty_log_with_zero_exit_is_rejected(self):
        self.assert_rejected("")

    def test_bare_marker_words_are_not_enough(self):
        # The guard must require the substantive marker lines, not the words
        # "Fetching", "Loaded", "Updating", and "Scanning" in unrelated text.
        text = (
            "Fetching something else\n"
            "Loaded a note\n"
            "Updating something else\n"
            "Scanning something else\n"
        )
        self.assert_rejected(text)

    def test_zero_advisory_count_is_rejected(self):
        text = clean_text().replace("Loaded 1380 security advisories", "Loaded 0 security advisories")
        self.assert_rejected(text)

    def test_missing_advisory_count_is_rejected(self):
        text = clean_text().replace("Loaded 1380 security advisories", "Loaded some security advisories")
        self.assert_rejected(text)

    def test_zero_dependency_count_is_rejected(self):
        text = clean_text().replace("(1 crate dependencies)", "(0 crate dependencies)")
        self.assert_rejected(text)

    def test_missing_dependency_count_is_rejected(self):
        text = clean_text().replace("(1 crate dependencies)", "(several crate dependencies)")
        self.assert_rejected(text)

    def test_out_of_order_markers_are_rejected(self):
        text = clean_text()
        lines = text.splitlines()
        fetch = next(line for line in lines if "Fetching advisory database" in line)
        loaded = next(line for line in lines if "Loaded" in line)
        reordered = [line for line in lines if line is not fetch]
        reordered.insert(reordered.index(loaded) + 1, fetch)
        self.assert_rejected("\n".join(reordered) + "\n")

    def test_wrong_expected_lockfile_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = write_tmp_log(tmp, clean_text())
            result = run_guard(log, 0, "/expected/elsewhere/Cargo.lock")
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("but the gate expected", result.stderr)

    def test_lockfile_path_suffix_is_not_accepted(self):
        # A scan of /a/b/Cargo.lock must not satisfy an expectation of
        # /b/Cargo.lock: the comparison is exact, not a suffix match.
        text = clean_text().replace(
            "Scanning Cargo.lock for vulnerabilities",
            "Scanning /a/b/Cargo.lock for vulnerabilities",
        )
        self.assert_rejected(text, "/b/Cargo.lock")

    def test_absolute_expected_lockfile_is_accepted(self):
        text = clean_text().replace(
            "Scanning Cargo.lock for vulnerabilities",
            "Scanning /tmp/absolute/path/Cargo.lock for vulnerabilities",
        )
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(
                run_guard(write_tmp_log(tmp, text), 0, "/tmp/absolute/path/Cargo.lock").returncode,
                0,
            )

    def test_nonzero_scanner_exit_is_rejected_even_with_clean_log(self):
        # The scanner exit code is authoritative and independent of the log:
        # a complete, clean-looking log must not rescue a nonzero scan, including
        # the 124/137 statuses produced by the bounded scanner timeout.
        log = LOGS / "clean-success.log"
        for rc in ("1", "2", "124", "137"):
            with self.subTest(rc=rc):
                result = run_guard(log, rc)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("release blocked", result.stderr)

    def test_non_numeric_scanner_exit_code_is_rejected(self):
        log = LOGS / "clean-success.log"
        for bogus in ("abc", "", "-1", "0x0", "1.0"):
            with self.subTest(rc=bogus):
                self.assertEqual(run_guard(log, bogus).returncode, 2)

    def test_missing_log_is_a_usage_error(self):
        self.assertEqual(run_guard(LOGS / "does-not-exist.log", 0).returncode, 2)

    def test_empty_expected_lockfile_is_a_usage_error(self):
        self.assertEqual(run_guard(LOGS / "clean-success.log", 0, "").returncode, 2)

    def test_wrong_argument_count_is_a_usage_error(self):
        for args in (
            [],
            [str(LOGS / "clean-success.log")],
            [str(LOGS / "clean-success.log"), "0"],
            ["a", "b", "c", "d"],
        ):
            with self.subTest(args=args):
                result = subprocess.run(
                    ["bash", str(GUARD), *args],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(result.returncode, 2)

    def test_rejection_prevents_following_command(self):
        # A shell sequence must not reach its next command after a rejected scan
        # and must reach it after an accepted scan.
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            sentinel = tmp_path / "sentinel"
            failing = LOGS / "index-error-rc0.log"
            passing = LOGS / "clean-success.log"
            script = 'set -e; bash "$1" "$2" "$3" "$4"; touch "$5"'

            rejected = subprocess.run(
                [
                    "bash", "-c", script, "guard",
                    str(GUARD), str(failing), "0", CAPTURED_LOCKFILE, str(sentinel),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(rejected.returncode, 0)
            self.assertFalse(sentinel.exists())

            accepted = subprocess.run(
                [
                    "bash", "-c", script, "guard",
                    str(GUARD), str(passing), "0", CAPTURED_LOCKFILE, str(sentinel),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(accepted.returncode, 0, accepted.stdout + accepted.stderr)
            self.assertTrue(sentinel.exists())

    def test_guard_is_executable_and_offline_only(self):
        self.assertTrue(os.access(GUARD, os.X_OK))
        text = GUARD.read_text()
        for forbidden in ("curl", "mktemp", "npm publish", "action-gh-release", "aws "):
            self.assertNotIn(forbidden, text)


if __name__ == "__main__":
    unittest.main()
