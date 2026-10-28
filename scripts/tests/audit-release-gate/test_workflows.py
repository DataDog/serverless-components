"""Offline structural tests for the release-gate workflows.

These tests parse the real workflow YAML and assert that the strict cargo-audit
gate runs inside each protected job, before that job's first publication side
effect, against the exact source revision that produced the artifacts. They also
run a set of negative mutations to prove the checks reject an absent, reordered,
masked, or unlocked gate.

PyYAML is a test-only dependency. `on:` is a YAML 1.1 boolean key, so the trigger
block is read through the `trigger()` helper rather than by key name.
"""
import unittest
from pathlib import Path

import yaml

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
WORKFLOWS = REPO / ".github" / "workflows"
PUBLISH = WORKFLOWS / "publish.yml"
RELEASE = WORKFLOWS / "release-datadog-serverless-compat.yml"
BUILD = WORKFLOWS / "build-datadog-serverless-compat.yml"
TESTS = WORKFLOWS / "audit-release-gate-tests.yml"

CHECKOUT_PIN = "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683"
RUST_SETUP_PIN = "actions-rust-lang/setup-rust-toolchain@9399c7bb15d4c7d47b27263d024f0a4978346ba4"
GH_RELEASE_PIN = "softprops/action-gh-release@da05d552573ad5aba039eaac05058a918a7bf631"
GATE_CALL = "cargo-audit-release-gate.sh"
GATE_COMMAND = './scripts/cargo-audit-release-gate.sh "$GITHUB_SHA"'
SOURCE_REF = "${{ github.sha }}"
BUILD_ONLY_CONDITION = "${{ inputs.publish == 'Build and Publish' }}"
DOWNLOAD_ARTIFACT_PREFIX = "actions/download-artifact@"
EXTERNAL_ARTIFACT_SELECTORS = ("run-id", "repository", "github-token")


def load(path):
    with path.open(encoding="utf-8") as handle:
        return yaml.safe_load(handle)


def trigger(doc):
    for key in ("on", True):
        if key in doc:
            return doc[key]
    raise AssertionError("workflow has no trigger block")


def step_run(step):
    return str(step.get("run", ""))


def find_step(steps, predicate):
    for index, step in enumerate(steps):
        if predicate(step):
            return index, step
    return None, None


def normalized_run(step):
    return " ".join(step_run(step).split())


def assert_no_failure_masks(doc):
    for job_name, job in doc["jobs"].items():
        if job.get("continue-on-error"):
            raise AssertionError(f"job '{job_name}' sets continue-on-error")
        job_condition = str(job.get("if", ""))
        if "always(" in job_condition or "cancelled(" in job_condition:
            raise AssertionError(f"job '{job_name}' has an error-masking condition")
        for step in job.get("steps", []):
            if step.get("continue-on-error"):
                raise AssertionError(f"job '{job_name}' has a step with continue-on-error")
            condition = str(step.get("if", ""))
            if "always(" in condition or "cancelled(" in condition:
                raise AssertionError(f"job '{job_name}' has an error-masking step condition")


def assert_same_run_artifact_downloads(steps, where):
    # Downloading artifacts with a run id, repository, or explicit token could
    # pull artifacts produced by a different run than the gated source revision.
    for step in steps:
        if not str(step.get("uses", "")).startswith(DOWNLOAD_ARTIFACT_PREFIX):
            continue
        with_block = step.get("with", {}) or {}
        for selector in EXTERNAL_ARTIFACT_SELECTORS:
            if selector in with_block:
                raise AssertionError(
                    f"{where}: artifact download must come from the current run, not '{selector}'"
                )


def assert_gate_step(step, where):
    if step is None:
        raise AssertionError(f"{where}: strict cargo-audit gate is missing")
    if "if" in step:
        raise AssertionError(f"{where}: gate step must be unconditional")
    if "continue-on-error" in step:
        raise AssertionError(f"{where}: gate step must not ignore failures")
    run = normalized_run(step)
    if run != GATE_COMMAND:
        raise AssertionError(
            f"{where}: gate step must run exactly '{GATE_COMMAND}', found '{run}'"
        )
    shell = step.get("shell")
    if shell is not None and str(shell) not in ("bash", "bash {0}"):
        raise AssertionError(f"{where}: gate step must use the reviewed bash shell, found '{shell}'")
    if step.get("working-directory") != "source":
        raise AssertionError(f"{where}: gate step must run from the source checkout")


def assert_checkout_ref(step, where):
    if step is None:
        raise AssertionError(f"{where}: checkout is missing")
    if step.get("uses") != CHECKOUT_PIN:
        raise AssertionError(f"{where}: checkout is not pinned to the reviewed commit")
    if step.get("with", {}).get("ref") != SOURCE_REF:
        raise AssertionError(f"{where}: checkout does not pin ref {SOURCE_REF}")


def assert_source_checkout(step, where):
    assert_checkout_ref(step, where)
    if step.get("with", {}).get("path") != "source":
        raise AssertionError(f"{where}: checkout does not use path 'source'")


def assert_rust_setup(step, where):
    if step is None:
        raise AssertionError(f"{where}: Rust toolchain setup is missing")
    if step.get("uses") != RUST_SETUP_PIN:
        raise AssertionError(f"{where}: Rust setup is not pinned to the reviewed commit")
    if step.get("with", {}).get("cache") is not False:
        raise AssertionError(f"{where}: Rust setup must disable the toolchain cache")


def assert_package_job(doc):
    package = doc["jobs"]["package"]
    steps = package["steps"]
    _, checkout = find_step(steps, lambda s: str(s.get("uses", "")).startswith("actions/checkout@"))
    assert_checkout_ref(checkout, "package")
    index, upload = find_step(
        steps, lambda s: str(s.get("uses", "")).startswith("actions/upload-artifact@")
    )
    if index is None:
        raise AssertionError("package job must upload the npm packages it produces")
    if upload.get("with", {}).get("name") != "npm-packages":
        raise AssertionError("package job must upload the 'npm-packages' artifact")


def assert_publish_workflow(doc):
    assert_no_failure_masks(doc)
    publish = doc["jobs"]["publish"]
    if publish.get("environment") != "Production":
        raise AssertionError("publish job must keep the Production environment approval")
    if publish.get("needs") != "package":
        raise AssertionError("publish job must depend on the package job")
    condition = " ".join(str(publish.get("if", "")).split())
    if condition != BUILD_ONLY_CONDITION:
        raise AssertionError("publish job must keep the build-only condition")

    assert_package_job(doc)

    steps = publish["steps"]
    assert_same_run_artifact_downloads(steps, "publish")
    artifact_names = [
        step.get("with", {}).get("name")
        for step in steps
        if str(step.get("uses", "")).startswith(DOWNLOAD_ARTIFACT_PREFIX)
    ]
    if "npm-packages" not in artifact_names:
        raise AssertionError("publish job must download the artifact produced in the same run")
    _, checkout = find_step(steps, lambda s: str(s.get("uses", "")).startswith("actions/checkout@"))
    assert_source_checkout(checkout, "publish")

    _, rust = find_step(
        steps,
        lambda s: str(s.get("uses", "")).startswith("actions-rust-lang/setup-rust-toolchain@"),
    )
    assert_rust_setup(rust, "publish")

    gate_index, gate_step = find_step(steps, lambda s: GATE_CALL in step_run(s))
    assert_gate_step(gate_step, "publish")

    publish_index, publisher = find_step(steps, lambda s: "npm publish" in step_run(s))
    if publish_index is None:
        raise AssertionError("publish job has no npm publish step")
    if "if" in publisher:
        raise AssertionError("npm publish step must not be conditional")
    if gate_index > publish_index:
        raise AssertionError("strict cargo-audit gate must run before the first npm publish")

    permissions = doc.get("permissions", {})
    if permissions.get("contents") != "read":
        raise AssertionError("publish workflow must grant only contents: read")
    if permissions.get("id-token") != "write":
        raise AssertionError("publish workflow must keep id-token: write for provenance")

    inputs = trigger(doc).get("workflow_dispatch", {}).get("inputs", {}) or {}
    for name in inputs:
        if "skip" in name.lower() or "bypass" in name.lower():
            raise AssertionError(f"publish workflow must not expose a bypass input: {name}")


def assert_release_workflow(doc):
    assert_no_failure_masks(doc)
    release = doc["jobs"]["release"]
    if release.get("permissions", {}).get("contents") != "write":
        raise AssertionError("release job must keep contents: write")
    if release.get("needs") != "build":
        raise AssertionError("release job must depend on the build job")
    # This freezes the reviewed release flow: the strict cargo-audit gate plus a
    # draft release is the release check, so no additional protected environment
    # (and therefore no extra manual approval) is introduced. This is an
    # invariant of the reviewed change, not a claim that environments are unsafe.
    if "environment" in release:
        raise AssertionError("release job must not add an extra protected environment approval")

    steps = release["steps"]
    assert_same_run_artifact_downloads(steps, "release")
    _, checkout = find_step(steps, lambda s: str(s.get("uses", "")).startswith("actions/checkout@"))
    assert_source_checkout(checkout, "release")

    _, rust = find_step(
        steps,
        lambda s: str(s.get("uses", "")).startswith("actions-rust-lang/setup-rust-toolchain@"),
    )
    assert_rust_setup(rust, "release")

    gate_index, gate_step = find_step(steps, lambda s: GATE_CALL in step_run(s))
    assert_gate_step(gate_step, "release")

    zip_index, _ = find_step(steps, lambda s: step_run(s).strip().startswith("zip "))
    if zip_index is None:
        raise AssertionError("release job has no zip step")
    release_index, gh_release = find_step(
        steps, lambda s: str(s.get("uses", "")).startswith("softprops/action-gh-release@")
    )
    if release_index is None:
        raise AssertionError("release job has no action-gh-release step")
    if gate_index > zip_index or gate_index > release_index:
        raise AssertionError("strict cargo-audit gate must run before the zip and the draft release")

    if gh_release.get("uses") != GH_RELEASE_PIN:
        raise AssertionError("action-gh-release pin changed")
    if gh_release.get("with", {}).get("draft") is not True:
        raise AssertionError("draft release behavior changed")
    if "if" in gh_release:
        raise AssertionError("the draft release step must not be conditional")

    if doc.get("permissions") != {}:
        raise AssertionError("release workflow top-level permissions must stay empty")


def assert_build_workflow(doc):
    builds = []
    checkouts = []
    for job_name, job in doc["jobs"].items():
        for step in job.get("steps", []):
            run = step_run(step)
            if "cargo build --release -p datadog-serverless-compat" in run:
                builds.append((job_name, run))
            if "cargo-audit" in run:
                raise AssertionError("the shared build workflow must not run audits")
            if str(step.get("uses", "")).startswith("actions/checkout@"):
                checkouts.append(step)

    if len(builds) != 5:
        raise AssertionError(f"expected 5 release builds, found {len(builds)}")
    for job_name, run in builds:
        if "--locked" not in run:
            raise AssertionError(f"release build in job '{job_name}' is not --locked")

    if len(checkouts) != 2:
        raise AssertionError(f"expected 2 producer checkouts, found {len(checkouts)}")
    for checkout in checkouts:
        assert_checkout_ref(checkout, "build producer")


def assert_tests_workflow(doc, raw):
    assert_no_failure_masks(doc)
    if "if" in doc["jobs"]["offline"]:
        raise AssertionError("offline gate tests must run unconditionally")
    if doc.get("permissions") != {"contents": "read"}:
        raise AssertionError("tests workflow must grant only contents: read")
    if "id-token" in raw:
        raise AssertionError("tests workflow must not request OIDC")

    for job_name, job in doc["jobs"].items():
        if "environment" in job:
            raise AssertionError(f"tests job '{job_name}' must not use a protected environment")

    for forbidden in ("npm publish", "action-gh-release", "gh workflow run", "git push"):
        if forbidden in raw:
            raise AssertionError(f"tests workflow must not publish or dispatch: {forbidden}")

    triggers = trigger(doc)
    if "workflow_dispatch" not in triggers:
        raise AssertionError("tests workflow needs workflow_dispatch for the opt-in network smoke")
    inputs = triggers["workflow_dispatch"].get("inputs", {}) or {}
    if "network_smoke" not in inputs:
        raise AssertionError("workflow_dispatch must expose the opt-in network_smoke input")

    pull_request = triggers.get("pull_request")
    if not pull_request or "paths" not in pull_request:
        raise AssertionError("pull_request trigger must be limited to relevant paths")
    for required in ("scripts/cargo-audit-release-gate.sh", "scripts/tests/audit-release-gate/**"):
        if required not in pull_request["paths"]:
            raise AssertionError(f"pull_request paths must include {required}")

    if "unittest discover" not in raw:
        raise AssertionError("tests workflow must run the deterministic offline suite")

    smoke = doc["jobs"].get("network-smoke")
    if smoke is None:
        raise AssertionError("tests workflow is missing the opt-in network smoke job")
    condition = str(smoke.get("if", ""))
    if "workflow_dispatch" not in condition or "inputs.network_smoke" not in condition:
        raise AssertionError("network smoke job must be gated behind workflow_dispatch and inputs.network_smoke")


class WorkflowStructureTests(unittest.TestCase):
    def test_publish_workflow(self):
        assert_publish_workflow(load(PUBLISH))

    def test_release_workflow(self):
        assert_release_workflow(load(RELEASE))

    def test_build_workflow(self):
        assert_build_workflow(load(BUILD))

    def test_tests_workflow(self):
        assert_tests_workflow(load(TESTS), TESTS.read_text())


class WorkflowMutationTests(unittest.TestCase):
    def mutated(self, path, mutate):
        doc = load(path)
        mutate(doc)
        return doc

    def assert_rejected(self, checker, doc, message):
        with self.assertRaises(AssertionError, msg=message):
            checker(doc)

    def test_disabled_or_error_masked_offline_tests_are_rejected(self):
        raw = TESTS.read_text()
        for mutation in ("job-error-mask", "step-error-mask", "conditional-skip"):
            with self.subTest(mutation=mutation):
                doc = load(TESTS)
                offline = doc["jobs"]["offline"]
                if mutation == "job-error-mask":
                    offline["continue-on-error"] = True
                elif mutation == "step-error-mask":
                    offline["steps"][-1]["continue-on-error"] = True
                else:
                    offline["if"] = False
                with self.assertRaises(AssertionError):
                    assert_tests_workflow(doc, raw)

    def test_removing_gate_is_rejected(self):
        def mutate(doc):
            steps = doc["jobs"]["publish"]["steps"]
            steps[:] = [step for step in steps if GATE_CALL not in step_run(step)]

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "gate removal must be rejected"
        )

    def test_reordering_gate_after_publish_is_rejected(self):
        def mutate(doc):
            steps = doc["jobs"]["publish"]["steps"]
            gate = next(step for step in steps if GATE_CALL in step_run(step))
            steps.remove(gate)
            steps.append(gate)

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "gate reorder must be rejected"
        )

    def test_continue_on_error_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["publish"]["steps"]:
                if GATE_CALL in step_run(step):
                    step["continue-on-error"] = True

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "continue-on-error must be rejected"
        )

    def test_always_condition_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["publish"]["steps"]:
                if "npm publish" in step_run(step):
                    step["if"] = "always()"

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "always() must be rejected"
        )

    def test_unpinned_source_ref_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["publish"]["steps"]:
                if str(step.get("uses", "")).startswith("actions/checkout@"):
                    step["with"]["ref"] = "main"

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "floating source ref must be rejected"
        )

    def test_missing_locked_flag_is_rejected(self):
        def mutate(doc):
            for job in doc["jobs"].values():
                for step in job.get("steps", []):
                    if "cargo build --release -p datadog-serverless-compat" in step_run(step):
                        step["run"] = step_run(step).replace(" --locked", "")

        self.assert_rejected(
            assert_build_workflow, self.mutated(BUILD, mutate), "unlocked build must be rejected"
        )

    def test_missing_producer_source_ref_is_rejected(self):
        def mutate(doc):
            removed = False
            for job in doc["jobs"].values():
                for step in job.get("steps", []):
                    if str(step.get("uses", "")).startswith("actions/checkout@"):
                        step.get("with", {}).pop("ref", None)
                        removed = True
            self.assertTrue(removed)

        self.assert_rejected(
            assert_build_workflow, self.mutated(BUILD, mutate), "unpinned producer checkout must be rejected"
        )

    def test_release_gate_after_draft_is_rejected(self):
        def mutate(doc):
            steps = doc["jobs"]["release"]["steps"]
            gate = next(step for step in steps if GATE_CALL in step_run(step))
            steps.remove(gate)
            steps.append(gate)

        self.assert_rejected(
            assert_release_workflow, self.mutated(RELEASE, mutate), "late release gate must be rejected"
        )

    def test_non_draft_release_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["release"]["steps"]:
                if str(step.get("uses", "")).startswith("softprops/action-gh-release@"):
                    step["with"]["draft"] = False

        self.assert_rejected(
            assert_release_workflow, self.mutated(RELEASE, mutate), "draft change must be rejected"
        )

    def test_tests_workflow_oidc_is_rejected(self):
        doc = load(TESTS)
        doc["permissions"] = {"contents": "read", "id-token": "write"}
        raw = TESTS.read_text() + "\n# id-token: write\n"
        self.assert_rejected(
            lambda candidate: assert_tests_workflow(candidate, raw), doc, "OIDC must be rejected"
        )

    # --- gate command masking ---------------------------------------------

    def mutate_gate_run(self, doc, replacement):
        for step in doc["jobs"]["publish"]["steps"]:
            if GATE_CALL in step_run(step):
                step["run"] = replacement(step_run(step))
                return
        raise AssertionError("gate step not found")

    def test_gate_or_true_masking_is_rejected(self):
        self.assert_rejected(
            assert_publish_workflow,
            self.mutated(PUBLISH, lambda doc: self.mutate_gate_run(doc, lambda run: run + " || true")),
            "|| true masking must be rejected",
        )

    def test_gate_exit_zero_masking_is_rejected(self):
        self.assert_rejected(
            assert_publish_workflow,
            self.mutated(PUBLISH, lambda doc: self.mutate_gate_run(doc, lambda run: run + "; exit 0")),
            "; exit 0 masking must be rejected",
        )

    def test_gate_set_plus_e_masking_is_rejected(self):
        self.assert_rejected(
            assert_publish_workflow,
            self.mutated(PUBLISH, lambda doc: self.mutate_gate_run(doc, lambda run: "set +e\n" + run)),
            "set +e masking must be rejected",
        )

    def test_gate_missing_expected_sha_is_rejected(self):
        self.assert_rejected(
            assert_publish_workflow,
            self.mutated(
                PUBLISH,
                lambda doc: self.mutate_gate_run(doc, lambda run: "./scripts/cargo-audit-release-gate.sh"),
            ),
            "unbound gate command must be rejected",
        )

    def test_gate_conditional_false_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["publish"]["steps"]:
                if GATE_CALL in step_run(step):
                    step["if"] = False

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "conditional gate must be rejected"
        )

    def test_gate_condition_expression_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["publish"]["steps"]:
                if GATE_CALL in step_run(step):
                    step["if"] = "${{ false }}"

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "gate expression must be rejected"
        )

    def test_gate_shell_change_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["publish"]["steps"]:
                if GATE_CALL in step_run(step):
                    step["shell"] = "sh"

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "non-bash gate shell must be rejected"
        )

    def test_gate_working_directory_change_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["publish"]["steps"]:
                if GATE_CALL in step_run(step):
                    step["working-directory"] = "."

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "wrong gate directory must be rejected"
        )

    # --- job-level and artifact boundaries --------------------

    def test_publish_job_continue_on_error_is_rejected(self):
        def mutate(doc):
            doc["jobs"]["publish"]["continue-on-error"] = True

        self.assert_rejected(
            assert_publish_workflow,
            self.mutated(PUBLISH, mutate),
            "job-level continue-on-error must be rejected",
        )

    def test_publish_job_build_only_condition_removed_is_rejected(self):
        def mutate(doc):
            doc["jobs"]["publish"].pop("if", None)

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "missing build-only condition must be rejected"
        )

    def test_publish_job_build_only_condition_replaced_is_rejected(self):
        def mutate(doc):
            doc["jobs"]["publish"]["if"] = "always()"

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "always() build condition must be rejected"
        )

    def test_publisher_cancelled_condition_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["publish"]["steps"]:
                if "npm publish" in step_run(step):
                    step["if"] = "cancelled()"

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "cancelled() masking must be rejected"
        )

    def test_npm_publish_condition_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["publish"]["steps"]:
                if "npm publish" in step_run(step):
                    step["if"] = "${{ always() }}"

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "conditional publish must be rejected"
        )

    def test_external_artifact_selectors_are_rejected(self):
        for selector in EXTERNAL_ARTIFACT_SELECTORS:
            with self.subTest(selector=selector):
                def mutate(doc, selector=selector):
                    for step in doc["jobs"]["publish"]["steps"]:
                        if str(step.get("uses", "")).startswith(DOWNLOAD_ARTIFACT_PREFIX):
                            step.setdefault("with", {})[selector] = "from-elsewhere"
                            return
                    raise AssertionError("no download-artifact step")

                self.assert_rejected(
                    assert_publish_workflow,
                    self.mutated(PUBLISH, mutate),
                    f"artifact selector {selector} must be rejected",
                )

    def test_release_external_artifact_selector_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["release"]["steps"]:
                if str(step.get("uses", "")).startswith(DOWNLOAD_ARTIFACT_PREFIX):
                    step.setdefault("with", {})["run-id"] = "1234"
                    return
            raise AssertionError("no download-artifact step")

        self.assert_rejected(
            assert_release_workflow, self.mutated(RELEASE, mutate), "release artifact selector must be rejected"
        )

    def test_package_checkout_ref_change_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["package"]["steps"]:
                if str(step.get("uses", "")).startswith("actions/checkout@"):
                    step["with"]["ref"] = "main"

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "floating package checkout must be rejected"
        )

    def test_package_artifact_name_change_is_rejected(self):
        def mutate(doc):
            for step in doc["jobs"]["package"]["steps"]:
                if str(step.get("uses", "")).startswith("actions/upload-artifact@"):
                    step["with"]["name"] = "other-packages"

        self.assert_rejected(
            assert_publish_workflow, self.mutated(PUBLISH, mutate), "renamed package artifact must be rejected"
        )

    def test_release_needs_change_is_rejected(self):
        def mutate(doc):
            doc["jobs"]["release"]["needs"] = None

        self.assert_rejected(
            assert_release_workflow, self.mutated(RELEASE, mutate), "release dependency change must be rejected"
        )

    def test_release_extra_environment_is_rejected(self):
        def mutate(doc):
            doc["jobs"]["release"]["environment"] = "Production"

        self.assert_rejected(
            assert_release_workflow, self.mutated(RELEASE, mutate), "extra release approval must be rejected"
        )


if __name__ == "__main__":
    unittest.main()
