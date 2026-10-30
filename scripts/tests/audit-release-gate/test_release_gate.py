"""Offline control-flow tests for scripts/cargo-audit-release-gate.sh.

Each test runs the real production gate inside a temporary git repository with a
tiny committed Cargo.lock and .cargo/audit.toml. The "downloaded" archive is a tar
of the replay scanner in fixtures/fake-scanner, and PATH doubles stand in only for
curl, sha256sum, and cargo; git, tar, cut, and coreutils timeout are real. The
doubles read fixture-only FAKE_* variables; the gate itself honors none of them.

Scanner exit statuses 124 and 137 are returned by the replay scanner through the
real timeout binary. That proves the gate's exit-status handling for a killed scan,
not actual timer expiry or signal delivery; the bounds themselves are covered by
the static command test below.

network_smoke.py is the opt-in runtime proof against the real scanner.
"""
import hashlib
import os
import re
import shlex
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
GATE = HERE.parents[1] / "cargo-audit-release-gate.sh"
FAKE_SCANNER = HERE / "fixtures" / "fake-scanner" / "cargo-audit"
STUBS = HERE / "fixtures" / "stubs"
LOGS = HERE / "fixtures" / "logs"

# Reviewed pins, written out independently of the production script.
ARCHIVE_URL = (
    "https://github.com/rustsec/rustsec/releases/download/"
    "cargo-audit/v0.22.2/cargo-audit-x86_64-unknown-linux-musl-v0.22.2.tgz"
)
ARCHIVE_SHA256 = "7fb9497f8594b389e5fce5ef9b92db08432996895b2e0c5a0167a69ed445c428"
SCANNER_VERSION = "cargo-audit-audit 0.22.2"
TRUSTED_DB = "https://github.com/RustSec/advisory-db.git"

LOCK = 'version = 3\n\n[[package]]\nname = "gate-fixture"\nversion = "0.1.0"\n'
POLICY = "[advisories]\nignore = []\n"
GIT_ENV = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull}
REAL_SHA256SUM = shutil.which("sha256sum")
if REAL_SHA256SUM is None:
    raise RuntimeError("sha256sum is required to run the offline release-gate tests")


def read_records(path):
    """Parse '---'-separated blocks of key=value lines; 'arg' keys accumulate."""
    runs = []
    if path.exists():
        for line in path.read_text().splitlines():
            if line == "---":
                runs.append({"arg": []})
            else:
                key, _, value = line.partition("=")
                if key == "arg":
                    runs[-1]["arg"].append(value)
                else:
                    runs[-1][key] = value
    return runs


def logical_commands(script_text):
    """Join backslash continuations and shell-split each line, dropping comments."""
    joined = script_text.replace("\\\n", " ")
    return [shlex.split(line, comments=True) for line in joined.splitlines()]


class ReleaseGateTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="audit-gate-test.")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.repo = self.root / "repo"
        self.lock = self.repo / "Cargo.lock"
        self.policy = self.repo / ".cargo" / "audit.toml"
        self.tmpdir = self.root / "tmp"
        self.tmpdir.mkdir()
        self.sentinel = self.root / "sentinel"
        self.scan_record = self.root / "scans.txt"
        self.curl_record = self.root / "curl.txt"

        self.policy.parent.mkdir(parents=True)
        self.write_inputs()
        self.git("init", "--quiet")
        self.git("add", "-A")
        self.git("-c", "user.name=gate", "-c", "user.email=gate@example.invalid",
                 "commit", "--quiet", "-m", "fixture")
        self.head = self.git("rev-parse", "HEAD")

        self.archive = self.root / "fixture.tgz"
        with tarfile.open(self.archive, "w:gz") as tar:
            tar.add(FAKE_SCANNER, arcname="cargo-audit-x86_64-unknown-linux-musl-v0.22.2/cargo-audit")

    def write_inputs(self):
        self.lock.write_text(LOCK)
        self.policy.write_text(POLICY)

    def git(self, *args):
        return subprocess.run(["git", "-C", str(self.repo), *args], env={**os.environ, **GIT_ENV},
                              capture_output=True, text=True, check=True).stdout.strip()

    def run_gate(self, args=None, script=GATE, log="clean-success.log", **fake_env):
        """Run `<gate> <args> && touch sentinel` from fresh record and sentinel state."""
        for path in (self.sentinel, self.scan_record, self.curl_record):
            path.unlink(missing_ok=True)
        env = {
            **os.environ, **GIT_ENV,
            "PATH": f"{STUBS}{os.pathsep}{os.environ['PATH']}",
            "TMPDIR": str(self.tmpdir),
            "REAL_SHA256SUM": REAL_SHA256SUM,
            "FAKE_ARCHIVE": str(self.archive),
            "FAKE_ARCHIVE_SHA256": ARCHIVE_SHA256,
            "FAKE_CURL_RECORD": str(self.curl_record),
            "FAKE_RECORD": str(self.scan_record),
            "FAKE_LOG": str(LOGS / log),
            **fake_env,
        }
        args = [self.head] if args is None else args
        command = 'sentinel="$1" gate="$2"; shift 2; "$gate" "$@" && touch "$sentinel"'
        return subprocess.run(["bash", "-c", command, "run-gate", str(self.sentinel), str(script), *args],
                              cwd=self.repo, env=env, capture_output=True, text=True, timeout=120)

    def assert_blocked(self, proc, message, rc=1):
        output = proc.stdout + proc.stderr
        self.assertEqual(proc.returncode, rc, output)
        self.assertFalse(self.sentinel.exists(), "gate failure reached the sentinel")
        self.assertIn(message, output)

    def test_clean_scan_passes_with_pinned_identity_and_invocation(self):
        proc = self.run_gate()
        out = proc.stdout
        self.assertEqual(proc.returncode, 0, out + proc.stderr)
        self.assertTrue(self.sentinel.exists())
        self.assertIn("cargo-audit release gate: result: PASS", out)
        for line in (
            f"expected-sha: {self.head}",
            f"head-sha: {self.head}",
            f"cargo-lock-sha256: {hashlib.sha256(LOCK.encode()).hexdigest()}",
            f"audit-toml-sha256: {hashlib.sha256(POLICY.encode()).hexdigest()}",
            f"scanner-version: {SCANNER_VERSION}",
            f"advisory-db-origin: {TRUSTED_DB}",
            "----- begin cargo-audit output -----",
            f"Scanning {self.lock} for vulnerabilities (1 crate dependencies)",
            "----- end cargo-audit output -----",
        ):
            self.assertIn(line, out)
        self.assertRegex(out, r"(?m)^advisory-db-revision: [0-9a-f]{40}$")

        [scan] = read_records(self.scan_record)
        self.assertEqual(scan["arg"], [
            "audit", "--file", str(self.lock), "--db", scan["db"], "--url", TRUSTED_DB,
            "--format", "terminal", "--color", "never",
        ])
        [curl] = read_records(self.curl_record)
        pairs = list(zip(curl["arg"], curl["arg"][1:]))
        for pair in (("--proto", "=https"), ("--connect-timeout", "15"), ("--max-time", "180")):
            self.assertIn(pair, pairs)
        self.assertEqual(curl["arg"][-1], ARCHIVE_URL)

    def test_installation_failures_block_before_scan(self):
        corrupt = self.root / "corrupt.tgz"
        corrupt.write_text("not a gzip archive\n")
        cases = [
            ({"FAKE_CURL_FAIL": "1"}, 1, "failed to download the pinned cargo-audit archive"),
            ({"FAKE_ARCHIVE_SHA256": "0" * 64}, 1, "pinned cargo-audit archive checksum mismatch"),
            ({"FAKE_ARCHIVE": str(corrupt)}, 1, "failed to extract the pinned cargo-audit archive"),
            ({"FAKE_VERSION": "cargo-audit-audit 0.22.1"}, 1, "unexpected cargo-audit version"),
            ({"FAKE_VERSION": "cargo-audit-audit 0.22.2 extra"}, 1, "unexpected cargo-audit version"),
            ({"FAKE_CARGO_FAIL": "1"}, 2, "cargo -V failed"),
        ]
        for fake_env, rc, message in cases:
            with self.subTest(fake_env=fake_env):
                self.assert_blocked(self.run_gate(**fake_env), message, rc=rc)
                self.assertEqual(read_records(self.scan_record), [])

    def test_source_identity_failures_block_before_scan(self):
        cases = [
            ("a" * 40, {}, "does not match expected release commit"),
            (None, {self.lock: LOCK + "# tampered\n"}, "working-tree Cargo.lock does not match"),
            (None, {self.policy: '[advisories]\nignore = ["RUSTSEC-0000-0000"]\n'},
             "working-tree .cargo/audit.toml does not match"),
            (None, {self.lock: None}, "missing Cargo.lock at repository root"),
            (None, {self.policy: None}, "missing .cargo/audit.toml at repository root"),
        ]
        for sha, edits, message in cases:
            with self.subTest(message=message):
                for path, text in edits.items():
                    if text is None:
                        path.unlink()
                    else:
                        path.write_text(text)
                try:
                    self.assert_blocked(self.run_gate(args=[sha] if sha else None), message)
                    self.assertEqual(read_records(self.scan_record), [])
                finally:
                    self.write_inputs()

    def test_usage_errors_exit_2_before_scan(self):
        for args in ([], [self.head, self.head], ["not-a-commit"], [self.head.upper()]):
            with self.subTest(args=args):
                proc = self.run_gate(args=args)
                self.assert_blocked(proc, "cargo-audit release gate: ERROR:", rc=2)
                self.assertFalse(self.curl_record.exists() or self.scan_record.exists())

    def test_scanner_verdicts(self):
        fetch_failure = {"log": os.devnull, "FAKE_RC": "1", "FAKE_NO_DB": "1"}
        cases = [
            ({"log": "warning-only-success.log"}, 0, "result: PASS"),
            ({"log": "vulnerable-failure.log", "FAKE_RC": "1"}, 1, "scanner exited 1"),
            ({"log": "index-error-rc0.log"}, 1, "incomplete crates.io index check"),
            ({"log": "quiet-output-rc0.log"}, 1, "missing the advisory database fetch progress line"),
            ({"log": "disabled-yanked-rc0.log"}, 1, "missing the crates.io index update progress line"),
            ({"log": "disabled-fetch-rc0.log"}, 1, "missing the advisory database fetch progress line"),
            (fetch_failure, 1, "advisory-db-origin: unavailable"),
            ({"FAKE_ORIGIN": "https://example.invalid/advisory-db.git"}, 1, "unexpected advisory database origin"),
            ({"FAKE_SCAN_PATH": "Cargo.lock"}, 1, f"scanned 'Cargo.lock' but the gate expected '{self.lock}'"),
            ({"FAKE_RC": "124"}, 124, "scanner exited 124; release blocked"),
            ({"FAKE_RC": "137"}, 137, "scanner exited 137; release blocked"),
        ]
        for fake_env, rc, message in cases:
            with self.subTest(fake_env=fake_env):
                proc = self.run_gate(**fake_env)
                if rc == 0:
                    self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                    self.assertTrue(self.sentinel.exists())
                    self.assertIn(message, proc.stdout)
                else:
                    self.assert_blocked(proc, message, rc=rc)
                self.assertEqual(len(read_records(self.scan_record)), 1)

    def test_retry_after_refresh_failure_uses_fresh_private_state(self):
        warm_db = self.tmpdir / "rustsec-db"
        warm_home = self.root / "warm-cargo-home"
        for warm in (warm_db, warm_home):
            (warm / "registry").mkdir(parents=True)
            (warm / "registry" / "cached").write_text("warm\n")
        warm_env = {"CARGO_HOME": str(warm_home)}

        failed = self.run_gate(log=os.devnull, FAKE_RC="1", FAKE_NO_DB="1", **warm_env)
        self.assert_blocked(failed, "scanner exited 1")
        scans = read_records(self.scan_record)
        passed = self.run_gate(**warm_env)
        self.assertEqual(passed.returncode, 0, passed.stdout + passed.stderr)
        scans += read_records(self.scan_record)

        self.assertEqual(len(scans), 2, scans)
        private = f"{self.tmpdir}/cargo-audit-release-gate."
        for scan in scans:
            self.assertTrue(scan["db"].startswith(private), scan)
            self.assertTrue(scan["home"].startswith(private), scan)
            self.assertEqual((scan["db_existed"], scan["home_empty"]), ("no", "yes"), scan)
            self.assertNotIn(scan["db"], (str(warm_db), str(warm_home)))
            self.assertNotIn(scan["home"], (str(warm_db), str(warm_home)))
        self.assertNotEqual(scans[0]["db"], scans[1]["db"])
        self.assertNotEqual(scans[0]["home"], scans[1]["home"])
        # Each run removed its private directory; the warm state is untouched.
        self.assertEqual([p.name for p in self.tmpdir.iterdir()], ["rustsec-db"])
        self.assertEqual((warm_home / "registry" / "cached").read_text(), "warm\n")

    def test_unexpected_guard_status_fails_closed(self):
        copies = self.root / "scripts"
        copies.mkdir()
        gate_copy = copies / GATE.name
        shutil.copy(GATE, gate_copy)
        guard_copy = copies / "cargo-audit-report-guard.sh"
        guard_copy.write_text("#!/usr/bin/env bash\nexit 127\n")
        guard_copy.chmod(0o755)
        self.assert_blocked(self.run_gate(script=gate_copy), "report guard returned an unexpected status (127)")

    def test_static_commands_keep_pins_and_bounds(self):
        commands = logical_commands(GATE.read_text())
        tokens = [token for command in commands for token in command]

        assignments = {c[1] for c in commands if len(c) == 2 and c[0] == "readonly"}
        for pin in (f"ARCHIVE_URL={ARCHIVE_URL}", f"ARCHIVE_SHA256={ARCHIVE_SHA256}",
                    f"SCANNER_VERSION_LINE={SCANNER_VERSION}", f"ADVISORY_DB_URL={TRUSTED_DB}"):
            self.assertIn(pin, assignments)

        [curl] = [c for c in commands if "curl" in c and any("$ARCHIVE_URL" in t for t in c)]
        curl_pairs = list(zip(curl, curl[1:]))
        for flag, value in (("--proto", "=https"), ("--connect-timeout", "15"), ("--max-time", "180")):
            self.assertEqual(curl.count(flag), 1, curl)
            self.assertIn((flag, value), curl_pairs)

        [scan] = [c for c in commands if "timeout" in c and "audit" in c]
        start = scan.index("timeout")
        self.assertEqual(scan[start:start + 5], ["timeout", "--kill-after=10s", "5m", "$scanner", "audit"])
        scan_pairs = list(zip(scan, scan[1:]))
        for pair in (("--file", "$lockfile"), ("--db", "$db"), ("--url", "$ADVISORY_DB_URL"),
                     ("--format", "terminal"), ("--color", "never")):
            self.assertIn(pair, scan_pairs)

        forbidden = re.compile(r"^--(no-fetch|stale|ignore|deny)")
        self.assertEqual([t for t in tokens if forbidden.match(t)], [])


if __name__ == "__main__":
    unittest.main()
