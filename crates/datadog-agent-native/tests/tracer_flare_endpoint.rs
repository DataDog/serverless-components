//! E2E tests for the tracer flare proxy endpoint.
//!
//! These tests ensure the Rust agent mirrors the Go `/tracer_flare/v1`
//! behaviour by forwarding requests to the Datadog serverless flare intake and
//! injecting the required headers.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use datadog_agent_native::{
    agent::AgentCoordinator,
    config::{operational_mode::OperationalMode, Config},
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const FLARE_PATH: &str = "/api/ui/support/serverless/flare";

#[derive(Clone, Default)]
struct CapturedRequests {
    inner: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

impl CapturedRequests {
    fn new() -> Self {
        Self::default()
    }

    fn capture(&self, headers: &HeaderMap) {
        let map = headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        self.inner.lock().unwrap().push(map);
    }

    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    fn latest(&self) -> HashMap<String, String> {
        self.inner
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("no captured requests")
    }
}

async fn mock_flare_handler(
    State(captured): State<CapturedRequests>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    captured.capture(&headers);
    assert!(
        !body.is_empty(),
        "flare payload should be forwarded to backend"
    );
    (StatusCode::ACCEPTED, "flare-ok".to_string())
}

async fn start_mock_backend(port: u16, captured: CapturedRequests) {
    let app = Router::new()
        .route(FLARE_PATH, post(mock_flare_handler))
        .with_state(captured);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind mock backend");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock backend server failed");
    });
}

fn build_test_config(agent_port: u16, site: String, api_key: &str) -> Config {
    Config {
        api_key: api_key.to_string(),
        site,
        env: Some("test".to_string()),
        service: Some("flare-test".to_string()),
        operational_mode: OperationalMode::HttpFixedPort,
        trace_agent_port: Some(agent_port),
        flush_timeout: 1,
        ..Config::default()
    }
}

#[tokio::test]
async fn tracer_flare_forwards_request_with_api_key() {
    let mock_port = 59600;
    let captured = CapturedRequests::new();
    start_mock_backend(mock_port, captured.clone()).await;

    // Wait for mock to start
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let agent_port = 59601;
    let mut coordinator = AgentCoordinator::new(build_test_config(
        agent_port,
        format!("127.0.0.1:{mock_port}"),
        "flare-test-key",
    ))
    .expect("coordinator");

    coordinator
        .start()
        .await
        .expect("failed to start coordinator");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{agent_port}/tracer_flare/v1?reason=test"
        ))
        .header("content-type", "application/json")
        .body(r#"{"payload":true}"#)
        .send()
        .await
        .expect("flare request failed");

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.text().await.unwrap(), "flare-ok");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    assert_eq!(
        captured.len(),
        1,
        "backend should receive exactly 1 request"
    );
    let headers = captured.latest();
    assert_eq!(
        headers.get("dd-api-key"),
        Some(&"flare-test-key".to_string())
    );
    assert!(
        headers.contains_key("user-agent"),
        "user-agent header should be present"
    );
    assert!(
        headers.contains_key("x-datadog-hostname"),
        "hostname header should be injected"
    );

    coordinator
        .shutdown()
        .await
        .expect("failed to shutdown coordinator");
}
