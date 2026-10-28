"""Opt-in real-scanner tests; excluded from offline test_*.py discovery.

Run from the repository root with cargo and network access. Each gate invocation
uses the real pinned download, checksum, fresh advisory DB and crates.io index.
Only temporary fixture lockfiles are scanned; nothing is published or uploaded.
"""
import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

GATE = Path(__file__).resolve().parents[2] / "cargo-audit-release-gate.sh"
VERSION = "cargo-audit-audit 0.22.2"
TRUSTED_DB = "https://github.com/RustSec/advisory-db.git"
LOCK = 'version = 3\n\n[[package]]\nname = "smoke-fixture"\nversion = "0.1.0"\n'
GIT_ENV = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull}


def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], check=True,
                          capture_output=True, text=True, env=GIT_ENV).stdout.strip()


def commit_all(repo, message):
    git(repo, "add", "-A")
    git(repo, "-c", "user.email=smoke@example.invalid", "-c", "user.name=smoke",
        "commit", "--quiet", "-m", message)
    return git(repo, "rev-parse", "HEAD")


def registry_package(name, version):
    return (f'\n[[package]]\nname = "{name}"\nversion = "{version}"\n'
            'source = "registry+https://github.com/rust-lang/crates.io-index"\n')


def make_repo(root, lock):
    repo = Path(root) / "repo"
    (repo / ".cargo").mkdir(parents=True)
    (repo / "Cargo.lock").write_text(lock)
    (repo / ".cargo" / "audit.toml").write_text("[advisories]\nignore = []\n")
    git(repo, "init", "--quiet", "--initial-branch=main")
    return repo, commit_all(repo, "smoke fixture")


class NetworkSmoke(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="audit-gate-smoke.")
        self.addCleanup(tmp.cleanup)
        self.root = tmp.name

    def assert_scanner_identity(self, output):
        self.assertIn(f"scanner-version: {VERSION}", output)
        self.assertIn(f"advisory-db-origin: {TRUSTED_DB}", output)
        self.assertIn("Fetching advisory database", output)
        self.assertRegex(output, r"(?m)^advisory-db-revision: (?:[0-9a-f]{40}|[0-9a-f]{64})$")
        self.assertRegex(output, r"(?m)^\s*Loaded [1-9][0-9]* security advisories")
        self.assertRegex(
            output, r"(?m)^\s*Scanning /.*/Cargo\.lock for vulnerabilities "
            r"\([1-9][0-9]* crate dependencies\)$",
        )

    def scan(self, repo, sha, accepted=True):
        result = subprocess.run(["bash", str(GATE), sha], cwd=repo, capture_output=True,
                                text=True, timeout=900)
        self.assertEqual(result.returncode == 0, accepted, result.stdout + result.stderr)
        self.assertIn("result: PASS" if accepted else "result: FAIL", result.stdout + result.stderr)
        self.assert_scanner_identity(result.stdout)
        return result.stdout

    def test_clean_fixture_and_retry_refresh(self):
        repo, sha = make_repo(self.root, LOCK)
        self.scan(repo, sha)
        # A retry must fetch again, not authorize from inherited state.
        self.scan(repo, sha)

    def test_findings_block_until_ignore_policy_is_committed(self):
        repo, sha = make_repo(self.root, LOCK + registry_package("openssl", "0.10.38"))
        blocked = self.scan(repo, sha, accepted=False)
        advisories = sorted(set(re.findall(r"RUSTSEC-\d{4}-\d{4}", blocked)))
        self.assertTrue(advisories, blocked)
        ignores = ",\n".join(f'    "{advisory}"' for advisory in advisories)
        (repo / ".cargo" / "audit.toml").write_text(f"[advisories]\nignore = [\n{ignores}\n]\n")
        self.scan(repo, commit_all(repo, "ignore the fixture advisories"))

    def test_informational_warning_passes(self):
        repo, sha = make_repo(self.root, LOCK + registry_package("paste", "1.0.15"))
        self.assertIn("allowed warning found", self.scan(repo, sha))


if __name__ == "__main__":
    unittest.main(verbosity=2)
