"""Opt-in network smoke suite for the cargo-audit release gate.

This suite is not part of the deterministic offline run. It downloads the real
pinned cargo-audit archive, verifies its reviewed checksum, installs it, clones a
fresh RustSec advisory database, updates the crates.io index, and scans small
fixture lockfiles. It requires a working Rust toolchain and network access.

Run it from the repository root with:

    python3 scripts/tests/audit-release-gate/network_smoke.py

It never publishes, signs, uploads, or pushes anything, and it never asserts that
the repository's own Cargo.lock passes: that lock is expected to be blocked while
unfixed advisories remain.

The file is deliberately named so that `unittest discover -p 'test_*.py'` skips it.
"""
import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
GATE = REPO / "scripts" / "cargo-audit-release-gate.sh"

SCANNER_VERSION_LINE = "cargo-audit-audit 0.22.2"
ADVISORY_DB_URL = "https://github.com/RustSec/advisory-db.git"
LOCK_HEADER = 'version = 3\n\n[[package]]\nname = "smoke-fixture"\nversion = "0.1.0"\n'
DEFAULT_POLICY = "[advisories]\nignore = []\n"

GIT_ENV = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull}


def registry_package(name, version):
    return (
        f'\n[[package]]\nname = "{name}"\nversion = "{version}"\n'
        'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
    )


def make_repo(root, lock_text, policy_text=DEFAULT_POLICY):
    repo = Path(root) / "repo"
    repo.mkdir()
    (repo / ".cargo").mkdir()
    (repo / "Cargo.lock").write_text(lock_text)
    (repo / ".cargo" / "audit.toml").write_text(policy_text)
    for args in (
        ("init", "--quiet", "--initial-branch=main", str(repo)),
        ("-C", str(repo), "add", "-A"),
        (
            "-C",
            str(repo),
            "-c",
            "user.email=smoke@example.invalid",
            "-c",
            "user.name=smoke",
            "commit",
            "--quiet",
            "-m",
            "smoke fixture",
        ),
    ):
        subprocess.run(["git", *args], check=True, capture_output=True, text=True, env=GIT_ENV)
    head = subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
        env=GIT_ENV,
    ).stdout.strip()
    return repo, head


def run_gate(repo, sha):
    return subprocess.run(
        ["bash", str(GATE), sha],
        cwd=str(repo),
        capture_output=True,
        text=True,
        check=False,
        timeout=900,
    )


class NetworkSmoke(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="audit-gate-smoke.")
        self.addCleanup(self._tmp.cleanup)

    def assert_scanner_identity(self, output):
        self.assertIn(f"scanner-version: {SCANNER_VERSION_LINE}", output)
        revision = re.search(r"^advisory-db-revision: ([0-9a-f]{40,64})$", output, re.MULTILINE)
        self.assertIsNotNone(revision, output)
        self.assertIn(f"advisory-db-origin: {ADVISORY_DB_URL}", output)
        # A per-execution fetch must be visible in the displayed log.
        self.assertIn("Fetching advisory database", output)
        # The real scanner must report a nonzero advisory count and scan the
        # absolute lockfile path with a nonzero crate dependency count. This is
        # the runtime proof for the structured report-guard contract.
        self.assertIsNotNone(
            re.search(r"^\s*Loaded [1-9][0-9]* security advisories", output, re.MULTILINE),
            output,
        )
        self.assertIsNotNone(
            re.search(
                r"^\s*Scanning /.*/Cargo\.lock for vulnerabilities \([1-9][0-9]* crate dependencies\)$",
                output,
                re.MULTILINE,
            ),
            output,
        )

    def test_clean_fixture_passes_and_refreshes(self):
        repo, head = make_repo(self._tmp.name, LOCK_HEADER)
        first = run_gate(repo, head)
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.assertIn("result: PASS", first.stdout)
        self.assert_scanner_identity(first.stdout)

        # Retrying the gate re-fetches advisory data instead of reusing state.
        second = run_gate(repo, head)
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assert_scanner_identity(second.stdout)

    def test_vulnerable_fixture_blocks_publication(self):
        lock = LOCK_HEADER + registry_package("openssl", "0.10.38")
        repo, head = make_repo(self._tmp.name, lock)
        result = run_gate(repo, head)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("result: FAIL", result.stdout + result.stderr)
        self.assertRegex(result.stdout, r"RUSTSEC-\d{4}-\d{4}")
        self.assert_scanner_identity(result.stdout)

    def test_informational_warning_fixture_passes(self):
        lock = LOCK_HEADER + registry_package("paste", "1.0.15")
        repo, head = make_repo(self._tmp.name, lock)
        result = run_gate(repo, head)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("allowed warning found", result.stdout)

    def test_existing_ignore_policy_is_honored(self):
        lock = LOCK_HEADER + registry_package("openssl", "0.10.38")
        repo, head = make_repo(self._tmp.name, lock)

        blocked = run_gate(repo, head)
        self.assertNotEqual(blocked.returncode, 0, blocked.stdout + blocked.stderr)
        advisories = sorted(set(re.findall(r"RUSTSEC-\d{4}-\d{4}", blocked.stdout)))
        self.assertTrue(advisories, blocked.stdout)

        policy = "[advisories]\nignore = [\n"
        for advisory in advisories:
            policy += f'    "{advisory}",\n'
        policy += "]\n"
        (repo / ".cargo" / "audit.toml").write_text(policy)
        subprocess.run(
            ["git", "-C", str(repo), "add", "-A"],
            check=True,
            capture_output=True,
            text=True,
            env=GIT_ENV,
        )
        subprocess.run(
            [
                "git",
                "-C",
                str(repo),
                "-c",
                "user.email=smoke@example.invalid",
                "-c",
                "user.name=smoke",
                "commit",
                "--quiet",
                "-m",
                "ignore the fixture advisories",
            ],
            check=True,
            capture_output=True,
            text=True,
            env=GIT_ENV,
        )
        new_head = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
            env=GIT_ENV,
        ).stdout.strip()

        allowed = run_gate(repo, new_head)
        self.assertEqual(allowed.returncode, 0, allowed.stdout + allowed.stderr)
        self.assertIn("result: PASS", allowed.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
