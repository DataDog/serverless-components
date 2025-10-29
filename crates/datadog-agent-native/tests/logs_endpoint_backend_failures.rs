//! Backend failure and retry logic tests for the Application Logs endpoint
//!
//! These tests verify that the proxy flusher correctly handles backend failures
//! and implements retry logic.

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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Mock backend that can simulate failures
struct FailingMockBackend {
    /// Requests received (successful)
    captured_requests: Arc<Mutex<Vec<CapturedRequest>>>,
    /// Number of times the backend has been called
    call_count: Arc<AtomicUsize>,
    /// How many times to fail before succeeding
    fail_count: usize,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct CapturedRequest {
    headers: HashMap<String, String>,
    query_params: HashMap<String, String>,
    body: Vec<u8>,
}

impl FailingMockBackend {
    fn new(fail_count: usize) -> Self {
        Self {
            captured_requests: Arc::new(Mutex::new(Vec::new())),
            call_count: Arc::new(AtomicUsize::new(0)),
            fail_count,
        }
    }

    async fn handler(
        Query(params): Query<HashMap<String, String>>,
        headers: HeaderMap,
        body: Bytes,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
        call_count: Arc<AtomicUsize>,
        fail_count: usize,
    ) -> (StatusCode, &'static str) {
        let count = call_count.fetch_add(1, Ordering::SeqCst);
        eprintln!("Backend call #{} (fail_count={})", count + 1, fail_count);

        // Fail the first N requests
        if count < fail_count {
            eprintln!("Returning 500 Internal Server Error");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        }

        // After fail_count, start succeeding
        let header_map: HashMap<String, String> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        captured.lock().unwrap().push(CapturedRequest {
            headers: header_map,
            query_params: params,
            body: body.to_vec(),
        });

        eprintln!("Returning 202 Accepted");
        (StatusCode::ACCEPTED, "{}")
    }

    async fn start(self, port: u16) -> tokio::task::JoinHandle<()> {
        let captured = Arc::clone(&self.captured_requests);
        let call_count = Arc::clone(&self.call_count);
        let fail_count = self.fail_count;

        let app = Router::new().route(
            "/api/v2/logs",
            post(move |query, headers, body| {
                let captured = Arc::clone(&captured);
                let call_count = Arc::clone(&call_count);
                async move {
                    Self::handler(query, headers, body, captured, call_count, fail_count).await
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
            .await
            .expect("Failed to bind failing mock backend");

        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("Failing mock backend server failed");
        })
    }
}

/// Test backend returning 500 error initially, then succeeding (retry logic)
#[tokio::test]
async fn test_backend_failure_with_retry() {
    // Backend will fail the first 2 attempts, then succeed on the 3rd
    let mock = FailingMockBackend::new(2);
    let captured = Arc::clone(&mock.captured_requests);
    let call_count = Arc::clone(&mock.call_count);
    mock.start(59220).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-failing-backend".to_string();
    config.site = "127.0.0.1:59220".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59221);
    config.flush_timeout = 1; // Quick flush for testing

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59221/v1/input")
        .json(&serde_json::json!({"message": "test with retry"}))
        .send()
        .await
        .expect("Request failed");

    // Wait for multiple flush attempts (with retries)
    // First flush will fail (backend returns 500) - happens at ~1s
    // Second flush will retry the failed request - happens at ~2s
    // Third flush will retry again - happens at ~3s
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    let total_calls = call_count.load(Ordering::SeqCst);
    eprintln!("Total backend calls: {}", total_calls);

    // Should have multiple attempts due to retries across multiple flush intervals
    // Note: The retry happens on the NEXT flush interval, not immediately
    assert!(
        total_calls >= 2,
        "Should have at least 2 backend calls (initial + retry), got {}",
        total_calls
    );

    // Eventually the request should succeed
    let requests = captured.lock().unwrap();
    assert!(
        !requests.is_empty(),
        "Request should eventually succeed after retries"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test backend continuously failing (verify retry attempts are made)
#[tokio::test]
async fn test_backend_continuous_failure() {
    // Backend will always fail
    let mock = FailingMockBackend::new(100); // Fail many times
    let call_count = Arc::clone(&mock.call_count);
    let captured = Arc::clone(&mock.captured_requests);
    mock.start(59222).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-always-failing".to_string();
    config.site = "127.0.0.1:59222".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59223);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:59223/v1/input")
        .json(&serde_json::json!({"message": "test with continuous failure"}))
        .send()
        .await
        .expect("Request failed");

    // Wait for multiple flush cycles with retry attempts
    // First flush will fail (backend returns 500) - happens at ~1s
    // Second flush will retry the failed request - happens at ~2s
    // Third flush will retry again - happens at ~3s
    // Fourth flush will retry again - happens at ~4s
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    let total_calls = call_count.load(Ordering::SeqCst);
    eprintln!("Total failed backend calls: {}", total_calls);

    // Should have multiple retry attempts across multiple flush intervals
    // Note: The retry happens on the NEXT flush interval, not immediately
    assert!(
        total_calls >= 2,
        "Should have at least 2 backend calls (retries), got {}",
        total_calls
    );

    // Should not have any successful requests captured
    let requests = captured.lock().unwrap();
    assert!(
        requests.is_empty(),
        "Should not have successful requests when backend always fails"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test backend recovering after initial failures
#[tokio::test]
async fn test_backend_recovery() {
    // Fail first request, succeed on subsequent ones
    let mock = FailingMockBackend::new(1);
    let captured = Arc::clone(&mock.captured_requests);
    let call_count = Arc::clone(&mock.call_count);
    mock.start(59224).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.api_key = "test-key-recovering-backend".to_string();
    config.site = "127.0.0.1:59224".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort;
    config.trace_agent_port = Some(59225);
    config.flush_timeout = 1;

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // Send first request
    client
        .post("http://127.0.0.1:59225/v1/input")
        .json(&serde_json::json!({"message": "first request"}))
        .send()
        .await
        .expect("Request failed");

    // Wait for flush and retry
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    // Send second request (backend should be healthy now)
    client
        .post("http://127.0.0.1:59225/v1/input")
        .json(&serde_json::json!({"message": "second request"}))
        .send()
        .await
        .expect("Request failed");

    // Wait for flush
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    let total_calls = call_count.load(Ordering::SeqCst);
    let successful_requests = captured.lock().unwrap().len();

    eprintln!("Total backend calls: {}", total_calls);
    eprintln!("Successful requests: {}", successful_requests);

    // Should have recovered and processed requests
    assert!(
        total_calls >= 2,
        "Should have at least 2 backend calls, got {}",
        total_calls
    );

    assert!(
        successful_requests >= 1,
        "Should have at least 1 successful request after recovery, got {}",
        successful_requests
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}
