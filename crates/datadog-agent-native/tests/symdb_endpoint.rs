//! E2E tests for the Symbol Database (SymDB) endpoint with mock backend
//!
//! These tests verify that the correct headers are added to requests
//! forwarded to the backend. Unlike debugger diagnostics, SymDB uses
//! X-Datadog-Additional-Tags header instead of ddtags query parameter.

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
    mock.start(59400).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Send request directly to mock backend
    let client = reqwest::Client::new();
    let response = client
        .post("http://127.0.0.1:59400/api/v2/debugger")
        .header("dd-request-id", "test-id-123")
        .header("dd-api-key", "test-key")
        .header("X-Datadog-Additional-Tags", "test:value")
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
    mock.start(59401).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Start agent pointing to mock backend
    let mut config = Config::default();
    config.api_key = "test-key-symdb-1".to_string();
    config.site = "127.0.0.1:59401".to_string(); // Point to mock backend
    config.service = Some("test-service".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59402);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Send request to SymDB endpoint
    let client = reqwest::Client::new();
    let symdb_payload = serde_json::json!({"type": "jvm", "data": "symbol_data"});

    let response = client
        .post("http://127.0.0.1:59402/symdb/v1/input")
        .header("Content-Type", "application/json")
        .json(&symdb_payload)
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
    mock.start(59403).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-symdb-2".to_string();
    config.site = "127.0.0.1:59403".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59404);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59404/symdb/v1/input")
        .json(&serde_json::json!({"type": "jvm"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured by mock backend");

    let request = &requests[0];
    assert_eq!(
        request.headers.get("dd-evp-origin"),
        Some(&"agent-symdb".to_string()),
        "DD-EVP-ORIGIN should be 'agent-symdb'"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that X-Datadog-Additional-Tags header is built with all tag sources
#[tokio::test]
async fn test_additional_tags_header() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59405).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-symdb-3".to_string();
    config.site = "127.0.0.1:59405".to_string();
    config.env = Some("production".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59406);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59406/symdb/v1/input")
        .header("X-Datadog-Additional-Tags", "custom:value,priority:high")
        .json(&serde_json::json!({"type": "jvm"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    assert!(
        request.headers.contains_key("x-datadog-additional-tags"),
        "X-Datadog-Additional-Tags header not found"
    );

    let tags = &request.headers["x-datadog-additional-tags"];
    eprintln!("X-Datadog-Additional-Tags: {}", tags);

    // Verify all tag sources are included (but NOT default_service or default_version)
    assert!(tags.contains("host:"), "Should contain hostname tag");
    assert!(
        tags.contains("default_env:production"),
        "Should contain environment tag"
    );
    assert!(
        tags.contains("agent_version:"),
        "Should contain agent version tag"
    );
    assert!(
        tags.contains("custom:value"),
        "Should contain custom header tags"
    );
    assert!(
        tags.contains("priority:high"),
        "Should contain custom header tags"
    );

    // Verify service and version are NOT added (this was the bug we fixed)
    assert!(
        !tags.contains("default_service:"),
        "Should NOT contain default_service tag"
    );
    assert!(
        !tags.contains("default_version:"),
        "Should NOT contain default_version tag"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that container tags are included in X-Datadog-Additional-Tags
#[tokio::test]
async fn test_container_tags_in_header() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59407).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-symdb-4".to_string();
    config.site = "127.0.0.1:59407".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59408);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59408/symdb/v1/input")
        .header("Datadog-Container-ID", "abc123def456")
        .json(&serde_json::json!({"type": "jvm"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let tags = &request.headers["x-datadog-additional-tags"];
    eprintln!("X-Datadog-Additional-Tags with container: {}", tags);

    // Verify container tags are included
    assert!(
        tags.contains("container_id:abc123def456"),
        "Should contain container_id tag from tagger"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that tags are NOT truncated (different from debugger diagnostics)
#[tokio::test]
async fn test_no_tag_truncation() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59409).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-symdb-5".to_string();
    config.site = "127.0.0.1:59409".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59410);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Create very long tags that would exceed 4001 characters
    let long_tags = "x".repeat(5000);

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59410/symdb/v1/input")
        .header(
            "X-Datadog-Additional-Tags",
            format!("longtag:{}", long_tags),
        )
        .json(&serde_json::json!({"type": "jvm"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let tags = &request.headers["x-datadog-additional-tags"];

    eprintln!("X-Datadog-Additional-Tags length: {}", tags.len());

    // SymDB does NOT truncate tags (unlike debugger diagnostics)
    assert!(
        tags.len() > 4001,
        "Tags should NOT be truncated for SymDB endpoint, got {} chars",
        tags.len()
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test DD-API-KEY header is added
#[tokio::test]
async fn test_dd_api_key_header() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59411).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-secret-symdb-key-12345".to_string();
    config.site = "127.0.0.1:59411".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59412);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59412/symdb/v1/input")
        .json(&serde_json::json!({"type": "jvm"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    assert_eq!(
        request.headers.get("dd-api-key"),
        Some(&"test-secret-symdb-key-12345".to_string()),
        "DD-API-KEY should match configured API key"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that X-Datadog-Additional-Tags merges with existing header
#[tokio::test]
async fn test_additional_tags_merges_with_existing_header() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59413).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-symdb-merge".to_string();
    config.site = "127.0.0.1:59413".to_string();
    config.env = Some("staging".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59414);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    // Send request with X-Datadog-Additional-Tags already set
    client
        .post("http://127.0.0.1:59414/symdb/v1/input")
        .header("X-Datadog-Additional-Tags", "from_header:value")
        .json(&serde_json::json!({"type": "jvm"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let tags = &request.headers["x-datadog-additional-tags"];
    eprintln!("Merged X-Datadog-Additional-Tags: {}", tags);

    // Verify both agent-added tags and existing header tags are present
    assert!(
        tags.contains("host:"),
        "Should contain agent-added hostname tag"
    );
    assert!(
        tags.contains("default_env:staging"),
        "Should contain agent-added env tag"
    );
    assert!(
        tags.contains("from_header:value"),
        "Should contain original header tag"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that ddtags query parameter is also merged into X-Datadog-Additional-Tags
#[tokio::test]
async fn test_ddtags_query_param_merged_into_header() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59415).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-symdb-ddtags".to_string();
    config.site = "127.0.0.1:59415".to_string();
    config.env = Some("test".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59416);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    // Send request with ddtags query parameter (Go's SymDB also merges these)
    client
        .post("http://127.0.0.1:59416/symdb/v1/input?ddtags=from_query:value")
        .json(&serde_json::json!({"type": "jvm"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let tags = &request.headers["x-datadog-additional-tags"];
    eprintln!("X-Datadog-Additional-Tags with ddtags: {}", tags);

    // Verify both agent-added tags and query parameter tags are present
    assert!(
        tags.contains("host:"),
        "Should contain agent-added hostname tag"
    );
    assert!(
        tags.contains("default_env:test"),
        "Should contain agent-added env tag"
    );
    assert!(
        tags.contains("from_query:value"),
        "Should contain ddtags query parameter"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test that config.tags are NOT included (regression test for bug fix)
#[tokio::test]
async fn test_config_tags_not_included() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59417).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-symdb-config-tags".to_string();
    config.site = "127.0.0.1:59417".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59418);
    config.flush_timeout = 1;

    // Add config tags - these should NOT appear in X-Datadog-Additional-Tags
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
        .post("http://127.0.0.1:59418/symdb/v1/input")
        .json(&serde_json::json!({"type": "jvm"}))
        .send()
        .await
        .expect("Request failed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];
    let tags = &request.headers["x-datadog-additional-tags"];
    eprintln!("X-Datadog-Additional-Tags without config.tags: {}", tags);

    // Verify config.tags are NOT included (this was the bug we fixed in Phase 2)
    assert!(
        !tags.contains("team:backend"),
        "Should NOT contain config.tags"
    );
    assert!(
        !tags.contains("region:us-east-1"),
        "Should NOT contain config.tags"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test multipart form data handling
#[tokio::test]
async fn test_multipart_form_data() {
    let mock = MockBackend::new();
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59419).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-symdb-multipart".to_string();
    config.site = "127.0.0.1:59419".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59420);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Create a simple multipart body manually (without reqwest multipart feature)
    let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";
    let multipart_body = format!(
        "--{}\r\nContent-Disposition: form-data; name=\"type\"\r\n\r\njvm\r\n--{}\r\nContent-Disposition: form-data; name=\"symbols\"; filename=\"data.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n\x01\x02\x03\x04\x05\r\n--{}--\r\n",
        boundary, boundary, boundary
    );

    let client = reqwest::Client::new();
    let response = client
        .post("http://127.0.0.1:59420/symdb/v1/input")
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={}", boundary),
        )
        .body(multipart_body.clone())
        .send()
        .await
        .expect("Request failed");

    assert_eq!(response.status(), 200, "Multipart request should succeed");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty(), "No requests captured");

    let request = &requests[0];

    // Verify multipart content-type was preserved
    let content_type = &request.headers.get("content-type");
    assert!(
        content_type.is_some(),
        "Content-Type header should be present"
    );
    assert!(
        content_type.unwrap().starts_with("multipart/form-data"),
        "Content-Type should be multipart/form-data, got: {}",
        content_type.unwrap()
    );

    // Verify body is not empty (contains multipart data)
    assert!(
        !request.body.is_empty(),
        "Body should contain multipart data"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}
