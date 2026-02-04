# Node.js Bridge for Datadog Serverless Components

**Date:** 2026-02-03
**Status:** Draft Design

## Overview

This design adds Node.js bindings to the Datadog serverless monitoring components, allowing JavaScript applications to programmatically start and stop the trace agent and DogStatsD services.

### Goals

1. Enable Node.js applications to embed Datadog serverless monitoring services
2. Provide JavaScript API for starting/stopping services with configuration
3. Support async/await patterns with status callbacks
4. Ensure graceful shutdown when Node.js process exits
5. Maintain backward compatibility with existing standalone binary

### Non-Goals

- Browser-based control (Node.js only)
- Runtime reconfiguration (config is set at start time)
- Multi-language FFI support (Node.js-specific via NAPI)

## Architecture

### Crate Structure

We'll restructure the workspace into three crates:

```
serverless-components/
├── crates/
│   ├── datadog-serverless-core/      # New: Core library
│   ├── datadog-serverless-compat/    # Modified: CLI wrapper
│   ├── datadog-serverless-node/      # New: NAPI addon
│   ├── datadog-trace-agent/
│   ├── dogstatsd/
│   └── datadog-fips/
```

#### 1. `datadog-serverless-core` (Library)

**Purpose:** Reusable core library containing all service lifecycle logic.

**Key Components:**
- `ServerlessServices` - Main struct managing both services
- `ServicesConfig` - Configuration for trace agent and DogStatsD
- `ServicesHandle` - Handle for stopping services and receiving status updates
- Tokio runtime management
- Service lifecycle (start, stop, flush)

**Dependencies:**
- `datadog-trace-agent`
- `dogstatsd`
- `tokio`
- No Node.js-specific dependencies

**API Surface:**
```rust
pub struct ServerlessServices {
    // Internal state
}

pub struct ServicesConfig {
    pub api_key: Option<String>,
    pub site: String,
    pub dogstatsd_port: u16,
    pub use_dogstatsd: bool,
    pub metric_namespace: Option<String>,
    pub log_level: String,
    pub https_proxy: Option<String>,
    // Additional config fields...
}

pub struct ServicesHandle {
    // Internal channels and state
}

impl ServerlessServices {
    /// Create new services with configuration
    pub fn new(config: ServicesConfig) -> Result<Self, ServicesError>;

    /// Start services asynchronously
    pub async fn start(self) -> Result<ServicesHandle, ServicesError>;
}

impl ServicesHandle {
    /// Stop services gracefully
    pub async fn stop(self) -> Result<(), ServicesError>;

    /// Check if services are running
    pub fn is_running(&self) -> bool;

    /// Get status updates channel
    pub fn status_receiver(&self) -> tokio::sync::mpsc::Receiver<ServiceStatus>;
}

pub enum ServiceStatus {
    TraceAgentReady,
    DogStatsDReady,
    Error(String),
    Stopped,
}
```

#### 2. `datadog-serverless-node` (NAPI Addon)

**Purpose:** Node.js native addon exposing JavaScript API.

**Technology:** napi-rs (https://napi.rs/)

**Key Components:**
- `DatadogServices` class (JavaScript-facing)
- Type conversions between JavaScript and Rust
- Async runtime bridge (Tokio ↔ Node.js event loop)
- Cleanup hooks for process exit

**JavaScript API:**
```typescript
interface ServicesConfig {
  apiKey?: string;
  site?: string;
  dogstatsdPort?: number;
  useDogstatsd?: boolean;
  metricNamespace?: string;
  logLevel?: string;
  httpsProxy?: string;
}

type StatusCallback = (status: ServiceStatus) => void;

interface ServiceStatus {
  type: 'ready' | 'error' | 'stopped';
  service?: 'trace-agent' | 'dogstatsd';
  message?: string;
}

class DatadogServices {
  /**
   * Start Datadog serverless services
   * @param config Service configuration
   * @param statusCallback Optional callback for status updates
   * @returns Promise that resolves when services are ready
   */
  async start(config: ServicesConfig, statusCallback?: StatusCallback): Promise<void>;

  /**
   * Stop services gracefully
   * @returns Promise that resolves when services are stopped
   */
  async stop(): Promise<void>;

  /**
   * Check if services are currently running
   */
  isRunning(): boolean;
}

export const services: DatadogServices;
```

**Dependencies:**
- `napi` and `napi-derive` from napi-rs
- `datadog-serverless-core`
- `tokio` (for async bridge)

#### 3. `datadog-serverless-compat` (Modified Binary)

**Purpose:** Standalone CLI wrapper (backward compatible).

**Changes:**
- Import `datadog-serverless-core`
- Read environment variables into `ServicesConfig`
- Call `ServerlessServices::new(config).start().await`
- Maintain current behavior exactly

**Minimal code:**
```rust
use datadog_serverless_core::{ServerlessServices, ServicesConfig};

#[tokio::main]
async fn main() {
    let config = ServicesConfig::from_env().expect("Failed to read config");

    let services = ServerlessServices::new(config)
        .expect("Failed to create services");

    let handle = services.start().await
        .expect("Failed to start services");

    // Keep running until signal
    tokio::signal::ctrl_c().await.ok();

    handle.stop().await.ok();
}
```

## Component Details

### Core Library: `datadog-serverless-core`

#### Service Lifecycle

**Initialization Phase:**
1. Validate configuration
2. Detect cloud environment (Lambda, Azure, etc.)
3. Setup logging subsystem
4. Create service components (aggregators, flushers, processors)

**Startup Phase:**
1. Create Tokio runtime
2. Spawn trace mini-agent task
3. Spawn DogStatsD server task (if enabled)
4. Wait for services to be ready
5. Return `ServicesHandle`

**Running Phase:**
- Services run in background Tokio tasks
- Periodic flushing of metrics/traces
- Status updates sent via channel
- Error handling and logging

**Shutdown Phase:**
1. Send cancellation signal to all tasks
2. Flush pending metrics/traces
3. Wait for graceful shutdown (with timeout)
4. Clean up resources

#### Internal Architecture

```rust
pub struct ServerlessServices {
    config: ServicesConfig,
    runtime: Option<tokio::runtime::Runtime>,
}

pub struct ServicesHandle {
    cancellation_token: CancellationToken,
    status_tx: mpsc::Sender<ServiceStatus>,
    status_rx: mpsc::Receiver<ServiceStatus>,
    services_task: JoinHandle<()>,
}

impl ServerlessServices {
    pub fn new(config: ServicesConfig) -> Result<Self, ServicesError> {
        // Validate config
        config.validate()?;

        // Create runtime
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        Ok(Self { config, runtime: Some(runtime) })
    }

    pub async fn start(mut self) -> Result<ServicesHandle, ServicesError> {
        let runtime = self.runtime.take()
            .ok_or(ServicesError::AlreadyStarted)?;

        let cancellation_token = CancellationToken::new();
        let (status_tx, status_rx) = mpsc::channel(32);

        // Spawn services in runtime
        let services_task = runtime.spawn(run_services(
            self.config,
            cancellation_token.clone(),
            status_tx.clone(),
        ));

        // Wait for ready signal or error
        // ... (implementation details)

        Ok(ServicesHandle {
            cancellation_token,
            status_tx,
            status_rx,
            services_task,
        })
    }
}

async fn run_services(
    config: ServicesConfig,
    cancel: CancellationToken,
    status: mpsc::Sender<ServiceStatus>,
) {
    // Similar to current main.rs logic
    // - Setup trace agent
    // - Setup DogStatsD
    // - Run flush loop
    // - Handle cancellation
}
```

### NAPI Addon: `datadog-serverless-node`

#### NAPI-RS Integration

**Class Definition:**
```rust
#[napi]
pub struct DatadogServices {
    handle: Arc<Mutex<Option<ServicesHandle>>>,
    status_callback: Arc<Mutex<Option<ThreadsafeFunction<ServiceStatus>>>>,
}

#[napi]
impl DatadogServices {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            handle: Arc::new(Mutex::new(None)),
            status_callback: Arc::new(Mutex::new(None)),
        }
    }

    #[napi]
    pub async fn start(
        &self,
        config: JsServicesConfig,
        status_callback: Option<JsFunction>,
    ) -> Result<()> {
        // Convert JS config to Rust config
        let rust_config = config.into_rust_config()?;

        // Setup status callback if provided
        if let Some(callback) = status_callback {
            let tsfn = callback.create_threadsafe_function(0, |ctx| {
                // Convert Rust status to JS value
                Ok(vec![ctx.value.to_js_object(ctx.env)?])
            })?;

            *self.status_callback.lock().unwrap() = Some(tsfn);
        }

        // Create and start services
        let services = ServerlessServices::new(rust_config)?;
        let handle = services.start().await?;

        // Spawn task to forward status updates
        if let Some(callback) = self.status_callback.lock().unwrap().as_ref() {
            let callback = callback.clone();
            let mut rx = handle.status_receiver();

            tokio::spawn(async move {
                while let Some(status) = rx.recv().await {
                    callback.call(Ok(status), ThreadsafeFunctionCallMode::NonBlocking);
                }
            });
        }

        *self.handle.lock().unwrap() = Some(handle);

        Ok(())
    }

    #[napi]
    pub async fn stop(&self) -> Result<()> {
        let handle = self.handle.lock().unwrap().take()
            .ok_or_else(|| Error::new(Status::InvalidArg, "Services not started"))?;

        handle.stop().await?;

        Ok(())
    }

    #[napi]
    pub fn is_running(&self) -> bool {
        self.handle.lock().unwrap()
            .as_ref()
            .map(|h| h.is_running())
            .unwrap_or(false)
    }
}

#[napi(object)]
pub struct JsServicesConfig {
    pub api_key: Option<String>,
    pub site: Option<String>,
    pub dogstatsd_port: Option<u32>,
    pub use_dogstatsd: Option<bool>,
    pub metric_namespace: Option<String>,
    pub log_level: Option<String>,
    pub https_proxy: Option<String>,
}

impl JsServicesConfig {
    fn into_rust_config(self) -> Result<ServicesConfig> {
        Ok(ServicesConfig {
            api_key: self.api_key,
            site: self.site.unwrap_or_else(|| "datadoghq.com".to_string()),
            dogstatsd_port: self.dogstatsd_port.unwrap_or(8125) as u16,
            use_dogstatsd: self.use_dogstatsd.unwrap_or(true),
            metric_namespace: self.metric_namespace,
            log_level: self.log_level.unwrap_or_else(|| "info".to_string()),
            https_proxy: self.https_proxy,
        })
    }
}
```

#### Cleanup Hooks

NAPI-RS provides lifecycle hooks for cleanup:

```rust
#[napi]
impl DatadogServices {
    // Called when Node.js process is about to exit
    #[napi(ts_return_type = "void")]
    pub fn register_cleanup_hook(&self, env: Env) -> Result<()> {
        let handle = self.handle.clone();

        env.add_env_cleanup_hook((), move |_| {
            // This runs on process exit
            if let Some(handle) = handle.lock().unwrap().take() {
                // Spawn blocking to wait for async shutdown
                tokio::runtime::Handle::current().block_on(async {
                    let _ = handle.stop().await;
                });
            }
        })?;

        Ok(())
    }
}
```

JavaScript usage:
```javascript
const { services } = require('@datadog/serverless-node');

// Automatically register cleanup
services.registerCleanupHook();

await services.start(config);
// Services will auto-stop on process exit
```

## Data Flow

### Startup Flow

```
JavaScript                    NAPI Layer                 Core Library               Tokio Runtime
    |                             |                            |                           |
    |--start(config)------------->|                            |                           |
    |                             |--new(config)------------->|                           |
    |                             |                            |--validate config          |
    |                             |                            |--create runtime---------->|
    |                             |                            |                           |
    |                             |<--ServicesHandle-----------|                           |
    |                             |                            |--spawn trace agent------->|
    |                             |                            |--spawn dogstatsd--------->|
    |                             |                            |                           |
    |                             |                            |<--ready signal------------|
    |<--Promise resolves----------|                            |                           |
    |                             |                            |                           |
    |--statusCallback('ready')    |                            |                           |
```

### Runtime Flow

```
Tokio Tasks                   Status Channel             NAPI Layer               JavaScript
    |                             |                            |                           |
    |--trace received             |                            |                           |
    |--metric received            |                            |                           |
    |--flush interval             |                            |                           |
    |--send metrics/traces        |                            |                           |
    |                             |                            |                           |
    |--error occurred------------>|                            |                           |
    |                             |--status update------------>|                           |
    |                             |                            |--call callback----------->|
    |                             |                            |                           |
    |                             |                            |                  callback('error', msg)
```

### Shutdown Flow

```
JavaScript                    NAPI Layer                 Core Library               Tokio Runtime
    |                             |                            |                           |
    |--stop()-------------------->|                            |                           |
    |                             |--handle.stop()------------>|                           |
    |                             |                            |--cancel token------------>|
    |                             |                            |                           |
    |                             |                            |                    <--stop DogStatsD
    |                             |                            |                    <--stop trace agent
    |                             |                            |                    <--flush pending
    |                             |                            |                           |
    |                             |<--Ok()--------------------|<--tasks joined------------|
    |<--Promise resolves----------|                            |                           |
```

### Auto-Cleanup on Exit

```
Node.js Process               Cleanup Hook               Core Library               Tokio Runtime
    |                             |                            |                           |
    |--exit signal                |                            |                           |
    |--trigger cleanup hooks----->|                            |                           |
    |                             |--check if running          |                           |
    |                             |--handle.stop()------------>|                           |
    |                             |                            |--graceful shutdown------->|
    |                             |                            |<--ok-----------------------|
    |                             |<--done--------------------|                           |
    |<--exit----------------------|                            |                           |
```

## Configuration

### Environment Variables (Backward Compatible)

The standalone binary (`datadog-serverless-compat`) continues to use environment variables:

- `DD_API_KEY` - Datadog API key
- `DD_SITE` - Datadog site (default: datadoghq.com)
- `DD_LOG_LEVEL` - Log level (default: info)
- `DD_DOGSTATSD_PORT` - DogStatsD port (default: 8125)
- `DD_USE_DOGSTATSD` - Enable DogStatsD (default: true)
- `DD_STATSD_METRIC_NAMESPACE` - Metric namespace prefix
- `DD_PROXY_HTTPS` / `HTTPS_PROXY` - HTTPS proxy

### JavaScript Configuration Object

Node.js applications pass configuration as an object:

```typescript
interface ServicesConfig {
  // Required for flushing metrics
  apiKey?: string;

  // Datadog site (datadoghq.com, datadoghq.eu, etc.)
  site?: string;

  // DogStatsD UDP port
  dogstatsdPort?: number;

  // Enable/disable DogStatsD
  useDogstatsd?: boolean;

  // Optional metric namespace prefix
  metricNamespace?: string;

  // Logging level (trace, debug, info, warn, error)
  logLevel?: string;

  // HTTPS proxy URL
  httpsProxy?: string;
}
```

### Configuration Validation

The `ServicesConfig` struct validates:
- Port numbers are valid (1-65535)
- Site is a valid Datadog site
- Log level is valid (trace, debug, info, warn, error)
- API key is present (warn if missing, but allow for testing)
- Metric namespace follows naming rules

## Error Handling

### Error Types

```rust
#[derive(Debug, thiserror::Error)]
pub enum ServicesError {
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Failed to detect cloud environment")]
    EnvironmentDetection,

    #[error("Failed to start trace agent: {0}")]
    TraceAgentStart(String),

    #[error("Failed to start DogStatsD: {0}")]
    DogStatsDStart(String),

    #[error("Services already started")]
    AlreadyStarted,

    #[error("Services not running")]
    NotRunning,

    #[error("Shutdown timeout exceeded")]
    ShutdownTimeout,

    #[error("Runtime error: {0}")]
    Runtime(String),
}
```

### JavaScript Error Handling

Errors are thrown as standard JavaScript errors:

```javascript
try {
  await services.start(config);
} catch (error) {
  if (error.message.includes('Invalid configuration')) {
    // Handle config error
  } else if (error.message.includes('Environment detection')) {
    // Handle environment error
  } else {
    // Generic error handling
  }
}
```

### Runtime Errors via Callback

Runtime errors after startup are delivered via status callback:

```javascript
await services.start(config, (status) => {
  if (status.type === 'error') {
    console.error(`Service ${status.service} error: ${status.message}`);
    // Handle runtime error
  }
});
```

### Logging

- Rust code uses `tracing` crate (already in use)
- Logs go to stdout/stderr
- JavaScript can capture logs via process stdout/stderr
- Log level controlled by config

## Graceful Shutdown

### Shutdown Sequence

1. **Cancellation Signal**
   - Set cancellation token
   - Signal all background tasks to stop

2. **Stop Accepting New Data**
   - Close DogStatsD UDP socket
   - Stop trace agent listener

3. **Flush Pending Data**
   - Flush aggregated metrics to Datadog API
   - Flush traces to Datadog API
   - Wait for HTTP requests to complete

4. **Cleanup Resources**
   - Stop background tasks
   - Drop aggregator handles
   - Shutdown Tokio runtime

5. **Timeout Protection**
   - If shutdown takes >5 seconds, force exit
   - Log warning about incomplete shutdown

### Implementation

```rust
impl ServicesHandle {
    pub async fn stop(self) -> Result<(), ServicesError> {
        // Signal cancellation
        self.cancellation_token.cancel();

        // Wait for graceful shutdown with timeout
        match tokio::time::timeout(
            Duration::from_secs(5),
            self.services_task
        ).await {
            Ok(Ok(())) => {
                // Successful shutdown
                Ok(())
            }
            Ok(Err(e)) => {
                // Task panicked
                Err(ServicesError::Runtime(format!("Task panic: {}", e)))
            }
            Err(_) => {
                // Timeout - force shutdown
                warn!("Shutdown timeout exceeded, forcing exit");
                Err(ServicesError::ShutdownTimeout)
            }
        }
    }
}
```

### Node.js Integration

Automatic cleanup on process exit:

```javascript
// Auto-registered by NAPI addon
process.on('beforeExit', async () => {
  if (services.isRunning()) {
    await services.stop();
  }
});

process.on('SIGTERM', async () => {
  if (services.isRunning()) {
    await services.stop();
  }
  process.exit(0);
});

process.on('SIGINT', async () => {
  if (services.isRunning()) {
    await services.stop();
  }
  process.exit(0);
});
```

## Testing Strategy

### Unit Tests

**Core Library (`datadog-serverless-core`):**
- Test config validation
- Test service lifecycle (start, stop, restart)
- Test error handling
- Test graceful shutdown
- Mock Tokio runtime for testing

**NAPI Addon (`datadog-serverless-node`):**
- Test type conversions (JS ↔ Rust)
- Test error propagation
- Test callback invocation
- Use napi-rs test utilities

### Integration Tests

**Rust Integration Tests:**
```rust
#[tokio::test]
async fn test_services_lifecycle() {
    let config = ServicesConfig {
        api_key: Some("test-key".to_string()),
        site: "datadoghq.com".to_string(),
        dogstatsd_port: 8125,
        use_dogstatsd: true,
        metric_namespace: None,
        log_level: "info".to_string(),
        https_proxy: None,
    };

    let services = ServerlessServices::new(config).unwrap();
    let handle = services.start().await.unwrap();

    assert!(handle.is_running());

    handle.stop().await.unwrap();
}
```

**JavaScript Integration Tests:**
```javascript
const { services } = require('@datadog/serverless-node');
const assert = require('assert');

describe('DatadogServices', () => {
  afterEach(async () => {
    if (services.isRunning()) {
      await services.stop();
    }
  });

  it('should start and stop services', async () => {
    await services.start({
      apiKey: 'test-key',
      site: 'datadoghq.com',
    });

    assert.strictEqual(services.isRunning(), true);

    await services.stop();

    assert.strictEqual(services.isRunning(), false);
  });

  it('should handle status callbacks', (done) => {
    services.start({
      apiKey: 'test-key',
    }, (status) => {
      if (status.type === 'ready') {
        done();
      }
    });
  });

  it('should auto-cleanup on exit', async () => {
    await services.start({ apiKey: 'test-key' });

    // Simulate process exit
    process.emit('beforeExit', 0);

    // Wait for cleanup
    await new Promise(resolve => setTimeout(resolve, 100));

    assert.strictEqual(services.isRunning(), false);
  });
});
```

### End-to-End Tests

Test with real Datadog API (in CI with test API key):
- Start services with real config
- Send test metrics via UDP
- Send test traces
- Verify data arrives at Datadog
- Stop services cleanly

### Performance Tests

- Memory leak detection (run for extended period)
- Throughput testing (metrics/traces per second)
- Shutdown time measurement
- Memory usage under load

## Build and Packaging

### Building the NAPI Addon

**Development:**
```bash
cd crates/datadog-serverless-node
npm install
npm run build
# Produces: index.node (native addon)
```

**Release:**
```bash
npm run build --release
# Produces optimized build
```

**Cross-compilation:**
- Use napi-rs CI templates for cross-platform builds
- Build for: Linux x64/ARM64, macOS x64/ARM64, Windows x64
- Publish pre-built binaries to npm

### NPM Package Structure

```
@datadog/serverless-node/
├── package.json
├── index.js (JavaScript entry point)
├── index.d.ts (TypeScript definitions)
└── platform-specific/
    ├── darwin-x64/
    │   └── datadog-serverless-node.darwin-x64.node
    ├── darwin-arm64/
    │   └── datadog-serverless-node.darwin-arm64.node
    ├── linux-x64/
    │   └── datadog-serverless-node.linux-x64.node
    ├── linux-arm64/
    │   └── datadog-serverless-node.linux-arm64.node
    └── win32-x64/
        └── datadog-serverless-node.win32-x64.node
```

**package.json:**
```json
{
  "name": "@datadog/serverless-node",
  "version": "0.1.0",
  "main": "index.js",
  "types": "index.d.ts",
  "napi": {
    "name": "datadog-serverless-node",
    "triples": {
      "defaults": true,
      "additional": [
        "aarch64-apple-darwin",
        "aarch64-unknown-linux-gnu"
      ]
    }
  },
  "scripts": {
    "build": "napi build --platform --release",
    "test": "mocha test/**/*.test.js"
  },
  "dependencies": {},
  "devDependencies": {
    "@napi-rs/cli": "^2.18.0",
    "mocha": "^10.0.0"
  }
}
```

### Workspace Configuration

**Root Cargo.toml:**
```toml
[workspace]
members = [
    "crates/datadog-fips",
    "crates/datadog-trace-agent",
    "crates/dogstatsd",
    "crates/datadog-serverless-core",    # New
    "crates/datadog-serverless-compat",
    "crates/datadog-serverless-node",    # New
]
```

**datadog-serverless-node/Cargo.toml:**
```toml
[package]
name = "datadog-serverless-node"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
datadog-serverless-core = { path = "../datadog-serverless-core" }
napi = { version = "2", features = ["async", "tokio_rt"] }
napi-derive = "2"
tokio = { version = "1", features = ["full"] }

[build-dependencies]
napi-build = "2"
```

## Implementation Phases

### Phase 1: Extract Core Library (Week 1)

**Tasks:**
1. Create `crates/datadog-serverless-core` crate
2. Move service logic from `main.rs` to core library
3. Design and implement `ServicesConfig` struct
4. Design and implement `ServerlessServices` struct
5. Design and implement `ServicesHandle` struct
6. Add unit tests for core library
7. Update `datadog-serverless-compat` to use core library
8. Verify standalone binary still works

**Deliverable:** Working core library with standalone binary using it.

### Phase 2: Build NAPI Addon (Week 2)

**Tasks:**
1. Create `crates/datadog-serverless-node` crate
2. Setup napi-rs dependencies and build configuration
3. Implement `DatadogServices` class with start/stop methods
4. Implement type conversions (JS ↔ Rust)
5. Implement status callbacks
6. Add error handling and conversion
7. Write JavaScript integration tests
8. Test on local machine

**Deliverable:** Basic NAPI addon that can start/stop services.

### Phase 3: Graceful Shutdown (Week 3)

**Tasks:**
1. Implement cleanup hooks in NAPI addon
2. Add timeout protection to shutdown
3. Implement graceful flush on exit
4. Test auto-cleanup behavior
5. Add process signal handlers (SIGTERM, SIGINT)
6. Write tests for shutdown scenarios

**Deliverable:** Addon with reliable graceful shutdown.

### Phase 4: Cross-Platform Build (Week 4)

**Tasks:**
1. Setup GitHub Actions for cross-compilation
2. Build for Linux x64, Linux ARM64
3. Build for macOS x64, macOS ARM64
4. Build for Windows x64 (if supported)
5. Create NPM package structure
6. Test pre-built binaries on each platform
7. Publish to NPM (test registry first)

**Deliverable:** Multi-platform NPM package.

### Phase 5: Documentation & Examples (Week 5)

**Tasks:**
1. Write README for NPM package
2. Add API documentation (JSDoc/TSDoc)
3. Create example Node.js applications
4. Write troubleshooting guide
5. Add performance tuning guide
6. Create migration guide from standalone binary

**Deliverable:** Complete documentation and examples.

### Phase 6: Production Readiness (Week 6)

**Tasks:**
1. Performance testing and optimization
2. Memory leak detection and fixes
3. End-to-end testing with real Datadog API
4. Security review
5. License compliance check
6. Final testing on all platforms
7. Publish v1.0.0 to NPM

**Deliverable:** Production-ready NPM package.

## Future Enhancements

### Optional Features (Not in Initial Release)

1. **Runtime Reconfiguration**
   - Update log level without restart
   - Change flush intervals dynamically
   - Add/remove tags at runtime

2. **Advanced Monitoring**
   - Expose internal metrics (queue sizes, flush latency, etc.)
   - Health check endpoint
   - Performance statistics API

3. **Additional Platforms**
   - Python bindings (PyO3)
   - Ruby bindings (magnus)
   - Go bindings (CGO)

4. **Enhanced Configuration**
   - Configuration file support (YAML/JSON)
   - Configuration validation CLI tool
   - Environment-specific profiles

## Success Criteria

### Functional Requirements

- ✅ JavaScript can start services with configuration object
- ✅ JavaScript can stop services gracefully
- ✅ Services auto-stop on Node.js process exit
- ✅ Status callbacks work for runtime events
- ✅ Standalone binary continues to work unchanged
- ✅ All existing functionality preserved

### Non-Functional Requirements

- ✅ Startup time < 100ms
- ✅ Shutdown time < 5s
- ✅ Memory overhead < 50MB
- ✅ No memory leaks in 24-hour run
- ✅ Cross-platform builds available (Linux, macOS, Windows)
- ✅ Test coverage > 80%
- ✅ Documentation complete

## Risks and Mitigations

### Risk: NAPI-RS Learning Curve

**Impact:** High
**Probability:** Medium
**Mitigation:**
- Start with simple example addon
- Use napi-rs documentation and examples
- Ask for help in napi-rs Discord

### Risk: Tokio Runtime in Node.js

**Impact:** High
**Probability:** Medium
**Mitigation:**
- Use napi-rs tokio_rt feature (tested pattern)
- Avoid blocking Node.js event loop
- Test thoroughly on all platforms

### Risk: Windows Build Issues

**Impact:** Medium
**Probability:** High
**Mitigation:**
- Document AWS_LC_FIPS_SYS_NO_ASM=1 requirement
- Consider skipping Windows initially
- Test early on Windows in CI

### Risk: Breaking Changes to Core Library

**Impact:** High
**Probability:** Low
**Mitigation:**
- Extensive integration tests
- Keep core library API minimal
- Version compatibility testing

### Risk: NPM Package Size

**Impact:** Low
**Probability:** Medium
**Mitigation:**
- Use release optimizations (already configured)
- Strip symbols from binaries
- Optional platform-specific packages

## Open Questions

1. **Should we support older Node.js versions?**
   - napi-rs supports Node.js 10+
   - Recommend requiring Node.js 16+ (LTS)

2. **Should configuration use camelCase or snake_case in JavaScript?**
   - Recommendation: camelCase (JavaScript convention)
   - Convert to snake_case internally

3. **Should we expose lower-level APIs (direct access to aggregators)?**
   - Recommendation: No, keep it simple
   - Can add in future if needed

4. **Should we bundle TypeScript definitions?**
   - Recommendation: Yes, improves DX
   - Generate from Rust using napi-rs

5. **How should we handle versioning between core and addon?**
   - Recommendation: Keep versions in sync
   - Core version matches addon version

## References

- [napi-rs Documentation](https://napi.rs/)
- [napi-rs Examples](https://github.com/napi-rs/napi-rs/tree/main/examples)
- [Tokio Documentation](https://tokio.rs/)
- [Node.js N-API Documentation](https://nodejs.org/api/n-api.html)
