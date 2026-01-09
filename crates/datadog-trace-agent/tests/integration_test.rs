// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

mod common;

use common::helpers::{create_test_trace_payload, send_tcp_request};
use common::mocks::{
    MockEnvVerifier, MockStatsFlusher, MockStatsProcessor, MockTraceFlusher,
};
use datadog_trace_agent::{
    config::Config, mini_agent::MiniAgent, trace_processor::ServerlessTraceProcessor,
};
use hyper::StatusCode;
use libdd_trace_utils::trace_utils;
use std::sync::Arc;
use std::time::Duration;

#[cfg(windows)]
use common::helpers::send_named_pipe_request;

/// Create a test config with TCP transport
pub fn create_tcp_test_config() -> Config {
    Config {
        dd_site: "mock-datadoghq.com".to_string(),
        dd_apm_receiver_port: 8126,
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
async fn test_mini_agent_tcp_handles_requests() {
    let config = Arc::new(create_tcp_test_config());
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
    let mut config = create_tcp_test_config();
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
    let trace_response = send_named_pipe_request(pipe_name, "/v0.4/traces", "POST", Some(trace_payload))
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
