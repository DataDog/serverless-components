# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a Rust workspace containing serverless monitoring components for Datadog, primarily used to instrument AWS Lambda Functions, Azure Functions, and Azure Spring Apps. The main binary `datadog-serverless-compat` combines trace agent and DogStatsD server functionality for serverless environments.

## Common Commands

### Building and Testing
- `cargo build --workspace` - Build all crates
- `cargo build --all` - Alternative build command
- `cargo build -p datadog-serverless-compat` - Build just the main binary
- `cargo test --workspace` - Run all tests
- `cargo nextest run --workspace` - Run tests with nextest (preferred in CI)
- `cargo test -p <crate-name>` - Run tests for a specific crate
- `cargo test -p <crate-name> <test-name>` - Run a specific test

### Code Quality
- `cargo fmt --all -- --check` - Check formatting
- `cargo fmt --all` - Format all code
- `cargo clippy --workspace --all-features` - Run clippy lints (all features)
- `cargo check --workspace` - Quick compile check

### Dependencies and Licensing
- `dd-rust-license-tool check` - Verify license compliance
- Requires installing: `cargo install dd-rust-license-tool`

### Protocol Buffer Setup
- `./scripts/install-protoc.sh $HOME` - Install protobuf compiler (required for build)
- This script installs protoc v28.0 and handles platform detection

## Architecture

The workspace contains four main crates:

### `datadog-serverless-compat` (Binary)
- Main executable combining trace agent and DogStatsD server
- Entry point: `crates/datadog-serverless-compat/src/main.rs`
- Orchestrates both tracing and metrics collection for serverless environments
- Handles environment detection (Lambda, Azure Functions, Azure Spring Apps, Cloud Functions)

### `datadog-trace-agent` (Library)
- Serverless trace processing and aggregation
- Key modules: aggregator, trace_processor, trace_flusher, stats_processor, mini_agent
- Depends on libdatadog components for trace processing

### `dogstatsd` (Library)
- DogStatsD metrics aggregation and flushing
- Beta status - primary purpose is serverless environments
- Uses Saluki for distribution metrics
- Limitations: no UDS support, uses ustr (memory leak prone)
- Features: `default` (rustls-tls), `fips` (FIPS-compliant crypto)

### `datadog-log-agent` (Library)
- Generic log aggregation and flushing to the Datadog Logs API
- Consumers provide `LogEntry` values; crate handles batching, compression, retry
- `LogEntry` has standard Datadog intake fields + `#[serde(flatten)] attributes` for runtime enrichment
- Key types: `AggregatorService`, `AggregatorHandle`, `LogFlusher`, `LogFlusherConfig`
- Features: `FlusherMode::Datadog` (zstd, `/api/v2/logs`) and `FlusherMode::ObservabilityPipelinesWorker`

### `datadog-fips` (Library)
- FIPS-compliant HTTP client utilities
- Provides reqwest adapter for FIPS compliance
- See clippy.toml for enforced usage patterns

## Development Notes

### FIPS Compliance
The project enforces FIPS compliance through clippy rules in `clippy.toml`. Use `datadog_fips::reqwest_adapter::create_reqwest_client_builder` instead of `reqwest::Client::builder`.

### Strict Code Quality
All crates use strict clippy settings that deny panic, unwrap, expect, todo, and unimplemented in non-test code.

### Windows Builds
On Windows, set `AWS_LC_FIPS_SYS_NO_ASM=1` environment variable when building to avoid FIPS assembly issues (datadog-fips crate is not fully supported on Windows).

### Dependencies
- Uses specific libdatadog revision: `4eb2b8673354f974591c61bab3f7d485b4c119e0`
- Uses specific Saluki revision: `c89b58e5784b985819baf11f13f7d35876741222`
- Protocol buffers are required for datadog-trace-agent compilation

### Release Optimization
The workspace uses aggressive release optimizations (opt-level "z", LTO, single codegen unit, stripping) for minimal binary size in serverless environments.

### Environment Variables
Key environment variables for datadog-serverless-compat:
- `DD_API_KEY` - Datadog API key (required for metrics flushing)
- `DD_LOG_LEVEL` - Logging level (default: info)
- `DD_DOGSTATSD_PORT` - DogStatsD port (default: 8125)
- `DD_SITE` - Datadog site (default: datadoghq.com)
- `DD_USE_DOGSTATSD` - Enable/disable DogStatsD (default: true)
- `DD_STATSD_METRIC_NAMESPACE` - Optional metric namespace prefix
- `DD_PROXY_HTTPS`, `HTTPS_PROXY` - HTTPS proxy settings

### `datadog-log-agent` environment variables
- `DD_LOGS_ENABLED` - Enable/disable log agent (default: true)
- `DD_LOGS_CONFIG_USE_COMPRESSION` - Enable zstd compression (default: true)
- `DD_LOGS_CONFIG_COMPRESSION_LEVEL` - zstd compression level (default: 3)
- `DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED` - Use OPW mode (default: false)
- `DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL` - OPW endpoint URL