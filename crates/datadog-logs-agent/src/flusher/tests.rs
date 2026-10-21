// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::aggregator::AggregatorService;
use crate::config::{Destination, LogFlusherConfig};
use crate::config::{FailureLogging, FlushPolicy};
use crate::intake_entry::IntakeEntry;
use crate::logs_additional_endpoint::LogsAdditionalEndpoint;
use crate::test_support::LevelCounter;
use mockito::Matcher;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn make_entry(msg: &str) -> IntakeEntry {
    IntakeEntry::from_message(msg, 1_700_000_000_000)
}

fn config_for_mock(mock_url: &str) -> LogFlusherConfig {
    // Use OPW mode pointing at the mock server to avoid HTTPS
    LogFlusherConfig {
        api_key: "test-api-key".to_string(),
        site: "datadoghq.com".to_string(),
        mode: Destination::ObservabilityPipelinesWorker {
            url: format!("{mock_url}/api/v2/logs"),
        },
        additional_endpoints: Vec::new(),
        use_compression: false,
        compression_level: 3,
        flush_timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn test_flush_empty_aggregator_does_not_call_api() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    // Server with no routes — any request would cause test failure
    let mock_server = mockito::Server::new_async().await;
    let config = config_for_mock(&mock_server.url());
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle);

    assert!(
        flusher.flush(vec![]).await.is_empty(),
        "empty flush should succeed"
    );
    // No mock assertions needed — absence of request is the assertion
}

#[tokio::test]
async fn test_flush_sends_post_with_api_key_header() {
    // Verify that Datadog mode sends both DD-API-KEY and DD-PROTOCOL:
    // agent-json headers. We call ship_batch directly to bypass
    // resolve_endpoint (which builds an HTTPS URL incompatible with the
    // HTTP mock server).
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let mut mock_server = mockito::Server::new_async().await;
    let mock = mock_server
        .mock("POST", "/api/v2/logs")
        .match_header("DD-API-KEY", "test-api-key")
        .match_header("DD-PROTOCOL", "agent-json")
        .with_status(202)
        .create_async()
        .await;

    let config = LogFlusherConfig {
        api_key: "test-api-key".to_string(),
        site: "datadoghq.com".to_string(),
        mode: Destination::Datadog,
        additional_endpoints: Vec::new(),
        use_compression: false,
        compression_level: 3,
        flush_timeout: Duration::from_secs(5),
    };

    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle);

    // Call ship_batch directly to use the mock server's HTTP URL instead
    // of the HTTPS URL that resolve_endpoint would produce.
    let url = format!("{}/api/v2/logs", mock_server.url());
    let batch = b"[{\"message\":\"test\"}]";
    flusher
        .ship_batch(batch, &url, false, "test-api-key", true)
        .await
        .expect("ship_batch should succeed");

    mock.assert_async().await;
}

#[tokio::test]
async fn test_flush_opw_mode_omits_dd_protocol_header() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let mut mock_server = mockito::Server::new_async().await;
    let opw_url = format!("{}/logs", mock_server.url());

    // Verify DD-PROTOCOL is NOT present in OPW requests
    let mock = mock_server
        .mock("POST", "/logs")
        .match_header("DD-API-KEY", "test-api-key")
        .match_header("DD-PROTOCOL", Matcher::Missing)
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let config = LogFlusherConfig {
        api_key: "test-api-key".to_string(),
        site: "unused".to_string(),
        mode: Destination::ObservabilityPipelinesWorker { url: opw_url },
        additional_endpoints: Vec::new(),
        use_compression: false,
        compression_level: 3,
        flush_timeout: Duration::from_secs(5),
    };

    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());

    handle
        .insert_batch(vec![make_entry("opw log")])
        .expect("insert");
    let result = flusher.flush(vec![]).await;
    assert!(result.is_empty(), "OPW flush should return empty on 200");
    mock.assert_async().await;
}

/// When the primary destination is OPW, additional endpoints are still Datadog
/// intakes and must receive the `DD-PROTOCOL: agent-json` header.
#[tokio::test]
async fn test_opw_primary_additional_endpoint_receives_dd_protocol_header() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let mut primary = mockito::Server::new_async().await;
    let mut extra = mockito::Server::new_async().await;

    // OPW primary must NOT have DD-PROTOCOL header
    let _primary_mock = primary
        .mock("POST", "/logs")
        .match_header("DD-PROTOCOL", Matcher::Missing)
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    // Additional endpoint (Datadog intake) MUST have DD-PROTOCOL header
    let extra_mock = extra
        .mock("POST", "/extra")
        .match_header("DD-PROTOCOL", "agent-json")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;

    let config = LogFlusherConfig {
        api_key: "key".to_string(),
        site: "datadoghq.com".to_string(),
        mode: Destination::ObservabilityPipelinesWorker {
            url: format!("{}/logs", primary.url()),
        },
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "extra-key".to_string(),
            url: format!("{}/extra", extra.url()),
            is_reliable: true,
        }],
        use_compression: false,
        compression_level: 3,
        flush_timeout: Duration::from_secs(5),
    };

    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle
        .insert_batch(vec![make_entry("test")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    extra_mock.assert_async().await;
}

#[tokio::test]
async fn test_flush_does_not_retry_on_403() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let mut mock_server = mockito::Server::new_async().await;
    // expect(1) means exactly one call — if retried, the test will fail
    let mock = mock_server
        .mock("POST", "/api/v2/logs")
        .with_status(403)
        .expect(1)
        .create_async()
        .await;

    let config = config_for_mock(&mock_server.url());
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());

    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");
    let result = flusher.flush(vec![]).await;
    // 403 is a permanent error — the batch is dropped, no builder to retry.
    assert!(
        result.is_empty(),
        "403 is a permanent error; no builder to retry"
    );
    mock.assert_async().await;
}

/// 429 (Too Many Requests) is a retryable 4xx — the retry loop must
/// continue rather than short-circuiting with a permanent failure.
#[tokio::test]
async fn test_flush_retries_on_429_then_succeeds() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let mut mock_server = mockito::Server::new_async().await;
    // First call → 429, second call → 200
    let _throttled = mock_server
        .mock("POST", "/api/v2/logs")
        .with_status(429)
        .expect(1)
        .create_async()
        .await;
    let _ok = mock_server
        .mock("POST", "/api/v2/logs")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let config = config_for_mock(&mock_server.url());
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());

    handle
        .insert_batch(vec![make_entry("throttled log")])
        .expect("insert");
    let result = flusher.flush(vec![]).await;
    assert!(result.is_empty(), "should succeed after 429 retry");
}

#[tokio::test]
async fn test_flush_retries_on_5xx_then_succeeds() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let mut mock_server = mockito::Server::new_async().await;
    // First call → 500, second call → 202
    let _fail_mock = mock_server
        .mock("POST", "/api/v2/logs")
        .with_status(500)
        .expect(1)
        .create_async()
        .await;
    let _ok_mock = mock_server
        .mock("POST", "/api/v2/logs")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;

    let config = config_for_mock(&mock_server.url());
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());

    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");
    let result = flusher.flush(vec![]).await;
    assert!(result.is_empty(), "should succeed on second attempt");
}

/// All additional endpoints receive the same batch when flush() is called.
#[tokio::test]
async fn test_additional_endpoints_all_receive_batch() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let mut primary = mockito::Server::new_async().await;
    let mut extra1 = mockito::Server::new_async().await;
    let mut extra2 = mockito::Server::new_async().await;

    let primary_mock = primary
        .mock("POST", "/api/v2/logs")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let extra1_mock = extra1
        .mock("POST", "/extra")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;
    let extra2_mock = extra2
        .mock("POST", "/extra")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let config = LogFlusherConfig {
        api_key: "key".to_string(),
        site: "datadoghq.com".to_string(),
        mode: Destination::ObservabilityPipelinesWorker {
            url: format!("{}/api/v2/logs", primary.url()),
        },
        additional_endpoints: vec![
            LogsAdditionalEndpoint {
                api_key: "extra-key-1".to_string(),
                url: format!("{}/extra", extra1.url()),
                is_reliable: true,
            },
            LogsAdditionalEndpoint {
                api_key: "extra-key-2".to_string(),
                url: format!("{}/extra", extra2.url()),
                is_reliable: true,
            },
        ],
        use_compression: false,
        compression_level: 3,
        flush_timeout: Duration::from_secs(5),
    };

    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle.insert_batch(vec![make_entry("hi")]).expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    primary_mock.assert_async().await;
    extra1_mock.assert_async().await;
    extra2_mock.assert_async().await;
}

/// Additional endpoints are dispatched concurrently: if they were sequential,
/// two endpoints each waiting at a Barrier(2) would deadlock — only concurrent
/// dispatch lets both handlers reach the barrier simultaneously.
#[tokio::test]
async fn test_additional_endpoints_dispatched_concurrently() {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio::sync::Barrier;

    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let barrier = Arc::new(Barrier::new(2));

    // Spawn a minimal HTTP server that waits at the barrier before
    // responding, so both must be in-flight at the same time to complete.
    async fn serve_once(barrier: Arc<Barrier>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await;
            barrier.wait().await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
        });
        format!("http://127.0.0.1:{}/logs", addr.port())
    }

    let url1 = serve_once(barrier.clone()).await;
    let url2 = serve_once(barrier.clone()).await;

    let mut primary = mockito::Server::new_async().await;
    let _primary_mock = primary
        .mock("POST", "/api/v2/logs")
        .with_status(202)
        .create_async()
        .await;

    let config = LogFlusherConfig {
        api_key: "key".to_string(),
        site: "datadoghq.com".to_string(),
        mode: Destination::ObservabilityPipelinesWorker {
            url: format!("{}/api/v2/logs", primary.url()),
        },
        additional_endpoints: vec![
            LogsAdditionalEndpoint {
                api_key: "extra-key-1".to_string(),
                url: url1,
                is_reliable: true,
            },
            LogsAdditionalEndpoint {
                api_key: "extra-key-2".to_string(),
                url: url2,
                is_reliable: true,
            },
        ],
        use_compression: false,
        compression_level: 3,
        flush_timeout: Duration::from_secs(5),
    };

    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle
        .insert_batch(vec![make_entry("concurrent")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
}

/// A builder returned by `flush` can be redriven on the next call.
///
/// The mock fails on the first 3 attempts (exhausting the per-invocation
/// retry budget), then succeeds on the 4th attempt (the next invocation).
/// This proves the cross-invocation retry path end-to-end.
#[tokio::test]
async fn test_cross_invocation_retry_delivers_on_redrive() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let mut mock_server = mockito::Server::new_async().await;
    // First 3 calls: transient 503 → exhausts per-invocation retry budget
    let _fail_mock = mock_server
        .mock("POST", "/api/v2/logs")
        .with_status(503)
        .expect(3)
        .create_async()
        .await;
    // 4th call: redriven on the next flush → succeeds
    let _ok_mock = mock_server
        .mock("POST", "/api/v2/logs")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let config = config_for_mock(&mock_server.url());
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle
        .insert_batch(vec![make_entry("retry-me")])
        .expect("insert");

    // First flush: all 3 attempts fail → returns the builder for retry.
    let failed = flusher.flush(vec![]).await;
    assert_eq!(failed.len(), 1, "one builder should be returned for retry");

    // Second flush: aggregator is empty; redrives the failed builder → succeeds.
    let result = flusher.flush(failed).await;
    assert!(
        result.is_empty(),
        "redriven builder should succeed on the next invocation"
    );
}

/// When the primary is OPW (compression disabled for OPW transport) and
/// `use_compression` is true, additional Datadog endpoints must still
/// receive `Content-Encoding: zstd` — they are Datadog intakes and honour
/// `DD_LOGS_CONFIG_USE_COMPRESSION` independently of the primary.
#[tokio::test]
async fn test_opw_primary_additional_endpoint_compresses_when_enabled() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let mut primary = mockito::Server::new_async().await;
    let mut extra = mockito::Server::new_async().await;

    // OPW primary must NOT have Content-Encoding
    let _primary_mock = primary
        .mock("POST", "/logs")
        .match_header("Content-Encoding", Matcher::Missing)
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    // Additional Datadog endpoint MUST receive Content-Encoding: zstd
    let extra_mock = extra
        .mock("POST", "/extra")
        .match_header("Content-Encoding", "zstd")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;

    let config = LogFlusherConfig {
        api_key: "key".to_string(),
        site: "datadoghq.com".to_string(),
        mode: Destination::ObservabilityPipelinesWorker {
            url: format!("{}/logs", primary.url()),
        },
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "extra-key".to_string(),
            url: format!("{}/extra", extra.url()),
            is_reliable: true,
        }],
        use_compression: true,
        compression_level: 3,
        flush_timeout: Duration::from_secs(5),
    };

    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle
        .insert_batch(vec![make_entry("compressed extra")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    extra_mock.assert_async().await;
}

/// Even when `use_compression: true`, the OPW primary endpoint must never
/// receive a `Content-Encoding` header — OPW does not support zstd.
#[tokio::test]
async fn test_opw_primary_never_compressed_even_when_flag_set() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let mut mock_server = mockito::Server::new_async().await;
    let mock = mock_server
        .mock("POST", "/logs")
        .match_header("Content-Encoding", Matcher::Missing)
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let config = LogFlusherConfig {
        api_key: "key".to_string(),
        site: "datadoghq.com".to_string(),
        mode: Destination::ObservabilityPipelinesWorker {
            url: format!("{}/logs", mock_server.url()),
        },
        additional_endpoints: vec![],
        use_compression: true, // flag set but must not reach OPW
        compression_level: 3,
        flush_timeout: Duration::from_secs(5),
    };

    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle
        .insert_batch(vec![make_entry("opw no compress")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    mock.assert_async().await;
}

/// Additional-endpoint failures are best-effort: their builders are NOT
/// included in the returned retry vec, even when they exhaust all retries.
/// Only primary-endpoint failures are tracked for cross-invocation retry.
#[tokio::test]
async fn test_additional_endpoint_failures_not_tracked_for_retry() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let mut primary = mockito::Server::new_async().await;
    let mut extra = mockito::Server::new_async().await;

    let _primary_mock = primary
        .mock("POST", "/api/v2/logs")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;
    // Additional endpoint always returns 503 — exhausts per-invocation retries.
    let _extra_mock = extra
        .mock("POST", "/extra")
        .with_status(503)
        .expect(3) // MAX_FLUSH_ATTEMPTS
        .create_async()
        .await;

    let config = LogFlusherConfig {
        api_key: "key".to_string(),
        site: "datadoghq.com".to_string(),
        mode: Destination::ObservabilityPipelinesWorker {
            url: format!("{}/api/v2/logs", primary.url()),
        },
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "extra-key".to_string(),
            url: format!("{}/extra", extra.url()),
            is_reliable: true,
        }],
        use_compression: false,
        compression_level: 3,
        flush_timeout: Duration::from_secs(5),
    };

    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle
        .insert_batch(vec![make_entry("test")])
        .expect("insert");

    let result = flusher.flush(vec![]).await;
    assert!(
        result.is_empty(),
        "additional-endpoint failures are best-effort and must not be tracked for retry"
    );
}

/// Default policy: client errors other than 408/425/429 are dropped after one attempt.
#[tokio::test]
async fn test_default_policy_drops_permanent_client_errors_after_one_attempt() {
    for status in [400_usize, 401, 404, 413] {
        let (service, handle) = AggregatorService::new();
        let _task = tokio::spawn(service.run());
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/v2/logs")
            .with_status(status)
            .expect(1)
            .create_async()
            .await;
        let client = reqwest::Client::builder().build().expect("client");
        let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone());
        handle
            .insert_batch(vec![make_entry("log")])
            .expect("insert");

        assert!(
            flusher.flush(vec![]).await.is_empty(),
            "status {status} must be dropped, not handed back"
        );
        mock.assert_async().await;
    }
}

/// Default policy: 408, 425 and 429 are retried like server errors, then handed back.
#[tokio::test]
async fn test_default_policy_retries_408_425_and_429() {
    for status in [408_usize, 425, 429] {
        let (service, handle) = AggregatorService::new();
        let _task = tokio::spawn(service.run());
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/v2/logs")
            .with_status(status)
            .expect(3)
            .create_async()
            .await;
        let client = reqwest::Client::builder().build().expect("client");
        let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone());
        handle
            .insert_batch(vec![make_entry("log")])
            .expect("insert");

        assert_eq!(
            flusher.flush(vec![]).await.len(),
            1,
            "status {status} is handed back"
        );
        mock.assert_async().await;
    }
}

/// Connection errors are retried and then handed back.
#[tokio::test]
async fn test_default_policy_hands_back_connection_errors() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let accepts = Arc::new(AtomicUsize::new(0));
    let accepts_in_task = Arc::clone(&accepts);
    let _server = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            accepts_in_task.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(
        config_for_mock(&format!("http://127.0.0.1:{port}")),
        client,
        handle.clone(),
    );
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    assert_eq!(flusher.flush(vec![]).await.len(), 1);
    assert_eq!(accepts.load(Ordering::SeqCst), 3);
}

/// A redriven request that fails again gets a fresh retry budget and is handed back again.
#[tokio::test]
async fn test_redriven_request_that_fails_again_is_handed_back_again() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/logs")
        .with_status(503)
        .expect(6)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone());
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    let failed = flusher.flush(vec![]).await;
    assert_eq!(failed.len(), 1);
    let failed_again = flusher.flush(failed).await;
    assert_eq!(failed_again.len(), 1);
    mock.assert_async().await;
}

/// Sends one batch in Datadog mode to a listener that drops every connection, so the
/// request is handed back and can be inspected.
async fn handed_back_datadog_request(use_compression: bool) -> reqwest::Request {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let _server = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let config = LogFlusherConfig {
        site: "datadoghq.eu".to_string(),
        mode: Destination::Datadog,
        use_compression,
        ..config_for_mock("http://unused")
    };
    // Route the public intake hostname to the local listener.
    let client = reqwest::Client::builder()
        .resolve("http-intake.logs.datadoghq.eu", addr)
        .build()
        .expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle
        .insert_batch(vec![make_entry("default path")])
        .expect("insert");

    let mut failed = flusher.flush(vec![]).await;
    assert_eq!(failed.len(), 1);
    failed.remove(0).build().expect("request")
}

/// Datadog mode by default sends compressed batches to the site's public intake with the
/// Datadog-intake headers.
#[tokio::test]
async fn test_default_datadog_request_when_compressed() {
    let req = handed_back_datadog_request(true).await;

    assert_eq!(
        req.url().as_str(),
        "https://http-intake.logs.datadoghq.eu/api/v2/logs"
    );
    assert_eq!(
        req.timeout(),
        Some(&config_for_mock("http://unused").flush_timeout)
    );
    let headers = req.headers();
    assert_eq!(headers["DD-API-KEY"], "test-api-key");
    assert_eq!(headers["DD-PROTOCOL"], "agent-json");
    assert_eq!(headers["Content-Type"], "application/json");
    assert_eq!(headers["Content-Encoding"], "zstd");
    let body =
        zstd::decode_all(req.body().expect("body").as_bytes().expect("bytes")).expect("zstd");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(json[0]["message"], "default path");
}

/// With compression off, Datadog mode sends the raw batch without Content-Encoding.
#[tokio::test]
async fn test_default_datadog_request_when_uncompressed() {
    let req = handed_back_datadog_request(false).await;

    assert_eq!(
        req.url().as_str(),
        "https://http-intake.logs.datadoghq.eu/api/v2/logs"
    );
    assert_eq!(
        req.timeout(),
        Some(&config_for_mock("http://unused").flush_timeout)
    );
    let headers = req.headers();
    assert_eq!(headers["DD-API-KEY"], "test-api-key");
    assert_eq!(headers["DD-PROTOCOL"], "agent-json");
    assert_eq!(headers["Content-Type"], "application/json");
    assert!(headers.get("Content-Encoding").is_none());
    let json: serde_json::Value =
        serde_json::from_slice(req.body().expect("body").as_bytes().expect("bytes")).expect("json");
    assert_eq!(json[0]["message"], "default path");
}

/// One-shot HTTP server. Records "<name>:received" when the full request has arrived and
/// "<name>:responded" just before answering 200 after `delay`; hands back the body.
async fn capture_once(
    name: &'static str,
    delay: Duration,
    events: Arc<std::sync::Mutex<Vec<String>>>,
) -> (String, tokio::sync::oneshot::Receiver<Vec<u8>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut raw = Vec::new();
        let mut buf = [0_u8; 8192];
        let body = loop {
            let n = stream.read(&mut buf).await.expect("read");
            assert!(n > 0, "connection closed before the request was complete");
            raw.extend_from_slice(&buf[..n]);
            let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .expect("request has content-length")
                .trim()
                .parse()
                .expect("content-length");
            while raw.len() < end + 4 + len {
                let n = stream.read(&mut buf).await.expect("read body");
                assert!(n > 0, "connection closed before the body was complete");
                raw.extend_from_slice(&buf[..n]);
            }
            break raw[end + 4..end + 4 + len].to_vec();
        };
        events
            .lock()
            .expect("lock")
            .push(format!("{name}:received"));
        tokio::time::sleep(delay).await;
        let _ = tx.send(body);
        events
            .lock()
            .expect("lock")
            .push(format!("{name}:responded"));
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
    });
    (format!("http://127.0.0.1:{port}/logs"), rx)
}

/// Default policy: per batch, additional endpoints are sent only after the primary
/// request has finished.
#[tokio::test]
async fn test_default_policy_sends_additional_endpoints_after_primary() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (primary_url, _primary_body) =
        capture_once("primary", Duration::from_millis(200), events.clone()).await;
    let (extra_url, _extra_body) = capture_once("extra", Duration::ZERO, events.clone()).await;

    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let config = LogFlusherConfig {
        mode: Destination::ObservabilityPipelinesWorker { url: primary_url },
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "extra-key".to_string(),
            url: extra_url,
            is_reliable: true,
        }],
        ..config_for_mock("http://unused")
    };
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle
        .insert_batch(vec![make_entry("ordered")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    assert_eq!(
        *events.lock().expect("lock"),
        vec![
            "primary:received",
            "primary:responded",
            "extra:received",
            "extra:responded"
        ]
    );
}

/// Default policy: when the primary fails, additional endpoints are still sent with their
/// own API key, and only the primary request is handed back.
#[tokio::test]
async fn test_default_extras_still_sent_with_their_own_key_when_primary_fails() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut primary = mockito::Server::new_async().await;
    let mut extra = mockito::Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/api/v2/logs")
        .match_header("DD-API-KEY", "test-api-key")
        .with_status(503)
        .expect(3)
        .create_async()
        .await;
    let extra_mock = extra
        .mock("POST", "/extra")
        .match_header("DD-API-KEY", "extra-key")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let config = LogFlusherConfig {
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "extra-key".to_string(),
            url: format!("{}/extra", extra.url()),
            is_reliable: true,
        }],
        ..config_for_mock(&primary.url())
    };
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    let failed = flusher.flush(vec![]).await;
    assert_eq!(failed.len(), 1);
    let req = failed
        .into_iter()
        .next()
        .expect("handed-back request")
        .build()
        .expect("request");
    assert_eq!(req.url().as_str(), format!("{}/api/v2/logs", primary.url()));
    assert_eq!(req.headers()["DD-API-KEY"], "test-api-key");
    primary_mock.assert_async().await;
    extra_mock.assert_async().await;
}

/// Every additional endpoint receives byte-identical compressed bodies that decompress to
/// exactly what the primary received.
#[tokio::test]
async fn test_additional_endpoints_receive_identical_compressed_bodies() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (primary_url, mut primary_body) =
        capture_once("primary", Duration::ZERO, events.clone()).await;
    let (url_1, mut body_1) = capture_once("extra1", Duration::ZERO, events.clone()).await;
    let (url_2, mut body_2) = capture_once("extra2", Duration::ZERO, events.clone()).await;

    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let config = LogFlusherConfig {
        mode: Destination::ObservabilityPipelinesWorker { url: primary_url },
        additional_endpoints: vec![
            LogsAdditionalEndpoint {
                api_key: "extra-key-1".to_string(),
                url: url_1,
                is_reliable: true,
            },
            LogsAdditionalEndpoint {
                api_key: "extra-key-2".to_string(),
                url: url_2,
                is_reliable: true,
            },
        ],
        use_compression: true,
        ..config_for_mock("http://unused")
    };
    let compression_level = config.compression_level;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle
        .insert_batch(
            // Varied enough that zstd levels 3 and 4 produce different bytes.
            (0..200_u64)
                .map(|i| {
                    let words: Vec<String> = (0..40)
                        .map(|j| format!("{:x}", (i * 40 + j) * 2_654_435_761 % 1_000_003))
                        .collect();
                    make_entry(&format!("same bytes {i} {}", words.join(" ")))
                })
                .collect(),
        )
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    let primary_body = primary_body.try_recv().expect("primary received a request");
    let body_1 = body_1.try_recv().expect("extra1 received a request");
    let body_2 = body_2.try_recv().expect("extra2 received a request");
    assert_eq!(body_1, body_2);
    assert_eq!(
        body_1,
        zstd::encode_all(primary_body.as_slice(), compression_level).expect("zstd"),
        "additional endpoints use the configured compression level"
    );
    let decompressed = zstd::decode_all(body_1.as_slice()).expect("zstd");
    assert_eq!(decompressed, primary_body);
    let decoded: serde_json::Value = serde_json::from_slice(&decompressed).expect("json");
    assert_eq!(decoded.as_array().expect("array").len(), 200);
}

/// Default: a request that keeps failing logs one warning per attempt plus a final one.
#[tokio::test(flavor = "current_thread")]
async fn test_default_logging_warns_on_every_failed_attempt() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/logs")
        .with_status(503)
        .expect(3)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone());
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    assert_eq!(flusher.flush(vec![]).await.len(), 1);
    mock.assert_async().await;
    assert_eq!(
        logged.warnings(),
        4,
        "3 transient-error warnings + 1 gave-up warning"
    );
    assert_eq!(logged.errors(), 0, "default logging uses warn!, not error!");
}

/// Default: a connection that keeps dropping logs one warning per attempt plus a final one.
#[tokio::test(flavor = "current_thread")]
async fn test_default_logging_warns_on_every_failed_network_attempt() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let _server = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(
        config_for_mock(&format!("http://127.0.0.1:{port}")),
        client,
        handle.clone(),
    );
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    assert_eq!(flusher.flush(vec![]).await.len(), 1);
    assert_eq!(
        logged.warnings(),
        4,
        "3 network-error warnings + 1 gave-up warning"
    );
    assert_eq!(logged.errors(), 0, "default logging uses warn!, not error!");
}

/// Default: a permanent client error logs exactly one warning.
#[tokio::test(flavor = "current_thread")]
async fn test_default_logging_warns_once_on_permanent_error() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/logs")
        .with_status(404)
        .expect(1)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone());
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    mock.assert_async().await;
    assert_eq!(logged.warnings(), 1);
    assert_eq!(logged.errors(), 0, "default logging uses warn!, not error!");
}

/// Default: an additional endpoint that keeps failing adds one more warning on top of
/// the per-attempt ones.
#[tokio::test(flavor = "current_thread")]
async fn test_default_logging_for_failing_additional_endpoint() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut primary = mockito::Server::new_async().await;
    let mut extra = mockito::Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/api/v2/logs")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let extra_mock = extra
        .mock("POST", "/extra")
        .with_status(503)
        .expect(3)
        .create_async()
        .await;
    let config = LogFlusherConfig {
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "extra-key".to_string(),
            url: format!("{}/extra", extra.url()),
            is_reliable: true,
        }],
        ..config_for_mock(&primary.url())
    };
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone());
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    primary_mock.assert_async().await;
    extra_mock.assert_async().await;
    assert_eq!(
        logged.warnings(),
        5,
        "3 transient + 1 gave-up + 1 additional-endpoint warning"
    );
    assert_eq!(logged.errors(), 0, "default logging uses warn!, not error!");
}

/// Default: each entry over the 1 MiB limit is dropped with exactly one warning.
#[tokio::test(flavor = "current_thread")]
async fn test_default_logging_warns_once_per_dropped_oversized_entry() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());

    let oversized = "a".repeat(crate::constants::MAX_LOG_BYTES + 1);
    handle
        .insert_batch(vec![make_entry(&oversized), make_entry(&oversized)])
        .expect("insert");
    assert!(handle.get_batches().await.expect("batches").is_empty());
    assert_eq!(logged.warnings(), 2);
    assert_eq!(logged.errors(), 0, "default logging uses warn!, not error!");
}

/// A resolver that counts its calls and returns `keys` in order; the last key repeats.
fn resolver(keys: &[Option<&'static str>], calls: Arc<AtomicUsize>) -> ApiKeyResolver {
    let keys: Vec<Option<String>> = keys.iter().map(|k| k.map(str::to_string)).collect();
    Arc::new(
        move || -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>> {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            let key = keys.get(n).or(keys.last()).cloned().flatten();
            Box::pin(async move { key })
        },
    )
}

/// In Datadog mode, `with_datadog_intake_url` replaces the public intake URL; headers
/// and compression are unchanged.
#[tokio::test]
async fn test_datadog_intake_url_override_is_used_in_datadog_mode() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/custom/path")
        .match_header("DD-API-KEY", "test-api-key")
        .match_header("DD-PROTOCOL", "agent-json")
        .match_header("Content-Encoding", "zstd")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let config = LogFlusherConfig {
        mode: Destination::Datadog,
        use_compression: true,
        ..config_for_mock("http://unused")
    };
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone())
        .with_datadog_intake_url(format!("{}/custom/path", server.url()));
    handle
        .insert_batch(vec![make_entry("override")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    mock.assert_async().await;
}

/// `with_datadog_intake_url` does not affect Observability Pipelines Worker mode.
#[tokio::test]
async fn test_datadog_intake_url_override_is_ignored_for_opw() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/logs")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone())
        .with_datadog_intake_url("http://127.0.0.1:1/never");
    handle
        .insert_batch(vec![make_entry("opw")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    mock.assert_async().await;
}

/// With a resolver, the primary uses the resolved key, not `LogFlusherConfig::api_key`.
#[tokio::test]
async fn test_api_key_resolver_key_is_used_for_the_primary() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/logs")
        .match_header("DD-API-KEY", "resolved-key")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone())
        .with_api_key_resolver(resolver(
            &[Some("resolved-key")],
            Arc::new(AtomicUsize::new(0)),
        ));
    handle
        .insert_batch(vec![make_entry("keyed")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    mock.assert_async().await;
}

/// The resolver is called on every flush, even when there is nothing to send.
#[tokio::test]
async fn test_api_key_resolver_is_called_on_every_flush() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock("http://127.0.0.1:1"), client, handle)
        .with_api_key_resolver(resolver(&[Some("k")], calls.clone()));

    for _ in 0..3 {
        assert!(flusher.flush(vec![]).await.is_empty());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

/// A key that changes between flushes (e.g. a reloaded secret) is used on the next flush.
#[tokio::test]
async fn test_reloaded_api_key_is_used_on_the_next_flush() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let first = server
        .mock("POST", "/api/v2/logs")
        .match_header("DD-API-KEY", "key-1")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let second = server
        .mock("POST", "/api/v2/logs")
        .match_header("DD-API-KEY", "key-2")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone())
        .with_api_key_resolver(resolver(
            &[Some("key-1"), Some("key-2")],
            Arc::new(AtomicUsize::new(0)),
        ));

    handle
        .insert_batch(vec![make_entry("one")])
        .expect("insert");
    assert!(flusher.flush(vec![]).await.is_empty());
    handle
        .insert_batch(vec![make_entry("two")])
        .expect("insert");
    assert!(flusher.flush(vec![]).await.is_empty());
    first.assert_async().await;
    second.assert_async().await;
}

/// If the key cannot be resolved, the primary is skipped with one error line, and
/// additional endpoints are still sent with their own keys.
#[tokio::test(flavor = "current_thread")]
async fn test_unresolved_api_key_skips_the_primary_but_sends_extras() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut primary = mockito::Server::new_async().await;
    let mut extra = mockito::Server::new_async().await;
    let primary_mock = primary
        .mock("POST", "/api/v2/logs")
        .expect(0)
        .create_async()
        .await;
    let extra_mock = extra
        .mock("POST", "/extra")
        .match_header("DD-API-KEY", "extra-key")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let config = LogFlusherConfig {
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "extra-key".to_string(),
            url: format!("{}/extra", extra.url()),
            is_reliable: true,
        }],
        ..config_for_mock(&primary.url())
    };
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone())
        .with_api_key_resolver(resolver(&[None], Arc::new(AtomicUsize::new(0))));
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    primary_mock.assert_async().await;
    extra_mock.assert_async().await;
    assert_eq!((logged.warnings(), logged.errors()), (0, 1));
}

/// The unresolved-key error is logged on every flush, even with nothing to send.
#[tokio::test(flavor = "current_thread")]
async fn test_unresolved_api_key_logs_an_error_even_without_batches() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock("http://127.0.0.1:1"), client, handle)
        .with_api_key_resolver(resolver(&[None], Arc::new(AtomicUsize::new(0))));

    assert!(flusher.flush(vec![]).await.is_empty());
    assert!(flusher.flush(vec![]).await.is_empty());
    assert_eq!(logged.errors(), 2);
}

fn bottlecap_policy() -> FlushPolicy {
    FlushPolicy {
        retry_client_errors: true,
        return_additional_endpoint_failures: true,
        parallel_additional_endpoints: true,
        failure_logging: FailureLogging::OncePerRequest,
    }
}

/// Reads one full HTTP/1.1 request (headers plus a Content-Length body) from `stream`.
async fn read_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    use tokio::io::AsyncReadExt as _;
    let mut raw = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        let n = stream.read(&mut buf).await.expect("read");
        assert!(n > 0, "connection closed before the request was complete");
        raw.extend_from_slice(&buf[..n]);
        let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .expect("request has content-length")
            .trim()
            .parse()
            .expect("content-length");
        while raw.len() < end + 4 + len {
            let n = stream.read(&mut buf).await.expect("read body");
            assert!(n > 0, "connection closed before the body was complete");
            raw.extend_from_slice(&buf[..n]);
        }
        return raw[end + 4..end + 4 + len].to_vec();
    }
}

#[test]
fn test_flush_policy_default_is_todays_behavior() {
    assert_eq!(
        FlushPolicy::default(),
        FlushPolicy {
            retry_client_errors: false,
            return_additional_endpoint_failures: false,
            parallel_additional_endpoints: false,
            failure_logging: FailureLogging::PerAttempt,
        }
    );
}

/// `retry` re-sends handed-back requests without draining the aggregator.
#[tokio::test]
async fn test_retry_does_not_drain_the_aggregator() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/logs")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone());
    handle
        .insert_batch(vec![make_entry("pending")])
        .expect("insert");

    assert!(flusher.retry(vec![]).await.is_empty());
    assert!(
        !mock.matched_async().await,
        "retry must not send the pending batch"
    );
    assert!(flusher.flush(vec![]).await.is_empty());
    mock.assert_async().await;
}

/// `retry` hands back the requests that still fail.
#[tokio::test]
async fn test_retry_hands_back_requests_that_still_fail() {
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/logs")
        .with_status(503)
        .expect(6)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone());
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    let failed = flusher.flush(vec![]).await;
    assert_eq!(failed.len(), 1);
    assert_eq!(flusher.retry(failed).await.len(), 1);
    mock.assert_async().await;
}

/// Under bottlecap's policy a 400 is retried in both `flush` and `retry`, each logging
/// one error line when it gives up.
#[tokio::test(flavor = "current_thread")]
async fn test_bottlecap_policy_retries_400_in_flush_and_retry() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/logs")
        .with_status(400)
        .expect(6)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone())
        .with_policy(bottlecap_policy());
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    let failed = flusher.flush(vec![]).await;
    assert_eq!(failed.len(), 1);
    assert_eq!(flusher.retry(failed).await.len(), 1);
    mock.assert_async().await;
    assert_eq!((logged.warnings(), logged.errors()), (0, 2));
}

/// 403 stays terminal even with `retry_client_errors`.
#[tokio::test(flavor = "current_thread")]
async fn test_bottlecap_policy_keeps_403_terminal() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/api/v2/logs")
        .with_status(403)
        .expect(1)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone())
        .with_policy(bottlecap_policy());
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    mock.assert_async().await;
    assert_eq!((logged.warnings(), logged.errors()), (0, 1));
}

/// `retry` never asks the key resolver: handed-back requests keep their headers.
#[tokio::test]
async fn test_retry_never_calls_the_api_key_resolver() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("POST", "/api/v2/logs")
        .with_status(503)
        .expect(6)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone())
        .with_api_key_resolver(resolver(&[Some("k")], calls.clone()));
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    let failed = flusher.flush(vec![]).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let _ = flusher.retry(failed).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// With `return_additional_endpoint_failures`, a failing extra is handed back and no
/// "dropped" warning is logged for it (default per-attempt logging).
#[tokio::test(flavor = "current_thread")]
async fn test_policy_returns_additional_endpoint_failures() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut primary = mockito::Server::new_async().await;
    let mut extra = mockito::Server::new_async().await;
    let _primary_mock = primary
        .mock("POST", "/api/v2/logs")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let extra_mock = extra
        .mock("POST", "/extra")
        .with_status(503)
        .expect(3)
        .create_async()
        .await;
    let config = LogFlusherConfig {
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "extra-key".to_string(),
            url: format!("{}/extra", extra.url()),
            is_reliable: true,
        }],
        ..config_for_mock(&primary.url())
    };
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone()).with_policy(FlushPolicy {
        return_additional_endpoint_failures: true,
        ..FlushPolicy::default()
    });
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    let failed = flusher.flush(vec![]).await;
    assert_eq!(failed.len(), 1);
    let request = failed
        .into_iter()
        .next()
        .expect("one")
        .build()
        .expect("request");
    assert_eq!(request.url().as_str(), format!("{}/extra", extra.url()));
    extra_mock.assert_async().await;
    assert_eq!(
        logged.warnings(),
        4,
        "3 transient + 1 gave-up, no 'dropped' warning"
    );
}

/// Under bottlecap's policy a failing extra is handed back with one error line.
#[tokio::test(flavor = "current_thread")]
async fn test_bottlecap_policy_failing_extra_logs_one_error() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut primary = mockito::Server::new_async().await;
    let mut extra = mockito::Server::new_async().await;
    let _primary_mock = primary
        .mock("POST", "/api/v2/logs")
        .with_status(202)
        .expect(1)
        .create_async()
        .await;
    let _extra_mock = extra
        .mock("POST", "/extra")
        .with_status(503)
        .expect(3)
        .create_async()
        .await;
    let config = LogFlusherConfig {
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "extra-key".to_string(),
            url: format!("{}/extra", extra.url()),
            is_reliable: true,
        }],
        ..config_for_mock(&primary.url())
    };
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone()).with_policy(bottlecap_policy());
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    assert_eq!(flusher.flush(vec![]).await.len(), 1);
    assert_eq!((logged.warnings(), logged.errors()), (0, 1));
}

/// With `parallel_additional_endpoints`, the extra is contacted while the primary
/// request is still in flight.
#[tokio::test]
async fn test_policy_parallel_additional_endpoints() {
    use tokio::io::AsyncWriteExt as _;
    const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    let extra_seen = Arc::new(tokio::sync::Notify::new());
    let extra_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let extra_port = extra_listener.local_addr().expect("addr").port();
    let signal = extra_seen.clone();
    tokio::spawn(async move {
        let (mut stream, _) = extra_listener.accept().await.expect("accept");
        let _ = read_request(&mut stream).await;
        signal.notify_one();
        let _ = stream.write_all(OK).await;
    });

    // The primary answers only after the extra was contacted, or after 2 s if it never is.
    let primary_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let primary_port = primary_listener.local_addr().expect("addr").port();
    let (overlap_tx, overlap_rx) = tokio::sync::oneshot::channel();
    let wait = extra_seen.clone();
    tokio::spawn(async move {
        let (mut stream, _) = primary_listener.accept().await.expect("accept");
        let _ = read_request(&mut stream).await;
        let overlapped = tokio::time::timeout(Duration::from_secs(2), wait.notified())
            .await
            .is_ok();
        let _ = overlap_tx.send(overlapped);
        let _ = stream.write_all(OK).await;
    });

    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let config = LogFlusherConfig {
        mode: Destination::ObservabilityPipelinesWorker {
            url: format!("http://127.0.0.1:{primary_port}/logs"),
        },
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "extra-key".to_string(),
            url: format!("http://127.0.0.1:{extra_port}/logs"),
            is_reliable: true,
        }],
        ..config_for_mock("http://unused")
    };
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config, client, handle.clone()).with_policy(FlushPolicy {
        parallel_additional_endpoints: true,
        ..FlushPolicy::default()
    });
    handle
        .insert_batch(vec![make_entry("parallel")])
        .expect("insert");

    assert!(flusher.flush(vec![]).await.is_empty());
    assert!(
        overlap_rx.await.expect("primary handled a request"),
        "the extra was contacted while the primary was in flight"
    );
}

/// With `FailureLogging::OncePerRequest`, a request that keeps failing logs one error
/// line and no warnings.
#[tokio::test(flavor = "current_thread")]
async fn test_once_per_request_logging_on_transient_failure() {
    let (logged, _guard) = LevelCounter::install();
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("POST", "/api/v2/logs")
        .with_status(503)
        .expect(3)
        .create_async()
        .await;
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(config_for_mock(&server.url()), client, handle.clone())
        .with_policy(FlushPolicy {
            failure_logging: FailureLogging::OncePerRequest,
            ..FlushPolicy::default()
        });
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    assert_eq!(flusher.flush(vec![]).await.len(), 1);
    assert_eq!((logged.warnings(), logged.errors()), (0, 1));
}

/// With `FailureLogging::OncePerRequest`, dropped connections log one error line.
#[tokio::test(flavor = "current_thread")]
async fn test_once_per_request_logging_on_network_errors() {
    let (logged, _guard) = LevelCounter::install();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let _server = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    let (service, handle) = AggregatorService::new();
    let _task = tokio::spawn(service.run());
    let client = reqwest::Client::builder().build().expect("client");
    let flusher = LogFlusher::new(
        config_for_mock(&format!("http://127.0.0.1:{port}")),
        client,
        handle.clone(),
    )
    .with_policy(FlushPolicy {
        failure_logging: FailureLogging::OncePerRequest,
        ..FlushPolicy::default()
    });
    handle
        .insert_batch(vec![make_entry("log")])
        .expect("insert");

    assert_eq!(flusher.flush(vec![]).await.len(), 1);
    assert_eq!((logged.warnings(), logged.errors()), (0, 1));
}
