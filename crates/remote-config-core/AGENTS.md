# Repository Guidelines

## Project Structure & Module Organization
The crate entry point is `src/lib.rs`, which wires bootstrap, CDN, HTTP, telemetry, and store modules. Service-facing workflows live in `src/service/` (e.g., `client.rs`, `refresh.rs`, `websocket.rs`). Trusted metadata snapshots ship in `src/embedded_roots/data/`, while typed helpers for RC keys, status, and Uptane logic sit alongside them in `src/*.rs`. High-level integration tests live under `tests/`, with reusable fixtures in `tests/common/` and an end-to-end remote config scenario in `tests/e2e_remote_config.rs`.

## Build, Test, and Development Commands
- `cargo fmt --all` — runs rustfmt; required before every PR.
- `cargo clippy --all-targets --all-features -D warnings` — linting aligned with CI expectations.
- `cargo check --all-targets` — fast validation during iterative work.
- `cargo test -p remote-config-core --all-features` — executes unit + integration suites, including async flows.
- `cargo test -- --ignored` — triggers any slow ignored cases when needed.
- `cargo doc -p remote-config-core --open` — verifies public API docs when changing exposed types.

## Coding Style & Naming Conventions
Follow Rust 2021 defaults: 4-space indents, `snake_case` modules/functions, `CamelCase` types, and `SCREAMING_SNAKE_CASE` consts. Keep modules cohesive; if a file grows beyond one concern (e.g., service state vs. transport), split it under `src/service/`. Prefer `tracing` spans for observability instead of ad-hoc logging, and propagate errors with the crate’s `thiserror` enums rather than `unwrap`/`expect`.

## Documentation Guidelines
1. **File headers**: Every Rust source file starts with a module-level doc comment summarizing the file, its responsibilities, and key dependencies.
2. **Function docs**: Each function—public or internal—must include a Rustdoc block explaining its behavior, parameters, notes, caveats, and expected errors.
3. **Branch annotations**: Within a function body, document significant branching (`match`, `if`, `loop`) to describe which scenario each arm handles (e.g., cache hits vs. misses, organization scoping differences).

## Testing Guidelines
Every net-new line of code must ship with a unit test; add it next to the implementation or inside `tests/` if it crosses module boundaries. Cover new logic with focused unit tests colocated with the module plus integration scenarios in `tests/`. Structure fixtures within `tests/common/fixtures.rs` and expose helpers via `tests/common/mod.rs`. Name tests after the behavior under test (`refresh_stream_retries_on_backoff`). When changing async flows, add a `#[tokio::test(flavor = "multi_thread")]` case. Aim for parity with existing coverage—critical paths touching `store` or `uptane` must include both happy-path and corruption-handling assertions.
Always run the full `cargo test -p remote-config-core --all-features` suite after modifying or adding code to ensure transformations don’t break unrelated paths.
Because of sandboxing please change the rust `target` folder to this crate directory (but keep the `target` folder name because we are git ignoring it, for example using 'CARGO_TARGET_DIR=$(pwd)/target') by using the environment variable in each cargo command.

## Commit & Pull Request Guidelines
Git history favors short imperative subjects (`update readme.md`, `fixes`). Follow the same style, limit to ~72 characters, and add a body only when context is not obvious. Each PR should summarize the change, call out risk areas (store, transport, telemetry), link to any tracking issue, and paste the latest `cargo test …` output. Provide screenshots or logs whenever modifying wire protocols or telemetry payloads so reviewers can confirm external impacts.
