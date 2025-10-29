# Datadog Agent Native - Python Bindings

Python wrapper for the Datadog Agent Native library, providing a native Rust-based agent for serverless and containerized environments.

## Installation

```bash
pip install datadog-agent-native
```

## Usage

### Basic Usage

```python
from datadog_agent_native import NativeAgent, DatadogAgentConfig, LogLevel

# Create configuration
config = DatadogAgentConfig(
    api_key="your_api_key_here",
    service="my-service",
    environment="production",
    version="1.0.0",
    log_level=LogLevel.INFO,
    dogstatsd_enabled=True,
)

# Start the agent
with NativeAgent(config) as agent:
    print(f"Agent version: {agent.version}")
    print(f"Trace agent port: {agent.bound_port}")
    print(f"DogStatsD port: {agent.dogstatsd_port}")

    # Your application code here
    # The agent will automatically collect traces and metrics

    # Wait for shutdown signal (optional)
    # reason = agent.wait_for_shutdown()
```

### Singleton Pattern

```python
from datadog_agent_native import create_instance, get_instance

# Create singleton instance
agent = create_instance()

# Later, get the instance from anywhere
agent = get_instance()
```

### Configuration Options

```python
config = DatadogAgentConfig(
    # Required (can also be set via DD_API_KEY environment variable)
    api_key="your_api_key",

    # Unified Service Tagging
    service="my-service",           # or DD_SERVICE
    environment="production",        # or DD_ENV
    version="1.0.0",                # or DD_VERSION

    # Datadog site (default: datadoghq.com)
    site="datadoghq.com",           # or DD_SITE

    # Features
    appsec_enabled=False,           # Enable Application Security
    remote_config_enabled=True,     # Enable Remote Configuration
    dogstatsd_enabled=False,        # Enable DogStatsD UDP server

    # Logging
    log_level=LogLevel.INFO,        # ERROR, WARN, INFO, DEBUG, TRACE

    # Operational mode
    operational_mode=OperationalMode.HTTP_EPHEMERAL_PORT,

    # Ports (None = default, 0 = ephemeral/OS-assigned, >0 = specific port)
    trace_agent_port=0,             # Trace agent port
    dogstatsd_port=0,               # DogStatsD UDP port

    # Unix Domain Socket permissions (Unix only)
    trace_agent_uds_permissions=0o600,  # Default: owner read/write only
)
```

## Operational Modes

The agent supports three operational modes:

1. **HTTP_FIXED_PORT** (0): Traditional mode with fixed HTTP ports (default 8126)
2. **HTTP_EPHEMERAL_PORT** (1): HTTP server with OS-assigned ports (recommended)
3. **HTTP_UDS** (2): Unix Domain Socket mode (Unix only, most secure for single-process)

## Log Levels

- `LogLevel.ERROR` (0)
- `LogLevel.WARN` (2)
- `LogLevel.INFO` (3)
- `LogLevel.DEBUG` (4)
- `LogLevel.TRACE` (5)

## Error Handling

```python
from datadog_agent_native import NativeAgent, DatadogAgentConfig, DatadogAgentException

try:
    agent = NativeAgent(config)
except DatadogAgentException as e:
    print(f"Failed to start agent: {e}")
```

## Requirements

- Python 3.9+
- The package bundles a prebuilt native library per platform. If you are
  working from source or need to override the bundled artifact, set the
  `DATADOG_AGENT_NATIVE_LIBRARY_PATH` environment variable to the absolute path
  of the compiled shared library.

## Packaging and Distribution

The repository includes a helper script that builds the Rust library, copies it
into `datadog_agent_native/bin/<platform>-<arch>/`, and creates a Python wheel
that pip can consume directly.

```bash
cd crates/datadog-agent-native/python
python -m pip install --upgrade build
python scripts/build_wheel.py --platform darwin --arch universal2
```

Key flags:

- `--target`: cross-compile the Rust crate for a specific target triple.
- `--platform` / `--arch`: control the destination folder (default: detected
  host).
- `--platform-dir`: override the exact folder name under `bin/` (useful for
  universal binaries).
- `--binary-path`: skip cargo and copy an existing `.dylib/.so/.dll`.

Run the script per target platform to produce platform-specific wheels. After
copying the artifact, the script invokes `python -m build`, leaving the wheel
and sdist in `python/dist/` ready for publication to PyPI or an internal index.

## License

Unless explicitly stated otherwise all files in this repository are licensed under the Apache 2 License.
This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2017 Datadog, Inc.
