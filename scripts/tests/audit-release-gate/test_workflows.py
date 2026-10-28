"""Offline structural tests for the release-gate workflows.

These tests parse the real workflow YAML and check that the strict cargo-audit
gate runs unconditionally inside each protected job, before that job's first
publication effect, against the exact source revision that produced the
artifacts. One table of negative mutations proves the checks reject
representative regressions. Expected pins, refs, conditions, and commands are
literals reviewed together with the workflows, never values read from them.

PyYAML is a test-only dependency. YAML 1.1 parses the `on:` key as boolean True,
so `load()` stores the trigger block back under "on".
"""
import shlex
import unittest
from pathlib import Path

import yaml

WORKFLOWS = Path(__file__).resolve().parents[3] / ".github" / "workflows"
PUBLISH = "publish.yml"
RELEASE = "release-datadog-serverless-compat.yml"
BUILD = "build-datadog-serverless-compat.yml"

CHECKOUT = "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683"
RUST_SETUP = "actions-rust-lang/setup-rust-toolchain@9399c7bb15d4c7d47b27263d024f0a4978346ba4"
GH_RELEASE = "softprops/action-gh-release@da05d552573ad5aba039eaac05058a918a7bf631"
PINS = {pin.split("@")[0]: pin for pin in (CHECKOUT, RUST_SETUP, GH_RELEASE)}
SOURCE_REF = "${{ github.sha }}"
GATE_COMMAND = './scripts/cargo-audit-release-gate.sh "$GITHUB_SHA"'
BUILD_ONLY = "${{ inputs.publish == 'Build and Publish' }}"
CROSS_RUN_SELECTORS = {"run-id", "repository", "github-token"}
SHELL_SEPARATORS = {"&&", "||", ";", "|"}


def load(name):
    doc = yaml.safe_load((WORKFLOWS / name).read_text(encoding="utf-8"))
    if True in doc:
        doc["on"] = doc.pop(True)
    return doc


def run_of(step):
    return str(step.get("run", ""))


def action_of(step):
    return str(step.get("uses", "")).split("@")[0]


def runs(text):
    return lambda step: text in run_of(step)


def using(action):
    return lambda step: action_of(step) == action


def first(steps, predicate):
    return next((index for index, step in enumerate(steps) if predicate(step)), None)


def withs(job, action):
    return [step.get("with") or {} for step in job.get("steps", []) if action_of(step) == action]


def shell_commands(run):
    """Yield the argv of each shell command in a run block, skipping comments."""
    for line in run.replace("\\\n", " ").splitlines():
        argv = []
        for token in shlex.split(line, comments=True) + [";"]:
            if token in SHELL_SEPARATORS:
                yield argv
                argv = []
            else:
                argv.append(token)


class WorkflowTests(unittest.TestCase):
    def check_common(self, doc, where):
        for name, job in doc["jobs"].items():
            for item in (job, *job.get("steps", [])):
                self.assertNotIn("continue-on-error", item, f"{where}/{name} ignores errors")
                action = action_of(item)
                if action in PINS:
                    self.assertEqual(item["uses"], PINS[action], f"{where}: {action} pin changed")
                if action == "actions/download-artifact":
                    selectors = CROSS_RUN_SELECTORS & set(item.get("with") or {})
                    self.assertFalse(selectors, f"{where}: cross-run artifact download")

    def check_gated_job(self, job, is_effect, where):
        steps = job["steps"]
        gate = first(steps, runs("cargo-audit-release-gate.sh"))
        self.assertIsNotNone(gate, f"{where}: gate missing")
        allowed = {"name", "run", "working-directory"}
        self.assertLessEqual(set(steps[gate]), allowed, f"{where}: gate must be unconditional and unmasked")
        self.assertEqual(run_of(steps[gate]).strip(), GATE_COMMAND, f"{where}: gate command changed")
        self.assertEqual(steps[gate].get("working-directory"), "source", f"{where}: gate directory")

        checkout = first(steps, using("actions/checkout"))
        rust = first(steps, using("actions-rust-lang/setup-rust-toolchain"))
        self.assertIsNotNone(checkout, f"{where}: source checkout missing")
        self.assertIsNotNone(rust, f"{where}: Rust setup missing")
        source = steps[checkout].get("with") or {}
        self.assertEqual((source.get("ref"), source.get("path")), (SOURCE_REF, "source"), f"{where}: source checkout")
        self.assertIs((steps[rust].get("with") or {}).get("cache"), False, f"{where}: Rust setup cache")
        self.assertLess(max(checkout, rust), gate, f"{where}: checkout and Rust setup must precede the gate")

        effect = first(steps, is_effect)
        self.assertIsNotNone(effect, f"{where}: publication effect missing")
        self.assertLess(gate, effect, f"{where}: gate must precede the first publication effect")
        for later in steps[gate:]:
            self.assertNotIn("if", later, f"{where}: step conditional after the gate")

    def check_publish(self, doc):
        self.check_common(doc, "publish")
        self.assertEqual(doc["permissions"], {"contents": "read", "id-token": "write"}, "publish permissions")
        inputs = doc["on"]["workflow_dispatch"]["inputs"]
        self.assertEqual(set(inputs), {"publish", "version"}, "publish inputs changed")
        self.assertEqual(inputs["publish"]["options"], ["Build", "Build and Publish"], "publish options")

        package, publish = doc["jobs"]["package"], doc["jobs"]["publish"]
        self.assertEqual(publish.get("environment"), "Production", "publish lost Production approval")
        self.assertEqual(publish.get("needs"), "package", "publish must need package")
        self.assertEqual(publish.get("if"), BUILD_ONLY, "publish lost the build-only condition")
        package_refs = [w.get("ref") for w in withs(package, "actions/checkout")]
        self.assertEqual(package_refs, [SOURCE_REF], "package checkout ref")
        self.assertIn("npm-packages", [w.get("name") for w in withs(package, "actions/upload-artifact")])
        self.assertIn("npm-packages", [w.get("name") for w in withs(publish, "actions/download-artifact")])
        self.check_gated_job(publish, runs("npm publish"), "publish")

    def check_release(self, doc):
        self.check_common(doc, "release")
        self.assertEqual(doc["permissions"], {}, "release top-level permissions must stay empty")
        release = doc["jobs"]["release"]
        # Reviewed flow: the strict gate plus a draft release is the release
        # check, so no extra protected environment approval is introduced.
        self.assertNotIn("environment", release, "release gained a protected environment")
        self.assertNotIn("if", release, "release job became conditional")
        self.assertEqual(release.get("needs"), "build", "release must need build")
        self.assertEqual(release.get("permissions"), {"contents": "write"}, "release job permissions")

        def is_effect(step):
            return run_of(step).lstrip().startswith("zip ") or action_of(step) == "softprops/action-gh-release"

        self.check_gated_job(release, is_effect, "release")
        drafts = [w.get("draft") is True for w in withs(release, "softprops/action-gh-release")]
        self.assertEqual(drafts, [True], "release must stay a single draft")

    def check_build(self, doc):
        self.check_common(doc, "builder")
        refs = [w.get("ref") for job in doc["jobs"].values() for w in withs(job, "actions/checkout")]
        self.assertEqual(refs, [SOURCE_REF, SOURCE_REF], "builder checkouts must pin the run commit")
        run_blocks = [run_of(step) for job in doc["jobs"].values() for step in job.get("steps", [])]
        self.assertFalse([run for run in run_blocks if "audit" in run], "builder must not audit")
        builds = [argv for run in run_blocks for argv in shell_commands(run) if argv[:2] == ["cargo", "build"]]
        self.assertEqual(len(builds), 5, f"expected 5 release builds, found {builds}")
        for argv in builds:
            self.assertLessEqual({"--release", "--locked"}, set(argv), f"unlocked build: {argv}")

    def test_real_publish_workflow(self):
        self.check_publish(load(PUBLISH))

    def test_real_release_workflow(self):
        self.check_release(load(RELEASE))

    def test_real_build_workflow(self):
        self.check_build(load(BUILD))

    def test_mutations_are_rejected(self):
        checks = {PUBLISH: self.check_publish, RELEASE: self.check_release, BUILD: self.check_build}
        for context, name, mutate, reason in MUTATIONS:
            with self.subTest(context):
                doc = load(name)
                mutate(doc)  # LookupError or KeyError here means the mutation target moved.
                with self.assertRaisesRegex(AssertionError, reason):
                    checks[name](doc)


# Mutation helpers fail loudly when their target is absent or already holds the
# requested value, so a case cannot pass by mutating nothing.
def step(doc, job, predicate):
    matches = [s for s in doc["jobs"][job]["steps"] if predicate(s)]
    if len(matches) != 1:
        raise LookupError(f"mutation target in job '{job}' matched {len(matches)} steps")
    return matches[0]


def put(target, key, value):
    if target.get(key) == value:
        raise LookupError(f"'{key}' already equals {value!r}")
    target[key] = value


def edit_run(target, old, new):
    if old not in run_of(target):
        raise LookupError(f"'{old}' not found in run block")
    target["run"] = run_of(target).replace(old, new)


def move_to_end(doc, job, predicate):
    steps = doc["jobs"][job]["steps"]
    steps.append(steps.pop(steps.index(step(doc, job, predicate))))


GATE = runs("cargo-audit-release-gate.sh")
BUILDER = "build-datadog-serverless-compat"
MACOS_BUILD = runs("datadog-serverless-compat --locked")

# (context, workflow, mutation, expected rejection reason)
MUTATIONS = [
    ("publish gate removed", PUBLISH,
     lambda d: d["jobs"]["publish"]["steps"].remove(step(d, "publish", GATE)), "gate missing"),
    ("publish gate moved after npm publish", PUBLISH,
     lambda d: move_to_end(d, "publish", GATE), "gate must precede"),
    ("release gate moved after the zip and draft release", RELEASE,
     lambda d: move_to_end(d, "release", GATE), "gate must precede"),
    ("publish gate masked with || true", PUBLISH,
     lambda d: edit_run(step(d, "publish", GATE), '"$GITHUB_SHA"', '"$GITHUB_SHA" || true'), "gate command"),
    ("publish gate skipped with if: false", PUBLISH,
     lambda d: put(step(d, "publish", GATE), "if", False), "unconditional and unmasked"),
    ("release gate ignores its own failure", RELEASE,
     lambda d: put(step(d, "release", GATE), "continue-on-error", True), "ignores errors"),
    ("publish gate no longer bound to the run commit", PUBLISH,
     lambda d: edit_run(step(d, "publish", GATE), ' "$GITHUB_SHA"', ""), "gate command"),
    ("publish source checkout floats to main", PUBLISH,
     lambda d: put(step(d, "publish", using("actions/checkout"))["with"], "ref", "main"), "source checkout"),
    ("builder setup checkout floats to main", BUILD,
     lambda d: put(step(d, "setup", using("actions/checkout"))["with"], "ref", "main"), "builder checkouts"),
    ("publish downloads npm packages from another run", PUBLISH,
     lambda d: put(step(d, "publish", using("actions/download-artifact"))["with"], "run-id", "1234"),
     "cross-run artifact"),
    ("publish loses Production approval", PUBLISH,
     lambda d: d["jobs"]["publish"].pop("environment"), "Production approval"),
    ("publish also runs on Build-only dispatches", PUBLISH,
     lambda d: put(d["jobs"]["publish"], "if", "${{ always() }}"), "build-only condition"),
    ("npm publish step made conditional", PUBLISH,
     lambda d: put(step(d, "publish", runs("npm publish")), "if", "${{ always() }}"), "conditional after the gate"),
    ("draft release becomes public", RELEASE,
     lambda d: put(step(d, "release", using("softprops/action-gh-release"))["with"], "draft", False),
     "single draft"),
    ("i686 backslash-continued build drops --locked", BUILD,
     lambda d: edit_run(step(d, BUILDER, runs("--target i686-pc-windows-msvc")), "--locked", ""),
     "unlocked build"),
    ("macOS build keeps --locked only inside a shell comment", BUILD,
     lambda d: edit_run(step(d, BUILDER, MACOS_BUILD), "--locked", "# --locked"), "unlocked build"),
    ("macOS build commented out", BUILD,
     lambda d: edit_run(step(d, BUILDER, MACOS_BUILD), "cargo", "# cargo"), "expected 5 release builds"),
]


if __name__ == "__main__":
    unittest.main()
