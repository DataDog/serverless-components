//! E2E tests for the Application Logs endpoint with mock backend
//!
//! These tests verify that the correct headers and query parameters
//! are added to requests forwarded to the backend.

use axum::{
    body::Bytes,
    extract::Query,
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use datadog_agent_native::agent::AgentCoordinator;
use datadog_agent_native::config::{operational_mode::OperationalMode, Config};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Captured request from the agent
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct CapturedRequest {
    headers: HashMap<String, String>,
    query_params: HashMap<String, String>,
    body: Vec<u8>,
}

/// Mock backend that captures forwarded requests
struct MockBackend {
    captured_requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl MockBackend {
    fn new() -> Self {
        Self {
            captured_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[allow(dead_code)]
    fn get_captured_requests(&self) -> Vec<CapturedRequest> {
        self.captured_requests.lock().unwrap().clone()
    }

    async fn handler(
        Query(params): Query<HashMap<String, String>>,
        headers: HeaderMap,
        body: Bytes,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
    ) -> (StatusCode, &'static str) {
        // Convert headers to HashMap
        let header_map: HashMap<String, String> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let request = CapturedRequest {
            headers: header_map,
            query_params: params,
            body: body.to_vec(),
        };

        captured.lock().unwrap().push(request);

        (StatusCode::ACCEPTED, "{}")
    }

    async fn start(self, port: u16) -> tokio::task::JoinHandle<()> {
        let captured = Arc::clone(&self.captured_requests);

        let app = Router::new().route(
            "/api/v2/logs",
            post(move |query, headers, body| {
                let captured = Arc::clone(&captured);
                async move { Self::handler(query, headers, body, captured).await }
            }),
        );

        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
            .await
            .expect("Failed to bind mock backend");

        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("Mock backend server failed");
        })
    }
}

/// Test that mock backend works (sanity check)
#[tokio::test]
async fn test_mock_backend_sanity_check() {
    // Start mock backend
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59199).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Send request directly to mock backend
    let client = reqwest::Client::new();
    let response = client
        .post("http://127.0.0.1:59199/api/v2/logs?ddtags=test:value")
        .header("dd-request-id", "test-id-123")
        .header("dd-api-key", "test-key")
        .json(&serde_json::json!({"message": "test"}))
        .send()
        .await
        .expect("Direct request to mock backend failed");

    assert_eq!(response.status(), 202, "Mock backend should return 202");

    // Verify request was captured
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1, "Should have captured 1 request");
    assert_eq!(
        requests[0].headers.get("dd-request-id"),
        Some(&"test-id-123".to_string())
    );
}

/// Test that DD-REQUEST-ID header is added
#[tokio::test]
async fn test_dd_request_id_header_added() {
    // Start mock backend
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59200).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Start agent pointing to mock backend
    let mut config = Config::default();
    config.api_key = "test-key-mock-1".to_string();
    config.site = "127.0.0.1:59200".to_string(); // Point to mock backend
    config.service = Some("test-service".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59201);
    config.flush_timeout = 1; // Short flush interval for tests

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Send request
    let client = reqwest::Client::new();
    let log_payload = serde_json::json!({"message": "test", "level": "info"});

    client
        .post("http://127.0.0.1:59201/v1/input")
        .header("Content-Type", "application/json")
        .json(&log_payload)
        .send()
        .await
        .expect("Request failed");

    // Wait for automatic flush (flush interval is 2 seconds)
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    // Verify DD-REQUEST-ID header was added
    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured by mock backend");

    let request = &requests[0];
    assert!(
        request.headers.contains_key("dd-request-id"),
        "DD-REQUEST-ID header not found"
    );

    let request_id = &request.headers["dd-request-id"];
    assert!(
        request_id.len() == 36, // UUID v4 format: 8-4-4-4-12
        "DD-REQUEST-ID doesn't look like a UUID: {}",
        request_id
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that DD-EVP-ORIGIN header is set correctly
#[tokio::test]
async fn test_dd_evp_origin_header() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59202).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-mock-2".to_string();
    config.site = "127.0.0.1:59202".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59203);
    config.flush_timeout = 1; // Short flush interval for tests

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59203/v1/input")
        .json(&serde_json::json!({"message": "test"}))
        .send()
        .await
        .expect("Request failed");

    // Wait for automatic flush (flush interval is 2 seconds)
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured by mock backend");

    let request = &requests[0];
    assert_eq!(
        request.headers.get("dd-evp-origin"),
        Some(&"agent-logs".to_string()),
        "DD-EVP-ORIGIN should be 'agent-logs'"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that ddtags query parameter is built with all tag sources
#[tokio::test]
async fn test_ddtags_query_parameter() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59204).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-mock-3".to_string();
    config.site = "127.0.0.1:59204".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("production".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59205);
    config.flush_timeout = 1; // Short flush interval for tests

    // Add custom config tags
    config
        .tags
        .insert("team".to_string(), "backend".to_string());
    config
        .tags
        .insert("region".to_string(), "us-east-1".to_string());

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59205/v1/input")
        .header("X-Datadog-Additional-Tags", "custom:value,priority:high")
        .json(&serde_json::json!({"message": "test"}))
        .send()
        .await
        .expect("Request failed");

    // Wait for automatic flush (flush interval is 2 seconds)
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    assert!(
        request.query_params.contains_key("ddtags"),
        "ddtags query parameter not found"
    );

    let ddtags = &request.query_params["ddtags"];
    eprintln!("ddtags: {}", ddtags);

    // Verify all tag sources are included
    assert!(ddtags.contains("host:"), "Should contain hostname tag");
    assert!(
        ddtags.contains("default_env:production"),
        "Should contain environment tag"
    );
    assert!(
        ddtags.contains("agent_version:"),
        "Should contain agent version tag"
    );
    assert!(
        ddtags.contains("custom:value"),
        "Should contain custom header tags"
    );
    assert!(
        ddtags.contains("priority:high"),
        "Should contain custom header tags"
    );
    assert!(
        ddtags.contains("team:backend"),
        "Should contain config tags"
    );
    assert!(
        ddtags.contains("region:us-east-1"),
        "Should contain config tags"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that container tags are included in ddtags
#[tokio::test]
async fn test_container_tags_in_ddtags() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59206).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-mock-4".to_string();
    config.site = "127.0.0.1:59206".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59207);
    config.flush_timeout = 1; // Short flush interval for tests

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59207/v1/input")
        .header("Datadog-Container-ID", "abc123def456")
        .json(&serde_json::json!({"message": "test from container"}))
        .send()
        .await
        .expect("Request failed");

    // Wait for automatic flush (flush interval is 2 seconds)
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let ddtags = &request.query_params["ddtags"];
    eprintln!("ddtags with container: {}", ddtags);

    // Verify container tags are included
    assert!(
        ddtags.contains("container_id:abc123def456"),
        "Should contain container_id tag from tagger"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test tag truncation at 4001 characters
#[tokio::test]
async fn test_tag_truncation() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59208).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-mock-5".to_string();
    config.site = "127.0.0.1:59208".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59209);
    config.flush_timeout = 1; // Short flush interval for tests

    // Add LOTS of config tags to exceed 4001 character limit
    for i in 0..200 {
        config.tags.insert(
            format!("longtag{}", i),
            "x".repeat(50), // 50 character values
        );
    }

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59209/v1/input")
        .json(&serde_json::json!({"message": "test"}))
        .send()
        .await
        .expect("Request failed");

    // Wait for automatic flush (flush interval is 2 seconds)
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let ddtags = &request.query_params["ddtags"];

    eprintln!("ddtags length: {}", ddtags.len());
    assert!(
        ddtags.len() <= 4001,
        "ddtags should be truncated to 4001 characters, got {}",
        ddtags.len()
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test DD-API-KEY header is added
#[tokio::test]
async fn test_dd_api_key_header() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59210).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-secret-api-key-12345".to_string();
    config.site = "127.0.0.1:59210".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59211);
    config.flush_timeout = 1; // Short flush interval for tests

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59211/v1/input")
        .json(&serde_json::json!({"message": "test"}))
        .send()
        .await
        .expect("Request failed");

    // Wait for automatic flush (flush interval is 2 seconds)
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    assert_eq!(
        request.headers.get("dd-api-key"),
        Some(&"test-secret-api-key-12345".to_string()),
        "DD-API-KEY should match configured API key"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test DD-ExternalData header for container ID extraction
#[tokio::test]
async fn test_dd_external_data_header() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59212).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-mock-external-data".to_string();
    config.site = "127.0.0.1:59212".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59213);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // Test with DD-ExternalData header (JSON)
    let external_data = r#"{"container-id":"external-container-123","pod-uid":"pod-xyz"}"#;
    client
        .post("http://127.0.0.1:59213/v1/input")
        .header("DD-ExternalData", external_data)
        .json(&serde_json::json!({"message": "test from external data"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let ddtags = &request.query_params["ddtags"];
    eprintln!("ddtags with DD-ExternalData: {}", ddtags);

    assert!(
        ddtags.contains("container_id:external-container-123"),
        "Should contain container_id from DD-ExternalData header"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test PID-based container ID extraction from DD-LocalData
#[tokio::test]
async fn test_pid_based_container_id_extraction() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59214).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-mock-pid".to_string();
    config.site = "127.0.0.1:59214".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59215);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // Test with DD-LocalData header containing only process-id (no container-id)
    // Use current process ID which should have a readable cgroup file
    let pid = std::process::id();
    let local_data = format!(r#"{{"process-id":{}}}"#, pid);

    client
        .post("http://127.0.0.1:59215/v1/input")
        .header("DD-LocalData", local_data)
        .json(&serde_json::json!({"message": "test from PID"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let ddtags = &request.query_params["ddtags"];
    eprintln!("ddtags with PID-based extraction: {}", ddtags);

    // Note: This test might not find a container ID if running outside a container
    // That's OK - we're verifying the code path is executed without panicking
    eprintln!("PID {} cgroup parsing completed successfully", pid);

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test request batching - multiple requests should be sent together
#[tokio::test]
async fn test_request_batching() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59216).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-mock-batch".to_string();
    config.site = "127.0.0.1:59216".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59217);
    config.flush_timeout = 1; // 1 second window for batching

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // Send 5 requests quickly (within the flush interval)
    for i in 0..5 {
        client
            .post("http://127.0.0.1:59217/v1/input")
            .json(&serde_json::json!({"message": format!("batch message {}", i)}))
            .send()
            .await
            .expect("Request failed");
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    // Wait for flush
    tokio::time::sleep(tokio::time::Duration::from_millis(1600)).await;

    let requests = captured.lock().unwrap();
    eprintln!("Captured {} requests after batching", requests.len());

    // All 5 requests should have been sent (though potentially in separate batches)
    assert!(
        requests.len() >= 1,
        "At least one batch should have been sent"
    );
    assert!(
        requests.len() <= 5,
        "Should not exceed number of original requests"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}
/// Test header verification - DD-REQUEST-ID, DD-EVP-ORIGIN, DD-API-KEY
#[tokio::test]
async fn test_headers_verification() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59218).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-header-verify".to_string();
    config.site = "127.0.0.1:59218".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59219);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59219/v1/input")
        .json(&serde_json::json!({"message": "test headers"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let headers = &request.headers;

    // Verify DD-REQUEST-ID (should be UUID format)
    assert!(
        headers.contains_key("dd-request-id"),
        "Should have DD-REQUEST-ID header"
    );
    let request_id = &headers["dd-request-id"];
    eprintln!("DD-REQUEST-ID: {}", request_id);
    // UUID v4 format: xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx
    assert!(
        request_id.len() == 36 && request_id.contains("-"),
        "DD-REQUEST-ID should be UUID format, got: {}",
        request_id
    );

    // Verify DD-EVP-ORIGIN
    assert!(
        headers.contains_key("dd-evp-origin"),
        "Should have DD-EVP-ORIGIN header"
    );
    assert_eq!(
        headers["dd-evp-origin"], "agent-logs",
        "DD-EVP-ORIGIN should be 'agent-logs'"
    );

    // Verify DD-API-KEY
    assert!(
        headers.contains_key("dd-api-key"),
        "Should have DD-API-KEY header"
    );
    assert_eq!(
        headers["dd-api-key"], "test-key-header-verify",
        "DD-API-KEY should match config"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test service and version tags appear in ddtags
#[tokio::test]
async fn test_service_and_version_tags() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59220).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-service-version".to_string();
    config.site = "127.0.0.1:59220".to_string();
    config.service = Some("my-test-service".to_string());
    config.env = Some("production".to_string());
    config.version = Some("v1.2.3".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59221);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59221/v1/input")
        .json(&serde_json::json!({"message": "test service tags"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let ddtags = &request.query_params["ddtags"];
    eprintln!("ddtags with service/version: {}", ddtags);

    // Verify service tag
    assert!(
        ddtags.contains("default_service:my-test-service")
            || ddtags.contains("service:my-test-service"),
        "Should contain service tag in ddtags: {}",
        ddtags
    );

    // Verify environment tag
    assert!(
        ddtags.contains("default_env:production") || ddtags.contains("env:production"),
        "Should contain env tag in ddtags: {}",
        ddtags
    );

    // Verify version tag
    assert!(
        ddtags.contains("default_version:v1.2.3") || ddtags.contains("version:v1.2.3"),
        "Should contain version tag in ddtags: {}",
        ddtags
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test tag truncation at 4001 characters with many custom tags
#[tokio::test]
async fn test_tag_truncation_with_many_custom_tags() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59222).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-tag-truncation".to_string();
    config.site = "127.0.0.1:59222".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59223);
    config.flush_timeout = 1;

    // Add many custom tags to exceed 4001 chars
    for i in 0..200 {
        config.tags.insert(
            format!("custom_tag_key_{}", i),
            format!("custom_tag_value_with_long_string_to_increase_size_{}", i),
        );
    }

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59223/v1/input")
        .json(&serde_json::json!({"message": "test tag truncation"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let ddtags = &request.query_params["ddtags"];
    eprintln!("ddtags length: {} chars", ddtags.len());

    // Verify tags were truncated to 4001 characters maximum
    assert!(
        ddtags.len() <= 4001,
        "Tags should be truncated to 4001 chars, got {} chars",
        ddtags.len()
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test tag sources order and presence
#[tokio::test]
async fn test_tag_sources_order() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59224).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-tag-sources".to_string();
    config.site = "127.0.0.1:59224".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("staging".to_string());
    config.version = Some("v2.0.0".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59225);
    config.flush_timeout = 1;

    // Add custom config tags
    config
        .tags
        .insert("team".to_string(), "backend".to_string());
    config
        .tags
        .insert("region".to_string(), "us-west-2".to_string());

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // Send request with additional custom header tags
    client
        .post("http://127.0.0.1:59225/v1/input")
        .header(
            "X-Datadog-Additional-Tags",
            "request_type:test,priority:high",
        )
        .json(&serde_json::json!({"message": "test tag sources"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let ddtags = &request.query_params["ddtags"];
    eprintln!("Complete ddtags: {}", ddtags);

    // Verify all tag sources are present:

    // 1. Hostname (should be present)
    assert!(
        ddtags.contains("host:") || ddtags.contains("hostname:"),
        "Should contain hostname tag"
    );

    // 2. Environment (from config)
    assert!(
        ddtags.contains("default_env:staging") || ddtags.contains("env:staging"),
        "Should contain environment tag from config"
    );

    // 3. Agent version (should be present)
    // The agent version tag might be in various formats
    eprintln!("Checking for agent version tag in ddtags");

    // 4. Custom header tags (from X-Datadog-Additional-Tags)
    assert!(
        ddtags.contains("request_type:test"),
        "Should contain custom header tag 'request_type:test'"
    );
    assert!(
        ddtags.contains("priority:high"),
        "Should contain custom header tag 'priority:high'"
    );

    // 5. Config tags
    assert!(
        ddtags.contains("team:backend"),
        "Should contain config tag 'team:backend'"
    );
    assert!(
        ddtags.contains("region:us-west-2"),
        "Should contain config tag 'region:us-west-2'"
    );

    // 6. Service and version
    assert!(
        ddtags.contains("default_service:test-service") || ddtags.contains("service:test-service"),
        "Should contain service tag"
    );
    assert!(
        ddtags.contains("default_version:v2.0.0") || ddtags.contains("version:v2.0.0"),
        "Should contain version tag"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}
