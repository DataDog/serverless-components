# AppSec HTTP Middleware Integration Guide

This guide explains how to integrate Datadog Application Security (AppSec) with your web application using the `datadog-agent-native` library.

## Overview

The AppSec module provides Web Application Firewall (WAF) capabilities that analyze HTTP traffic for security threats. It:

- Detects security attacks (SQL injection, XSS, etc.)
- Collects API security schemas
- Attaches security metadata to distributed traces
- Reports threats to Datadog

## Quick Start

### 1. Enable AppSec in Configuration

```rust
use datadog_agent_native::config::Config;
use datadog_agent_native::appsec::Processor;

let config = Config {
    appsec_enabled: true,
    api_security_enabled: true,  // Optional: enable API security sampling
    api_security_sample_delay: std::time::Duration::from_secs(30),
    appsec_waf_timeout: std::time::Duration::from_millis(5),
    ..Config::default()
};

let mut processor = Processor::new(&config)?;
```

### 2. Process HTTP Requests

For each HTTP request, follow this pattern:

```rust
use datadog_agent_native::appsec::{HttpRequest, HttpTransaction};

// 1. Create HTTP request representation
let request = HttpRequest {
    method: "GET".to_string(),
    uri: "/api/users/123".to_string(),
    route: Some("/api/users/{id}".to_string()),
    client_ip: Some("192.168.1.1".to_string()),
    headers: headers_map,
    query_params: query_map,
    body: Some(body_bytes),
    ..Default::default()
};

// 2. Start security analysis
let transaction = HttpTransaction::from_request(request);
processor.process_http_request(request_id, &transaction).await;

// 3. Process request in your application
let response = your_app_handler(request).await;

// 4. Complete security analysis with response
transaction.set_response(response);
processor.process_http_response(request_id, &transaction).await;
```

## API Reference

### Core Types

#### `HttpRequest`
Represents an HTTP request for AppSec analysis.

```rust
pub struct HttpRequest {
    /// HTTP method (GET, POST, etc.)
    pub method: String,
    /// Full URI path (without query string)
    pub uri: String,
    /// Route pattern (e.g., "/api/users/{id}")
    pub route: Option<String>,
    /// Client IP address
    pub client_ip: Option<String>,
    /// Request headers (lowercase keys, no cookies)
    pub headers: HashMap<String, Vec<String>>,
    /// Cookies from Cookie header
    pub cookies: HashMap<String, Vec<String>>,
    /// Query parameters
    pub query_params: HashMap<String, Vec<String>>,
    /// Path parameters (e.g., {id} -> "123")
    pub path_params: HashMap<String, String>,
    /// Request body as bytes
    pub body: Option<Vec<u8>>,
}
```

#### `HttpResponse`
Represents an HTTP response for AppSec analysis.

```rust
pub struct HttpResponse {
    /// HTTP status code (200, 404, etc.)
    pub status_code: i64,
    /// Response headers (lowercase keys, no cookies)
    pub headers: HashMap<String, Vec<String>>,
    /// Response body as bytes
    pub body: Option<Vec<u8>>,
}
```

#### `HttpTransaction`
Combines request and response for complete analysis.

```rust
pub struct HttpTransaction {
    pub request: HttpRequest,
    pub response: Option<HttpResponse>,
}
```

### Processor Methods

#### `process_http_request`
Analyzes the HTTP request and creates a security context.

```rust
pub async fn process_http_request(
    &mut self,
    request_id: &str,
    payload: &dyn InvocationPayload
)
```

**Parameters:**
- `request_id`: Unique identifier for this request (typically a trace ID)
- `payload`: HTTP request data implementing `InvocationPayload` trait

**Use when:** An HTTP request is received, before application processing.

#### `process_http_response`
Completes analysis with the response and performs API security sampling.

```rust
pub async fn process_http_response(
    &mut self,
    request_id: &str,
    payload: &dyn InvocationPayload
)
```

**Parameters:**
- `request_id`: Same ID used in `process_http_request`
- `payload`: Complete HTTP transaction with request and response

**Use when:** After generating the response, before sending to client.

#### `process_http_transaction`
Convenience method combining both request and response analysis.

```rust
pub async fn process_http_transaction(
    &mut self,
    request_id: &str,
    payload: &dyn InvocationPayload
)
```

**Use when:** You have both request and response available at once (e.g., in tests or proxies).

## Integration Patterns

### Pattern 1: Middleware (Recommended)

Integrate as middleware in your web framework:

```rust
async fn appsec_middleware(
    request: Request,
    next: Next,
) -> Response {
    let request_id = generate_request_id();

    // Convert to AppSec format
    let appsec_request = convert_to_appsec(request);
    let mut transaction = HttpTransaction::from_request(appsec_request);

    // Analyze request
    APPSEC_PROCESSOR.lock().await
        .process_http_request(&request_id, &transaction).await;

    // Handle request
    let response = next.run(request).await;

    // Analyze response
    transaction.set_response(convert_response(&response));
    APPSEC_PROCESSOR.lock().await
        .process_http_response(&request_id, &transaction).await;

    response
}
```

### Pattern 2: Manual Wrapping

Wrap your request handlers:

```rust
async fn handle_user_request(req: UserRequest) -> UserResponse {
    let request_id = req.trace_id();

    // Start analysis
    let appsec_req = HttpRequest {
        method: req.method().to_string(),
        uri: req.path().to_string(),
        // ... populate fields
    };
    let mut transaction = HttpTransaction::from_request(appsec_req);
    appsec_processor.process_http_request(&request_id, &transaction).await;

    // Your logic
    let response = process_user_request(req).await;

    // Complete analysis
    let appsec_resp = HttpResponse {
        status_code: response.status() as i64,
        // ... populate fields
    };
    transaction.set_response(appsec_resp);
    appsec_processor.process_http_response(&request_id, &transaction).await;

    response
}
```

### Pattern 3: Proxy Mode

Intercept HTTP traffic at the proxy level:

```rust
async fn proxy_handler(client_req: Request) -> Response {
    let request_id = generate_id();

    // Analyze incoming request
    let appsec_req = parse_request(&client_req);
    let mut transaction = HttpTransaction::from_request(appsec_req);
    processor.process_http_request(&request_id, &transaction).await;

    // Forward to backend
    let backend_resp = forward_to_backend(client_req).await;

    // Analyze response
    let appsec_resp = parse_response(&backend_resp);
    transaction.set_response(appsec_resp);
    processor.process_http_response(&request_id, &transaction).await;

    backend_resp
}
```

## Configuration Options

### AppSec Configuration

```rust
Config {
    // Enable AppSec WAF
    appsec_enabled: true,

    // Custom WAF rules file (optional, uses default if None)
    appsec_rules: Some("/path/to/rules.json".to_string()),

    // WAF execution timeout (default: 5ms)
    appsec_waf_timeout: Duration::from_millis(5),

    // Enable API Security schema collection
    api_security_enabled: true,

    // Minimum delay between API Security samples per endpoint
    api_security_sample_delay: Duration::from_secs(30),

    // ... other config
}
```

### Environment Variables

- `DD_APPSEC_ENABLED=true` - Enable AppSec
- `DD_APPSEC_RULES=/path/to/rules.json` - Custom rules file
- `DD_APPSEC_WAF_TIMEOUT=5000` - WAF timeout in microseconds
- `DD_API_SECURITY_ENABLED=true` - Enable API Security
- `DD_API_SECURITY_SAMPLE_DELAY=30` - Sampling delay in seconds

## How It Works

1. **Request Analysis**: When `process_http_request` is called:
   - Creates a WAF context for the request
   - Extracts HTTP metadata (headers, query params, body)
   - Runs WAF rules against request data
   - Collects security events and tags

2. **Response Analysis**: When `process_http_response` is called:
   - Runs WAF rules against response data
   - Performs API Security sampling (if enabled)
   - Extracts API schemas from response bodies
   - Marks security context as complete

3. **Trace Integration**: Security tags are attached to spans:
   - `appsec.event=true` if threats detected
   - `_dd.appsec.json` contains security events
   - `_dd.appsec.waf.duration` shows WAF overhead
   - API schema information (if sampled)

## Best Practices

### 1. Request IDs
Use consistent request IDs across AppSec and tracing:

```rust
// Good: Use trace ID as request ID
let request_id = span.trace_id().to_string();
processor.process_http_request(&request_id, &transaction).await;

// Bad: Generate random IDs
let request_id = Uuid::new_v4().to_string();
```

### 2. Error Handling
AppSec analysis should not block requests:

```rust
// Analyze request, but don't fail if AppSec errors
if let Err(e) = processor.process_http_request(&req_id, &txn).await {
    warn!("AppSec request analysis failed: {}", e);
}

// Continue processing request regardless
let response = handle_request(request).await;
```

### 3. Performance
- WAF timeout default (5ms) prevents blocking requests
- API Security sampling prevents overload
- Context buffer (default 5) limits memory usage

### 4. Thread Safety
Share the processor using `Arc<Mutex<Processor>>`:

```rust
use std::sync::Arc;
use tokio::sync::Mutex;

let processor = Arc::new(Mutex::new(Processor::new(&config)?));

// In async handler
let mut proc = processor.lock().await;
proc.process_http_request(&request_id, &transaction).await;
```

## Troubleshooting

### No Security Tags on Traces
- Verify `appsec_enabled = true` in config
- Ensure request IDs match between AppSec and trace spans
- Check WAF timeout isn't too aggressive

### High Latency
- Increase `appsec_waf_timeout` if seeing timeouts
- Review custom WAF rules complexity
- Check if body parsing is slow (large bodies)

### Memory Usage
- Reduce context buffer capacity if needed
- Limit body sizes for analysis
- Check for request ID leaks (contexts not cleaned up)

## Examples

See the `examples/` directory for complete integration examples:
- `appsec_http_middleware.rs` - Basic HTTP middleware integration
- Framework-specific examples (Axum, Actix, etc.)

## Further Reading

- [Datadog AppSec Documentation](https://docs.datadoghq.com/security/application_security/)
- [WAF Rules Format](https://docs.datadoghq.com/security/application_security/threats/custom_rules/)
- [API Security Monitoring](https://docs.datadoghq.com/security/application_security/api_security/)
