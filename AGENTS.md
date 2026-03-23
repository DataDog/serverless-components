# Repository Guidelines

## Project Structure & Module Organization
- `crates/` hosts all Rust workspaces: `datadog-serverless-compat` (binary entrypoint), `datadog-logs-agent`, `datadog-trace-agent`, `dogstatsd`, and `datadog-fips`. Each crate keeps its own `src/`, `tests/`, and `examples/` when needed.
- `docs/` captures design notes such as `DESIGN-LOG-INTAKE.md`; update diagrams here before referencing them in PRs.
- `scripts/` contains developer helpers like `test-log-intake.sh` for spinning up a capture server; treat these as canonical workflow recipes.
- The `target/` directory is build output and should not be checked in. Temporary assets or captures belong under `tmp/` entries ignored by git.

## Build, Test, and Development Commands
- `cargo build --workspace` — quick validation that every crate compiles together.
- `cargo build -p datadog-serverless-compat --release` — produces the Lambda/Azure bundle with LTO + size optimizations.
- `cargo test --workspace` — runs unit + integration suites; add `-- --ignored` for slow network-reliant cases.
- `cargo fmt --all -- --check` and `cargo clippy --workspace --all-features` — enforce rustfmt/clippy gates that run in CI.
- `./scripts/test-log-intake.sh [--real]` — end-to-end log pipeline test; `--real` pushes to Datadog with `DD_API_KEY` and `DD_SITE`.

## Coding Style & Naming Conventions
- Standard Rust 4-space indentation, snake_case modules, UpperCamelCase types, and SCREAMING_SNAKE_CASE env vars (e.g., `DD_LOGS_ENABLED`).
- Keep files focused: separate AWS/Azure adapters into their own modules under each crate.
- Always build HTTP clients via `datadog_fips::reqwest_adapter::create_reqwest_client_builder` (clippy enforces this) and document env-driven behavior with doc comments.
- Run `cargo fmt` before committing; do not overwrite generated files under `target/`.

## Testing Guidelines
- Prefer `#[tokio::test]` async tests for agents, mirroring production runtimes; integration suites live under `crates/datadog-logs-agent/tests/` and similar per crate.
- Name tests by observable behavior (`flushes_batches_on_interval`, `retries_opw_payload`), and add fixtures under each crate’s `tests/data`.
- Cover batching, retry, and configuration fallbacks; when adding env vars, include a table entry in `README.md` plus a test proving defaulting.
- Validate manual flows with `./scripts/test-log-intake.sh` and capture the command output in the PR when changes touch logging.

## Commit & Pull Request Guidelines
- Follow the existing `<type>(scope): summary` pattern seen in `git log` (`feat(log-agent): derive Clone on LogFlusher`, `fix(log-agent): improve main.rs wiring`). Keep the summary imperative and under ~72 characters.
- Each PR description should include: purpose, testing commands run, affected crates, and links to tracking tickets (SVLS-####) when applicable. Attach screenshots or trace dumps if behavior changes are observable.
- Squash noisy fixups before requesting review, ensure CI (fmt, clippy, tests) is green, and mention any manual steps reviewers must perform (e.g., setting `DD_API_KEY`).

## Security & Configuration Tips
- Never commit secrets; rely on env vars (`DD_API_KEY`, `DD_SITE`, `DD_LOGS_ENABLED`, `DD_PROXY_HTTPS`) and document defaults in README tables.
- When adding outbound HTTP integrations, reuse the shared `datadog-fips` client so TLS remains FIPS-compliant. Log configuration errors at `warn` level but avoid leaking keys.
