//! E2E tests for the Debugger Diagnostics endpoint with mock backend
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
            "/api/v2/debugger",
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
    mock.start(59300).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Send request directly to mock backend
    let client = reqwest::Client::new();
    let response = client
        .post("http://127.0.0.1:59300/api/v2/debugger?ddtags=test:value")
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
    mock.start(59301).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Start agent pointing to mock backend
    let mut config = Config::default();
    config.api_key = "test-key-debugger-1".to_string();
    config.site = "127.0.0.1:59301".to_string(); // Point to mock backend
    config.service = Some("test-service".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59302);
    config.flush_timeout = 1; // Short flush interval for tests

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Send request to debugger diagnostics endpoint
    let client = reqwest::Client::new();
    let diagnostic_payload = serde_json::json!({"status": "ok", "diagnostics": []});

    let response = client
        .post("http://127.0.0.1:59302/debugger/v1/diagnostics")
        .header("Content-Type", "application/json")
        .json(&diagnostic_payload)
        .send()
        .await
        .expect("Request failed");

    assert_eq!(response.status(), 200, "Request should succeed");

    // Wait for request to be forwarded
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
    mock.start(59303).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-debugger-2".to_string();
    config.site = "127.0.0.1:59303".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59304);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59304/debugger/v1/diagnostics")
        .json(&serde_json::json!({"status": "ok"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured by mock backend");

    let request = &requests[0];
    assert_eq!(
        request.headers.get("dd-evp-origin"),
        Some(&"agent-debugger".to_string()),
        "DD-EVP-ORIGIN should be 'agent-debugger'"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that ddtags query parameter is built with all tag sources
#[tokio::test]
async fn test_ddtags_query_parameter() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59305).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-debugger-3".to_string();
    config.site = "127.0.0.1:59305".to_string();
    config.env = Some("production".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59306);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59306/debugger/v1/diagnostics")
        .header("X-Datadog-Additional-Tags", "custom:value,priority:high")
        .json(&serde_json::json!({"status": "ok"}))
        .send()
        .await
        .expect("Request failed");

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

    // Verify all tag sources are included (but NOT default_service or default_version)
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

    // Verify service and version are NOT added (this was the bug we fixed)
    assert!(
        !ddtags.contains("default_service:"),
        "Should NOT contain default_service tag"
    );
    assert!(
        !ddtags.contains("default_version:"),
        "Should NOT contain default_version tag"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that container tags are included in ddtags
#[tokio::test]
async fn test_container_tags_in_ddtags() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59307).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-debugger-4".to_string();
    config.site = "127.0.0.1:59307".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59308);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59308/debugger/v1/diagnostics")
        .header("Datadog-Container-ID", "abc123def456")
        .json(&serde_json::json!({"status": "ok"}))
        .send()
        .await
        .expect("Request failed");

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
    mock.start(59309).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-debugger-5".to_string();
    config.site = "127.0.0.1:59309".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59310);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Create very long tags to exceed 4001 character limit
    let long_tags = "x".repeat(5000);

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59310/debugger/v1/diagnostics")
        .header(
            "X-Datadog-Additional-Tags",
            format!("longtag:{}", long_tags),
        )
        .json(&serde_json::json!({"status": "ok"}))
        .send()
        .await
        .expect("Request failed");

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
    mock.start(59311).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-secret-debugger-key-12345".to_string();
    config.site = "127.0.0.1:59311".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59312);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59312/debugger/v1/diagnostics")
        .json(&serde_json::json!({"status": "ok"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    assert_eq!(
        request.headers.get("dd-api-key"),
        Some(&"test-secret-debugger-key-12345".to_string()),
        "DD-API-KEY should match configured API key"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that ddtags merges with existing query parameter
#[tokio::test]
async fn test_ddtags_merges_with_query_param() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59313).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-debugger-merge".to_string();
    config.site = "127.0.0.1:59313".to_string();
    config.env = Some("staging".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59314);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    // Send request with ddtags in query parameter
    client
        .post("http://127.0.0.1:59314/debugger/v1/diagnostics?ddtags=from_query:value")
        .json(&serde_json::json!({"status": "ok"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let ddtags = &request.query_params["ddtags"];
    eprintln!("Merged ddtags: {}", ddtags);

    // Verify both agent-added tags and query parameter tags are present
    assert!(
        ddtags.contains("host:"),
        "Should contain agent-added hostname tag"
    );
    assert!(
        ddtags.contains("default_env:staging"),
        "Should contain agent-added env tag"
    );
    assert!(
        ddtags.contains("from_query:value"),
        "Should contain original query parameter tag"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that config.tags are NOT included (regression test for bug fix)
#[tokio::test]
async fn test_config_tags_not_included() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59315).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-debugger-config-tags".to_string();
    config.site = "127.0.0.1:59315".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59316);
    config.flush_timeout = 1;

    // Add config tags - these should NOT appear in ddtags
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
        .post("http://127.0.0.1:59316/debugger/v1/diagnostics")
        .json(&serde_json::json!({"status": "ok"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let ddtags = &request.query_params["ddtags"];
    eprintln!("ddtags without config.tags: {}", ddtags);

    // Verify config.tags are NOT included (this was the bug we fixed in Phase 1)
    assert!(
        !ddtags.contains("team:backend"),
        "Should NOT contain config.tags"
    );
    assert!(
        !ddtags.contains("region:us-east-1"),
        "Should NOT contain config.tags"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}
