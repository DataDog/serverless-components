"""Offline control-flow tests for scripts/cargo-audit-release-gate.sh.

These tests run the real production helper against a temporary git repository, a
fixture scanner archive, and local PATH test doubles for curl, sha256sum, and
cargo. They never touch the network, install a real scanner, run a publisher, or
read any environment variable that the production helper itself honors. The test
doubles only shape the helper's inputs and record what the helper did.

A separate opt-in suite, network_smoke.py, exercises the real download, checksum,
database refresh, and scan path on a runner with network access.
"""
import hashlib
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
GATE = REPO / "scripts" / "cargo-audit-release-gate.sh"
FIXTURES = HERE / "fixtures"
FAKE_SCANNER = FIXTURES / "fake-scanner" / "cargo-audit"
STUBS = FIXTURES / "stubs"

ARCHIVE_URL = (
    "https://github.com/rustsec/rustsec/releases/download/"
    "cargo-audit/v0.22.2/cargo-audit-x86_64-unknown-linux-musl-v0.22.2.tgz"
)
ARCHIVE_SHA256 = "7fb9497f8594b389e5fce5ef9b92db08432996895b2e0c5a0167a69ed445c428"
SCANNER_VERSION_LINE = "cargo-audit-audit 0.22.2"
ADVISORY_DB_URL = "https://github.com/RustSec/advisory-db.git"
LOCK_TEXT = 'version = 3\n\n[[package]]\nname = "gate-fixture"\nversion = "0.1.0"\n'
POLICY_TEXT = '[advisories]\nignore = []\n'


def sha256_file(path):
    digest = hashlib.sha256()
    digest.update(Path(path).read_bytes())
    return digest.hexdigest()


def find_real(tool):
    for directory in os.environ["PATH"].split(os.pathsep):
        candidate = Path(directory) / tool
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    raise RuntimeError(f"cannot find {tool} on PATH")


class GateHarness:
    """Builds one isolated gate fixture: a git repo, an archive, and stubs."""

    def __init__(self, root):
        self.root = Path(root)
        self.repo = self.root / "repo"
        self.stub_dir = self.root / "stubs"
        self.archive = self.root / "fixture.tgz"
        self.record = self.root / "scanner-record.txt"
        self.sentinel = self.root / "sentinel"
        self.real_sha256sum = find_real("sha256sum")
        self.real_timeout = find_real("timeout")
        self._make_repo()
        self._make_stub_dir()
        self._make_archive()

    def _git(self, *args):
        env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull}
        return subprocess.run(["git", *args], capture_output=True, text=True, check=True, env=env)

    def _make_repo(self):
        self.repo.mkdir()
        (self.repo / ".cargo").mkdir()
        (self.repo / "Cargo.lock").write_text(LOCK_TEXT)
        (self.repo / ".cargo" / "audit.toml").write_text(POLICY_TEXT)
        self._git("init", "--quiet", "--initial-branch=main", str(self.repo))
        self._git("-C", str(self.repo), "add", "-A")
        self._git(
            "-C",
            str(self.repo),
            "-c",
            "user.email=gate@example.invalid",
            "-c",
            "user.name=gate",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        )
        self.head_sha = self._git("-C", str(self.repo), "rev-parse", "HEAD").stdout.strip()

    def _make_stub_dir(self):
        self.stub_dir.mkdir()
        for name in ("curl", "sha256sum", "cargo", "timeout"):
            destination = self.stub_dir / name
            shutil.copy(STUBS / name, destination)
            destination.chmod(0o755)

    def _make_archive(self):
        stage = self.root / "stage"
        archive_root = stage / "cargo-audit-x86_64-unknown-linux-musl-v0.22.2"
        archive_root.mkdir(parents=True)
        binary = archive_root / "cargo-audit"
        shutil.copy(FAKE_SCANNER, binary)
        binary.chmod(0o755)
        subprocess.run(
            ["tar", "-czf", str(self.archive), "-C", str(stage), archive_root.name],
            check=True,
        )
        self.archive_real_sha = sha256_file(self.archive)

    def env(self, **overrides):
        env = dict(os.environ)
        env["PATH"] = f"{self.stub_dir}{os.pathsep}{env['PATH']}"
        env["REAL_SHA256SUM"] = self.real_sha256sum
        env["REAL_TIMEOUT"] = self.real_timeout
        env["FAKE_CURL_SOURCE"] = str(self.archive)
        env["FAKE_CURL_MODE"] = "ok"
        env["FAKE_ARCHIVE_REAL_SHA"] = self.archive_real_sha
        env["FAKE_ARCHIVE_SHA"] = ARCHIVE_SHA256
        env["FAKE_ARCHIVE_SHA_MODE"] = "ok"
        env["FAKE_CARGO_MODE"] = "ok"
        env["FAKE_TIMEOUT_MODE"] = "exec"
        env["FAKE_SCANNER_MODE"] = "success"
        env["FAKE_SCANNER_RECORD"] = str(self.record)
        env["GIT_CONFIG_GLOBAL"] = os.devnull
        env["GIT_CONFIG_SYSTEM"] = os.devnull
        env.update(overrides)
        return env

    def path_without(self, *excluded):
        """Build a PATH view of the real tools plus the stubs, minus exclusions.

        Used to prove the gate's required-tool check, which cannot be exercised
        by removing a tool from the inherited PATH while the stubs still provide
        it.
        """
        shim = self.root / ("shim-no-" + "-".join(excluded))
        if shim.exists():
            return str(shim)
        shim.mkdir()
        seen = set()
        sources = [self.stub_dir, *os.environ["PATH"].split(os.pathsep)]
        for source in sources:
            directory = Path(source)
            if not directory.is_dir():
                continue
            for entry in sorted(directory.iterdir()):
                name = entry.name
                if name in excluded or name in seen:
                    continue
                try:
                    if entry.is_file() and os.access(entry, os.X_OK):
                        (shim / name).symlink_to(entry)
                        seen.add(name)
                except OSError:
                    continue
        return str(shim)

    def run_gate(self, script=None, expected_sha=None, sentinel=None, **env_overrides):
        script = script or GATE
        expected_sha = expected_sha if expected_sha is not None else self.head_sha
        sentinel = sentinel or self.sentinel
        command = f'"{script}" "{expected_sha}" && touch "{sentinel}"'
        return subprocess.run(
            ["bash", "-c", command],
            cwd=str(self.repo),
            env=self.env(**env_overrides),
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )

    def run_raw(self, args, **env_overrides):
        return subprocess.run(
            ["bash", *[str(arg) for arg in args]],
            cwd=str(self.repo),
            env=self.env(**env_overrides),
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )

    def scanner_runs(self):
        if not self.record.exists():
            return []
        runs = []
        current = {}
        for line in self.record.read_text().splitlines():
            if not line:
                continue
            key, _, value = line.partition("=")
            if key == "db" and current:
                runs.append(current)
                current = {}
            current[key] = value
        if current:
            runs.append(current)
        return runs


class ReleaseGateTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="audit-gate-test.")
        self.addCleanup(self._tmp.cleanup)
        self.harness = GateHarness(self._tmp.name)

    def assert_blocked(self, proc, message=None):
        self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertFalse(self.harness.sentinel.exists(), "gate failure reached the sentinel")
        if message is not None:
            self.assertIn(message, proc.stdout + proc.stderr)

    # --- accepted path -----------------------------------------------------

    def test_success_path_reaches_sentinel_and_logs_identity(self):
        proc = self.harness.run_gate()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertTrue(self.harness.sentinel.exists())
        self.assertIn("result: PASS", proc.stdout)
        self.assertIn(f"expected-sha: {self.harness.head_sha}", proc.stdout)
        self.assertIn(f"scanner-version: {SCANNER_VERSION_LINE}", proc.stdout)
        self.assertIn(f"advisory-db-origin: {ADVISORY_DB_URL}", proc.stdout)
        self.assertIn("advisory-db-revision:", proc.stdout)
        self.assertNotIn("advisory-db-revision: unavailable", proc.stdout)
        self.assertIn("cargo-lock-sha256:", proc.stdout)
        self.assertIn("audit-toml-sha256:", proc.stdout)

    def test_scanner_invocation_uses_pinned_flags_and_no_bypasses(self):
        proc = self.harness.run_gate()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        runs = self.harness.scanner_runs()
        self.assertEqual(len(runs), 1, runs)
        args = runs[0]["args"]
        self.assertIn(f"--url {ADVISORY_DB_URL}", args)
        self.assertIn("--format terminal", args)
        self.assertIn("--color never", args)
        self.assertIn("--file", args)
        self.assertIn("--db", args)
        for bypass in ("--no-fetch", "--stale", "--deny", "--ignore"):
            self.assertNotIn(bypass, args)

    def test_report_log_is_displayed(self):
        proc = self.harness.run_gate()
        self.assertIn("begin cargo-audit output", proc.stdout)
        self.assertIn("end cargo-audit output", proc.stdout)
        runs = self.harness.scanner_runs()
        self.assertEqual(len(runs), 1, runs)
        scanned = runs[0]["file"]
        self.assertTrue(scanned.startswith("/"), scanned)
        self.assertIn(f"Scanning {scanned} for vulnerabilities (1 crate dependencies)", proc.stdout)

    def test_scan_of_a_substituted_lockfile_blocks_sentinel(self):
        # The scanner reports a different lockfile than the one the gate passed
        # to it; the pure guard must reject the mismatch before the sentinel.
        proc = self.harness.run_gate(FAKE_SCANNER_SCAN_PATH="Cargo.lock")
        self.assert_blocked(proc, "but the gate expected")

    def test_download_is_bounded(self):
        record = self.harness.root / "curl-args.txt"
        proc = self.harness.run_gate(FAKE_CURL_RECORD=str(record))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        argv = record.read_text()
        self.assertIn("--connect-timeout 15", argv)
        self.assertIn("--max-time 180", argv)
        self.assertIn("--proto =https", argv)

    def test_scanner_timeout_is_bounded_and_blocks_sentinel(self):
        for mode, expected in (("fail124", 124), ("fail137", 137)):
            with self.subTest(mode=mode):
                record = self.harness.root / f"timeout-{mode}.txt"
                proc = self.harness.run_gate(
                    FAKE_TIMEOUT_MODE=mode,
                    FAKE_TIMEOUT_RECORD=str(record),
                )
                self.assert_blocked(proc)
                self.assertEqual(proc.returncode, expected, proc.stdout + proc.stderr)
                argv = record.read_text()
                self.assertIn("--kill-after=10s", argv)
                self.assertIn("5m", argv)
                self.assertIn("audit", argv)
        # The bounded timeout fired before the scanner could run.
        self.assertEqual(self.harness.scanner_runs(), [])

    def test_missing_timeout_tool_blocks_sentinel(self):
        proc = self.harness.run_gate(PATH=self.harness.path_without("timeout"))
        self.assert_blocked(proc, "required tool not found on PATH: timeout")
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)

    # --- scanner verdicts --------------------------------------------------

    def test_findings_block_sentinel(self):
        proc = self.harness.run_gate(FAKE_SCANNER_MODE="findings")
        self.assert_blocked(proc)
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)

    def test_hidden_index_error_blocks_sentinel(self):
        proc = self.harness.run_gate(FAKE_SCANNER_MODE="index-error")
        self.assert_blocked(proc)

    def test_yanked_diagnostic_blocks_sentinel(self):
        proc = self.harness.run_gate(FAKE_SCANNER_MODE="yanked-error")
        self.assert_blocked(proc)

    def test_missing_progress_markers_block_sentinel(self):
        for mode in ("quiet", "disabled-yanked", "disabled-fetch"):
            with self.subTest(mode=mode):
                self.harness.sentinel.unlink(missing_ok=True)
                proc = self.harness.run_gate(FAKE_SCANNER_MODE=mode)
                self.assert_blocked(proc)

    def test_fetch_failure_blocks_sentinel(self):
        proc = self.harness.run_gate(FAKE_SCANNER_MODE="fetch-error")
        self.assert_blocked(proc)
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)

    def test_config_error_blocks_sentinel(self):
        proc = self.harness.run_gate(FAKE_SCANNER_MODE="config-error")
        self.assert_blocked(proc)

    def test_untrusted_advisory_origin_blocks_sentinel(self):
        proc = self.harness.run_gate(FAKE_SCANNER_MODE="wrong-origin")
        self.assert_blocked(proc, "unexpected advisory database origin")

    # --- installation and prerequisite failures ----------------------------

    def test_download_failure_blocks_sentinel(self):
        proc = self.harness.run_gate(FAKE_CURL_MODE="fail")
        self.assert_blocked(proc, "failed to download")

    def test_checksum_mismatch_blocks_sentinel(self):
        proc = self.harness.run_gate(FAKE_ARCHIVE_SHA_MODE="bad")
        self.assert_blocked(proc, "checksum mismatch")

    def test_extraction_failure_blocks_sentinel(self):
        corrupt = Path(self._tmp.name) / "corrupt.tgz"
        corrupt.write_text("this is not a gzip archive\n")
        proc = self.harness.run_gate(
            FAKE_CURL_SOURCE=str(corrupt),
            FAKE_ARCHIVE_REAL_SHA=sha256_file(corrupt),
            FAKE_ARCHIVE_SHA_MODE="ok",
        )
        self.assert_blocked(proc, "failed to extract")

    def test_version_mismatch_blocks_sentinel(self):
        proc = self.harness.run_gate(FAKE_SCANNER_VERSION_LINE="cargo-audit-audit 0.22.1")
        self.assert_blocked(proc, "unexpected cargo-audit version")

    def test_non_exact_version_is_rejected(self):
        # The check must match the whole version line, not a substring.
        proc = self.harness.run_gate(
            FAKE_SCANNER_VERSION_LINE="cargo-audit-audit 0.22.20 extra"
        )
        self.assert_blocked(proc, "unexpected cargo-audit version")

    def test_broken_cargo_toolchain_blocks_sentinel(self):
        proc = self.harness.run_gate(FAKE_CARGO_MODE="fail")
        self.assert_blocked(proc, "cargo -V failed")

    # --- source and policy identity ---------------------------------------

    def test_sha_mismatch_blocks_sentinel(self):
        proc = self.harness.run_gate(expected_sha="a" * 40)
        self.assert_blocked(proc, "does not match expected release commit")

    def test_working_tree_lockfile_mismatch_blocks_sentinel(self):
        (self.harness.repo / "Cargo.lock").write_text(LOCK_TEXT + "\n# tampered\n")
        proc = self.harness.run_gate()
        self.assert_blocked(proc, "Cargo.lock does not match")

    def test_working_tree_policy_mismatch_blocks_sentinel(self):
        (self.harness.repo / ".cargo" / "audit.toml").write_text(
            '[advisories]\nignore = ["RUSTSEC-0000-0000"]\n'
        )
        proc = self.harness.run_gate()
        self.assert_blocked(proc, "audit.toml does not match")

    def test_missing_lockfile_blocks_sentinel(self):
        (self.harness.repo / "Cargo.lock").unlink()
        proc = self.harness.run_gate()
        self.assert_blocked(proc, "missing Cargo.lock")

    def test_missing_policy_blocks_sentinel(self):
        (self.harness.repo / ".cargo" / "audit.toml").unlink()
        proc = self.harness.run_gate()
        self.assert_blocked(proc, "missing .cargo/audit.toml")

    # --- usage and input validation ---------------------------------------

    def test_usage_and_input_validation(self):
        cases = [
            ([GATE], 2),
            ([GATE, "a", "b"], 2),
            ([GATE, "not-a-commit"], 2),
            ([GATE, "A" * 40], 2),
        ]
        for args, expected in cases:
            with self.subTest(args=args):
                proc = self.harness.run_raw(args)
                self.assertEqual(proc.returncode, expected, proc.stdout + proc.stderr)

    # --- freshness and retry ----------------------------------------------

    def test_fresh_private_directories_per_execution(self):
        first = self.harness.run_gate()
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        second = self.harness.run_gate()
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)

        runs = self.harness.scanner_runs()
        self.assertEqual(len(runs), 2, runs)
        for run in runs:
            self.assertTrue(run["db"].startswith(os.path.join(tempfile.gettempdir(), "cargo-audit-release-gate")))
            self.assertTrue(run["cargo_home"].startswith(os.path.join(tempfile.gettempdir(), "cargo-audit-release-gate")))
            self.assertNotEqual(run["db"], run["cargo_home"])
        self.assertNotEqual(runs[0]["db"], runs[1]["db"])
        self.assertNotEqual(runs[0]["cargo_home"], runs[1]["cargo_home"])

    def test_inherited_warm_cache_cannot_authorize_refresh_failure(self):
        # The inherited environment carries a warm RustSec clone with an
        # untrusted origin plus a warm Cargo home, both under the inherited
        # TMPDIR. The stub below would authorize the release if it were handed
        # the warm database, and fails when the database path is fresh. The gate
        # must therefore fail here rather than reuse any inherited state.
        inherited = self.harness.root / "inherited"
        inherited.mkdir()
        warm_db = inherited / "rustsec-db"
        warm_db.mkdir()
        self.harness._git("init", "--quiet", str(warm_db))
        self.harness._git(
            "-C", str(warm_db), "remote", "add", "origin", "https://example.invalid/advisory-db.git"
        )
        (warm_db / "README.md").write_text("warm fixture advisory database\n")
        self.harness._git("-C", str(warm_db), "add", "-A")
        self.harness._git(
            "-C",
            str(warm_db),
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=fixture",
            "commit",
            "--quiet",
            "-m",
            "warm fixture database",
        )
        warm_cargo_home = inherited / "cargo-home"
        (warm_cargo_home / "registry" / "index").mkdir(parents=True)
        (warm_cargo_home / ".package-cache").write_text("warm cargo cache\n")

        # Prove the stub discriminates: it succeeds on the warm path and fails
        # on a fresh path that simulates an unavailable network refresh.
        warm = self.harness.run_raw(
            [FAKE_SCANNER, "audit", "--db", str(warm_db), "--file", "Cargo.lock"],
            FAKE_SCANNER_MODE="warm-authorize",
            FAKE_SCANNER_RECORD="",
        )
        self.assertEqual(warm.returncode, 0, warm.stdout + warm.stderr)
        self.assertIn("Fetching advisory database", warm.stdout)
        fresh = self.harness.run_raw(
            [FAKE_SCANNER, "audit", "--db", str(inherited / "no-such-db"), "--file", "Cargo.lock"],
            FAKE_SCANNER_MODE="warm-authorize",
            FAKE_SCANNER_RECORD="",
        )
        self.assertNotEqual(fresh.returncode, 0, fresh.stdout + fresh.stderr)

        proc = self.harness.run_gate(
            FAKE_SCANNER_MODE="warm-authorize",
            TMPDIR=str(inherited),
            CARGO_HOME=str(warm_cargo_home),
        )
        self.assert_blocked(proc)
        runs = self.harness.scanner_runs()
        self.assertEqual(len(runs), 1, runs)
        fresh_work = str(inherited / "cargo-audit-release-gate")
        self.assertTrue(runs[0]["db"].startswith(fresh_work), runs)
        self.assertTrue(runs[0]["cargo_home"].startswith(fresh_work), runs)
        self.assertNotEqual(runs[0]["db"], str(warm_db))
        self.assertNotEqual(runs[0]["cargo_home"], str(warm_cargo_home))
        # The inherited untrusted-origin database is never inspected or reused.
        self.assertNotIn("example.invalid", proc.stdout + proc.stderr)

    def test_retry_after_refresh_failure_rescans_and_can_pass(self):
        failed = self.harness.run_gate(FAKE_SCANNER_MODE="fetch-error")
        self.assert_blocked(failed)
        passed = self.harness.run_gate()
        self.assertEqual(passed.returncode, 0, passed.stdout + passed.stderr)
        runs = self.harness.scanner_runs()
        self.assertEqual(len(runs), 2, runs)
        self.assertNotEqual(runs[0]["db"], runs[1]["db"])
        self.assertNotEqual(runs[0]["cargo_home"], runs[1]["cargo_home"])

    # --- helper robustness ------------------------------------------------

    def test_unexpected_guard_status_is_rejected(self):
        copydir = self.harness.root / "copyscripts"
        copydir.mkdir()
        gate_copy = copydir / "cargo-audit-release-gate.sh"
        shutil.copy(GATE, gate_copy)
        gate_copy.chmod(0o755)
        guard_copy = copydir / "cargo-audit-report-guard.sh"
        guard_copy.write_text("#!/usr/bin/env bash\nexit 127\n")
        guard_copy.chmod(0o755)
        proc = self.harness.run_gate(script=gate_copy)
        self.assert_blocked(proc, "unexpected status")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)

    def test_pinned_constants_are_literal_and_reviewed(self):
        text = GATE.read_text()
        self.assertIn(ARCHIVE_URL, text)
        self.assertIn(ARCHIVE_SHA256, text)
        self.assertIn(SCANNER_VERSION_LINE, text)
        self.assertIn(ADVISORY_DB_URL, text)
        # No dynamic digest lookup and no alternate-archive environment hook.
        self.assertNotIn("sha256sum -c", text)
        self.assertNotIn("curl --head", text)


if __name__ == "__main__":
    unittest.main()
