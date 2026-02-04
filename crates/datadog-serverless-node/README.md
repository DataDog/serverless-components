# @datadog/serverless-node

Node.js bindings for Datadog serverless monitoring, providing DogStatsD and trace agent functionality for serverless environments like AWS Lambda, Azure Functions, and Azure Spring Apps.

## Features

- Native Node.js bindings to high-performance Rust-based monitoring services
- DogStatsD metrics collection and aggregation
- Trace agent for distributed tracing
- Built-in support for serverless environments
- Asynchronous status callbacks
- Thread-safe and efficient

## Installation

```bash
npm install @datadog/serverless-node
```

## Quick Start

```javascript
const { DatadogServices } = require('@datadog/serverless-node');

// Create a new instance
const services = new DatadogServices();

// Configure and start services
const config = {
  apiKey: process.env.DD_API_KEY,
  site: 'datadoghq.com',
  dogstatsdPort: 8125,
  useDogstatsd: true,
  logLevel: 'info'
};

// Start services with optional status callback
services.start(config, (status) => {
  console.log('Service status:', status.status);
});

// Register automatic cleanup on process exit (recommended)
services.registerCleanupHook();

// Later, when shutting down explicitly
services.stop();
```

## API Reference

### Class: DatadogServices

The main class for controlling Datadog serverless monitoring services.

#### Constructor

```javascript
const services = new DatadogServices();
```

Creates a new instance of DatadogServices. Does not start any services until `start()` is called.

#### Methods

##### start(config, statusCallback?)

Starts the Datadog monitoring services.

**Parameters:**

- `config` (Object): Configuration object with the following properties:
  - `apiKey` (String, optional): Datadog API key for authentication. Can also be set via `DD_API_KEY` environment variable.
  - `dogstatsdPort` (Number, optional): DogStatsD server port. Default: `8125`
  - `site` (String, optional): Datadog site (e.g., 'datadoghq.com', 'datadoghq.eu'). Default: `'datadoghq.com'`
  - `useDogstatsd` (Boolean, optional): Whether to enable DogStatsD metrics collection. Default: `true`
  - `metricNamespace` (String, optional): Optional namespace prefix for all metrics
  - `httpsProxy` (String, optional): HTTPS proxy URL for outbound connections
  - `logLevel` (String, optional): Log level - one of 'trace', 'debug', 'info', 'warn', 'error'. Default: `'info'`

- `statusCallback` (Function, optional): Callback function that receives status updates
  - Receives a single parameter: `status` object with a `status` property
  - Status values: `'starting'`, `'running'`, `'stopping'`, `'stopped'`

**Returns:** void

**Throws:**
- Error if services are already started or starting
- Error if configuration is invalid (e.g., empty site, invalid log level, invalid port)

**Example:**

```javascript
const config = {
  apiKey: 'your-api-key',
  site: 'datadoghq.com',
  dogstatsdPort: 8125,
  useDogstatsd: true,
  metricNamespace: 'myapp',
  logLevel: 'info'
};

services.start(config, (status) => {
  console.log('Status changed to:', status.status);
});
```

##### stop()

Stops the Datadog monitoring services.

**Returns:** void

**Throws:**
- Error if services are not running

**Example:**

```javascript
services.stop();
```

##### isRunning()

Check if services are currently running.

**Returns:** Boolean - `true` if services are running, `false` otherwise

**Example:**

```javascript
if (services.isRunning()) {
  console.log('Services are running');
  services.stop();
}
```

##### registerCleanupHook()

Register an automatic cleanup hook that stops services when the Node.js process exits. This is the recommended way to ensure proper resource cleanup without manual signal handling.

**Returns:** void

**Throws:**
- Error if cleanup hook registration fails

**Example:**

```javascript
const services = new DatadogServices();

services.start({
  apiKey: process.env.DD_API_KEY
});

// Automatically stop services on process exit
services.registerCleanupHook();

// Services will be stopped automatically when process exits
```

**Important Notes:**
- Call this method after starting services
- The cleanup hook ensures services stop even if your application crashes or exits unexpectedly
- Cleanup happens automatically on process termination (exit, Ctrl+C, SIGTERM, etc.)
- You can still call `stop()` explicitly for immediate shutdown
- The stop operation has a 5-second timeout to prevent hanging on exit

### Types

#### JsServicesConfig

Configuration object for Datadog services.

```typescript
interface JsServicesConfig {
  apiKey?: string;
  dogstatsdPort?: number;
  site?: string;
  useDogstatsd?: boolean;
  metricNamespace?: string;
  httpsProxy?: string;
  logLevel?: string;
}
```

#### JsServiceStatus

Service status object passed to status callbacks.

```typescript
interface JsServiceStatus {
  status: 'starting' | 'running' | 'stopping' | 'stopped';
}
```

## Usage Examples

### Basic Usage

```javascript
const { DatadogServices } = require('@datadog/serverless-node');

const services = new DatadogServices();

// Minimal configuration
services.start({
  apiKey: process.env.DD_API_KEY
});

// Check if running
console.log('Running:', services.isRunning());

// Stop when done
services.stop();
```

### With Status Callbacks

```javascript
const { DatadogServices } = require('@datadog/serverless-node');

const services = new DatadogServices();

const config = {
  apiKey: process.env.DD_API_KEY,
  logLevel: 'debug'
};

services.start(config, (status) => {
  switch (status.status) {
    case 'starting':
      console.log('Services are starting...');
      break;
    case 'running':
      console.log('Services are now running');
      break;
    case 'stopping':
      console.log('Services are stopping...');
      break;
    case 'stopped':
      console.log('Services have stopped');
      break;
  }
});
```

### AWS Lambda Handler Example

```javascript
const { DatadogServices } = require('@datadog/serverless-node');

let services;

// Initialize services once
function initializeDatadog() {
  if (!services) {
    services = new DatadogServices();
    services.start({
      apiKey: process.env.DD_API_KEY,
      site: process.env.DD_SITE || 'datadoghq.com',
      logLevel: process.env.DD_LOG_LEVEL || 'info',
      metricNamespace: 'lambda'
    });
  }
}

exports.handler = async (event) => {
  initializeDatadog();

  // Your Lambda function logic here
  const response = {
    statusCode: 200,
    body: JSON.stringify({ message: 'Hello from Lambda!' })
  };

  return response;
};
```

### Graceful Shutdown

Proper shutdown handling ensures that metrics and traces are flushed before your application exits. The library provides multiple approaches to ensure graceful shutdown.

#### Automatic Cleanup (Recommended)

The simplest approach is to use the built-in cleanup hook:

```javascript
const { DatadogServices } = require('@datadog/serverless-node');

const services = new DatadogServices();

services.start({
  apiKey: process.env.DD_API_KEY
});

// Automatically stop services on process exit
services.registerCleanupHook();
```

This approach:
- Works with all process termination scenarios (exit, Ctrl+C, SIGTERM, crashes)
- Requires no manual signal handling
- Has a 5-second timeout to prevent hanging
- Recommended for most applications

#### Manual Signal Handling

For more control over shutdown logging and behavior, use signal handlers:

```javascript
const { DatadogServices } = require('@datadog/serverless-node');

const services = new DatadogServices();

services.start({
  apiKey: process.env.DD_API_KEY
});

// Signal handler for graceful shutdown
function shutdown(signal) {
  console.log(`Received ${signal}, shutting down gracefully...`);
  if (services.isRunning()) {
    services.stop();
  }
  process.exit(0);
}

// Register signal handlers for common termination signals
process.on('SIGTERM', () => shutdown('SIGTERM')); // Kubernetes, Docker
process.on('SIGINT', () => shutdown('SIGINT'));   // Ctrl+C
process.on('SIGQUIT', () => shutdown('SIGQUIT')); // Ctrl+\
```

#### Combined Approach (Belt and Suspenders)

For maximum reliability, use both cleanup hooks and signal handlers:

```javascript
const services = new DatadogServices();

services.start({
  apiKey: process.env.DD_API_KEY
});

// Register automatic cleanup as fallback
services.registerCleanupHook();

// Also handle signals explicitly for logging
process.on('SIGTERM', () => {
  console.log('Received SIGTERM, exiting gracefully...');
  if (services.isRunning()) {
    services.stop();
  }
  process.exit(0);
});

process.on('SIGINT', () => {
  console.log('Received SIGINT (Ctrl+C), exiting...');
  if (services.isRunning()) {
    services.stop();
  }
  process.exit(0);
});
```

#### Shutdown Timeout

The `stop()` method automatically includes a 5-second timeout. If shutdown takes longer than 5 seconds, the operation will be aborted to prevent hanging. In practice, shutdown typically completes in under 1 second.

#### AWS Lambda Considerations

In AWS Lambda, the execution environment may be frozen after your handler returns. For best results:

```javascript
const { DatadogServices } = require('@datadog/serverless-node');

let services;

function initializeDatadog() {
  if (!services) {
    services = new DatadogServices();
    services.start({
      apiKey: process.env.DD_API_KEY,
      metricNamespace: 'lambda'
    });
    // Cleanup hook ensures flush on container teardown
    services.registerCleanupHook();
  }
}

exports.handler = async (event) => {
  initializeDatadog();

  // Your handler logic
  const result = await processEvent(event);

  // No need to manually stop - cleanup hook handles it
  return result;
};
```

### Error Handling

```javascript
const { DatadogServices } = require('@datadog/serverless-node');

const services = new DatadogServices();

try {
  services.start({
    apiKey: process.env.DD_API_KEY,
    site: '', // Invalid - will throw error
  });
} catch (error) {
  console.error('Failed to start services:', error.message);
}

// Prevent double start
try {
  services.start({ apiKey: 'key1' });
  services.start({ apiKey: 'key2' }); // Throws error
} catch (error) {
  console.error('Cannot start twice:', error.message);
}

// Prevent stop when not running
const newServices = new DatadogServices();
try {
  newServices.stop(); // Throws error - not running
} catch (error) {
  console.error('Cannot stop when not running:', error.message);
}
```

### Environment-based Configuration

```javascript
const { DatadogServices } = require('@datadog/serverless-node');

const services = new DatadogServices();

// Use environment variables for configuration
const config = {
  apiKey: process.env.DD_API_KEY,
  site: process.env.DD_SITE || 'datadoghq.com',
  dogstatsdPort: parseInt(process.env.DD_DOGSTATSD_PORT || '8125', 10),
  useDogstatsd: process.env.DD_USE_DOGSTATSD !== 'false',
  metricNamespace: process.env.DD_METRIC_NAMESPACE,
  httpsProxy: process.env.HTTPS_PROXY,
  logLevel: process.env.DD_LOG_LEVEL || 'info'
};

services.start(config);
```

## Configuration Validation

The library validates configuration on startup and will throw errors for:

- Empty or whitespace-only `site`
- Invalid `logLevel` (must be one of: trace, debug, info, warn, error)
- Invalid `dogstatsdPort` (must be 1-65535)

## Supported Platforms

This package includes pre-built native binaries for:

- macOS (Intel and Apple Silicon)
- Linux x86_64
- Linux ARM64

## Environment Variables

While you can configure the services programmatically, the following environment variables are also recognized:

- `DD_API_KEY` - Datadog API key
- `DD_SITE` - Datadog site
- `DD_DOGSTATSD_PORT` - DogStatsD port
- `DD_USE_DOGSTATSD` - Enable/disable DogStatsD
- `DD_LOG_LEVEL` - Logging level
- `DD_METRIC_NAMESPACE` - Metric namespace prefix
- `HTTPS_PROXY` - HTTPS proxy URL

Note: Configuration passed to `start()` takes precedence over environment variables.

## Performance

This library uses native Rust code for high performance:

- Zero-copy data transfer between JavaScript and Rust where possible
- Efficient thread-safe communication
- Asynchronous operations don't block the Node.js event loop
- Native DogStatsD implementation with pre-aggregation

## Troubleshooting

### Services not starting

If services don't start in local development:
- Services are designed for serverless environments and may not fully initialize locally
- Check logs by setting `logLevel: 'debug'`
- Verify your API key is valid
- Ensure you're not behind a firewall blocking Datadog endpoints

### Memory issues

- Call `stop()` when shutting down to properly clean up resources
- Only create one instance of `DatadogServices` per application

### Port conflicts

If you see port binding errors:
- Check that the configured `dogstatsdPort` is not in use
- Try a different port number
- Set `useDogstatsd: false` if you don't need metrics

## License

Apache-2.0

## Contributing

This package is part of the [DataDog/serverless-components](https://github.com/DataDog/serverless-components) monorepo.

## Support

For issues and questions:
- GitHub Issues: https://github.com/DataDog/serverless-components/issues
- Datadog Support: https://docs.datadoghq.com/help/
