// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

mod common;

use common::helpers::{create_test_trace_payload, send_tcp_request};
use common::mock_server::MockServer;
use common::mocks::{MockEnvVerifier, MockStatsFlusher, MockStatsProcessor, MockTraceFlusher};
use datadog_trace_agent::{
    config::{Config, test_helpers::create_tcp_test_config},
    mini_agent::MiniAgent,
    proxy_flusher::ProxyFlusher,
    trace_flusher::TraceFlusher,
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
    let stats_url = format!("{}/api/v0.2/stats", mock_server_url);

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
pub fn create_mini_agent_with_real_flushers(
    config: Arc<Config>,
) -> (MiniAgent, tokio::task::JoinHandle<()>) {
    use datadog_trace_agent::{
        aggregator::TraceAggregator, stats_concentrator_service::StatsConcentratorService,
        stats_flusher::ServerlessStatsFlusher, stats_processor::ServerlessStatsProcessor,
        trace_flusher::ServerlessTraceFlusher,
    };

    let (service, stats_concentrator_handle) = StatsConcentratorService::new(config.clone());
    let stats_concentrator_service_handle = tokio::spawn(service.run());

    let aggregator = Arc::new(tokio::sync::Mutex::new(TraceAggregator::default()));
    let mini_agent = MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(ServerlessTraceProcessor {
            stats_concentrator: Some(stats_concentrator_handle.clone()),
        }),
        trace_flusher: Arc::new(ServerlessTraceFlusher::new(
            aggregator.clone(),
            config.clone(),
        )),
        stats_processor: Arc::new(ServerlessStatsProcessor {}),
        stats_flusher: Arc::new(ServerlessStatsFlusher {
            stats_concentrator: Some(stats_concentrator_handle),
        }),
        env_verifier: Arc::new(MockEnvVerifier),
        proxy_flusher: Arc::new(ProxyFlusher::new(config.clone())),
    };
    (mini_agent, stats_concentrator_service_handle)
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

/// Helper to verify stats request sent to mock server
pub fn verify_stats_request(mock_server: &common::mock_server::MockServer) {
    let stats_reqs = mock_server.get_requests_for_path("/api/v0.2/stats");

    assert!(
        !stats_reqs.is_empty(),
        "Expected at least one stats request to mock server"
    );

    let stats_req = &stats_reqs[0];
    assert_eq!(stats_req.method, "POST", "Expected POST method");

    let content_type = stats_req
        .headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == "content-type")
        .map(|(_, v)| v.as_str());
    assert_eq!(
        content_type,
        Some("application/msgpack"),
        "Expected msgpack content-type"
    );

    let api_key = stats_req
        .headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == "dd-api-key")
        .map(|(_, v)| v.as_str());
    assert_eq!(api_key, Some("test-api-key"), "Expected API key header");

    assert!(
        !stats_req.body.is_empty(),
        "Expected non-empty stats payload"
    );
}

/// Helper to verify stats request was not sent to mock server
pub fn verify_no_stats_request(mock_server: &common::mock_server::MockServer) {
    let stats_reqs = mock_server.get_requests_for_path("/api/v0.2/stats");
    assert!(
        stats_reqs.is_empty(),
        "Expected no stats request to mock server, received {} request(s)",
        stats_reqs.len()
    );
}

#[cfg(test)]
#[tokio::test]
#[serial]
async fn test_mini_agent_tcp_handles_requests() {
    let config = Arc::new(create_tcp_test_config(8126));
    let test_port = config.dd_apm_receiver_port;
    let mini_agent = MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(ServerlessTraceProcessor {
            stats_concentrator: None,
        }),
        trace_flusher: Arc::new(MockTraceFlusher),
        stats_processor: Arc::new(MockStatsProcessor),
        stats_flusher: Arc::new(MockStatsFlusher),
        env_verifier: Arc::new(MockEnvVerifier),
        proxy_flusher: Arc::new(ProxyFlusher::new(config)),
    };

    // Start the mini agent
    let agent_handle = tokio::spawn(async move {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let _ = mini_agent.start_mini_agent(shutdown_rx, None).await;
    });

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Test /info endpoint
    let info_response = send_tcp_request(test_port, "/info", "GET", None, &[])
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
        json["client_drop_p0s"], false,
        "Expected client_drop_p0s to be false"
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
    let trace_response =
        send_tcp_request(test_port, "/v0.4/traces", "POST", Some(trace_payload), &[])
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
        trace_processor: Arc::new(ServerlessTraceProcessor {
            stats_concentrator: None,
        }),
        trace_flusher: Arc::new(MockTraceFlusher),
        stats_processor: Arc::new(MockStatsProcessor),
        stats_flusher: Arc::new(MockStatsFlusher),
        env_verifier: Arc::new(MockEnvVerifier),
        proxy_flusher: Arc::new(ProxyFlusher::new(config)),
    };

    // Start the mini agent
    let agent_handle = tokio::spawn(async move {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let _ = mini_agent.start_mini_agent(shutdown_rx, None).await;
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
    let mock_server: MockServer = MockServer::start().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut config = create_tcp_test_config(8127);
    configure_mock_endpoints(&mut config, &mock_server.url());
    let config = Arc::new(config);
    let test_port = config.dd_apm_receiver_port;

    let (mini_agent, stats_concentrator_service_handle) =
        create_mini_agent_with_real_flushers(config);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let agent_handle = tokio::spawn(async move {
        let _ = mini_agent
            .start_mini_agent(shutdown_rx, Some(stats_concentrator_service_handle))
            .await;
    });

    // Wait for server to be ready
    let mut server_ready = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(response) = send_tcp_request(test_port, "/info", "GET", None, &[]).await {
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
    let trace_response =
        send_tcp_request(test_port, "/v0.4/traces", "POST", Some(trace_payload), &[])
            .await
            .expect("Failed to send /v0.4/traces request");
    assert_eq!(trace_response.status(), StatusCode::OK);

    // Wait for trace flush
    tokio::time::sleep(FLUSH_WAIT_DURATION).await;
    verify_trace_request(&mock_server);

    // Trigger shutdown to force flush in progress concentrator buckets
    let _ = shutdown_tx.send(());
    let _ = agent_handle.await;
    verify_stats_request(&mock_server); // Stats generator should generate stats from trace payload
}

#[cfg(test)]
#[tokio::test]
#[serial]
async fn test_concentrator_task_death_shuts_down_mini_agent() {
    let mock_server: MockServer = MockServer::start().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut config = create_tcp_test_config(8129);
    configure_mock_endpoints(&mut config, &mock_server.url());
    let config = Arc::new(config);
    let test_port = config.dd_apm_receiver_port;

    let (mini_agent, stats_concentrator_service_handle) =
        create_mini_agent_with_real_flushers(config);
    let abort_handle = stats_concentrator_service_handle.abort_handle();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let agent_handle = tokio::spawn(async move {
        mini_agent
            .start_mini_agent(shutdown_rx, Some(stats_concentrator_service_handle))
            .await
            .map_err(|e| e.to_string())
    });

    // Wait for server to be ready
    let mut server_ready = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(response) = send_tcp_request(test_port, "/info", "GET", None, &[]).await
            && response.status().is_success()
        {
            server_ready = true;
            break;
        }
    }
    assert!(
        server_ready,
        "Mini agent server failed to start within timeout"
    );

    // Kill the concentrator task to simulate unexpected task death
    abort_handle.abort();

    // Mini agent should detect the task death and exit with an error
    let result = tokio::time::timeout(Duration::from_secs(2), agent_handle)
        .await
        .expect("mini agent should have exited after concentrator task death");
    assert!(
        result.expect("agent task should not panic").is_err(),
        "mini agent should return an error when the concentrator task dies"
    );
}

#[cfg(test)]
#[tokio::test]
#[serial]
async fn test_mini_agent_tcp_with_real_flushers_and_tracer_computed_stats() {
    let mock_server: MockServer = MockServer::start().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut config = create_tcp_test_config(8128); // use different port to avoid race condition with other tests
    configure_mock_endpoints(&mut config, &mock_server.url());
    let config = Arc::new(config);
    let test_port = config.dd_apm_receiver_port;

    let (mini_agent, _stats_concentrator_service_handle) =
        create_mini_agent_with_real_flushers(config);

    let agent_handle = tokio::spawn(async move {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let _ = mini_agent.start_mini_agent(shutdown_rx, None).await;
    });

    // Wait for server to be ready
    let mut server_ready = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(response) = send_tcp_request(test_port, "/info", "GET", None, &[]).await
            && response.status().is_success()
        {
            server_ready = true;
            break;
        }
    }
    assert!(
        server_ready,
        "Mini agent server failed to start within timeout"
    );

    // Send trace data
    let trace_payload = create_test_trace_payload();
    let trace_response = send_tcp_request(
        test_port,
        "/v0.4/traces",
        "POST",
        Some(trace_payload),
        &[("Datadog-Client-Computed-Stats", "true")],
    )
    .await
    .expect("Failed to send /v0.4/traces request");
    assert_eq!(trace_response.status(), StatusCode::OK);

    // Wait for flush
    tokio::time::sleep(FLUSH_WAIT_DURATION).await;

    verify_trace_request(&mock_server);
    verify_no_stats_request(&mock_server); // Stats generator should not generate stats from trace payload when Datadog-Client-Computed-Stats header is present in trace payload

    // Clean up
    agent_handle.abort();
}

#[cfg(all(test, windows, feature = "windows-pipes"))]
#[tokio::test]
#[serial]
async fn test_mini_agent_named_pipe_with_real_flushers() {
    let mock_server = MockServer::start().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let pipe_name = r"\\.\pipe\dd_trace_real_flusher_test";
    let mut config = create_tcp_test_config(0);
    configure_mock_endpoints(&mut config, &mock_server.url());
    config.dd_apm_windows_pipe_name = Some(pipe_name.to_string());
    config.dd_apm_receiver_port = 0;
    let config = Arc::new(config);

    let (mini_agent, _stats_concentrator_service_handle) =
        create_mini_agent_with_real_flushers(config);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let agent_handle = tokio::spawn(async move {
        let _ = mini_agent.start_mini_agent(shutdown_rx, None).await;
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

    // Wait for trace flush
    tokio::time::sleep(FLUSH_WAIT_DURATION).await;
    verify_trace_request(&mock_server);

    // Trigger shutdown to force flush in progress concentrator buckets
    let _ = shutdown_tx.send(());
    let _ = agent_handle.await;
    verify_stats_request(&mock_server);
}
