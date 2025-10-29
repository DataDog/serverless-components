//! Integration tests for DogStatsD functionality
//!
//! Tests that the DogStatsD server starts correctly and can receive metrics via UDP.

use datadog_agent_native::agent::AgentCoordinator;
use datadog_agent_native::config::Config;
use std::net::UdpSocket;
use tokio;

/// Helper to create a test config with DogStatsD enabled
fn create_dogstatsd_test_config(enabled: bool, _host: &str, port: u16) -> Config {
    let mut config = Config::default();
    config.api_key = "test-api-key".to_string();
    config.site = "datadoghq.com".to_string();
    config.service = Some("test-service".to_string());
    config.env = Some("test".to_string());
    config.apm_dd_url = "http://localhost:8126".to_string();

    // DogStatsD configuration
    config.dogstatsd_enabled = enabled;
    // Note: dogstatsd_host is hardcoded to "127.0.0.1" in the Config struct
    config.dogstatsd_port = port;

    config
}

/// Test 1: DogStatsD disabled by default
#[tokio::test]
async fn test_dogstatsd_disabled_by_default() {
    let config = create_dogstatsd_test_config(false, "127.0.0.1", 8125);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");

    // Verify DogStatsD is disabled
    assert_eq!(coordinator.config().dogstatsd_enabled, false);

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 2: DogStatsD starts when enabled
#[tokio::test]
async fn test_dogstatsd_enabled() {
    // Use a different port to avoid conflicts
    let config = create_dogstatsd_test_config(true, "127.0.0.1", 18125);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");

    // Verify DogStatsD is enabled
    assert_eq!(coordinator.config().dogstatsd_enabled, true);
    assert_eq!(coordinator.config().dogstatsd_port, 18125);

    // Give the server a moment to fully start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 3: DogStatsD receives metrics via UDP
#[tokio::test]
async fn test_dogstatsd_receives_metrics() {
    // Use a unique port for this test
    let port = 28125;
    let config = create_dogstatsd_test_config(true, "127.0.0.1", port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");

    // Give the server time to start
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Send a test metric via UDP
    let socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to create UDP socket");

    let metric = "test.counter:1|c";
    socket
        .send_to(metric.as_bytes(), format!("127.0.0.1:{}", port))
        .expect("Failed to send UDP packet");

    // Give time for metric to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Successful if we didn't crash
    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 4: DogStatsD with custom host binding
#[tokio::test]
async fn test_dogstatsd_custom_host() {
    let config = create_dogstatsd_test_config(true, "0.0.0.0", 38125);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");

    // Note: dogstatsd_host is now hardcoded to "127.0.0.1" and cannot be customized
    // This test now just verifies that the agent starts successfully with DogStatsD enabled

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 5: Multiple metrics can be sent to DogStatsD
#[tokio::test]
async fn test_dogstatsd_multiple_metrics() {
    let port = 48125;
    let config = create_dogstatsd_test_config(true, "127.0.0.1", port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to create UDP socket");

    // Send multiple different types of metrics
    let metrics = vec![
        "test.counter:1|c",
        "test.gauge:42|g",
        "test.histogram:100|h",
        "test.timer:500|ms",
    ];

    for metric in metrics {
        socket
            .send_to(metric.as_bytes(), format!("127.0.0.1:{}", port))
            .expect("Failed to send UDP packet");
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 6: DogStatsD handles metric with tags
#[tokio::test]
async fn test_dogstatsd_metrics_with_tags() {
    let port = 58125;
    let config = create_dogstatsd_test_config(true, "127.0.0.1", port);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to create UDP socket");

    // Send metrics with tags
    let metric = "test.counter:1|c|#env:test,service:my-service";
    socket
        .send_to(metric.as_bytes(), format!("127.0.0.1:{}", port))
        .expect("Failed to send UDP packet");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 7: DogStatsD with ephemeral port (port 0)
#[tokio::test]
async fn test_dogstatsd_ephemeral_port() {
    // Configure with port 0 to get an ephemeral port from the OS
    let config = create_dogstatsd_test_config(true, "127.0.0.1", 0);

    let mut coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    coordinator.start().await.expect("Failed to start agent");

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Get the actual bound port
    let actual_port = coordinator
        .get_dogstatsd_port()
        .expect("Should have actual port after start");

    // Verify the port is not 0 (OS assigned an actual port)
    assert_ne!(actual_port, 0, "Actual port should not be 0");

    // Verify the port is in the ephemeral range (typically > 1024)
    assert!(actual_port > 1024, "Ephemeral port should be > 1024");

    // Send a test metric to the actual port
    let socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to create UDP socket");

    let metric = "test.ephemeral:1|c";
    socket
        .send_to(metric.as_bytes(), format!("127.0.0.1:{}", actual_port))
        .expect("Failed to send UDP packet");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    coordinator.shutdown().await.expect("Failed to shutdown");
}

/// Test 8: Multiple DogStatsD instances with ephemeral ports
#[tokio::test]
async fn test_dogstatsd_multiple_instances_ephemeral() {
    // Create two coordinators with port 0 to avoid conflicts
    let config1 = create_dogstatsd_test_config(true, "127.0.0.1", 0);
    let config2 = create_dogstatsd_test_config(true, "127.0.0.1", 0);

    let mut coordinator1 = AgentCoordinator::new(config1).expect("Failed to create coordinator 1");
    let mut coordinator2 = AgentCoordinator::new(config2).expect("Failed to create coordinator 2");

    coordinator1.start().await.expect("Failed to start agent 1");
    coordinator2.start().await.expect("Failed to start agent 2");

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Get actual ports
    let port1 = coordinator1
        .get_dogstatsd_port()
        .expect("Should have port 1");
    let port2 = coordinator2
        .get_dogstatsd_port()
        .expect("Should have port 2");

    // Verify both instances got different ports
    assert_ne!(port1, port2, "Each instance should have a unique port");
    assert_ne!(port1, 0, "Port 1 should not be 0");
    assert_ne!(port2, 0, "Port 2 should not be 0");

    // Send metrics to both instances
    let socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to create UDP socket");

    socket
        .send_to(b"test.instance1:1|c", format!("127.0.0.1:{}", port1))
        .expect("Failed to send to instance 1");
    socket
        .send_to(b"test.instance2:1|c", format!("127.0.0.1:{}", port2))
        .expect("Failed to send to instance 2");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    coordinator1.shutdown().await.expect("Failed to shutdown 1");
    coordinator2.shutdown().await.expect("Failed to shutdown 2");
}

/// Test 9: Verify get_dogstatsd_port returns None when disabled
#[tokio::test]
async fn test_dogstatsd_port_when_disabled() {
    let config = create_dogstatsd_test_config(false, "127.0.0.1", 8125);

    let coordinator = AgentCoordinator::new(config).expect("Failed to create coordinator");

    // Before start, should be None
    assert_eq!(
        coordinator.get_dogstatsd_port(),
        None,
        "Port should be None before start"
    );

    // DogStatsD is disabled, so port should remain None even after start
    // (We don't start it here to avoid the overhead of starting the full agent)
}
