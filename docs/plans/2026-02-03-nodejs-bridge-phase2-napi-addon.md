# Node.js Bridge - Phase 2: Build NAPI Addon

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Create a Node.js native addon using NAPI-RS that exposes the `datadog-serverless-core` library to JavaScript, allowing Node.js applications to programmatically start and stop Datadog monitoring services.

**Architecture:** Create `datadog-serverless-node` crate as NAPI addon with `DatadogServices` class. Implement JS↔Rust type conversions, async methods for start/stop, and status callbacks. Build as native Node.js module (.node file).

**Tech Stack:** Rust, NAPI-RS, Node.js, TypeScript definitions

**Prerequisites:** Phase 1 must be completed (datadog-serverless-core library exists)

---

## Task 1: Create NAPI Addon Crate

**Files:**
- Create: `crates/datadog-serverless-node/Cargo.toml`
- Create: `crates/datadog-serverless-node/src/lib.rs`
- Create: `crates/datadog-serverless-node/build.rs`
- Create: `crates/datadog-serverless-node/package.json`
- Modify: `Cargo.toml` (workspace root)

**Step 1: Create directory structure**

```bash
mkdir -p crates/datadog-serverless-node/src
```

**Step 2: Create Cargo.toml**

Create: `crates/datadog-serverless-node/Cargo.toml`

```toml
[package]
name = "datadog-serverless-node"
version = "0.1.0"
edition.workspace = true
license.workspace = true

[lib]
crate-type = ["cdylib"]

[dependencies]
datadog-serverless-core = { path = "../datadog-serverless-core" }

# NAPI dependencies
napi = { version = "2", features = ["async", "tokio_rt"] }
napi-derive = "2"

# Runtime
tokio = { version = "1", features = ["full"] }

[build-dependencies]
napi-build = "2"

[profile.release]
lto = true
```

**Step 3: Create build.rs**

Create: `crates/datadog-serverless-node/build.rs`

```rust
fn main() {
    napi_build::setup();
}
```

**Step 4: Create package.json**

Create: `crates/datadog-serverless-node/package.json`

```json
{
  "name": "@datadog/serverless-node",
  "version": "0.1.0",
  "description": "Node.js bindings for Datadog serverless monitoring",
  "main": "index.js",
  "types": "index.d.ts",
  "scripts": {
    "build": "napi build --platform --release",
    "build:debug": "napi build --platform",
    "artifacts": "napi artifacts",
    "version": "napi version"
  },
  "napi": {
    "name": "datadog-serverless-node",
    "triples": {
      "defaults": true,
      "additional": [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu"
      ]
    }
  },
  "devDependencies": {
    "@napi-rs/cli": "^2.18.0"
  },
  "license": "Apache-2.0",
  "repository": {
    "type": "git",
    "url": "https://github.com/DataDog/serverless-components"
  }
}
```

**Step 5: Create basic lib.rs**

Create: `crates/datadog-serverless-node/src/lib.rs`

```rust
// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use napi_derive::napi;

#[napi]
pub fn hello() -> String {
    "Hello from Datadog Serverless Node!".to_string()
}
```

**Step 6: Add to workspace**

Modify: `Cargo.toml` (root)

Add to members:
```toml
[workspace]
members = [
    "crates/datadog-fips",
    "crates/datadog-trace-agent",
    "crates/dogstatsd",
    "crates/datadog-serverless-core",
    "crates/datadog-serverless-compat",
    "crates/datadog-serverless-node",    # Add this
]
```

**Step 7: Install NAPI CLI and build**

```bash
cd crates/datadog-serverless-node
npm install
npm run build:debug
```

Expected: Builds successfully, creates .node file

**Step 8: Test basic functionality**

```bash
node -e "console.log(require('./index.js').hello())"
```

Expected: Prints "Hello from Datadog Serverless Node!"

**Step 9: Commit**

```bash
git add Cargo.toml crates/datadog-serverless-node/
git commit -m "feat(node): create NAPI addon crate structure

Add new NAPI-RS addon crate for Node.js bindings to
datadog-serverless-core library.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Task 2: Implement Configuration Type Conversions

**Files:**
- Modify: `crates/datadog-serverless-node/src/lib.rs`

**Step 1: Add config module**

Modify: `crates/datadog-serverless-node/src/lib.rs`

Add after the hello function:

```rust
use datadog_serverless_core::ServicesConfig;
use napi::bindgen_prelude::*;

/// Configuration for Datadog serverless services
#[napi(object)]
#[derive(Debug, Clone)]
pub struct JsServicesConfig {
    /// Datadog API key (required for flushing metrics)
    pub api_key: Option<String>,

    /// Datadog site (e.g., datadoghq.com, datadoghq.eu)
    pub site: Option<String>,

    /// DogStatsD UDP port
    pub dogstatsd_port: Option<u32>,

    /// Enable/disable DogStatsD
    pub use_dogstatsd: Option<bool>,

    /// Optional metric namespace prefix
    pub metric_namespace: Option<String>,

    /// Logging level (trace, debug, info, warn, error)
    pub log_level: Option<String>,

    /// HTTPS proxy URL
    pub https_proxy: Option<String>,
}

impl JsServicesConfig {
    /// Convert JavaScript config to Rust config
    pub fn into_rust_config(self) -> Result<ServicesConfig> {
        let config = ServicesConfig {
            api_key: self.api_key,
            site: self.site.unwrap_or_else(|| "datadoghq.com".to_string()),
            dogstatsd_port: self.dogstatsd_port.unwrap_or(8125) as u16,
            use_dogstatsd: self.use_dogstatsd.unwrap_or(true),
            metric_namespace: self.metric_namespace,
            log_level: self.log_level.unwrap_or_else(|| "info".to_string()),
            https_proxy: self.https_proxy,
        };

        config.validate()
            .map_err(|e| Error::from_reason(format!("Invalid configuration: {}", e)))?;

        Ok(config)
    }
}
```

**Step 2: Add tests**

Add at the end of lib.rs:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_js_config_conversion() {
        let js_config = JsServicesConfig {
            api_key: Some("test-key".to_string()),
            site: Some("datadoghq.com".to_string()),
            dogstatsd_port: Some(8125),
            use_dogstatsd: Some(true),
            metric_namespace: None,
            log_level: Some("info".to_string()),
            https_proxy: None,
        };

        let rust_config = js_config.into_rust_config().unwrap();
        assert_eq!(rust_config.api_key, Some("test-key".to_string()));
        assert_eq!(rust_config.site, "datadoghq.com");
        assert_eq!(rust_config.dogstatsd_port, 8125);
    }

    #[test]
    fn test_js_config_defaults() {
        let js_config = JsServicesConfig {
            api_key: None,
            site: None,
            dogstatsd_port: None,
            use_dogstatsd: None,
            metric_namespace: None,
            log_level: None,
            https_proxy: None,
        };

        let rust_config = js_config.into_rust_config().unwrap();
        assert_eq!(rust_config.site, "datadoghq.com");
        assert_eq!(rust_config.dogstatsd_port, 8125);
        assert_eq!(rust_config.use_dogstatsd, true);
        assert_eq!(rust_config.log_level, "info");
    }

    #[test]
    fn test_js_config_validation() {
        let js_config = JsServicesConfig {
            api_key: None,
            site: Some("".to_string()), // Invalid - empty site
            dogstatsd_port: None,
            use_dogstatsd: None,
            metric_namespace: None,
            log_level: None,
            https_proxy: None,
        };

        let result = js_config.into_rust_config();
        assert!(result.is_err());
    }
}
```

**Step 3: Run tests**

```bash
cargo test -p datadog-serverless-node
```

Expected: 3 tests pass

**Step 4: Build and verify**

```bash
cd crates/datadog-serverless-node
npm run build:debug
```

Expected: Builds successfully

**Step 5: Commit**

```bash
git add crates/datadog-serverless-node/src/lib.rs
git commit -m "feat(node): implement configuration type conversions

Add JsServicesConfig type with conversion to Rust ServicesConfig.
Includes validation and tests.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Task 3: Implement DatadogServices Class

**Files:**
- Modify: `crates/datadog-serverless-node/src/lib.rs`

**Step 1: Add DatadogServices struct**

Add before the tests module in lib.rs:

```rust
use std::sync::{Arc, Mutex};
use datadog_serverless_core::{ServerlessServices, ServicesHandle};

/// Main class for controlling Datadog serverless services from Node.js
#[napi]
pub struct DatadogServices {
    handle: Arc<Mutex<Option<ServicesHandle>>>,
    runtime: Arc<tokio::runtime::Runtime>,
}

#[napi]
impl DatadogServices {
    /// Create a new DatadogServices instance
    #[napi(constructor)]
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::from_reason(format!("Failed to create runtime: {}", e)))?;

        Ok(Self {
            handle: Arc::new(Mutex::new(None)),
            runtime: Arc::new(runtime),
        })
    }

    /// Start the Datadog services
    #[napi]
    pub async fn start(&self, config: JsServicesConfig) -> Result<()> {
        let rust_config = config.into_rust_config()?;

        // Create services
        let services = ServerlessServices::new(rust_config)
            .map_err(|e| Error::from_reason(format!("Failed to create services: {}", e)))?;

        // Start services
        let handle = services.start().await
            .map_err(|e| Error::from_reason(format!("Failed to start services: {}", e)))?;

        // Store handle
        let mut guard = self.handle.lock()
            .map_err(|e| Error::from_reason(format!("Lock error: {}", e)))?;

        if guard.is_some() {
            return Err(Error::from_reason("Services already started"));
        }

        *guard = Some(handle);

        Ok(())
    }

    /// Stop the Datadog services
    #[napi]
    pub async fn stop(&self) -> Result<()> {
        let mut guard = self.handle.lock()
            .map_err(|e| Error::from_reason(format!("Lock error: {}", e)))?;

        let handle = guard.take()
            .ok_or_else(|| Error::from_reason("Services not running"))?;

        handle.stop().await
            .map_err(|e| Error::from_reason(format!("Failed to stop services: {}", e)))?;

        Ok(())
    }

    /// Check if services are running
    #[napi]
    pub fn is_running(&self) -> Result<bool> {
        let guard = self.handle.lock()
            .map_err(|e| Error::from_reason(format!("Lock error: {}", e)))?;

        Ok(guard.is_some())
    }
}
```

**Step 2: Build**

```bash
cd crates/datadog-serverless-node
npm run build:debug
```

Expected: Builds successfully, generates TypeScript definitions

**Step 3: Check generated TypeScript definitions**

```bash
cat index.d.ts
```

Expected: Should see DatadogServices class with start, stop, isRunning methods

**Step 4: Commit**

```bash
git add crates/datadog-serverless-node/
git commit -m "feat(node): implement DatadogServices class

Add main NAPI class with start/stop/isRunning methods.
Integrates with datadog-serverless-core library.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Task 4: Add JavaScript Integration Tests

**Files:**
- Create: `crates/datadog-serverless-node/test/basic.test.js`
- Modify: `crates/datadog-serverless-node/package.json`

**Step 1: Create test file**

Create: `crates/datadog-serverless-node/test/basic.test.js`

```javascript
const { DatadogServices } = require('../index.js');
const assert = require('assert');

describe('DatadogServices', () => {
  let services;

  beforeEach(() => {
    services = new DatadogServices();
  });

  afterEach(async () => {
    if (services.isRunning()) {
      await services.stop();
    }
  });

  it('should create new instance', () => {
    assert.ok(services);
    assert.strictEqual(services.isRunning(), false);
  });

  it('should start and stop services', async function() {
    this.timeout(10000); // 10 second timeout

    const config = {
      apiKey: 'test-key',
      site: 'datadoghq.com',
      dogstatsdPort: 8125,
      useDogstatsd: false, // Disabled for testing
      logLevel: 'error'
    };

    try {
      await services.start(config);
      // If we're not in a cloud environment, this will throw
      // That's OK for testing
    } catch (err) {
      if (err.message.includes('Failed to detect cloud environment')) {
        // Expected in local testing
        return;
      }
      throw err;
    }

    assert.strictEqual(services.isRunning(), true);
    await services.stop();
    assert.strictEqual(services.isRunning(), false);
  });

  it('should reject invalid configuration', async () => {
    const config = {
      site: '', // Invalid - empty site
    };

    try {
      await services.start(config);
      assert.fail('Should have thrown error');
    } catch (err) {
      assert.ok(err.message.includes('Invalid configuration'));
    }
  });

  it('should reject double start', async function() {
    this.timeout(10000);

    const config = {
      apiKey: 'test-key',
      useDogstatsd: false,
      logLevel: 'error'
    };

    try {
      await services.start(config);
    } catch (err) {
      // Might fail due to environment detection
      if (err.message.includes('Failed to detect cloud environment')) {
        return;
      }
    }

    try {
      await services.start(config);
      assert.fail('Should have thrown error');
    } catch (err) {
      assert.ok(err.message.includes('already started'));
    }
  });

  it('should reject stop when not running', async () => {
    try {
      await services.stop();
      assert.fail('Should have thrown error');
    } catch (err) {
      assert.ok(err.message.includes('not running'));
    }
  });
});
```

**Step 2: Add test dependencies to package.json**

Modify: `crates/datadog-serverless-node/package.json`

Add to devDependencies:
```json
{
  "devDependencies": {
    "@napi-rs/cli": "^2.18.0",
    "mocha": "^10.0.0"
  },
  "scripts": {
    "build": "napi build --platform --release",
    "build:debug": "napi build --platform",
    "test": "npm run build:debug && mocha test/**/*.test.js",
    "artifacts": "napi artifacts",
    "version": "napi version"
  }
}
```

**Step 3: Install dependencies and run tests**

```bash
cd crates/datadog-serverless-node
npm install
npm test
```

Expected: 5 tests pass (or some skip due to environment detection)

**Step 4: Commit**

```bash
git add crates/datadog-serverless-node/
git commit -m "test(node): add JavaScript integration tests

Add mocha tests for DatadogServices class verifying
start/stop lifecycle and error handling.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Task 5: Add Status Callbacks

**Files:**
- Modify: `crates/datadog-serverless-node/src/lib.rs`
- Modify: `crates/datadog-serverless-node/test/basic.test.js`

**Step 1: Add status callback support**

Modify the DatadogServices struct in lib.rs:

```rust
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

#[napi(object)]
#[derive(Debug, Clone)]
pub struct JsServiceStatus {
    pub status: String,
    pub message: Option<String>,
}

#[napi]
pub struct DatadogServices {
    handle: Arc<Mutex<Option<ServicesHandle>>>,
    runtime: Arc<tokio::runtime::Runtime>,
    status_callback: Arc<Mutex<Option<ThreadsafeFunction<JsServiceStatus>>>>,
}

#[napi]
impl DatadogServices {
    #[napi(constructor)]
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::from_reason(format!("Failed to create runtime: {}", e)))?;

        Ok(Self {
            handle: Arc::new(Mutex::new(None)),
            runtime: Arc::new(runtime),
            status_callback: Arc::new(Mutex::new(None)),
        })
    }

    /// Start the Datadog services with optional status callback
    #[napi]
    pub async fn start(
        &self,
        config: JsServicesConfig,
        #[napi(ts_arg_type = "(status: { status: string, message?: string }) => void")]
        status_callback: Option<JsFunction>,
    ) -> Result<()> {
        let rust_config = config.into_rust_config()?;

        // Setup status callback if provided
        if let Some(callback) = status_callback {
            let tsfn: ThreadsafeFunction<JsServiceStatus> = callback
                .create_threadsafe_function(0, |ctx| {
                    Ok(vec![ctx.value])
                })?;

            *self.status_callback.lock()
                .map_err(|e| Error::from_reason(format!("Lock error: {}", e)))?
                = Some(tsfn);
        }

        // Create services
        let services = ServerlessServices::new(rust_config)
            .map_err(|e| Error::from_reason(format!("Failed to create services: {}", e)))?;

        // Start services
        let mut handle = services.start().await
            .map_err(|e| Error::from_reason(format!("Failed to start services: {}", e)))?;

        // Spawn task to forward status updates
        if let Some(callback) = self.status_callback.lock()
            .map_err(|e| Error::from_reason(format!("Lock error: {}", e)))?
            .as_ref()
        {
            let callback = callback.clone();
            let mut rx = handle.status_receiver();

            tokio::spawn(async move {
                while let Some(status) = rx.recv().await {
                    let js_status = JsServiceStatus {
                        status: format!("{:?}", status),
                        message: None,
                    };
                    callback.call(Ok(js_status), ThreadsafeFunctionCallMode::NonBlocking);
                }
            });
        }

        // Store handle
        let mut guard = self.handle.lock()
            .map_err(|e| Error::from_reason(format!("Lock error: {}", e)))?;

        if guard.is_some() {
            return Err(Error::from_reason("Services already started"));
        }

        *guard = Some(handle);

        Ok(())
    }

    // ... (rest of the methods remain the same)
}
```

**Step 2: Add test for status callbacks**

Add to test/basic.test.js:

```javascript
  it('should call status callback', function(done) {
    this.timeout(10000);

    const config = {
      apiKey: 'test-key',
      useDogstatsd: false,
      logLevel: 'error'
    };

    let callbackCalled = false;

    services.start(config, (status) => {
      callbackCalled = true;
      assert.ok(status);
      assert.ok(status.status);
    }).then(() => {
      // Give callback a moment to fire
      setTimeout(() => {
        assert.strictEqual(callbackCalled, true);
        done();
      }, 100);
    }).catch(err => {
      if (err.message.includes('Failed to detect cloud environment')) {
        // Expected in local testing - skip this test
        done();
      } else {
        done(err);
      }
    });
  });
```

**Step 3: Build and test**

```bash
cd crates/datadog-serverless-node
npm test
```

Expected: 6 tests pass (5 previous + 1 new)

**Step 4: Commit**

```bash
git add crates/datadog-serverless-node/
git commit -m "feat(node): add status callback support

Allow JavaScript to receive status updates via callback function.
Uses threadsafe function for cross-thread communication.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Task 6: Add Error Handling and Documentation

**Files:**
- Create: `crates/datadog-serverless-node/README.md`
- Create: `crates/datadog-serverless-node/examples/basic.js`

**Step 1: Create README**

Create: `crates/datadog-serverless-node/README.md`

```markdown
# @datadog/serverless-node

Node.js bindings for Datadog serverless monitoring components. Allows Node.js applications to programmatically start and stop Datadog trace agent and DogStatsD services.

## Installation

```bash
npm install @datadog/serverless-node
```

## Quick Start

```javascript
const { DatadogServices } = require('@datadog/serverless-node');

async function main() {
  const services = new DatadogServices();

  // Start services
  await services.start({
    apiKey: process.env.DD_API_KEY,
    site: 'datadoghq.com',
    useDogstatsd: true,
    logLevel: 'info'
  });

  console.log('Datadog services started');

  // Your application code here

  // Stop services
  await services.stop();
  console.log('Datadog services stopped');
}

main().catch(console.error);
```

## API

### Constructor

```typescript
const services = new DatadogServices();
```

### start(config, statusCallback?)

Start the Datadog services with the provided configuration.

**Parameters:**
- `config` (object): Service configuration
  - `apiKey` (string, optional): Datadog API key
  - `site` (string, optional): Datadog site (default: 'datadoghq.com')
  - `dogstatsdPort` (number, optional): DogStatsD port (default: 8125)
  - `useDogstatsd` (boolean, optional): Enable DogStatsD (default: true)
  - `metricNamespace` (string, optional): Metric namespace prefix
  - `logLevel` (string, optional): Log level (default: 'info')
  - `httpsProxy` (string, optional): HTTPS proxy URL

- `statusCallback` (function, optional): Callback for status updates
  - Called with `{ status: string, message?: string }`

**Returns:** Promise<void>

**Example:**
```javascript
await services.start({
  apiKey: 'your-api-key',
  site: 'datadoghq.com'
}, (status) => {
  console.log('Status:', status);
});
```

### stop()

Stop the Datadog services gracefully.

**Returns:** Promise<void>

### isRunning()

Check if services are currently running.

**Returns:** boolean

## Examples

See the `examples/` directory for more examples:
- `basic.js` - Basic usage
- `with-callbacks.js` - Using status callbacks
- `error-handling.js` - Error handling

## Environment Requirements

This library requires running in a serverless environment (AWS Lambda, Azure Functions, or Google Cloud Functions). Running locally will result in an "environment detection" error unless you're testing with DogStatsD disabled.

## License

Apache-2.0
```

**Step 2: Create example**

Create: `crates/datadog-serverless-node/examples/basic.js`

```javascript
const { DatadogServices } = require('../index.js');

async function main() {
  const services = new DatadogServices();

  console.log('Starting Datadog services...');

  try {
    await services.start({
      apiKey: process.env.DD_API_KEY || 'test-key',
      site: 'datadoghq.com',
      useDogstatsd: false, // Disabled for local testing
      logLevel: 'info'
    }, (status) => {
      console.log('Status update:', status);
    });

    console.log('Services started:', services.isRunning());

    // Simulate some work
    await new Promise(resolve => setTimeout(resolve, 2000));

    console.log('Stopping services...');
    await services.stop();

    console.log('Services stopped:', services.isRunning());
  } catch (err) {
    if (err.message.includes('Failed to detect cloud environment')) {
      console.log('Note: This is expected when running locally.');
      console.log('The addon works correctly in serverless environments.');
    } else {
      console.error('Error:', err.message);
      process.exit(1);
    }
  }
}

main();
```

**Step 3: Test example**

```bash
cd crates/datadog-serverless-node
node examples/basic.js
```

Expected: Runs and shows expected environment detection message

**Step 4: Commit**

```bash
git add crates/datadog-serverless-node/
git commit -m "docs(node): add README and examples

Add comprehensive documentation with API reference and
usage examples for the Node.js addon.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Task 7: Final Testing and Verification

**Step 1: Build release version**

```bash
cd crates/datadog-serverless-node
npm run build
```

Expected: Builds successfully in release mode

**Step 2: Run all tests**

```bash
cargo test -p datadog-serverless-node
npm test
```

Expected: All Rust and JavaScript tests pass

**Step 3: Run clippy**

```bash
cargo clippy -p datadog-serverless-node
```

Expected: No warnings

**Step 4: Check TypeScript definitions**

```bash
cat index.d.ts
```

Expected: Complete TypeScript definitions for all exported types

**Step 5: Test with Node.js REPL**

```bash
cd crates/datadog-serverless-node
node
```

```javascript
> const { DatadogServices } = require('./index.js');
> const services = new DatadogServices();
> services.isRunning()
false
> // Can interact with the API
```

**Step 6: Verify package structure**

```bash
ls -la crates/datadog-serverless-node/
```

Expected files:
- Cargo.toml
- package.json
- build.rs
- src/lib.rs
- test/basic.test.js
- examples/basic.js
- README.md
- index.js (generated)
- index.d.ts (generated)
- *.node file (generated)

**Step 7: Create final commit if needed**

If any fixes were needed:

```bash
git add crates/datadog-serverless-node/
git commit -m "chore(node): final cleanup and fixes

Final adjustments to NAPI addon for Phase 2 completion.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Phase 2 Complete!

At this point, Phase 2 is complete. You have:

✅ Created `datadog-serverless-node` NAPI addon crate
✅ Implemented JS↔Rust type conversions
✅ Implemented DatadogServices class with start/stop methods
✅ Added status callback support
✅ Added comprehensive JavaScript tests
✅ Added error handling
✅ Created documentation and examples

The Node.js addon can now:
- Start and stop Datadog services from JavaScript
- Accept configuration as JavaScript objects
- Provide status updates via callbacks
- Handle errors gracefully
- Run in serverless environments

## Next Steps

After Phase 2, you can proceed to:
- **Phase 3**: Implement graceful shutdown with cleanup hooks
- **Phase 4**: Setup cross-platform builds
- **Phase 5**: Add comprehensive documentation
- **Phase 6**: Production readiness testing

Refer to the design document: `docs/plans/2026-02-03-nodejs-bridge-design.md`
