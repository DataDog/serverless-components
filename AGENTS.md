# AGENTS.md

This file gives guidance to coding agents and review bots that work in this repository.

## Who uses this code

This repository has libraries and binaries for serverless monitoring. Changes here can reach
more than one product:

- `datadog-serverless-compat` is the binary for Azure Functions, Azure Spring Apps and
  Google Cloud Functions. It uses the crates in this repository directly.
- The Datadog Lambda Extension (`DataDog/datadog-lambda-extension`, also called bottlecap)
  pins crates from this repository by Git revision. These include `dogstatsd`,
  `datadog-fips`, `datadog-agent-config` and `datadog-agent-trace-sampler`.
- Much of the trace and metrics logic ports behavior from the Go Datadog Agent
  (`DataDog/datadog-agent`, mostly `pkg/trace`).

## Code style

- Do not add a defensive check unless you can name the input that makes it fail. For example,
  do not check that an index is in range when the function that returns it cannot return an
  index out of range.
- Keep comments concise. Describe the code as it is, not the history of the change.
- Describe a rule in one place. Do not repeat the same rule in several doc comments.
- Before you write a helper, search the repository for one that does the same job.

## Settings

An environment variable is a contract with users, so treat a new one as a design decision.

- Before you add an environment variable, search the Go Agent and the Lambda Extension for one
  that does the same job. Use the same name and the same meaning when you can.
- When this repository and the Lambda Extension configure the same shared crate, use the same
  settings in both. If they must differ, say why in the PR description.
- When a change turns on a new behavior by default, say so in the PR description. Also say how
  a user turns it off.

## Tests

Tests are code that reviewers must read and maintainers must keep. Add a test only when it
checks something that no other test checks.

- Before you add a test for one case, look for a table test that covers it. Add the case to
  that table instead.
- When a pure function makes a decision, test the decision with a table of inputs that calls
  the function directly. Then tests through the full request path need only one or two cases,
  for the part that they add.
- Do not test the behavior of another crate again. For example, the sampler crate has its own
  tests for its rate limits, so a test in the trace agent checks only how the trace agent
  calls the sampler.
- Do not compute the expected result with the same expression as the code under test. Write
  the expected values in the test.
- Do not add a test for a setting that no code reads, or a loop over an input that the code
  does not read.
- Do not repeat the same environment setup in many tests. Put the parsing in a function that
  takes the raw values, test that function with a table, and use one test for the
  environment.

## Pull requests

- Keep PRs small, so that a reviewer can read all of the PR: usually no more than 300 lines of
  core code. Tests, lock files and generated files are not core code.
- If a PR has more than one behavior change, put each one in a separate PR.
- Keep the number of test lines in proportion to the core code. Many more test lines than core
  lines usually means duplicate tests. Before you open the PR, read the tests again for the
  problems in [Tests](#tests).
- Make sure that the PR description agrees with the code. When you change the code, update the
  title and the description.

## Reviewing a pull request

- Read the code before the PR description. Then check that the description agrees with the
  code.
- Report the number of core lines and the number of test lines. If the PR is larger than the
  size in [Pull requests](#pull-requests), suggest how to split it.
- Look for duplicates outside the diff, such as a helper that the repository already has or a
  test that another crate already has.
- For each new setting and each new default, check the rules in [Settings](#settings).
- Mark each finding as important, minor or nit.
