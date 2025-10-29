#[cfg(feature = "appsec")]
use datadog_agent_native::appsec::{HttpRequest, HttpResponse, HttpTransaction, Processor};
#[cfg(feature = "appsec")]
use datadog_agent_native::config::Config;
#[cfg(feature = "appsec")]
use std::collections::HashMap;
/// Example: AppSec HTTP Middleware Integration
///
/// This example demonstrates how to integrate Datadog AppSec (Application Security)
/// with your web application using the HTTP middleware API.
///
/// The AppSec processor analyzes HTTP requests and responses for security threats
/// using a Web Application Firewall (WAF) and attaches security tags to traces.

#[cfg(feature = "appsec")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Configure AppSec
    let config = Config {
        appsec_enabled: true,
        api_security_enabled: true,
        ..Config::default()
    };

    // Create the AppSec processor
    let mut processor = Processor::new(&config)?;

    println!("AppSec processor initialized successfully!");
    println!("WAF rules loaded and ready to analyze HTTP traffic.\n");

    // Simulate an HTTP request
    simulate_http_request(&mut processor).await;

    Ok(())
}

#[cfg(not(feature = "appsec"))]
fn main() {
    eprintln!(
        "The appsec_http_middleware example requires the 'appsec' feature. Re-run with `--features appsec`."
    );
}

/// Simulates processing an HTTP request through AppSec middleware
#[cfg(feature = "appsec")]
async fn simulate_http_request(processor: &mut Processor) {
    // Generate a unique request ID (in a real app, this could be a trace ID)
    let request_id = "req-12345-example";

    println!("=== Simulating HTTP Request ===");
    println!("Request ID: {}", request_id);

    // 1. Parse the incoming HTTP request
    let http_request = HttpRequest {
        method: "POST".to_string(),
        uri: "/api/users".to_string(),
        route: Some("/api/users".to_string()),
        client_ip: Some("192.168.1.100".to_string()),
        headers: {
            let mut headers = HashMap::new();
            headers.insert(
                "content-type".to_string(),
                vec!["application/json".to_string()],
            );
            headers.insert("user-agent".to_string(), vec!["Mozilla/5.0".to_string()]);
            headers
        },
        cookies: HashMap::new(),
        query_params: HashMap::new(),
        path_params: HashMap::new(),
        body: Some(br#"{"username": "alice", "email": "alice@example.com"}"#.to_vec()),
    };

    println!("Request: {} {}", http_request.method, http_request.uri);
    println!("Client IP: {}", http_request.client_ip.as_ref().unwrap());

    // 2. Create HTTP transaction and analyze the request
    let mut transaction = HttpTransaction::from_request(http_request);

    // Process the request through AppSec WAF
    processor
        .process_http_request(request_id, &transaction)
        .await;
    println!("✓ Request analyzed by WAF");

    // 3. Application processes the request (your business logic here)
    println!("✓ Application processing request...");

    // 4. Generate the HTTP response
    let http_response = HttpResponse {
        status_code: 201,
        headers: {
            let mut headers = HashMap::new();
            headers.insert(
                "content-type".to_string(),
                vec!["application/json".to_string()],
            );
            headers
        },
        body: Some(br#"{"id": 123, "username": "alice"}"#.to_vec()),
    };

    println!("Response: {} {}", http_response.status_code, "Created");

    // 5. Add response to transaction and analyze it
    transaction.set_response(http_response);
    processor
        .process_http_response(request_id, &transaction)
        .await;
    println!("✓ Response analyzed by WAF");

    println!("\n=== Security Analysis Complete ===");
    println!("Security tags have been collected and will be attached to traces.");
    println!("If threats were detected, they would be reported to Datadog.\n");
}

// ============================================================================
// Example: Integration with Axum Web Framework
// ============================================================================
// NOTE: This module is not compiled by default as axum is not a dependency
// To use this example, uncomment the code below and add axum to your Cargo.toml

#[cfg(feature = "appsec")]
#[allow(dead_code, unused_imports, unused_mut)]
mod axum_integration {
    use axum::{
        body::Body,
        extract::State,
        http::{Request, Response, StatusCode},
        middleware::Next,
    };
    use datadog_agent_native::appsec::{HttpRequest as AppSecRequest, Processor};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Shared state containing the AppSec processor
    pub struct AppState {
        pub appsec: Arc<Mutex<Processor>>,
    }

    /// Axum middleware for AppSec integration
    pub async fn appsec_middleware(
        State(state): State<Arc<AppState>>,
        request: Request<Body>,
        next: Next,
    ) -> Response<Body> {
        // Generate request ID (use real trace ID in production)
        let request_id = uuid::Uuid::new_v4().to_string();

        // Convert Axum request to AppSec format
        let appsec_request = AppSecRequest {
            method: request.method().to_string(),
            uri: request.uri().path().to_string(),
            route: None,     // Extract from router in real implementation
            client_ip: None, // Extract from headers/connection
            headers: std::collections::HashMap::new(), // Convert from request.headers()
            cookies: std::collections::HashMap::new(),
            query_params: std::collections::HashMap::new(),
            path_params: std::collections::HashMap::new(),
            body: None, // Read body if needed
        };

        // Analyze request
        let mut processor = state.appsec.lock().await;
        let transaction =
            datadog_agent_native::appsec::HttpTransaction::from_request(appsec_request);
        processor
            .process_http_request(&request_id, &transaction)
            .await;
        drop(processor);

        // Process request through application
        let response = next.run(request).await;

        // Analyze response
        let mut processor = state.appsec.lock().await;
        // Convert response and call processor.process_http_response()
        drop(processor);

        response
    }
}

// ============================================================================
// Example: Manual Integration Pattern
// ============================================================================

/// Example showing the manual integration pattern without a web framework
#[cfg(feature = "appsec")]
#[allow(dead_code)]
mod manual_integration {
    use datadog_agent_native::appsec::{HttpRequest, HttpResponse, HttpTransaction, Processor};

    pub async fn handle_request(
        processor: &mut Processor,
        method: &str,
        path: &str,
        body: Vec<u8>,
    ) -> (u16, Vec<u8>) {
        let request_id = "unique-request-id";

        // Build HTTP request
        let request = HttpRequest {
            method: method.to_string(),
            uri: path.to_string(),
            route: Some(path.to_string()),
            body: Some(body),
            ..Default::default()
        };

        // Start security analysis
        let mut transaction = HttpTransaction::from_request(request);
        processor
            .process_http_request(request_id, &transaction)
            .await;

        // Your application logic
        let (status, response_body) = your_app_logic(method, path);

        // Complete security analysis
        let response = HttpResponse {
            status_code: i64::from(status),
            body: Some(response_body.clone()),
            ..Default::default()
        };
        transaction.set_response(response);
        processor
            .process_http_response(request_id, &transaction)
            .await;

        (status, response_body)
    }

    fn your_app_logic(_method: &str, _path: &str) -> (u16, Vec<u8>) {
        (200, b"OK".to_vec())
    }
}
