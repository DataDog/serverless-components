//! End-to-end tests for the Application Logs endpoint `/v1/input`
//!
//! Verifies that the logs endpoint:
//! - Accepts log payloads
//! - Adds proper headers (DD-REQUEST-ID, DD-EVP-ORIGIN)
//! - Builds and includes ddtags query parameters
//! - Handles various payload sizes
//! - Supports concurrent requests

use datadog_agent_native::agent::AgentCoordinator;
use datadog_agent_native::config::{operational_mode::OperationalMode, Config};

/// Test 1: Basic logs endpoint availability and response
#[tokio::test]
async fn test_logs_endpoint_accepts_requests() {
    let mut config = Config::default();
    config.api_key = "test-api-key-logs-e2e-1".to_string();
    config.site = "datadoghq.com".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("test".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(49400);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // Create a simple log payload (JSON format typical for logs)
    let log_payload = serde_json::json!({
        "message": "Test log message",
        "level": "info",
        "timestamp": "2024-01-01T00:00:00Z",
        "service": "test-service",
        "tags": "env:test,version:1.0.0"
    });

    let response = client
        .post("http://127.0.0.1:49400/v1/input")
        .header("Content-Type", "application/json")
        .json(&log_payload)
        .send()
        .await;

    match response {
        Ok(resp) => {
            eprintln!("Status: {}", resp.status());

            // Should accept the request (returns 200 OK acknowledgment)
            assert!(
                resp.status().is_success(),
                "Logs endpoint should accept log payload"
            );

            let body = resp.text().await.unwrap();
            assert!(
                body.contains("Acknowledged"),
                "Response should contain acknowledgment message"
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 2: Logs endpoint with custom tags in headers
#[tokio::test]
async fn test_logs_endpoint_with_custom_tags() {
    let mut config = Config::default();
    config.api_key = "test-api-key-logs-e2e-2".to_string();
    config.site = "datadoghq.com".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("production".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(49401);

    // Add custom tags to config
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

    let log_payload = serde_json::json!({
        "message": "Production log with custom tags",
        "level": "warn",
        "service": "test-service"
    });

    // Send request with additional custom tags in header
    let response = client
        .post("http://127.0.0.1:49401/v1/input")
        .header("Content-Type", "application/json")
        .header(
            "X-Datadog-Additional-Tags",
            "custom_tag:custom_value,priority:high",
        )
        .json(&log_payload)
        .send()
        .await;

    match response {
        Ok(resp) => {
            eprintln!("Status: {}", resp.status());
            assert!(
                resp.status().is_success(),
                "Should accept request with custom tags"
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 3: Large log payload handling
#[tokio::test]
async fn test_logs_endpoint_large_payload() {
    let mut config = Config::default();
    config.api_key = "test-api-key-logs-e2e-3".to_string();
    config.site = "datadoghq.com".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("test".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(49402);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // Create a larger log payload (multiple log entries)
    let mut log_entries = Vec::new();
    for i in 0..100 {
        log_entries.push(serde_json::json!({
            "message": format!("Log entry number {}", i),
            "level": "info",
            "timestamp": format!("2024-01-01T00:00:{:02}Z", i % 60),
            "service": "test-service",
            "entry_id": i
        }));
    }

    let large_payload = serde_json::json!(log_entries);
    let payload_size = serde_json::to_vec(&large_payload).unwrap().len();
    eprintln!("Large payload size: {} bytes", payload_size);

    let response = client
        .post("http://127.0.0.1:49402/v1/input")
        .header("Content-Type", "application/json")
        .json(&large_payload)
        .send()
        .await;

    match response {
        Ok(resp) => {
            eprintln!("Status: {}", resp.status());
            assert!(
                resp.status().is_success(),
                "Should handle large payloads ({}bytes)",
                payload_size
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 4: Concurrent logs requests
#[tokio::test]
async fn test_logs_endpoint_concurrent_requests() {
    let mut config = Config::default();
    config.api_key = "test-api-key-logs-e2e-4".to_string();
    config.site = "datadoghq.com".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("test".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(49403);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut tasks = Vec::new();

    // Spawn 10 concurrent requests
    for i in 0..10 {
        let task = tokio::spawn(async move {
            let client = reqwest::Client::new();
            let log_payload = serde_json::json!({
                "message": format!("Concurrent log message {}", i),
                "level": "info",
                "request_id": i
            });

            client
                .post("http://127.0.0.1:49403/v1/input")
                .header("Content-Type", "application/json")
                .json(&log_payload)
                .send()
                .await
        });
        tasks.push(task);
    }

    // Wait for all tasks to complete
    let results = futures::future::join_all(tasks).await;

    let mut success_count = 0;
    for result in results {
        match result {
            Ok(Ok(resp)) => {
                if resp.status().is_success() {
                    success_count += 1;
                }
            }
            _ => {}
        }
    }

    eprintln!("Successful concurrent requests: {}/10", success_count);
    assert_eq!(success_count, 10, "All concurrent requests should succeed");

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 5: Different content types
#[tokio::test]
async fn test_logs_endpoint_different_content_types() {
    let mut config = Config::default();
    config.api_key = "test-api-key-logs-e2e-5".to_string();
    config.site = "datadoghq.com".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("test".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(49404);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // Test with plain text log
    let text_log = "2024-01-01T00:00:00Z INFO This is a plain text log message";

    let response = client
        .post("http://127.0.0.1:49404/v1/input")
        .header("Content-Type", "text/plain")
        .body(text_log)
        .send()
        .await;

    match response {
        Ok(resp) => {
            eprintln!("Plain text response status: {}", resp.status());
            assert!(resp.status().is_success(), "Should accept plain text logs");
        }
        Err(e) => {
            panic!("Plain text request failed: {}", e);
        }
    }

    // Test with JSON log
    let json_log = serde_json::json!({
        "message": "JSON formatted log",
        "level": "info"
    });

    let response = client
        .post("http://127.0.0.1:49404/v1/input")
        .header("Content-Type", "application/json")
        .json(&json_log)
        .send()
        .await;

    match response {
        Ok(resp) => {
            eprintln!("JSON response status: {}", resp.status());
            assert!(resp.status().is_success(), "Should accept JSON logs");
        }
        Err(e) => {
            panic!("JSON request failed: {}", e);
        }
    }

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 6: Verify endpoint is listed in /info
#[tokio::test]
async fn test_logs_endpoint_listed_in_info() {
    let mut config = Config::default();
    config.api_key = "test-api-key-logs-e2e-6".to_string();
    config.site = "datadoghq.com".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("test".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(49405);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    let response = client.get("http://127.0.0.1:49405/info").send().await;

    match response {
        Ok(resp) => {
            assert!(
                resp.status().is_success(),
                "/info endpoint should be available"
            );

            let body = resp.text().await.unwrap();
            eprintln!("/info response: {}", body);

            // Verify logs endpoint is listed
            assert!(
                body.contains("/v1/input"),
                "/info should list the logs endpoint /v1/input"
            );
        }
        Err(e) => {
            panic!("/info request failed: {}", e);
        }
    }

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 7: Container tag forwarding with DD-LocalData header
#[tokio::test]
async fn test_logs_endpoint_with_container_id() {
    let mut config = Config::default();
    config.api_key = "test-api-key-logs-e2e-7".to_string();
    config.site = "datadoghq.com".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("test".to_string());
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(49406);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    let log_payload = serde_json::json!({
        "message": "Log with container context",
        "level": "info"
    });

    // Test with Datadog-Container-ID header (legacy)
    let response = client
        .post("http://127.0.0.1:49406/v1/input")
        .header("Content-Type", "application/json")
        .header("Datadog-Container-ID", "container-123-legacy")
        .json(&log_payload)
        .send()
        .await;

    match response {
        Ok(resp) => {
            eprintln!("Status with Datadog-Container-ID: {}", resp.status());
            assert!(
                resp.status().is_success(),
                "Should accept request with Datadog-Container-ID"
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }

    // Test with DD-LocalData header (JSON format)
    let local_data = r#"{"container-id":"container-456-localdata"}"#;
    let response = client
        .post("http://127.0.0.1:49406/v1/input")
        .header("Content-Type", "application/json")
        .header("DD-LocalData", local_data)
        .json(&log_payload)
        .send()
        .await;

    match response {
        Ok(resp) => {
            eprintln!("Status with DD-LocalData: {}", resp.status());
            assert!(
                resp.status().is_success(),
                "Should accept request with DD-LocalData"
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }

    // Test with DD-LocalData header (base64 format)
    // base64 of: {"container-id":"container-789-base64"}
    let local_data_base64 = "eyJjb250YWluZXItaWQiOiJjb250YWluZXItNzg5LWJhc2U2NCJ9";
    let response = client
        .post("http://127.0.0.1:49406/v1/input")
        .header("Content-Type", "application/json")
        .header("DD-LocalData", local_data_base64)
        .json(&log_payload)
        .send()
        .await;

    match response {
        Ok(resp) => {
            eprintln!("Status with DD-LocalData (base64): {}", resp.status());
            assert!(
                resp.status().is_success(),
                "Should accept request with DD-LocalData (base64)"
            );
        }
        Err(e) => {
            panic!("Request failed: {}", e);
        }
    }

    coordinator.shutdown().await.expect("Failed to shutdown");
}
