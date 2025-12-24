// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use datadog_trace_agent::{
    config::Config,
    env_verifier::EnvVerifier,
    mini_agent::MiniAgent,
    stats_flusher::StatsFlusher,
    stats_processor::StatsProcessor,
    trace_flusher::TraceFlusher,
    trace_processor::TraceProcessor,
};
use datadog_trace_protobuf::pb;
use datadog_trace_utils::trace_utils::{MiniAgentMetadata, SendData};
use ddcommon::hyper_migration;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};

/// Mock trace processor for testing
struct MockTraceProcessor;

#[async_trait::async_trait]
impl TraceProcessor for MockTraceProcessor {
    async fn process_traces(
        &self,
        _config: Arc<Config>,
        _req: hyper_migration::HttpRequest,
        _trace_tx: Sender<SendData>,
        _mini_agent_metadata: Arc<MiniAgentMetadata>,
    ) -> Result<hyper_migration::HttpResponse, Box<dyn std::error::Error + Send + Sync>> {
        // Return a simple 200 OK response
        Ok(hyper::Response::builder()
            .status(200)
            .body(hyper_migration::Body::from("OK"))?)
    }
}

/// Mock trace flusher for testing
struct MockTraceFlusher;

#[async_trait::async_trait]
impl TraceFlusher for MockTraceFlusher {
    async fn start_trace_flusher(&self, mut _trace_rx: Receiver<SendData>) {
        // Do nothing - just consume messages
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }
}

/// Mock stats processor for testing
struct MockStatsProcessor;

#[async_trait::async_trait]
impl StatsProcessor for MockStatsProcessor {
    async fn process_stats(
        &self,
        _config: Arc<Config>,
        _req: hyper_migration::HttpRequest,
        _stats_tx: Sender<pb::ClientStatsPayload>,
    ) -> Result<hyper_migration::HttpResponse, Box<dyn std::error::Error + Send + Sync>> {
        Ok(hyper::Response::builder()
            .status(200)
            .body(hyper_migration::Body::from("OK"))?)
    }
}

/// Mock stats flusher for testing
struct MockStatsFlusher;

#[async_trait::async_trait]
impl StatsFlusher for MockStatsFlusher {
    async fn start_stats_flusher(
        &self,
        _config: Arc<Config>,
        mut _stats_rx: Receiver<pb::ClientStatsPayload>,
    ) {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }
}

/// Mock env verifier for testing
struct MockEnvVerifier;

#[async_trait::async_trait]
impl EnvVerifier for MockEnvVerifier {
    async fn verify_environment(
        &self,
        _timeout_ms: u64,
        _env_type: &datadog_trace_utils::trace_utils::EnvironmentType,
        _os: &str,
    ) -> MiniAgentMetadata {
        MiniAgentMetadata {
            function_name: Some("test-function".to_string()),
            ..Default::default()
        }
    }
}

/// Create a test config with TCP transport
fn create_tcp_test_config(port: u16) -> Config {
    Config {
        dd_site: "datadoghq.com".to_string(),
        dd_apm_receiver_port: port,
        dd_apm_windows_pipe_name: None,
        dd_dogstatsd_port: 8125,
        env_type: datadog_trace_utils::trace_utils::EnvironmentType::Local,
        app_name: Some("test-app".to_string()),
        max_request_content_length: 10_000_000,
        obfuscation_config: Default::default(),
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
async fn test_trace_agent_tcp_accepts_connection() {
    use tokio::time::{timeout, Duration};

    let test_port = 18126; // Use a non-standard port for testing
    let config = Arc::new(create_tcp_test_config(test_port));

    let mini_agent = MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(MockTraceProcessor),
        trace_flusher: Arc::new(MockTraceFlusher),
        stats_processor: Arc::new(MockStatsProcessor),
        stats_flusher: Arc::new(MockStatsFlusher),
        env_verifier: Arc::new(MockEnvVerifier),
    };

    // Start the mini agent in the background
    let agent_handle = tokio::spawn(async move {
        mini_agent.start_mini_agent().await
    });

    // Give the server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Try to connect via TCP
    let connect_result = timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(format!("127.0.0.1:{}", test_port)),
    )
    .await;

    // Verify connection succeeds
    assert!(
        connect_result.is_ok(),
        "Failed to connect to TCP server within timeout"
    );
    assert!(
        connect_result.unwrap().is_ok(),
        "TCP connection failed"
    );

    // Clean up: the agent is running in the background and will be dropped
    agent_handle.abort();
}

#[cfg(test)]
#[tokio::test]
async fn test_trace_agent_tcp_handles_http_request() {
    use hyper::body::Body;
    use hyper::Request;
    use tokio::time::{timeout, Duration};

    let test_port = 18127;
    let config = Arc::new(create_tcp_test_config(test_port));

    let mini_agent = MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(MockTraceProcessor),
        trace_flusher: Arc::new(MockTraceFlusher),
        stats_processor: Arc::new(MockStatsProcessor),
        stats_flusher: Arc::new(MockStatsFlusher),
        env_verifier: Arc::new(MockEnvVerifier),
    };

    // Start the mini agent in the background
    let agent_handle = tokio::spawn(async move {
        mini_agent.start_mini_agent().await
    });

    // Give the server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send an HTTP request to the /info endpoint
    let client = hyper::Client::new();
    let uri = format!("http://127.0.0.1:{}/info", test_port);

    let request_result = timeout(
        Duration::from_secs(2),
        client.request(Request::builder()
            .uri(uri)
            .method("GET")
            .body(Body::empty())
            .unwrap()),
    )
    .await;

    // Verify request succeeds
    assert!(
        request_result.is_ok(),
        "HTTP request timed out"
    );

    let response = request_result.unwrap();
    assert!(
        response.is_ok(),
        "HTTP request failed: {:?}",
        response.err()
    );

    let response = response.unwrap();
    assert_eq!(response.status(), 200, "Expected 200 OK response");

    // Clean up
    agent_handle.abort();
}

#[cfg(all(test, windows))]
#[tokio::test]
async fn test_trace_agent_named_pipe_accepts_connection() {
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::time::{timeout, Duration};

    let pipe_name = r"\\.\pipe\dd_trace_test_pipe";
    let mut config = create_tcp_test_config(0); // Port 0 when using named pipe
    config.dd_apm_windows_pipe_name = Some(pipe_name.to_string());
    let config = Arc::new(config);

    let mini_agent = MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(MockTraceProcessor),
        trace_flusher: Arc::new(MockTraceFlusher),
        stats_processor: Arc::new(MockStatsProcessor),
        stats_flusher: Arc::new(MockStatsFlusher),
        env_verifier: Arc::new(MockEnvVerifier),
    };

    // Start the mini agent in the background
    let agent_handle = tokio::spawn(async move {
        mini_agent.start_mini_agent().await
    });

    // Give the server time to create the pipe
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Try to connect via named pipe
    let connect_result = timeout(
        Duration::from_secs(2),
        ClientOptions::new().open(pipe_name),
    )
    .await;

    // Verify connection succeeds
    assert!(
        connect_result.is_ok(),
        "Failed to connect to named pipe within timeout"
    );
    assert!(
        connect_result.unwrap().is_ok(),
        "Named pipe connection failed"
    );

    // Clean up
    agent_handle.abort();
}

#[cfg(all(test, windows))]
#[tokio::test]
async fn test_trace_agent_named_pipe_handles_http_request() {
    use hyper::body::Body;
    use hyper::Request;
    use hyper_util::rt::TokioIo;
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::time::{timeout, Duration};

    let pipe_name = r"\\.\pipe\dd_trace_test_pipe_http";
    let mut config = create_tcp_test_config(0);
    config.dd_apm_windows_pipe_name = Some(pipe_name.to_string());
    let config = Arc::new(config);

    let mini_agent = MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(MockTraceProcessor),
        trace_flusher: Arc::new(MockTraceFlusher),
        stats_processor: Arc::new(MockStatsProcessor),
        stats_flusher: Arc::new(MockStatsFlusher),
        env_verifier: Arc::new(MockEnvVerifier),
    };

    // Start the mini agent in the background
    let agent_handle = tokio::spawn(async move {
        mini_agent.start_mini_agent().await
    });

    // Give the server time to create the pipe
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect to the named pipe
    let client = ClientOptions::new()
        .open(pipe_name)
        .expect("Failed to connect to named pipe");

    // Wrap the pipe in TokioIo for hyper
    let io = TokioIo::new(client);

    // Send an HTTP request over the pipe
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .expect("Failed to perform HTTP handshake");

    // Spawn connection task
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("Connection error: {}", e);
        }
    });

    let request = Request::builder()
        .uri("/info")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response_result = timeout(Duration::from_secs(2), sender.send_request(request)).await;

    // Verify request succeeds
    assert!(
        response_result.is_ok(),
        "HTTP request over named pipe timed out"
    );

    let response = response_result.unwrap();
    assert!(
        response.is_ok(),
        "HTTP request over named pipe failed: {:?}",
        response.err()
    );

    let response = response.unwrap();
    assert_eq!(
        response.status(),
        200,
        "Expected 200 OK response from named pipe"
    );

    // Clean up
    agent_handle.abort();
}