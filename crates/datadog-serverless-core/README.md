# datadog-serverless-core

Core library for managing Datadog serverless monitoring services (trace agent and DogStatsD) in serverless environments such as AWS Lambda, Azure Functions, Azure Spring Apps, and Google Cloud Functions.

## Overview

This library provides a unified interface for starting, managing, and stopping Datadog's trace agent and DogStatsD services in serverless environments. It handles:

- Environment detection (AWS Lambda, Azure Functions, Azure Spring Apps, Cloud Functions)
- Trace collection and aggregation
- Metrics collection and aggregation via DogStatsD
- Automatic flushing of traces and metrics
- Graceful shutdown and cleanup

## Usage

### Basic Usage

```rust
use datadog_serverless_core::{ServerlessServices, ServicesConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create configuration
    let config = ServicesConfig {
        api_key: Some("your-api-key".to_string()),
        dogstatsd_port: 8125,
        site: "datadoghq.com".to_string(),
        use_dogstatsd: true,
        metric_namespace: None,
        https_proxy: None,
        log_level: "info".to_string(),
    };

    // Start services
    let services = ServerlessServices::new(config);
    let handle = services.start().await?;

    // Services are now running...
    // Your serverless function code here

    // Stop services
    handle.stop().await?;

    Ok(())
}
```

### Configuration from Environment Variables

```rust
use datadog_serverless_core::{ServerlessServices, ServicesConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration from environment variables
    let config = ServicesConfig::from_env()?;

    // Start services
    let services = ServerlessServices::new(config);
    let handle = services.start().await?;

    // Services are now running...

    Ok(())
}
```

### Monitoring Service Status

```rust
use datadog_serverless_core::{ServerlessServices, ServicesConfig, ServiceStatus};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServicesConfig::from_env()?;
    let services = ServerlessServices::new(config);
    let handle = services.start().await?;

    // Check if services are running
    if handle.is_running().await {
        println!("Services are running");
    }

    // Subscribe to status updates
    let mut status_rx = handle.status_receiver();
    tokio::spawn(async move {
        while let Ok(status) = status_rx.recv().await {
            match status {
                ServiceStatus::Starting => println!("Services starting"),
                ServiceStatus::Running => println!("Services running"),
                ServiceStatus::Stopping => println!("Services stopping"),
                ServiceStatus::Stopped => println!("Services stopped"),
            }
        }
    });

    // Stop services
    handle.stop().await?;

    Ok(())
}
```

## Environment Variables

The library supports the following environment variables when using `ServicesConfig::from_env()`:

| Variable | Description | Default |
|----------|-------------|---------|
| `DD_API_KEY` | Datadog API key for authentication | None (optional) |
| `DD_DOGSTATSD_PORT` | DogStatsD server port | 8125 |
| `DD_SITE` | Datadog site (e.g., datadoghq.com, datadoghq.eu) | datadoghq.com |
| `DD_USE_DOGSTATSD` | Enable/disable DogStatsD | true |
| `DD_STATSD_METRIC_NAMESPACE` | Optional metric namespace prefix | None |
| `DD_PROXY_HTTPS` or `HTTPS_PROXY` | HTTPS proxy URL | None |
| `DD_LOG_LEVEL` | Log level (trace, debug, info, warn, error) | info |

## Architecture

### Components

The library consists of three main modules:

#### `config`
- `ServicesConfig`: Configuration struct for both trace agent and DogStatsD
- `from_env()`: Load configuration from environment variables
- `validate()`: Validate configuration before starting services

#### `error`
- `ServicesError`: Error types for configuration, startup, and runtime errors
- Comprehensive error handling with context

#### `services`
- `ServerlessServices`: Main coordinator for service lifecycle
- `ServicesHandle`: Handle for monitoring and controlling running services
- `ServiceStatus`: Enum representing service states (Starting, Running, Stopping, Stopped)

### Service Lifecycle

1. **Starting**: Services are initializing
2. **Running**: Both trace agent and DogStatsD are operational
3. **Stopping**: Services are shutting down gracefully
4. **Stopped**: All services have been stopped and cleaned up

### Automatic Flushing

- Traces are flushed automatically by the trace agent
- Metrics are flushed every 10 seconds by default
- Final flush occurs on shutdown to ensure no data loss

## Testing

Run the test suite:

```bash
cargo test -p datadog-serverless-core
```

Run tests with nextest (preferred):

```bash
cargo nextest run -p datadog-serverless-core
```

Note: Some tests may fail in non-cloud environments as they require environment detection to succeed. This is expected behavior.

## Dependencies

- `datadog-trace-agent`: Trace processing and aggregation
- `dogstatsd`: Metrics aggregation and flushing
- `tokio`: Async runtime
- `tracing`: Logging framework
- `thiserror`: Error handling

## License

Apache-2.0

Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
