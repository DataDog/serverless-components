//! Integration tests for DogStatsD proxy endpoint
//!
//! Tests the /dogstatsd/v2/proxy HTTP endpoint that receives metrics via HTTP POST
//! and forwards them via UDP to the local DogStatsD server.

use datadog_agent_native::agent::AgentCoordinator;
use datadog_agent_native::config::{operational_mode::OperationalMode, Config};
use std::time::Duration;
use tokio;

/// Helper to create a test config with DogStatsD enabled and trace agent running
fn create_proxy_test_config(dogstatsd_port: u16, trace_agent_port: u16) -> Config {
    let mut config = Config::default();
    config.api_key = "test-api-key-proxy".to_string();
    config.site = "datadoghq.com".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("test".to_string());
    config.apm_dd_url = "http://localhost:8126".to_string();
    config.operational_mode = OperationalMode::HttpFixedPort; // Need HTTP mode to start HTTP server

    // DogStatsD configuration
    config.dogstatsd_enabled = true;
    config.dogstatsd_port = dogstatsd_port;

    // Trace agent configuration
    config.trace_agent_port = Some(trace_agent_port);

    config
}

/// Test 1: Basic proxy forwarding - single metric
#[tokio::test]
async fn test_proxy_single_metric() {
    let dogstatsd_port = 19125;
    let trace_port = 19126;
    let config = create_proxy_test_config(dogstatsd_port, trace_port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");

    // Give servers time to start
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send metric via HTTP POST to proxy endpoint
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{}/dogstatsd/v2/proxy",
            trace_port
        ))
        .body("test.proxy.counter:1|c")
        .send()
        .await
        .expect("Failed to send HTTP request");

    assert_eq!(response.status(), 200, "Proxy endpoint should return 200");

    // Give time for UDP forwarding to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 2: Empty body handling
#[tokio::test]
async fn test_proxy_empty_body() {
    let dogstatsd_port = 19225;
    let trace_port = 19226;
    let config = create_proxy_test_config(dogstatsd_port, trace_port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send empty body
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{}/dogstatsd/v2/proxy",
            trace_port
        ))
        .body("")
        .send()
        .await
        .expect("Failed to send HTTP request");

    // Should still return 200 with empty body
    assert_eq!(response.status(), 200, "Empty body should return 200");

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 3: Multiple metrics - newline delimited
#[tokio::test]
async fn test_proxy_multiple_metrics() {
    let dogstatsd_port = 19325;
    let trace_port = 19326;
    let config = create_proxy_test_config(dogstatsd_port, trace_port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send multiple metrics separated by newlines
    let metrics = "test.counter:1|c\ntest.gauge:42|g\ntest.histogram:100|h";

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{}/dogstatsd/v2/proxy",
            trace_port
        ))
        .body(metrics)
        .send()
        .await
        .expect("Failed to send HTTP request");

    assert_eq!(response.status(), 200, "Multiple metrics should return 200");

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 4: Metrics with tags
#[tokio::test]
async fn test_proxy_metrics_with_tags() {
    let dogstatsd_port = 19425;
    let trace_port = 19426;
    let config = create_proxy_test_config(dogstatsd_port, trace_port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send metric with tags
    let metric = "test.counter:1|c|#env:prod,service:api";

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{}/dogstatsd/v2/proxy",
            trace_port
        ))
        .body(metric)
        .send()
        .await
        .expect("Failed to send HTTP request");

    assert_eq!(response.status(), 200, "Tagged metric should return 200");

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 5: Large payload with many metrics
#[tokio::test]
async fn test_proxy_large_payload() {
    let dogstatsd_port = 19525;
    let trace_port = 19526;
    let config = create_proxy_test_config(dogstatsd_port, trace_port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create a large payload with many metrics
    let mut metrics = Vec::new();
    for i in 0..100 {
        metrics.push(format!("test.metric.{}:{}|c", i, i));
    }
    let payload = metrics.join("\n");

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{}/dogstatsd/v2/proxy",
            trace_port
        ))
        .body(payload)
        .send()
        .await
        .expect("Failed to send HTTP request");

    assert_eq!(response.status(), 200, "Large payload should return 200");

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 6: Rapid consecutive requests
#[tokio::test]
async fn test_proxy_rapid_requests() {
    let dogstatsd_port = 19625;
    let trace_port = 19626;
    let config = create_proxy_test_config(dogstatsd_port, trace_port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = reqwest::Client::new();

    // Send multiple rapid requests
    for i in 0..10 {
        let metric = format!("test.rapid.{}:1|c", i);
        let response = client
            .post(format!(
                "http://127.0.0.1:{}/dogstatsd/v2/proxy",
                trace_port
            ))
            .body(metric)
            .send()
            .await
            .expect("Failed to send HTTP request");

        assert_eq!(
            response.status(),
            200,
            "Rapid request {} should return 200",
            i
        );
    }

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 7: Different metric types
#[tokio::test]
async fn test_proxy_different_metric_types() {
    let dogstatsd_port = 19725;
    let trace_port = 19726;
    let config = create_proxy_test_config(dogstatsd_port, trace_port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = reqwest::Client::new();

    // Test different metric types
    let metric_types = vec![
        "test.counter:1|c",       // Counter
        "test.gauge:42|g",        // Gauge
        "test.histogram:100|h",   // Histogram
        "test.timer:500|ms",      // Timer
        "test.set:user123|s",     // Set
        "test.distribution:25|d", // Distribution
    ];

    for metric in metric_types {
        let response = client
            .post(format!(
                "http://127.0.0.1:{}/dogstatsd/v2/proxy",
                trace_port
            ))
            .body(metric)
            .send()
            .await
            .expect("Failed to send HTTP request");

        assert_eq!(
            response.status(),
            200,
            "Metric '{}' should return 200",
            metric
        );
    }

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 8: Proxy with ephemeral DogStatsD port
#[tokio::test]
async fn test_proxy_with_ephemeral_port() {
    let dogstatsd_port = 0; // Ephemeral port
    let trace_port = 19826;
    let config = create_proxy_test_config(dogstatsd_port, trace_port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Get the actual DogStatsD port
    let actual_port = coordinator
        .get_dogstatsd_port()
        .expect("Should have actual DogStatsD port");

    assert_ne!(actual_port, 0, "Actual port should not be 0");

    // Send metric via proxy - it should forward to the actual ephemeral port
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{}/dogstatsd/v2/proxy",
            trace_port
        ))
        .body("test.ephemeral:1|c")
        .send()
        .await
        .expect("Failed to send HTTP request");

    assert_eq!(
        response.status(),
        200,
        "Proxy with ephemeral port should return 200"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 9: Trailing newlines handling
#[tokio::test]
async fn test_proxy_trailing_newlines() {
    let dogstatsd_port = 19925;
    let trace_port = 19926;
    let config = create_proxy_test_config(dogstatsd_port, trace_port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send metrics with trailing newlines
    let metrics = "test.counter:1|c\ntest.gauge:42|g\n\n";

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{}/dogstatsd/v2/proxy",
            trace_port
        ))
        .body(metrics)
        .send()
        .await
        .expect("Failed to send HTTP request");

    assert_eq!(
        response.status(),
        200,
        "Trailing newlines should be handled gracefully"
    );

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 10: Content-Type flexibility
#[tokio::test]
async fn test_proxy_content_type_flexibility() {
    let dogstatsd_port = 20025;
    let trace_port = 20026;
    let config = create_proxy_test_config(dogstatsd_port, trace_port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = reqwest::Client::new();

    // Test with different content types
    let content_types = vec![
        "text/plain",
        "application/octet-stream",
        "", // No content type
    ];

    for content_type in content_types {
        let mut request_builder = client
            .post(format!(
                "http://127.0.0.1:{}/dogstatsd/v2/proxy",
                trace_port
            ))
            .body("test.counter:1|c");

        if !content_type.is_empty() {
            request_builder = request_builder.header("Content-Type", content_type);
        }

        let response = request_builder
            .send()
            .await
            .expect("Failed to send HTTP request");

        assert_eq!(
            response.status(),
            200,
            "Content-Type '{}' should return 200",
            content_type
        );
    }

    coordinator.shutdown().await.expect("Failed to shutdown");
}
