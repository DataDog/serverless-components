// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

mod common;

use common::helpers::{create_test_trace_payload, send_tcp_request};
use common::mocks::{MockEnvVerifier, MockStatsFlusher, MockStatsProcessor, MockTraceFlusher};
use datadog_trace_agent::{
    config::Config, mini_agent::MiniAgent, trace_flusher::TraceFlusher,
    trace_processor::ServerlessTraceProcessor,
};
use hyper::StatusCode;
use libdd_trace_utils::trace_utils;
use serial_test::serial;
use std::sync::Arc;
use std::time::Duration;

#[cfg(windows)]
use common::helpers::send_named_pipe_request;

/// Create a test config with TCP transport
pub fn create_tcp_test_config(port: u16) -> Config {
    Config {
        dd_site: "mock-datadoghq.com".to_string(),
        dd_apm_receiver_port: port,
        dd_apm_windows_pipe_name: None,
        dd_dogstatsd_port: 8125,
        env_type: trace_utils::EnvironmentType::AzureFunction,
        app_name: Some("test-app".to_string()),
        max_request_content_length: 10_000_000,
        obfuscation_config: libdd_trace_obfuscation::obfuscation_config::ObfuscationConfig::new()
            .unwrap(),
        os: std::env::consts::OS.to_string(),
        tags: datadog_trace_agent::config::Tags::new(),
        stats_flush_interval: 10,
        trace_flush_interval: 5,
        trace_intake: libdd_common::Endpoint::default(),
        trace_stats_intake: libdd_common::Endpoint::default(),
        verify_env_timeout: 1000,
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
        config,
        trace_processor: Arc::new(ServerlessTraceProcessor {}),
        trace_flusher: Arc::new(MockTraceFlusher),
        stats_processor: Arc::new(MockStatsProcessor),
        stats_flusher: Arc::new(MockStatsFlusher),
        env_verifier: Arc::new(MockEnvVerifier),
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

#[cfg(all(test, windows))]
#[tokio::test]
async fn test_mini_agent_named_pipe_handles_requests() {
    let pipe_name = r"\\.\pipe\dd_trace_integration_test";
    let mut config = create_tcp_test_config(0);
    config.dd_apm_windows_pipe_name = Some(pipe_name.to_string());
    config.dd_apm_receiver_port = 0;
    let config = Arc::new(config);

    let mini_agent = MiniAgent {
        config,
        trace_processor: Arc::new(ServerlessTraceProcessor {}),
        trace_flusher: Arc::new(MockTraceFlusher),
        stats_processor: Arc::new(MockStatsProcessor),
        stats_flusher: Arc::new(MockStatsFlusher),
        env_verifier: Arc::new(MockEnvVerifier),
    };

    // Start the mini agent
    let agent_handle = tokio::spawn(async move {
        let _ = mini_agent.start_mini_agent().await;
    });

    // Give server time to create pipe
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Test /info endpoint
    let info_response = send_named_pipe_request(pipe_name, "/info", "GET", None)
        .await
        .expect("Failed to send /info request over named pipe");
    assert_eq!(
        info_response.status(),
        StatusCode::OK,
        "Expected 200 OK from /info endpoint over named pipe"
    );

    // Test /v0.4/traces endpoint with real trace data
    let trace_payload = create_test_trace_payload();
    let trace_response =
        send_named_pipe_request(pipe_name, "/v0.4/traces", "POST", Some(trace_payload))
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
async fn test_mini_agent_with_real_flushers() {
    use common::mock_server::MockServer;
    use datadog_trace_agent::{
        aggregator::TraceAggregator, stats_flusher::ServerlessStatsFlusher,
        stats_processor::ServerlessStatsProcessor, trace_flusher::ServerlessTraceFlusher,
    };

    // Start mock HTTP server to intercept trace/stats requests
    let mock_server = MockServer::start().await;

    // Give mock server a moment to be ready
    tokio::time::sleep(Duration::from_millis(50)).await;

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
    config.trace_flush_interval = 1; // 1 second
    config.stats_flush_interval = 1; // 1 second

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
    };

    // Start the mini agent
    let agent_handle = tokio::spawn(async move {
        let _ = mini_agent.start_mini_agent().await;
    });

    // Wait for server to be ready by polling /info endpoint
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

    // Send trace data through the mini agent
    let trace_payload = create_test_trace_payload();
    let trace_response = send_tcp_request(test_port, "/v0.4/traces", "POST", Some(trace_payload))
        .await
        .expect("Failed to send /v0.4/traces request");

    assert_eq!(
        trace_response.status(),
        StatusCode::OK,
        "Expected 200 OK from /v0.4/traces endpoint"
    );

    // Wait for the trace flusher to flush (interval is 1 second + buffer)
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Verify the mock server received the trace request
    let trace_reqs = mock_server.get_requests_for_path("/api/v0.2/traces");

    assert!(
        !trace_reqs.is_empty(),
        "Expected at least one trace request to mock server"
    );

    // Validate the trace request
    let trace_req = &trace_reqs[0];
    assert_eq!(trace_req.method, "POST", "Expected POST method");

    // Check headers
    let content_type = trace_req
        .headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == "content-type")
        .map(|(_, v)| v.as_str());
    // The real flusher uses application/x-protobuf after coalescing traces
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

    // The body should be non-empty protobuf data
    assert!(
        !trace_req.body.is_empty(),
        "Expected non-empty trace payload"
    );

    println!("✓ Trace flusher successfully sent data to mock server");
    println!("  - Received {} trace request(s)", trace_reqs.len());
    println!("  - Payload size: {} bytes", trace_req.body.len());
    println!("  - Headers: {} present", trace_req.headers.len());

    // Clean up
    agent_handle.abort();
}

#[cfg(all(test, windows))]
#[tokio::test]
#[serial]
async fn test_mini_agent_named_pipe_with_real_flushers() {
    use common::mock_server::MockServer;
    use datadog_trace_agent::{
        aggregator::TraceAggregator, stats_flusher::ServerlessStatsFlusher,
        stats_processor::ServerlessStatsProcessor, trace_flusher::ServerlessTraceFlusher,
    };

    // Start mock HTTP server to intercept trace/stats requests
    let mock_server = MockServer::start().await;

    // Give mock server a moment to be ready
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create config pointing to mock server
    let trace_url = format!("{}/api/v0.2/traces", mock_server.url());
    let stats_url = format!("{}/api/v0.6/stats", mock_server.url());

    let mut config = create_tcp_test_config(0);
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
    config.trace_flush_interval = 1; // 1 second
    config.stats_flush_interval = 1; // 1 second

    // Configure for named pipe
    let pipe_name = r"\\.\pipe\dd_trace_real_flusher_test";
    config.dd_apm_windows_pipe_name = Some(pipe_name.to_string());
    config.dd_apm_receiver_port = 0;

    let config = Arc::new(config);

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
    };

    // Start the mini agent
    let agent_handle = tokio::spawn(async move {
        let _ = mini_agent.start_mini_agent().await;
    });

    // Wait for server to be ready by polling /info endpoint
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

    // Send trace data through the mini agent via named pipe
    let trace_payload = create_test_trace_payload();
    let trace_response =
        send_named_pipe_request(pipe_name, "/v0.4/traces", "POST", Some(trace_payload))
            .await
            .expect("Failed to send /v0.4/traces request over named pipe");

    assert_eq!(
        trace_response.status(),
        StatusCode::OK,
        "Expected 200 OK from /v0.4/traces endpoint over named pipe"
    );

    // Wait for the trace flusher to flush (interval is 1 second + buffer)
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Verify the mock server received the trace request
    let trace_reqs = mock_server.get_requests_for_path("/api/v0.2/traces");

    assert!(
        !trace_reqs.is_empty(),
        "Expected at least one trace request to mock server"
    );

    // Validate the trace request
    let trace_req = &trace_reqs[0];
    assert_eq!(trace_req.method, "POST", "Expected POST method");

    // Check headers
    let content_type = trace_req
        .headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == "content-type")
        .map(|(_, v)| v.as_str());
    // The real flusher uses application/x-protobuf after coalescing traces
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

    // The body should be non-empty protobuf data
    assert!(
        !trace_req.body.is_empty(),
        "Expected non-empty trace payload"
    );

    println!("✓ [Named Pipe] Trace flusher successfully sent data to mock server");
    println!("  - Received {} trace request(s)", trace_reqs.len());
    println!("  - Payload size: {} bytes", trace_req.body.len());
    println!("  - Headers: {} present", trace_req.headers.len());

    // Clean up
    agent_handle.abort();
}
