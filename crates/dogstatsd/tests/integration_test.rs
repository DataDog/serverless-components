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

#[cfg(windows)]
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

    let mut metrics_flusher = Flusher::new(FlusherConfig {
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
    let dogstatsd_config = DogStatsDConfig {
        host: "127.0.0.1".to_string(),
        port: 18125,
        metric_namespace: None,
        windows_pipe_name: None,
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

#[cfg(test)]
#[cfg(windows)]
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
    assert_eq!(response.series[0].series[0].metric, "test.metric");

    // Cleanup
    cancel_token.cancel();
    drop(client);
    let _ = timeout(Duration::from_millis(500), dogstatsd_task).await;
    handle.shutdown().expect("shutdown failed");
}

#[cfg(test)]
#[cfg(windows)]
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
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            let dogstatsd = DogStatsD::new(
                &DogStatsDConfig {
                    host: String::new(),
                    port: 0,
                    metric_namespace: None,
                    windows_pipe_name: Some(pipe_name.to_string()),
                },
                handle,
                cancel_token,
            )
            .await;
            dogstatsd.spin().await;
        })
    };

    sleep(Duration::from_millis(100)).await;

    // First client - connect, send, disconnect
    {
        let mut client1 = ClientOptions::new().open(pipe_name).expect("client1 open");
        client1.write_all(b"metric1:1|c\n").await.expect("write1");
        client1.flush().await.expect("flush1");
    } // client1 drops here (disconnect)

    sleep(Duration::from_millis(200)).await;

    // Second client - reconnect and send
    let mut client2 = ClientOptions::new().open(pipe_name).expect("client2 open");
    client2.write_all(b"metric2:2|c\n").await.expect("write2");
    client2.flush().await.expect("flush2");

    sleep(Duration::from_millis(100)).await;

    // Verify both metrics received
    let response = handle.flush().await.expect("flush failed");
    assert_eq!(response.series.len(), 2);

    // Cleanup
    cancel_token.cancel();
    drop(client2);
    let _ = timeout(Duration::from_millis(500), dogstatsd_task).await;
    handle.shutdown().expect("shutdown failed");
}

#[cfg(test)]
#[cfg(windows)]
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
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            let dogstatsd = DogStatsD::new(
                &DogStatsDConfig {
                    host: String::new(),
                    port: 0,
                    metric_namespace: None,
                    windows_pipe_name: Some(pipe_name.to_string()),
                },
                handle,
                cancel_token,
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
