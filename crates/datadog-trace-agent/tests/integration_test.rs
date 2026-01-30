// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

mod common;

use common::helpers::{create_test_trace_payload, send_tcp_request};
use common::mock_server::MockServer;
use common::mocks::{MockEnvVerifier, MockStatsFlusher, MockStatsProcessor, MockTraceFlusher};
use datadog_trace_agent::{
    config::test_helpers::create_tcp_test_config, mini_agent::MiniAgent,
    proxy_flusher::ProxyFlusher, trace_flusher::TraceFlusher,
    trace_processor::ServerlessTraceProcessor,
};
use http_body_util::BodyExt;
use hyper::StatusCode;
use serde_json::Value;
use serial_test::serial;
use std::sync::Arc;
use std::time::Duration;

#[cfg(all(windows, feature = "windows-pipes"))]
use common::helpers::send_named_pipe_request;

const FLUSH_WAIT_DURATION: Duration = Duration::from_millis(1500);

/// Helper to configure a config with mock server endpoints
pub fn configure_mock_endpoints(config: &mut Config, mock_server_url: &str) {
    let trace_url = format!("{}/api/v0.2/traces", mock_server_url);
    let stats_url = format!("{}/api/v0.6/stats", mock_server_url);

    config.trace_intake = libdd_common::Endpoint {
        url: trace_url.parse().unwrap(),
        api_key: Some("test-api-key".into()),
        ..Default::default()
    };
    config.trace_stats_intake = libdd_common::Endpoint {
        url: stats_url.parse().unwrap(),
        api_key: Some("test-api-key".into()),
        ..Default::default()
    };
    config.trace_flush_interval_secs = 1;
    config.stats_flush_interval_secs = 1;
}

/// Helper to create a mini agent with real flushers
pub fn create_mini_agent_with_real_flushers(config: Arc<Config>) -> MiniAgent {
    use datadog_trace_agent::{
        aggregator::TraceAggregator, stats_flusher::ServerlessStatsFlusher,
        stats_processor::ServerlessStatsProcessor, trace_flusher::ServerlessTraceFlusher,
    };

    let aggregator = Arc::new(tokio::sync::Mutex::new(TraceAggregator::default()));
    MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(ServerlessTraceProcessor {}),
        trace_flusher: Arc::new(ServerlessTraceFlusher::new(
            aggregator.clone(),
            config.clone(),
        )),
        stats_processor: Arc::new(ServerlessStatsProcessor {}),
        stats_flusher: Arc::new(ServerlessStatsFlusher {}),
        env_verifier: Arc::new(MockEnvVerifier),
        proxy_flusher: Arc::new(ProxyFlusher::new(config.clone())),
    }
}

/// Helper to verify trace request sent to mock server
pub fn verify_trace_request(mock_server: &common::mock_server::MockServer) {
    let trace_reqs = mock_server.get_requests_for_path("/api/v0.2/traces");

    assert!(
        !trace_reqs.is_empty(),
        "Expected at least one trace request to mock server"
    );

    let trace_req = &trace_reqs[0];
    assert_eq!(trace_req.method, "POST", "Expected POST method");

    let content_type = trace_req
        .headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == "content-type")
        .map(|(_, v)| v.as_str());
    assert_eq!(
        content_type,
        Some("application/x-protobuf"),
        "Expected protobuf content-type"
    );

    let api_key = trace_req
        .headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == "dd-api-key")
        .map(|(_, v)| v.as_str());
    assert_eq!(api_key, Some("test-api-key"), "Expected API key header");

    assert!(
        !trace_req.body.is_empty(),
        "Expected non-empty trace payload"
    );
}

/// Create a test config with TCP transport
pub fn create_tcp_test_config(port: u16) -> Config {
    Config {
        dd_site: "mock-datadoghq.com".to_string(),
        dd_apm_receiver_port: port,
        dd_apm_windows_pipe_name: None,
        dd_dogstatsd_port: 8125,
        dd_dogstatsd_windows_pipe_name: None,
        env_type: trace_utils::EnvironmentType::AzureFunction,
        app_name: Some("test-app".to_string()),
        max_request_content_length: 10_000_000,
        obfuscation_config: libdd_trace_obfuscation::obfuscation_config::ObfuscationConfig::new()
            .unwrap(),
        os: std::env::consts::OS.to_string(),
        tags: datadog_trace_agent::config::Tags::new(),
        stats_flush_interval_secs: 10,
        trace_flush_interval_secs: 5,
        trace_intake: libdd_common::Endpoint::default(),
        trace_stats_intake: libdd_common::Endpoint::default(),
        profiling_intake: libdd_common::Endpoint::default(),
        proxy_request_timeout_secs: 30,
        proxy_request_max_retries: 3,
        proxy_request_retry_backoff_base_ms: 100,
        verify_env_timeout_ms: 1000,
        proxy_url: None,
    }
}

#[cfg(test)]
#[tokio::test]
#[serial]
async fn test_mini_agent_tcp_handles_requests() {
    let config = Arc::new(create_tcp_test_config(8126));
    let test_port = config.dd_apm_receiver_port;
    let mini_agent = MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(ServerlessTraceProcessor {}),
        trace_flusher: Arc::new(MockTraceFlusher),
        stats_processor: Arc::new(MockStatsProcessor),
        stats_flusher: Arc::new(MockStatsFlusher),
        env_verifier: Arc::new(MockEnvVerifier),
        proxy_flusher: Arc::new(ProxyFlusher::new(config)),
    };

    // Start the mini agent
    let agent_handle = tokio::spawn(async move {
        let _ = mini_agent.start_mini_agent().await;
    });

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Test /info endpoint
    let info_response = send_tcp_request(test_port, "/info", "GET", None)
        .await
        .expect("Failed to send /info request");
    assert_eq!(
        info_response.status(),
        StatusCode::OK,
        "Expected 200 OK from /info endpoint"
    );

    // Verify /info endpoint response content
    let body = info_response
        .into_body()
        .collect()
        .await
        .expect("Failed to read /info response body")
        .to_bytes();
    let json: Value =
        serde_json::from_slice(&body).expect("Failed to parse /info response as JSON");

    // Check endpoints array
    assert_eq!(
        json["endpoints"],
        serde_json::json!([
            "/v0.4/traces",
            "/v0.6/stats",
            "/info",
            "/profiling/v1/input"
        ]),
        "Expected endpoints array"
    );

    // Check client_drop_p0s flag
    assert_eq!(
        json["client_drop_p0s"], true,
        "Expected client_drop_p0s to be true"
    );

    // Check config object
    let config = &json["config"];
    assert_eq!(
        config["receiver_port"], test_port,
        "Expected receiver_port to match test port"
    );
    assert_eq!(
        config["statsd_port"], 8125,
        "Expected statsd_port to be 8125"
    );
    assert_eq!(
        config["receiver_socket"], "",
        "Expected empty receiver_socket for TCP"
    );

    // Test /v0.4/traces endpoint with real trace data
    let trace_payload = create_test_trace_payload();
    let trace_response = send_tcp_request(test_port, "/v0.4/traces", "POST", Some(trace_payload))
        .await
        .expect("Failed to send /v0.4/traces request");
    assert_eq!(
        trace_response.status(),
        StatusCode::OK,
        "Expected 200 OK from /v0.4/traces endpoint"
    );

    // Clean up
    agent_handle.abort();
}

#[cfg(all(test, windows, feature = "windows-pipes"))]
#[tokio::test]
async fn test_mini_agent_named_pipe_handles_requests() {
    // Use just the pipe name without \\.\pipe\ prefix, matching datadog-agent behavior
    let pipe_name = "dd_trace_integration_test";
    let pipe_path = format!(r"\\.\pipe\{}", pipe_name); // Full path for client connections
    let mut config = create_tcp_test_config(0);
    config.dd_apm_windows_pipe_name = Some(pipe_path.clone());
    let config = Arc::new(config);

    let mini_agent = MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(ServerlessTraceProcessor {}),
        trace_flusher: Arc::new(MockTraceFlusher),
        stats_processor: Arc::new(MockStatsProcessor),
        stats_flusher: Arc::new(MockStatsFlusher),
        env_verifier: Arc::new(MockEnvVerifier),
        proxy_flusher: Arc::new(ProxyFlusher::new(config)),
    };

    // Start the mini agent
    let agent_handle = tokio::spawn(async move {
        let _ = mini_agent.start_mini_agent().await;
    });

    // Give server time to create pipe
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Test /info endpoint
    let info_response = send_named_pipe_request(&pipe_path, "/info", "GET", None)
        .await
        .expect("Failed to send /info request over named pipe");
    assert_eq!(
        info_response.status(),
        StatusCode::OK,
        "Expected 200 OK from /info endpoint over named pipe"
    );

    // Verify /info endpoint response content
    let body = info_response
        .into_body()
        .collect()
        .await
        .expect("Failed to read /info response body")
        .to_bytes();
    let json: Value =
        serde_json::from_slice(&body).expect("Failed to parse /info response as JSON");

    // Check config object specific to named pipe
    let config_value = &json["config"];
    assert_eq!(
        config_value["receiver_port"], 0,
        "Expected receiver_port to be 0 for named pipe"
    );
    assert_eq!(
        config_value["statsd_port"], 8125,
        "Expected statsd_port to be 8125"
    );
    assert_eq!(
        config_value["receiver_socket"], pipe_path,
        "Expected receiver_socket to match full pipe path"
    );

    // Test /v0.4/traces endpoint with real trace data
    let trace_payload = create_test_trace_payload();
    let trace_response =
        send_named_pipe_request(&pipe_path, "/v0.4/traces", "POST", Some(trace_payload))
            .await
            .expect("Failed to send /v0.4/traces request over named pipe");
    assert_eq!(
        trace_response.status(),
        StatusCode::OK,
        "Expected 200 OK from /v0.4/traces endpoint over named pipe"
    );

    // Clean up
    agent_handle.abort();
}

#[cfg(test)]
#[tokio::test]
#[serial]
async fn test_mini_agent_tcp_with_real_flushers() {
    let mock_server = MockServer::start().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut config = create_tcp_test_config(8127);
<<<<<<< HEAD
    config.trace_intake = libdd_common::Endpoint {
        url: trace_url.parse().unwrap(),
        api_key: Some("test-api-key".into()),
        ..Default::default()
    };
    config.trace_stats_intake = libdd_common::Endpoint {
        url: stats_url.parse().unwrap(),
        api_key: Some("test-api-key".into()),
        ..Default::default()
    };
    // Set short flush intervals for faster testing
    config.trace_flush_interval_secs = 1; // 1 second
    config.stats_flush_interval_secs = 1; // 1 second

    let config = Arc::new(config);
    let test_port = config.dd_apm_receiver_port;

    // Create mini agent with REAL flushers
    let aggregator = Arc::new(tokio::sync::Mutex::new(TraceAggregator::default()));
    let mini_agent = MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(ServerlessTraceProcessor {}),
        trace_flusher: Arc::new(ServerlessTraceFlusher::new(
            aggregator.clone(),
            config.clone(),
        )),
        stats_processor: Arc::new(ServerlessStatsProcessor {}),
        stats_flusher: Arc::new(ServerlessStatsFlusher {}),
        env_verifier: Arc::new(MockEnvVerifier),
        proxy_flusher: Arc::new(ProxyFlusher::new(config.clone())),
    };
=======
    configure_mock_endpoints(&mut config, &mock_server.url());
    let config = Arc::new(config);
    let test_port = config.dd_apm_receiver_port;

    let mini_agent = create_mini_agent_with_real_flushers(config);
>>>>>>> e680a1b (Extract test helpers)

    let agent_handle = tokio::spawn(async move {
        let _ = mini_agent.start_mini_agent().await;
    });

    // Wait for server to be ready
    let mut server_ready = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(response) = send_tcp_request(test_port, "/info", "GET", None).await {
            if response.status().is_success() {
                server_ready = true;
                break;
            }
        }
    }
    assert!(
        server_ready,
        "Mini agent server failed to start within timeout"
    );

    // Send trace data
    let trace_payload = create_test_trace_payload();
    let trace_response = send_tcp_request(test_port, "/v0.4/traces", "POST", Some(trace_payload))
        .await
        .expect("Failed to send /v0.4/traces request");
    assert_eq!(trace_response.status(), StatusCode::OK);

    // Wait for flush
    tokio::time::sleep(FLUSH_WAIT_DURATION).await;

    verify_trace_request(&mock_server);

    agent_handle.abort();
}

#[cfg(all(test, windows))]
#[tokio::test]
#[serial]
async fn test_mini_agent_named_pipe_with_real_flushers() {
    let mock_server = MockServer::start().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

<<<<<<< HEAD
    // Create config pointing to mock server
    let trace_url = format!("{}/api/v0.2/traces", mock_server.url());
    let stats_url = format!("{}/api/v0.6/stats", mock_server.url());

    let mut config = create_tcp_test_config(8127);
    config.trace_intake = libdd_common::Endpoint {
        url: trace_url.parse().unwrap(),
        api_key: Some("test-api-key".into()),
        ..Default::default()
    };
    config.trace_stats_intake = libdd_common::Endpoint {
        url: stats_url.parse().unwrap(),
        api_key: Some("test-api-key".into()),
        ..Default::default()
    };
    // Set short flush intervals for faster testing
    config.trace_flush_interval_secs = 1; // 1 second
    config.stats_flush_interval_secs = 1; // 1 second

    // Configure for named pipe
=======
>>>>>>> e680a1b (Extract test helpers)
    let pipe_name = r"\\.\pipe\dd_trace_real_flusher_test";
    let mut config = create_tcp_test_config(0);
    configure_mock_endpoints(&mut config, &mock_server.url());
    config.dd_apm_windows_pipe_name = Some(pipe_name.to_string());
    config.dd_apm_receiver_port = 0;
    let config = Arc::new(config);

<<<<<<< HEAD
    // Create mini agent with REAL flushers
    let aggregator = Arc::new(tokio::sync::Mutex::new(TraceAggregator::default()));
    let mini_agent = MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(ServerlessTraceProcessor {}),
        trace_flusher: Arc::new(ServerlessTraceFlusher::new(
            aggregator.clone(),
            config.clone(),
        )),
        stats_processor: Arc::new(ServerlessStatsProcessor {}),
        stats_flusher: Arc::new(ServerlessStatsFlusher {}),
        env_verifier: Arc::new(MockEnvVerifier),
        proxy_flusher: Arc::new(ProxyFlusher::new(config.clone())),
    };
=======
    let mini_agent = create_mini_agent_with_real_flushers(config);
>>>>>>> e680a1b (Extract test helpers)

    let agent_handle = tokio::spawn(async move {
        let _ = mini_agent.start_mini_agent().await;
    });

    // Wait for server to be ready
    let mut server_ready = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(response) = send_named_pipe_request(pipe_name, "/info", "GET", None).await {
            if response.status().is_success() {
                server_ready = true;
                break;
            }
        }
    }
    assert!(
        server_ready,
        "Mini agent named pipe server failed to start within timeout"
    );

    // Send trace data via named pipe
    let trace_payload = create_test_trace_payload();
    let trace_response =
        send_named_pipe_request(pipe_name, "/v0.4/traces", "POST", Some(trace_payload))
            .await
            .expect("Failed to send /v0.4/traces request over named pipe");
    assert_eq!(trace_response.status(), StatusCode::OK);

    // Wait for flush
    tokio::time::sleep(FLUSH_WAIT_DURATION).await;

    verify_trace_request(&mock_server);

    agent_handle.abort();
}
