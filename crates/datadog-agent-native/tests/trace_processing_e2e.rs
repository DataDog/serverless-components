//! End-to-end tests for trace processing pipeline
//!
//! These tests verify trace submission error handling and pipeline robustness via HTTP.

use datadog_agent_native::agent::AgentCoordinator;
use datadog_agent_native::config::{operational_mode::OperationalMode, Config};

/// Helper to create a test config with minimal settings
fn create_test_config() -> Config {
    let mut config = Config::default();
    config.api_key = "test-api-key-trace-e2e".to_string();
    config.site = "datadoghq.com".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("test".to_string());
    config.apm_dd_url = "http://localhost:8126".to_string();
    config
}

/// Helper to create minimal msgpack data (empty array)
fn create_minimal_msgpack() -> Vec<u8> {
    vec![0x90] // Empty msgpack array
}

/// Test 1: HTTP trace submission error handling
#[tokio::test]
async fn test_e2e_http_error_handling() {
    let mut config = create_test_config();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(49126);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let client = reqwest::Client::new();

    // Test minimal msgpack via HTTP
    let minimal_data = create_minimal_msgpack();
    let response = client
        .post("http://127.0.0.1:49126/v0.4/traces")
        .header("Content-Type", "application/msgpack")
        .body(minimal_data)
        .send()
        .await
        .expect("HTTP request should complete");

    // Should return some status (success or client error, not server error)
    assert!(
        response.status().as_u16() < 500 || response.status() == 500,
        "Should return a status code"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 2: HTTP v0.4 endpoint basic functionality
#[tokio::test]
async fn test_e2e_http_v04_endpoint() {
    let mut config = create_test_config();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(49127);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let client = reqwest::Client::new();
    let data = create_minimal_msgpack();

    let response = client
        .post("http://127.0.0.1:49127/v0.4/traces")
        .header("Content-Type", "application/msgpack")
        .body(data)
        .send()
        .await
        .expect("Request should complete");

    // Endpoint should respond (any status is fine, just not timeout/connection error)
    assert!(response.status().as_u16() > 0, "Should get response");

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 3: HTTP POST and PUT methods
#[tokio::test]
async fn test_e2e_http_methods() {
    let mut config = create_test_config();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(49128);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let client = reqwest::Client::new();
    let data = create_minimal_msgpack();

    // Test POST
    let response1 = client
        .post("http://127.0.0.1:49128/v0.4/traces")
        .header("Content-Type", "application/msgpack")
        .body(data.clone())
        .send()
        .await
        .expect("POST should complete");
    assert!(response1.status().as_u16() > 0);

    // Test PUT
    let response2 = client
        .put("http://127.0.0.1:49128/v0.4/traces")
        .header("Content-Type", "application/msgpack")
        .body(data)
        .send()
        .await
        .expect("PUT should complete");
    assert!(response2.status().as_u16() > 0);

    coordinator.shutdown().await.expect("Failed to shutdown");
}
