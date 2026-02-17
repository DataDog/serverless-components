// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use dogstatsd::metric::SortedTags;
use dogstatsd::{
    aggregator::Aggregator as MetricsAggregator,
    aggregator_service::{AggregatorHandle, AggregatorService},
    api_key::ApiKeyFactory,
    constants::CONTEXTS,
    datadog::{DdDdUrl, MetricsIntakeUrlPrefix, MetricsIntakeUrlPrefixOverride},
    dogstatsd::{DogStatsD, DogStatsDConfig},
    flusher::{Flusher, FlusherConfig},
};
use mockito::Server;
use std::sync::Arc;
use tokio::{
    net::UdpSocket,
    time::{sleep, timeout, Duration},
};
use tokio_util::sync::CancellationToken;
use zstd::zstd_safe::CompressionLevel;

#[cfg(all(windows, feature = "windows-pipes"))]
use tokio::{io::AsyncWriteExt, net::windows::named_pipe::ClientOptions};

#[cfg(test)]
#[tokio::test]
async fn dogstatsd_server_ships_series() {
    use dogstatsd::datadog::RetryStrategy;

    let mut mock_server = Server::new_async().await;

    let mock = mock_server
        .mock("POST", "/api/v2/series")
        .match_header("DD-API-KEY", "mock-api-key")
        .match_header("Content-Type", "application/json")
        .with_status(202)
        .create_async()
        .await;

    // Create the aggregator service
    let (service, handle) =
        AggregatorService::new(SortedTags::parse("sometkey:somevalue").unwrap(), CONTEXTS)
            .expect("failed to create aggregator service");

    // Start the service in a background task
    tokio::spawn(service.run());

    let _ = start_dogstatsd(handle.clone()).await;

    let api_key_factory = ApiKeyFactory::new("mock-api-key");

    let metrics_flusher = Flusher::new(FlusherConfig {
        api_key_factory: Arc::new(api_key_factory),
        aggregator_handle: handle.clone(),
        metrics_intake_url_prefix: MetricsIntakeUrlPrefix::new(
            None,
            MetricsIntakeUrlPrefixOverride::maybe_new(
                None,
                Some(DdDdUrl::new(mock_server.url()).expect("failed to create URL")),
            ),
        )
        .expect("failed to create URL"),
        https_proxy: None,
        ca_cert_path: None,
        timeout: std::time::Duration::from_secs(5),
        retry_strategy: RetryStrategy::Immediate(3),
        compression_level: CompressionLevel::try_from(6)
            .expect("failed to create compression level"),
    });

    let server_address = "127.0.0.1:18125";
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("unable to bind UDP socket");
    let metric = "custom_metric:1|g";

    socket
        .send_to(metric.as_bytes(), &server_address)
        .await
        .expect("unable to send metric");

    let flush = async {
        while !mock.matched() {
            sleep(Duration::from_millis(100)).await;
            metrics_flusher.flush().await;
        }
    };

    let result = timeout(Duration::from_millis(1000), flush).await;

    match result {
        Ok(_) => mock.assert(),
        Err(_) => panic!("timed out before server received metric flush"),
    }
}

async fn start_dogstatsd(aggregator_handle: AggregatorHandle) -> CancellationToken {
    start_dogstatsd_on_port(aggregator_handle, 18125, None).await
}

async fn start_dogstatsd_on_port(
    aggregator_handle: AggregatorHandle,
    port: u16,
    buffer_size: Option<usize>,
) -> CancellationToken {
    let dogstatsd_config = DogStatsDConfig {
        host: "127.0.0.1".to_string(),
        port,
        metric_namespace: None,
        #[cfg(all(windows, feature = "windows-pipes"))]
        windows_pipe_name: None,
        so_rcvbuf: None,
        buffer_size,
    };
    let dogstatsd_cancel_token = tokio_util::sync::CancellationToken::new();
    let dogstatsd_client = DogStatsD::new(
        &dogstatsd_config,
        aggregator_handle,
        dogstatsd_cancel_token.clone(),
    )
    .await;

    tokio::spawn(async move {
        dogstatsd_client.spin().await;
    });

    dogstatsd_cancel_token
}

#[cfg(test)]
#[tokio::test]
async fn test_send_with_retry_immediate_failure() {
    use dogstatsd::datadog::{DdApi, DdDdUrl, RetryStrategy};
    use dogstatsd::metric::{parse, SortedTags};

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/series")
        .with_status(500)
        .with_body("Internal Server Error")
        .expect(3)
        .create_async()
        .await;

    let retry_strategy = RetryStrategy::Immediate(3);
    let dd_api = DdApi::new(
        "test_key".to_string(),
        MetricsIntakeUrlPrefix::new(
            None,
            MetricsIntakeUrlPrefixOverride::maybe_new(
                None,
                Some(DdDdUrl::new(server.url()).expect("failed to create URL")),
            ),
        )
        .expect("failed to create URL"),
        None,
        None,
        Duration::from_secs(1),
        retry_strategy.clone(),
        6,
    );

    // Create a series using the Aggregator
    let mut aggregator = MetricsAggregator::new(SortedTags::parse("test:value").unwrap(), 1)
        .expect("failed to create aggregator");
    let metric = parse("test:1|c").expect("failed to parse metric");
    aggregator.insert(metric).expect("failed to insert metric");
    let series = aggregator.to_series();

    let result = dd_api.ship_series(&series).await;

    // The result should be an error since we got a 500 response
    assert!(result.is_err());

    // Verify that the mock was called exactly 3 times
    mock.assert_async().await;
}

#[cfg(test)]
#[tokio::test]
async fn test_send_with_retry_linear_backoff_success() {
    use dogstatsd::datadog::{DdApi, DdDdUrl, RetryStrategy};
    use dogstatsd::metric::{parse, SortedTags};

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/series")
        .with_status(500)
        .with_body("Internal Server Error")
        .expect(1)
        .create_async()
        .await;

    let success_mock = server
        .mock("POST", "/api/v2/series")
        .with_status(200)
        .with_body("Success")
        .expect(1)
        .create_async()
        .await;

    let retry_strategy = RetryStrategy::LinearBackoff(3, 1); // 3 attempts, 1ms delay
    let dd_api = DdApi::new(
        "test_key".to_string(),
        MetricsIntakeUrlPrefix::new(
            None,
            MetricsIntakeUrlPrefixOverride::maybe_new(
                None,
                Some(DdDdUrl::new(server.url()).expect("failed to create URL")),
            ),
        )
        .expect("failed to create URL"),
        None,
        None,
        Duration::from_secs(1),
        retry_strategy.clone(),
        6,
    );

    // Create a series using the Aggregator
    let mut aggregator = MetricsAggregator::new(SortedTags::parse("test:value").unwrap(), 1)
        .expect("failed to create aggregator");
    let metric = parse("test:1|c").expect("failed to parse metric");
    aggregator.insert(metric).expect("failed to insert metric");
    let series = aggregator.to_series();

    let result = dd_api.ship_series(&series).await;

    // The result should be Ok since we got a 200 response on retry
    assert!(result.is_ok());
    if let Ok(response) = result {
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    } else {
        panic!("Expected Ok result");
    }

    // Verify that both mocks were called exactly once
    mock.assert_async().await;
    success_mock.assert_async().await;
}

#[cfg(test)]
#[tokio::test]
async fn test_send_with_retry_immediate_failure_after_one_attempt() {
    use dogstatsd::datadog::{DdApi, DdDdUrl, RetryStrategy};
    use dogstatsd::flusher::ShippingError;
    use dogstatsd::metric::{parse, SortedTags};

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/series")
        .with_status(500)
        .with_body("Internal Server Error")
        .expect(1)
        .create_async()
        .await;

    let retry_strategy = RetryStrategy::Immediate(1); // Only 1 attempt
    let dd_api = DdApi::new(
        "test_key".to_string(),
        MetricsIntakeUrlPrefix::new(
            None,
            MetricsIntakeUrlPrefixOverride::maybe_new(
                None,
                Some(DdDdUrl::new(server.url()).expect("failed to create URL")),
            ),
        )
        .expect("failed to create URL"),
        None,
        None,
        Duration::from_secs(1),
        retry_strategy.clone(),
        6,
    );

    // Create a series using the Aggregator
    let mut aggregator = MetricsAggregator::new(SortedTags::parse("test:value").unwrap(), 1)
        .expect("failed to create aggregator");
    let metric = parse("test:1|c").expect("failed to parse metric");
    aggregator.insert(metric).expect("failed to insert metric");
    let series = aggregator.to_series();

    let result = dd_api.ship_series(&series).await;

    // The result should be an error since we got a 500 response
    assert!(result.is_err());
    if let Err(ShippingError::Destination(Some(status), msg)) = result {
        assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(msg, "Failed to send request after 1 attempts");
    } else {
        panic!("Expected ShippingError::Destination with status 500");
    }

    // Verify that the mock was called exactly once
    mock.assert_async().await;
}

/// Verifies that `buffer_size` actually controls how many bytes the server
/// reads per UDP packet. Sends a payload larger than a small buffer and
/// checks that metrics are lost, then sends the same payload to a server
/// with a large enough buffer and checks that all metrics arrive.
#[cfg(test)]
#[tokio::test]
async fn test_buffer_size_limits_udp_read() {
    use dogstatsd::metric::SortedTags;

    // Build a payload of ~40 metrics, each ~30 bytes → ~1200 bytes total.
    // With buffer_size=256 the server will truncate the packet and lose most
    // metrics. With buffer_size=8192 everything fits.
    let metrics: Vec<String> = (0..40)
        .map(|i| format!("buf.test.m{}:{}|c", i, i))
        .collect();
    let payload = metrics.join("\n") + "\n";
    assert!(
        payload.len() > 256,
        "payload must exceed the small buffer size"
    );

    // --- Small buffer: expect truncation ---
    let (service_small, handle_small) =
        AggregatorService::new(SortedTags::parse("test:value").unwrap(), CONTEXTS)
            .expect("aggregator service creation failed");
    tokio::spawn(service_small.run());

    let cancel_small = start_dogstatsd_on_port(handle_small.clone(), 18126, Some(256)).await;
    sleep(Duration::from_millis(50)).await;

    let socket = UdpSocket::bind("0.0.0.0:0").await.expect("unable to bind");
    socket
        .send_to(payload.as_bytes(), "127.0.0.1:18126")
        .await
        .expect("send failed");
    sleep(Duration::from_millis(100)).await;

    let resp_small = handle_small.flush().await.expect("flush failed");
    let small_count: usize = resp_small.series.iter().map(|s| s.len()).sum();

    cancel_small.cancel();
    handle_small.shutdown().expect("shutdown failed");

    // --- Large buffer: expect all metrics ---
    let (service_large, handle_large) =
        AggregatorService::new(SortedTags::parse("test:value").unwrap(), CONTEXTS)
            .expect("aggregator service creation failed");
    tokio::spawn(service_large.run());

    let cancel_large = start_dogstatsd_on_port(handle_large.clone(), 18127, Some(8192)).await;
    sleep(Duration::from_millis(50)).await;

    let socket2 = UdpSocket::bind("0.0.0.0:0").await.expect("unable to bind");
    socket2
        .send_to(payload.as_bytes(), "127.0.0.1:18127")
        .await
        .expect("send failed");
    sleep(Duration::from_millis(100)).await;

    let resp_large = handle_large.flush().await.expect("flush failed");
    let large_count: usize = resp_large.series.iter().map(|s| s.len()).sum();

    cancel_large.cancel();
    handle_large.shutdown().expect("shutdown failed");

    // The small buffer must have fewer metrics than the large buffer
    assert!(
        small_count < large_count,
        "small buffer ({} metrics) should receive fewer metrics than large buffer ({} metrics)",
        small_count,
        large_count
    );
    assert_eq!(
        large_count, 40,
        "large buffer should receive all 40 metrics"
    );
}

#[cfg(test)]
#[cfg(all(windows, feature = "windows-pipes"))]
#[tokio::test]
async fn test_named_pipe_basic_communication() {
    let pipe_name = r"\\.\pipe\test_dogstatsd_basic";
    let (service, handle) = AggregatorService::new(SortedTags::parse("test:value").unwrap(), 1_024)
        .expect("aggregator service creation failed");
    tokio::spawn(service.run());

    let cancel_token = CancellationToken::new();

    // Start DogStatsD server
    let dogstatsd_task = {
        let handle = handle.clone();
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            let dogstatsd = DogStatsD::new(
                &DogStatsDConfig {
                    host: String::new(),
                    port: 0,
                    metric_namespace: None,
                    windows_pipe_name: Some(pipe_name.to_string()),
                    so_rcvbuf: None,
                    buffer_size: None,
                },
                handle,
                cancel_token,
            )
            .await;
            dogstatsd.spin().await;
        })
    };

    sleep(Duration::from_millis(100)).await;

    // Connect client and send metric
    let mut client = ClientOptions::new().open(pipe_name).expect("client open");
    client
        .write_all(b"test.metric:42|c\n")
        .await
        .expect("write failed");
    client.flush().await.expect("flush failed");

    sleep(Duration::from_millis(100)).await;

    // Verify metric was received
    let response = handle.flush().await.expect("flush failed");
    assert_eq!(response.series.len(), 1);

    // Cleanup
    cancel_token.cancel();
    let result = timeout(Duration::from_millis(500), dogstatsd_task).await;
    assert!(result.is_ok(), "task should complete after cancellation");
    handle.shutdown().expect("shutdown failed");
}

#[cfg(test)]
#[cfg(all(windows, feature = "windows-pipes"))]
#[tokio::test]
async fn test_named_pipe_disconnect_reconnect() {
    let pipe_name = r"\\.\pipe\test_dogstatsd_reconnect";
    let (service, handle) = AggregatorService::new(SortedTags::parse("test:value").unwrap(), 1_024)
        .expect("aggregator service creation failed");
    tokio::spawn(service.run());

    let cancel_token = CancellationToken::new();

    // Start DogStatsD server
    let dogstatsd_task = {
        let handle = handle.clone();
        let cancel_token_clone = cancel_token.clone();
        tokio::spawn(async move {
            let dogstatsd = DogStatsD::new(
                &DogStatsDConfig {
                    host: String::new(),
                    port: 0,
                    metric_namespace: None,
                    windows_pipe_name: Some(pipe_name.to_string()),
                    so_rcvbuf: None,
                    buffer_size: None,
                },
                handle,
                cancel_token_clone,
            )
            .await;
            dogstatsd.spin().await;
        })
    };

    sleep(Duration::from_millis(100)).await;

    // First client - connect, send, disconnect
    {
        let mut client1 = ClientOptions::new().open(pipe_name).expect("client1 open");
        client1
            .write_all(b"test.metric:1|c\n")
            .await
            .expect("write1");
        client1.flush().await.expect("flush1");
    } // client1 drops here (disconnect)

    sleep(Duration::from_millis(100)).await;

    // Second client - connect and send (creates new pipe each time)
    let mut client2 = ClientOptions::new().open(pipe_name).expect("client2 open");
    client2
        .write_all(b"test.metric:2|c\n")
        .await
        .expect("write2");
    client2.flush().await.expect("flush2");

    sleep(Duration::from_millis(100)).await;

    // Verify both metrics received and aggregated
    let response = handle.flush().await.expect("flush failed");
    assert!(
        !response.series.is_empty(),
        "Expected at least one series with metrics"
    );

    // Cleanup
    cancel_token.cancel();
    let result = timeout(Duration::from_millis(500), dogstatsd_task).await;
    assert!(result.is_ok(), "tasks should complete after cancellation");
    handle.shutdown().expect("shutdown failed");
}

#[cfg(test)]
#[cfg(all(windows, feature = "windows-pipes"))]
#[tokio::test]
async fn test_named_pipe_cancellation() {
    let pipe_name = r"\\.\pipe\test_dogstatsd_cancel";
    let (service, handle) = AggregatorService::new(SortedTags::parse("test:value").unwrap(), 1_024)
        .expect("aggregator service creation failed");
    tokio::spawn(service.run());

    let cancel_token = CancellationToken::new();

    // Start DogStatsD server
    let dogstatsd_task = {
        let handle = handle.clone();
        let cancel_token_clone = cancel_token.clone();
        tokio::spawn(async move {
            let dogstatsd = DogStatsD::new(
                &DogStatsDConfig {
                    host: String::new(),
                    port: 0,
                    metric_namespace: None,
                    windows_pipe_name: Some(pipe_name.to_string()),
                    so_rcvbuf: None,
                    buffer_size: None,
                },
                handle,
                cancel_token_clone,
            )
            .await;
            dogstatsd.spin().await;
        })
    };

    sleep(Duration::from_millis(100)).await;

    // Cancel immediately
    cancel_token.cancel();

    // Task should complete quickly
    let result = timeout(Duration::from_millis(500), dogstatsd_task).await;
    assert!(result.is_ok(), "task should complete after cancellation");

    handle.shutdown().expect("shutdown failed");
}

#[cfg(test)]
#[cfg(all(windows, feature = "windows-pipes"))]
#[tokio::test]
async fn test_buffer_split_message() {
    let pipe_name = r"\\.\pipe\test_dogstatsd_buffer_split";
    let (service, handle) = AggregatorService::new(SortedTags::parse("test:value").unwrap(), 1_024)
        .expect("aggregator service creation failed");
    tokio::spawn(service.run());

    let cancel_token = CancellationToken::new();

    // Start DogStatsD server
    let dogstatsd_task = {
        let handle = handle.clone();
        let cancel_token_clone = cancel_token.clone();
        tokio::spawn(async move {
            let dogstatsd = DogStatsD::new(
                &DogStatsDConfig {
                    host: String::new(),
                    port: 0,
                    metric_namespace: None,
                    windows_pipe_name: Some(pipe_name.to_string()),
                    so_rcvbuf: None,
                    buffer_size: None,
                },
                handle,
                cancel_token_clone,
            )
            .await;
            dogstatsd.spin().await;
        })
    };

    sleep(Duration::from_millis(100)).await;

    // Connect client and send partial message (no newline)
    let mut client = ClientOptions::new().open(pipe_name).expect("client open");
    client
        .write_all(b"test.split:1|")
        .await
        .expect("write partial");
    client.flush().await.expect("flush partial");

    // Wait briefly to simulate message arriving in separate reads
    sleep(Duration::from_millis(50)).await;

    // Verify no metrics yet - buffer should be holding incomplete message
    let response = handle.flush().await.expect("flush failed");
    assert!(
        response.series.is_empty(),
        "Expected no series from incomplete message without newline"
    );

    // Send the completion of the message
    client.write_all(b"c\n").await.expect("write completion");
    client.flush().await.expect("flush completion");

    sleep(Duration::from_millis(100)).await;

    // Verify metric was received and aggregated
    let response = handle.flush().await.expect("flush failed");
    assert!(
        !response.series.is_empty(),
        "Expected at least one series with metrics from buffered split message"
    );

    // Cleanup
    cancel_token.cancel();
    let result = timeout(Duration::from_millis(500), dogstatsd_task).await;
    assert!(result.is_ok(), "tasks should complete after cancellation");
    handle.shutdown().expect("shutdown failed");
}
