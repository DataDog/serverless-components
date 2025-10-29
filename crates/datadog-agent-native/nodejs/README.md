# Datadog Agent Native - Node.js Bindings

Node.js wrapper for the Datadog Agent Native library, providing a native Rust-based agent for serverless and containerized environments.

## Installation

```bash
npm install @datadog/agent-native
```

## Usage

### Basic Usage

```javascript
const { NativeAgent, DatadogAgentConfig, LogLevel } = require('@datadog/agent-native');

// Create configuration
const config = new DatadogAgentConfig({
  apiKey: 'your_api_key_here',
  service: 'my-service',
  environment: 'production',
  version: '1.0.0',
  logLevel: LogLevel.INFO,
  dogstatsdEnabled: true
});

// Start the agent
const agent = new NativeAgent(config);

console.log(`Agent version: ${agent.version}`);
console.log(`Trace agent port: ${agent.boundPort}`);
console.log(`DogStatsD port: ${agent.dogstatsdPort}`);

// Your application code here
// The agent will automatically collect traces and metrics

// Stop the agent when done
process.on('SIGINT', () => {
  agent.stop();
  process.exit(0);
});
```

### Singleton Pattern

```javascript
const { createInstance, getInstance } = require('@datadog/agent-native');

// Create singleton instance
const agent = createInstance();

// Later, get the instance from anywhere
const agent = getInstance();
```

### Configuration Options

```javascript
const config = new DatadogAgentConfig({
  // Required (can also be set via DD_API_KEY environment variable)
  apiKey: 'your_api_key',

  // Unified Service Tagging
  service: 'my-service',           // or DD_SERVICE
  environment: 'production',        // or DD_ENV
  version: '1.0.0',                // or DD_VERSION

  // Datadog site (default: datadoghq.com)
  site: 'datadoghq.com',           // or DD_SITE

  // Features
  appsecEnabled: false,            // Enable Application Security
  remoteConfigEnabled: true,       // Enable Remote Configuration
  dogstatsdEnabled: false,         // Enable DogStatsD UDP server

  // Logging
  logLevel: LogLevel.INFO,         // ERROR, WARN, INFO, DEBUG, TRACE

  // Operational mode
  operationalMode: OperationalMode.HTTP_EPHEMERAL_PORT,

  // Ports (null = default, 0 = ephemeral/OS-assigned, >0 = specific port)
  traceAgentPort: 0,               // Trace agent port
  dogstatsdPort: 0,                // DogStatsD UDP port

  // Unix Domain Socket permissions (Unix only)
  traceAgentUdsPermissions: 0o600  // Default: owner read/write only
});
```

## Operational Modes

The agent supports three operational modes:

```javascript
const { OperationalMode } = require('@datadog/agent-native');

// Traditional mode with fixed HTTP ports (default 8126)
OperationalMode.HTTP_FIXED_PORT

// HTTP server with OS-assigned ports (recommended)
OperationalMode.HTTP_EPHEMERAL_PORT

// Unix Domain Socket mode (Unix only, most secure for single-process)
OperationalMode.HTTP_UDS
```

## Log Levels

```javascript
const { LogLevel } = require('@datadog/agent-native');

LogLevel.ERROR  // 0
LogLevel.WARN   // 2
LogLevel.INFO   // 3
LogLevel.DEBUG  // 4
LogLevel.TRACE  // 5
```

## Error Handling

```javascript
const { NativeAgent, DatadogAgentException } = require('@datadog/agent-native');

try {
  const agent = new NativeAgent(config);
} catch (error) {
  if (error instanceof DatadogAgentException) {
    console.error(`Failed to start agent: ${error.message}`);
  }
}
```

## Requirements

- Node.js 12+
- The native library (`libdatadog_agent_native.so` / `.dylib` / `.dll`) must be in:
  - The same directory as the Node.js package
  - A parent directory
  - Your system's library path (LD_LIBRARY_PATH / DYLD_LIBRARY_PATH / PATH)

## Dependencies

This package uses FFI (Foreign Function Interface) to communicate with the native library:

- `ffi-napi`: Node.js FFI bindings
- `ref-napi`: Turn Buffer instances into "pointers"
- `ref-struct-di`: Create ABI-compliant struct instances

## License

Unless explicitly stated otherwise all files in this repository are licensed under the Apache 2 License.
This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2017 Datadog, Inc.
