# Datadog Agent Native

[![Rust](https://img.shields.io/badge/rust-1.70%2B-blue.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

A high-performance, native Rust implementation of the Datadog Agent designed for embedded deployment in serverless, containerized, and edge computing environments.

## Overview

The **Datadog Agent Native** crate contains the core services (trace/log/metric collectors, remote configuration client, etc.) that power Datadog’s embedded/serverless agents. It is designed to be embedded inside a larger runtime (AWS Lambda extension, custom bootstrap, language shim) rather than being the standalone binary you ship to customers.

Out of the box it covers the major building blocks you need to construct an agent binary: configuration loading, async orchestration, telemetry pipelines, and optional AppSec/WAF support. You decide how to package it—either by linking it directly into your runtime (via the Rust API) or exposing the provided C ABI.

### Key Features

- **🚀 High Performance**: Written in Rust for maximum performance and minimal overhead
- **📊 Multiple Telemetry Pipelines**: Traces (APM), Logs, Metrics (DogStatsD), and optional Application Security (WAF)
- **🔌 Multiple Integration Modes**: HTTP servers (fixed/ephemeral ports) or Unix Domain Sockets
- **🔒 Security First**: Optional FIPS 140-2 compliant cryptography, AppSec/WAF integration (feature flag)
- **🎛️ Dynamic Configuration**: Support for Datadog Remote Configuration
- **🌐 Language Bindings**: C, C#, Python, Node.js, and other language FFI support
- **♻️ Production Ready**: Automatic retries, exponential backoff, panic safety, comprehensive error handling
- **📦 Zero External Dependencies**: Self-contained binary with no runtime dependencies

## Architecture

```
┌────────────────────────────────────────────────────────────────────────────┐
│                          Datadog Agent Native                              │
│                                                                            │
│  ┌────────────────────────────────────────────────────────────────────┐    │
│  │                        Agent Coordinator                           │    │
│  │  (Lifecycle Management, Component Orchestration, Shutdown Control) │    │
│  └────────────┬─────────────────────────────────────────┬─────────────┘    │
│               │                                         │                  │
│               │            Event Bus (MPSC)             │                  │
│               │     (Inter-component Communication)     │                  │
│  ┌────────────▼─────────────┬────────────┬──────────────▼─────────┐        │
│  │                          │            │                        │        │
│  │   ┌──────────────┐       │            │      ┌──────────────┐  │        │
│  │   │Trace Agent   │       │            │      │ Logs Agent   │  │        │
│  │   │ (HTTP/UDS)   │       │            │      │  (HTTP/UDS)  │  │        │
│  │   └──────┬───────┘       │            │      └──────┬───────┘  │        │
│  │          │               │            │             │          │        │
│  │   ┌──────▼────────┐      │            │      ┌──────▼────────┐ │        │
│  │   │ Trace         │      │ DogStatsD  │      │ Log           │ │        │
│  │   │ Processor     │◄─────┤  Adapter   │      │ Processor     │ │        │
│  │   └──────┬────────┘      │            │      └──────┬────────┘ │        │
│  │          │               │            │             │          │        │
│  │   ┌──────▼────────┐      │            │      ┌──────▼────────┐ │        │
│  │   │ Trace         │      │            │      │ Log           │ │        │
│  │   │ Aggregator    │      │            │      │ Aggregator    │ │        │
│  │   └──────┬────────┘      │            │      └──────┬────────┘ │        │
│  │          │               │            │             │          │        │
│  │   ┌──────▼────────┐      │            │      ┌──────▼────────┐ │        │
│  │   │Stats Generator│      │            │      │ Log Batcher   │ │        │
│  │   └──────┬────────┘      │            │      └──────┬────────┘ │        │
│  │          │               │            │             │          │        │
│  │   ┌──────▼────────┐      │            │      ┌──────▼────────┐ │        │
│  │   │ Trace Flusher │      │            │      │ Log Flusher   │ │        │
│  │   │ (Backend API) │      │            │      │ (Backend API) │ │        │
│  │   └───────────────┘      │            │      └───────────────┘ │        │
│  │                          │            │                        │        │
│  └──────────────────────────┴────────────┴────────────────────────┘        │
│                                                                            │
│  ┌─────────────────────┐  ┌──────────────┐  ┌─────────────────────────┐    │
│  │  AppSec/WAF (opt)   │  │   Remote     │  │  Enhanced Metrics       │    │
│  │  (libddwaf feature) │  │   Config     │  │  (Usage Tracking)       │    │
│  └─────────────────────┘  └──────────────┘  └─────────────────────────┘    │
│                                                                            │
│  ┌──────────────────────────────────────────────────────────────────┐      │
│  │                     Configuration System                         │      │
│  │              (Environment Variables, Defaults)                   │      │
│  └──────────────────────────────────────────────────────────────────┘      │
│                                                                            │
│  ┌──────────────────────────────────────────────────────────────────┐      │
│  │                     FFI Layer (C Bindings)                       │      │
│  │     (C, C#, Python, JavaScript, Go language interoperability)    │      │
│  └──────────────────────────────────────────────────────────────────┘      │
└────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
                        ┌──────────────────────────┐
                        │  Datadog Backend API     │
                        │  (Traces, Logs, Metrics) │
                        └──────────────────────────┘
```

### Component Overview

- **Agent Coordinator** – Boots every subsystem, shares configuration/state, and drives graceful shutdown when signals arrive.
- **Event Bus** – A lightweight MPSC queue that lets producers emit health or flush events without hard coupling between services.
- **Trace Agent Stack** – HTTP/UDS front ends feed the trace processor, aggregator, stats generator, and flusher so spans are enriched, batched, and retried safely.
- **Logs Agent Stack** – Mirrors the trace pipeline for logs: ingestion handlers, processors, batchers, and flushers targeting the Datadog logs intake.
- **DogStatsD Adapter** – UDP/UDS listener plus aggregator that accepts StatsD metrics from the workload and forwards them with the same retry/backoff logic.
- **AppSec/WAF (optional)** – When the `appsec` Cargo feature is enabled, libddwaf inspects traces for threats before they leave the host; otherwise it is entirely compiled out.
- **Remote Config** – Uptane/TUF client and sled-backed cache that keeps live configuration bundles synced with Datadog and pushes rule changes to other components.
- **Enhanced Metrics** – Collects host/serverless usage signals (invocations, cold starts, throttles) for billing insights and alerting.
- **Configuration System** – Figment-based loader that merges defaults, YAML (tests), and `DD_*` environment variables into a single strongly typed `Config`.
- **FFI Layer** – Panic-safe C ABI so embedders in C/C#/Python/Node/etc. can spin up the same agent logic inside their runtime.

## Module Structure

This crate is organized into specialized modules for different observability domains:

| Module | Purpose |
|--------|---------|
| `agent/` | Agent coordinator and lifecycle management |
| `traces/` | APM trace collection, aggregation, and forwarding |
| `logs/` | Log ingestion, processing, and batching |
| `metrics/` | DogStatsD and enhanced metrics collection |
| `appsec/` | Application Security (WAF) integration |
| `remote_config/` | Remote configuration client |
| `config/` | Configuration management (environment variables) |
| `ffi/` | C-compatible FFI bindings for language interoperability |
| `http/` | HTTP server and client utilities |
| `event_bus/` | Inter-component communication |
| `dogstatsd_adapter/` | DogStatsD integration adapter |
| `logger/` | Logging and tracing infrastructure |
| `fips/` | FIPS 140-2 cryptography support |
| `tags/` | Unified service tagging |
| `proc/` | Process utilities (hostname, clock, system info) |

## Cargo Features

- `appsec` *(off by default)* – pulls in libddwaf and the AppSec pipeline. Enable with `cargo build --features appsec` when you need in-process WAF scanning.
- `fips` *(on by default in the workspace)* – uses the Datadog FIPS-compliant TLS adapter for reqwest/hyper stacks. Disable via `--no-default-features --features "appsec"` if you need a non-FIPS build.

## Installation

### As a Rust Dependency

Add to your `Cargo.toml`:

```toml
[dependencies]
datadog-agent-native = { git = "https://github.com/DataDog/serverless-components", branch = "main" }
tokio = { version = "1.47", features = ["full"] }
```

### As a C Library

Build the native library:

```bash
cargo build --release
```

This produces:
- `target/release/libdatadog_agent_native.so` (Linux)
- `target/release/libdatadog_agent_native.dylib` (macOS)
- `target/release/datadog_agent_native.dll` (Windows)
- `target/release/datadog_agent_native.h` (C header)

## Usage Examples

### Rust API

#### Basic Usage

```rust
use datadog_agent_native::agent::AgentCoordinator;
use datadog_agent_native::config::Config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration from environment variables (DD_API_KEY, DD_SITE, etc.)
    let config = Config::default();

    // Create and start the agent
    let mut coordinator = AgentCoordinator::new(config)?;
    coordinator.start().await?;

    println!("Agent started successfully!");

    // Wait for shutdown signal (Ctrl+C or SIGTERM)
    coordinator.wait_for_shutdown().await;

    // Graceful shutdown
    coordinator.shutdown().await?;

    Ok(())
}
```

#### Environment Variable Configuration

```rust
use datadog_agent_native::config::Config;
use std::env;

// Set environment variables
env::set_var("DD_API_KEY", "your-api-key");
env::set_var("DD_SITE", "datadoghq.com");
env::set_var("DD_SERVICE", "my-service");
env::set_var("DD_ENV", "production");
env::set_var("DD_VERSION", "1.0.0");
env::set_var("DD_TRACE_AGENT_PORT", "8126");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configuration is automatically loaded from environment
    let config = Config::default();

    let mut coordinator = AgentCoordinator::new(config)?;
    coordinator.start().await?;
    coordinator.wait_for_shutdown().await;
    coordinator.shutdown().await?;

    Ok(())
}
```

### C API (FFI)

#### Basic Usage

```c
#include "datadog_agent_native.h"
#include <stdio.h>
#include <stdlib.h>

int main() {
    // Configure the agent
    DatadogAgentOptions options = {
        .api_key = "your-api-key-here",
        .site = "datadoghq.com",
        .service = "my-c-service",
        .env = "production",
        .version = "1.0.0",
        .appsec_enabled = 0,
        .remote_config_enabled = 1,
        .log_level = 3,  // Info
        .operational_mode = 0,  // HttpFixedPort
        .trace_agent_port = 8126,
        .dogstatsd_enabled = 0,
        .dogstatsd_port = 0,
        .trace_agent_uds_permissions = -1,
    };

    // Start the agent
    DatadogAgentStartResult result = datadog_agent_start(options);
    if (result.error != DATADOG_OK || result.agent == NULL) {
        fprintf(stderr, "Failed to start Datadog agent: %d\n", result.error);
        return 1;
    }

    printf("Agent started successfully!\n");
    printf("Version: %s\n", result.version);
    printf("Bound port: %d\n", result.bound_port);

    // Wait for shutdown signal
    int shutdown_reason = datadog_agent_wait_for_shutdown(result.agent);
    printf("Shutdown reason: %d\n", shutdown_reason);

    // Stop the agent
    datadog_agent_stop(result.agent);

    return 0;
}
```

### Python API

```python
from datadog_agent_native import NativeAgent, DatadogAgentConfig, LogLevel

# Create configuration
config = DatadogAgentConfig(
    api_key="your-api-key",
    site="datadoghq.com",
    service="my-python-app",
    environment="production",
    version="1.0.0",
    log_level=LogLevel.INFO,
    dogstatsd_enabled=True
)

# Start the agent (using context manager)
with NativeAgent(config) as agent:
    print(f"Agent version: {agent.version}")
    print(f"Trace port: {agent.bound_port}")
    print(f"DogStatsD port: {agent.dogstatsd_port}")

    # Your application code here...

    # Wait for shutdown
    agent.wait_for_shutdown()
```

For full Python API documentation, see [`python/README.md`](python/README.md).

### Node.js API

```javascript
const { NativeAgent, DatadogAgentConfig, LogLevel } = require('@datadog/agent-native');

// Create configuration
const config = new DatadogAgentConfig({
  apiKey: 'your-api-key',
  site: 'datadoghq.com',
  service: 'my-node-app',
  environment: 'production',
  version: '1.0.0',
  logLevel: LogLevel.INFO,
  dogstatsdEnabled: true
});

// Start the agent
const agent = new NativeAgent(config);

console.log(`Agent version: ${agent.version}`);
console.log(`Trace port: ${agent.boundPort}`);
console.log(`DogStatsD port: ${agent.dogstatsdPort}`);

// Your application code here...

// Graceful shutdown
process.on('SIGINT', () => {
  agent.stop();
  process.exit(0);
});
```

For full Node.js API documentation, see [`nodejs/README.md`](nodejs/README.md).

## Configuration

### Operational Modes

The agent supports three operational modes:

| Mode | Description | Use Case |
|------|-------------|----------|
| **`HttpFixedPort`** | HTTP servers on fixed TCP ports (default: 8126 for traces) | Traditional deployment, known ports |
| **`HttpEphemeralPort`** | HTTP servers on OS-assigned ephemeral ports | Testing, CI/CD, avoiding port conflicts |
| **`HttpUds`** | HTTP servers on Unix Domain Sockets | Local communication, better security, multi-process |

### Environment Variables

All configuration is provided via environment variables with the `DD_` prefix:

#### Core Settings
- `DD_API_KEY` – Datadog API key (required)
- `DD_SITE` – Datadog site (default: `datadoghq.com`)
- `DD_SERVICE`, `DD_ENV`, `DD_VERSION` – Unified service tagging
- `DD_TAGS` – Additional tags (`key:value,key:value`)
- `DD_FLUSH_TIMEOUT` – Flush interval (seconds)
- `DD_LOG_LEVEL` – `error`, `warn`, `info`, `debug`, `trace`
- `DD_AGENT_MODE` – `http_fixed_port`, `http_ephemeral_port`, `http_uds`

#### Networking
- `DD_PROXY_HTTPS` – HTTPS proxy URL
- `DD_PROXY_NO_PROXY` – comma-separated hosts that bypass the proxy
- `DD_HTTP_PROTOCOL` – force `http1` or allow auto-selection

#### Trace Settings
- `DD_APM_DD_URL` – Custom trace intake URL
- `DD_APM_ADDITIONAL_ENDPOINTS` – JSON map of extra trace endpoints
- `DD_APM_FILTER_TAGS_REQUIRE` / `DD_APM_FILTER_TAGS_REJECT`
- `DD_APM_FILTER_TAGS_REGEX_REQUIRE` / `DD_APM_FILTER_TAGS_REGEX_REJECT`
- `DD_APM_FEATURES` – Comma-separated feature flags
- `DD_TRACE_PROPAGATION_STYLE`, `DD_TRACE_PROPAGATION_STYLE_EXTRACT`
- `DD_TRACE_PROPAGATION_EXTRACT_FIRST`, `DD_TRACE_PROPAGATION_HTTP_BAGGAGE_ENABLED`
- `DD_TRACE_AGENT_PORT` – Override trace HTTP port (`0` for ephemeral)
- `DD_TRACE_AGENT_UDS_PATH` – Custom Unix Domain Socket path
- `DD_TRACE_AGENT_UDS_PERMISSIONS` – Octal permissions for the socket file

#### AppSec Settings
- `DD_APPSEC_ENABLED` – Enable Application Security (default: `false`)
- `DD_APPSEC_RULES` – Path to custom WAF rules file
- `DD_APPSEC_WAF_TIMEOUT` – WAF execution timeout (microseconds)
- `DD_API_SECURITY_ENABLED` – Enable API security scan
- `DD_API_SECURITY_SAMPLE_DELAY` – Delay before sampling (seconds)

#### Remote Config
- `DD_REMOTE_CONFIGURATION_ENABLED` – Enable remote configuration (default: `true`)
- `DD_REMOTE_CONFIGURATION_API_KEY` – Dedicated remote-config API key
- `DD_REMOTE_CONFIGURATION_KEY` – Legacy remote-config key
- `DD_REMOTE_CONFIGURATION_NO_TLS` – Allow HTTP (default: `false`)
- `DD_REMOTE_CONFIGURATION_NO_TLS_VALIDATION` – Skip TLS validation (default: `false`)

#### DogStatsD Settings
- `DD_DOGSTATSD_ENABLED` – Enable DogStatsD UDP server (default: `false`)
- `DD_DOGSTATSD_PORT` – DogStatsD UDP port (`0` for ephemeral)

#### Advanced Settings
- `DD_METRICS_AGENT_PORT`, `DD_LOGS_AGENT_PORT` – Override secondary HTTP ports
- `DD_METRICS_CONFIG_COMPRESSION_LEVEL`, `DD_LOGS_CONFIG_COMPRESSION_LEVEL`
- `DD_LOGS_CONFIG_ADDITIONAL_ENDPOINTS` – JSON list of additional log endpoints
- `DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED`
- `DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL`
- `DD_ENHANCED_METRICS`, `DD_COMPUTE_TRACE_STATS_ON_EXTENSION`

## API Reference

### Rust API

#### `AgentCoordinator`

Main coordinator for agent lifecycle:

```rust
impl AgentCoordinator {
    /// Create a new agent coordinator with the given configuration
    pub fn new(config: Config) -> Result<Self, AgentError>;

    /// Start the agent and all services
    pub async fn start(&mut self) -> Result<(), AgentError>;

    /// Stop the agent gracefully
    pub async fn shutdown(self) -> Result<(), AgentError>;

    /// Wait for shutdown signal (SIGINT/SIGTERM)
    pub async fn wait_for_shutdown(&self);

    /// Get agent handle for accessing services
    pub fn handle(&self) -> &AgentHandle;
}
```

#### `Config`

Configuration structure:

```rust
impl Config {
    /// Create configuration with defaults (loads from environment)
    pub fn default() -> Self;
}
```

### C API (FFI)

#### Error Codes

```c
typedef enum {
    DATADOG_OK = 0,
    DATADOG_NULL_POINTER = 1,
    DATADOG_INVALID_STRING = 2,
    DATADOG_CONFIG_ERROR = 3,
    DATADOG_INIT_ERROR = 4,
    DATADOG_STARTUP_ERROR = 5,
    DATADOG_SHUTDOWN_ERROR = 6,
    DATADOG_RUNTIME_ERROR = 7,
    DATADOG_INVALID_DATA_FORMAT = 8,
    DATADOG_SUBMISSION_ERROR = 9,
    DATADOG_NOT_AVAILABLE = 10,
    DATADOG_UNKNOWN_ERROR = 99,
} DatadogError;
```

#### Core Functions

```c
// Start the agent
DatadogAgentStartResult datadog_agent_start(DatadogAgentOptions options);

// Stop the agent
DatadogError datadog_agent_stop(DatadogAgent* agent);

// Wait for shutdown signal
int datadog_agent_wait_for_shutdown(DatadogAgent* agent);

// Get agent version
const char* datadog_agent_version(void);
```

## Testing

### Run All Tests

```bash
cargo test --workspace
```

### Run Specific Test Suites

```bash
# Unit tests only
cargo test --lib

# Integration tests
cargo test --test '*'

# FFI tests
cargo test --test ffi_operational_modes

# Specific module tests
cargo test --lib traces::
cargo test --lib logs::
```

### Run with Code Coverage

```bash
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --html
```

Open `target/llvm-cov/html/index.html` to view coverage report.

### End-to-End Tests

```bash
# Trace endpoint E2E test
cargo test --test trace_endpoint_e2e

# Logs endpoint E2E test
cargo test --test logs_endpoint_e2e

# Remote config E2E test
cargo test --test remote_config_endpoint_e2e
```

## Building for Production

### Optimized Release Build

```bash
# Standard release build
cargo build --release

# Smaller binary with link-time optimization
RUSTFLAGS="-C link-arg=-s" cargo build --release

# FIPS mode (for compliance)
cargo build --release --features fips
```

### Cross-Compilation

```bash
# Linux x86_64
cargo build --release --target x86_64-unknown-linux-gnu

# Linux ARM64
cargo build --release --target aarch64-unknown-linux-gnu

# macOS x86_64
cargo build --release --target x86_64-apple-darwin

# macOS ARM64 (Apple Silicon)
cargo build --release --target aarch64-apple-darwin

# Windows
cargo build --release --target x86_64-pc-windows-msvc
```

## Resource Limits

- **Trace Buffer**: 50MB total in-memory limit
- **Log Buffer**: 10,000 logs or 10MB per batch
- **Event Bus**: 100 event capacity (configurable)
- **Max Retries**: 3 attempts with exponential backoff

## Security

### FIPS 140-2 Compliance

Enable FIPS mode for cryptographic compliance:

```bash
cargo build --release --features fips
```

This enables:
- FIPS-validated cryptographic modules
- Restricted cipher suites
- FIPS-compliant TLS

### AppSec / WAF

Application Security Monitoring provides:
- SQL injection detection
- XSS (Cross-Site Scripting) protection
- Command injection detection
- Path traversal prevention
- Custom rule support

Enable via environment variable:

```bash
export DD_APPSEC_ENABLED=true
```

### Thread Safety

**IMPORTANT**: The FFI `DatadogAgent` pointer is **NOT thread-safe**. Each pointer must be accessed from only one thread. Use separate agent instances for concurrent threads, or add external synchronization (mutex).

## Troubleshooting

### Enable Debug Logging

```bash
export DD_LOG_LEVEL=debug
export RUST_LOG=datadog_agent_native=debug
```

### Common Issues

#### Port Already in Use

```
Error: Address already in use (port 8126)
```

**Solution**: Use ephemeral ports or UDS mode:

```bash
export DD_OPERATIONAL_MODE=http_ephemeral_port
```

#### Connection Refused

```
Error: Connection refused (Datadog backend)
```

**Solution**: Check API key and site configuration:

```bash
export DD_API_KEY=your-valid-api-key
export DD_SITE=datadoghq.com  # or .eu, .us3, etc.
```

#### Missing API Key

```
Error: API key not configured
```

**Solution**: Set via environment:

```bash
export DD_API_KEY=your-api-key
```

## Examples

See the [`examples/`](examples/) directory for more usage examples:
- AppSec HTTP middleware integration
- Custom metric collection
- Trace enrichment

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

### Development Setup

```bash
# Clone repository
git clone https://github.com/DataDog/serverless-components
cd serverless-components/crates/datadog-agent-native

# Install Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build
cargo build

# Run tests
cargo test

# Format code
cargo fmt

# Lint
cargo clippy -- -D warnings
```

## Documentation

### Generate Documentation

```bash
# Generate and open API docs
cargo doc --open --no-deps

# Generate with private items
cargo doc --document-private-items --no-deps
```

## License

Copyright 2025-Present Datadog, Inc.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
