// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the `datadog-log-agent` crate.
//!
//! These tests exercise two intake paths:
//!
//! **Direct intake** (bottlecap / in-process):
//!   `LogEntry` → `AggregatorHandle::insert_batch` → `LogFlusher::flush` → HTTP endpoint
//!
//! **Network intake** (serverless-compat / over HTTP):
//!   HTTP POST → `LogServer` → `AggregatorHandle::insert_batch` → `LogFlusher::flush` → HTTP endpoint
//!
//! HTTP traffic is directed to a local `mockito` server via
//! `FlusherMode::ObservabilityPipelinesWorker`, which accepts a direct URL.
//! Datadog-mode-specific headers (`DD-PROTOCOL`) are covered by unit tests in `flusher.rs`.

#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::expect_used)]

use datadog_log_agent::{
    AggregatorService, FlusherMode, LogEntry, LogFlusher, LogFlusherConfig, LogServer,
    LogServerConfig, LogsAdditionalEndpoint,
};
use mockito::{Matcher, Server};
use std::time::Duration;
use tokio::time::sleep;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .build()
        .expect("failed to build HTTP client")
}

/// Config that routes all flushes to `mock_url/logs` via OPW mode.
fn opw_config(mock_url: &str) -> LogFlusherConfig {
    LogFlusherConfig {
        api_key: "test-api-key".to_string(),
        site: "ignored.datadoghq.com".to_string(),
        mode: FlusherMode::ObservabilityPipelinesWorker {
            url: format!("{}/logs", mock_url),
        },
        additional_endpoints: Vec::new(),
        use_compression: false,
        compression_level: 0,
        flush_timeout: Duration::from_secs(5),
    }
}

fn entry(msg: &str) -> LogEntry {
    LogEntry::new(msg, 1_700_000_000_000)
}

// ── Pipeline happy path ───────────────────────────────────────────────────────

/// Inserting log entries and flushing sends a single POST to the endpoint.
#[tokio::test]
async fn test_pipeline_inserts_and_flushes() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/logs")
        .match_header("DD-API-KEY", "test-api-key")
        .with_status(200)
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());

    handle
        .insert_batch(vec![entry("hello"), entry("world")])
        .expect("insert_batch");

    let result = LogFlusher::new(opw_config(&server.url()), build_client(), handle)
        .flush()
        .await;

    assert!(result, "flush should return true on 200");
    mock.assert_async().await;
}

/// Flushing with no entries makes no HTTP request.
#[tokio::test]
async fn test_empty_flush_makes_no_request() {
    let server = Server::new_async().await;
    // Any unexpected request would return 501 and cause an assertion failure below.

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());

    let url = server.url();
    let result = LogFlusher::new(opw_config(&url), build_client(), handle)
        .flush()
        .await;

    assert!(result, "empty flush should return true");
    // No mock was set up — if a request had been made, mockito would panic.
    drop(server);
}

// ── JSON payload shape ────────────────────────────────────────────────────────

/// The flushed payload is a valid JSON array containing each inserted entry.
#[tokio::test]
async fn test_payload_is_json_array_with_correct_fields() {
    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());

    handle
        .insert_batch(vec![LogEntry {
            message: "user login".to_string(),
            timestamp: 1_700_000_001_000,
            hostname: Some("web-01".to_string()),
            service: Some("auth".to_string()),
            ddsource: Some("nodejs".to_string()),
            ddtags: Some("env:prod,version:2.0".to_string()),
            status: Some("info".to_string()),
            attributes: serde_json::Map::new(),
        }])
        .expect("insert");

    let batches = handle.get_batches().await.expect("get_batches");
    assert_eq!(batches.len(), 1);

    let arr: serde_json::Value = serde_json::from_slice(&batches[0]).expect("valid JSON");
    let entries = arr.as_array().expect("JSON array");
    assert_eq!(entries.len(), 1);

    let e = &entries[0];
    assert_eq!(e["message"], "user login");
    assert_eq!(e["timestamp"], 1_700_000_001_000_i64);
    assert_eq!(e["hostname"], "web-01");
    assert_eq!(e["service"], "auth");
    assert_eq!(e["ddsource"], "nodejs");
    assert_eq!(e["ddtags"], "env:prod,version:2.0");
    assert_eq!(e["status"], "info");
}

/// Absent optional fields are not serialized into the JSON payload.
#[tokio::test]
async fn test_absent_optional_fields_not_serialized() {
    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());

    handle
        .insert_batch(vec![LogEntry::new("minimal", 0)])
        .expect("insert");

    let batches = handle.get_batches().await.expect("get_batches");
    let arr: serde_json::Value = serde_json::from_slice(&batches[0]).expect("valid JSON");
    let e = &arr[0];

    assert_eq!(e["message"], "minimal");
    assert!(e.get("hostname").is_none(), "hostname absent");
    assert!(e.get("service").is_none(), "service absent");
    assert!(e.get("ddsource").is_none(), "ddsource absent");
    assert!(e.get("ddtags").is_none(), "ddtags absent");
    assert!(e.get("status").is_none(), "status absent");
}

// ── Runtime-specific attributes ───────────────────────────────────────────────

/// Lambda-specific attributes are flattened into the top-level JSON object.
#[tokio::test]
async fn test_lambda_attributes_flattened_at_top_level() {
    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());

    let mut attrs = serde_json::Map::new();
    attrs.insert(
        "lambda".to_string(),
        serde_json::json!({
            "arn": "arn:aws:lambda:us-east-1:123456789012:function:my-fn",
            "request_id": "abc-123"
        }),
    );

    handle
        .insert_batch(vec![LogEntry {
            message: "invocation complete".to_string(),
            timestamp: 0,
            hostname: Some("my-fn".to_string()),
            service: Some("my-fn".to_string()),
            ddsource: Some("lambda".to_string()),
            ddtags: Some("env:prod".to_string()),
            status: Some("info".to_string()),
            attributes: attrs,
        }])
        .expect("insert");

    let batches = handle.get_batches().await.expect("get_batches");
    let arr: serde_json::Value = serde_json::from_slice(&batches[0]).expect("valid JSON");
    let e = &arr[0];

    // Lambda object is a top-level key (flattened via #[serde(flatten)])
    assert_eq!(
        e["lambda"]["arn"],
        "arn:aws:lambda:us-east-1:123456789012:function:my-fn"
    );
    assert_eq!(e["lambda"]["request_id"], "abc-123");
}

/// Azure-specific attributes are flattened into the top-level JSON object.
#[tokio::test]
async fn test_azure_attributes_flattened_at_top_level() {
    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());

    let mut attrs = serde_json::Map::new();
    attrs.insert(
        "azure".to_string(),
        serde_json::json!({
            "resource_id": "/subscriptions/sub-123/resourceGroups/rg/providers/Microsoft.Web/sites/my-fn",
            "operation_name": "Microsoft.Web/sites/functions/run/action"
        }),
    );

    handle
        .insert_batch(vec![LogEntry {
            message: "azure function triggered".to_string(),
            timestamp: 0,
            hostname: Some("my-azure-fn".to_string()),
            service: Some("payments".to_string()),
            ddsource: Some("azure-functions".to_string()),
            ddtags: Some("env:staging".to_string()),
            status: Some("info".to_string()),
            attributes: attrs,
        }])
        .expect("insert");

    let batches = handle.get_batches().await.expect("get_batches");
    let arr: serde_json::Value = serde_json::from_slice(&batches[0]).expect("valid JSON");
    let e = &arr[0];

    assert_eq!(e["ddsource"], "azure-functions");
    assert!(
        e["azure"]["resource_id"]
            .as_str()
            .unwrap_or("")
            .contains("Microsoft.Web"),
        "azure resource_id present"
    );
    assert_eq!(
        e["azure"]["operation_name"],
        "Microsoft.Web/sites/functions/run/action"
    );
}

// ── Batch limits ──────────────────────────────────────────────────────────────

/// Exactly MAX_BATCH_ENTRIES entries produce a single batch.
#[tokio::test]
async fn test_max_entries_fits_in_one_batch() {
    const MAX: usize = datadog_log_agent::constants::MAX_BATCH_ENTRIES;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());

    let entries: Vec<LogEntry> = (0..MAX).map(|i| entry(&format!("log {i}"))).collect();
    handle.insert_batch(entries).expect("insert");

    let batches = handle.get_batches().await.expect("get_batches");
    assert_eq!(
        batches.len(),
        1,
        "exactly MAX_BATCH_ENTRIES fits in one batch"
    );

    let arr: serde_json::Value = serde_json::from_slice(&batches[0]).expect("valid JSON");
    assert_eq!(arr.as_array().unwrap().len(), MAX);
}

/// MAX_BATCH_ENTRIES + 1 entries split into two batches; two POSTs are sent.
#[tokio::test]
async fn test_overflow_produces_two_batches_and_two_posts() {
    const MAX: usize = datadog_log_agent::constants::MAX_BATCH_ENTRIES;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/logs")
        .match_header("DD-API-KEY", "test-api-key")
        .with_status(200)
        .expect(2) // exactly 2 requests expected
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());

    let entries: Vec<LogEntry> = (0..=MAX).map(|i| entry(&format!("log {i}"))).collect();
    handle.insert_batch(entries).expect("insert");

    let result = LogFlusher::new(opw_config(&server.url()), build_client(), handle)
        .flush()
        .await;

    assert!(result);
    mock.assert_async().await;
}

// ── Oversized entries ─────────────────────────────────────────────────────────

/// Entries exceeding MAX_LOG_BYTES are silently dropped; valid entries still flush.
#[tokio::test]
async fn test_oversized_entry_dropped_valid_entries_still_flush() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/logs")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());

    let oversized = LogEntry::new(
        "x".repeat(datadog_log_agent::constants::MAX_LOG_BYTES + 1),
        0,
    );
    let normal = entry("this one is fine");

    handle
        .insert_batch(vec![oversized, normal])
        .expect("insert");

    let result = LogFlusher::new(opw_config(&server.url()), build_client(), handle)
        .flush()
        .await;

    assert!(result, "flush should succeed for valid entries");
    mock.assert_async().await;
}

/// All entries oversized means nothing to flush — no HTTP request.
#[tokio::test]
async fn test_all_oversized_entries_produces_no_request() {
    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());

    let oversized = LogEntry::new(
        "x".repeat(datadog_log_agent::constants::MAX_LOG_BYTES + 1),
        0,
    );
    handle.insert_batch(vec![oversized]).expect("insert");

    let batches = handle.get_batches().await.expect("get_batches");
    assert!(
        batches.is_empty(),
        "oversized-only aggregator should produce no batches"
    );
}

// ── Concurrent producers ──────────────────────────────────────────────────────

/// Two cloned handles can insert concurrently; all entries appear in the flush.
#[tokio::test]
async fn test_concurrent_producers_all_entries_flushed() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/logs")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());

    let h1 = handle.clone();
    let h2 = handle.clone();

    let (r1, r2) = tokio::join!(
        tokio::spawn(async move {
            h1.insert_batch(vec![entry("from-producer-1")])
                .expect("h1 insert")
        }),
        tokio::spawn(async move {
            h2.insert_batch(vec![entry("from-producer-2")])
                .expect("h2 insert")
        }),
    );
    r1.expect("task 1");
    r2.expect("task 2");

    let result = LogFlusher::new(opw_config(&server.url()), build_client(), handle)
        .flush()
        .await;

    assert!(result);
    mock.assert_async().await;
}

// ── OPW mode ──────────────────────────────────────────────────────────────────

/// OPW mode sends to the custom URL and omits the DD-PROTOCOL header.
#[tokio::test]
async fn test_opw_mode_uses_custom_url_and_omits_dd_protocol() {
    let mut server = Server::new_async().await;
    let opw_path = "/opw-endpoint";
    let mock = server
        .mock("POST", opw_path)
        .match_header("DD-API-KEY", "test-api-key")
        .match_header("DD-PROTOCOL", Matcher::Missing)
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());
    handle.insert_batch(vec![entry("opw log")]).expect("insert");

    let config = LogFlusherConfig {
        api_key: "test-api-key".to_string(),
        site: "ignored".to_string(),
        mode: FlusherMode::ObservabilityPipelinesWorker {
            url: format!("{}{}", server.url(), opw_path),
        },
        additional_endpoints: Vec::new(),
        use_compression: false,
        compression_level: 0,
        flush_timeout: Duration::from_secs(5),
    };

    let result = LogFlusher::new(config, build_client(), handle)
        .flush()
        .await;

    assert!(result);
    mock.assert_async().await;
}

// ── Compression ───────────────────────────────────────────────────────────────

/// OPW mode always disables compression regardless of `use_compression` setting.
/// The request must NOT carry `Content-Encoding: zstd` in OPW mode.
///
/// Note: zstd compression in Datadog mode is verified in `flusher.rs` unit tests
/// via `ship_batch` directly, since Datadog mode constructs an HTTPS URL that
/// cannot be intercepted by a plain HTTP mock server.
#[tokio::test]
async fn test_opw_mode_disables_compression_regardless_of_config() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/logs")
        .match_header("Content-Encoding", Matcher::Missing) // must not be compressed
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());
    handle
        .insert_batch(vec![entry("not compressed in OPW")])
        .expect("insert");

    // use_compression: true — but OPW mode overrides this to false
    let config = LogFlusherConfig {
        api_key: "key".to_string(),
        site: "ignored".to_string(),
        mode: FlusherMode::ObservabilityPipelinesWorker {
            url: format!("{}/logs", server.url()),
        },
        additional_endpoints: Vec::new(),
        use_compression: true,
        compression_level: 3,
        flush_timeout: Duration::from_secs(5),
    };

    let result = LogFlusher::new(config, build_client(), handle)
        .flush()
        .await;

    assert!(result);
    mock.assert_async().await;
}

// ── Retry behaviour ───────────────────────────────────────────────────────────

/// A transient 500 is retried; flush succeeds when the subsequent attempt returns 200.
#[tokio::test]
async fn test_retry_on_500_succeeds_on_second_attempt() {
    let mut server = Server::new_async().await;

    let _fail = server
        .mock("POST", "/logs")
        .with_status(500)
        .expect(1)
        .create_async()
        .await;
    let _ok = server
        .mock("POST", "/logs")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());
    handle
        .insert_batch(vec![entry("retry me")])
        .expect("insert");

    let result = LogFlusher::new(opw_config(&server.url()), build_client(), handle)
        .flush()
        .await;

    assert!(result, "should succeed after retry");
}

/// A 403 is a permanent error; flush fails without additional retry attempts.
#[tokio::test]
async fn test_permanent_error_on_403_no_retry() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/logs")
        .with_status(403)
        .expect(1) // must be called exactly once — no retries
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());
    handle
        .insert_batch(vec![entry("forbidden")])
        .expect("insert");

    let result = LogFlusher::new(opw_config(&server.url()), build_client(), handle)
        .flush()
        .await;

    assert!(!result, "403 should cause flush to return false");
    mock.assert_async().await;
}

/// All three retry attempts fail with 503; flush returns false.
#[tokio::test]
async fn test_exhausted_retries_returns_false() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/logs")
        .with_status(503)
        .expect(3) // MAX_FLUSH_ATTEMPTS = 3
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());
    handle
        .insert_batch(vec![entry("keep failing")])
        .expect("insert");

    let result = LogFlusher::new(opw_config(&server.url()), build_client(), handle)
        .flush()
        .await;

    assert!(!result, "exhausted retries should return false");
    mock.assert_async().await;
}

// ── Additional endpoints ──────────────────────────────────────────────────────

/// When additional endpoints are configured, the same batch is shipped to each.
#[tokio::test]
async fn test_additional_endpoints_receive_same_batch() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;

    let primary_mock = primary
        .mock("POST", "/logs")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let secondary_mock = secondary
        .mock("POST", "/extra")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());
    handle
        .insert_batch(vec![entry("multi-endpoint")])
        .expect("insert");

    let config = LogFlusherConfig {
        api_key: "test-api-key".to_string(),
        site: "ignored".to_string(),
        mode: FlusherMode::ObservabilityPipelinesWorker {
            url: format!("{}/logs", primary.url()),
        },
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "secondary-api-key".to_string(),
            url: format!("{}/extra", secondary.url()),
            is_reliable: true,
        }],
        use_compression: false,
        compression_level: 0,
        flush_timeout: Duration::from_secs(5),
    };

    let result = LogFlusher::new(config, build_client(), handle)
        .flush()
        .await;

    assert!(result);
    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
}

/// Additional endpoint failure does not cause flush() to return false
/// (additional endpoints are best-effort).
#[tokio::test]
async fn test_additional_endpoint_failure_does_not_affect_return_value() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;

    let _primary_mock = primary
        .mock("POST", "/logs")
        .with_status(200)
        .create_async()
        .await;

    let _secondary_mock = secondary
        .mock("POST", "/extra")
        .with_status(500) // secondary always fails
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    let _task = tokio::spawn(svc.run());
    handle.insert_batch(vec![entry("test")]).expect("insert");

    let config = LogFlusherConfig {
        api_key: "key".to_string(),
        site: "ignored".to_string(),
        mode: FlusherMode::ObservabilityPipelinesWorker {
            url: format!("{}/logs", primary.url()),
        },
        additional_endpoints: vec![LogsAdditionalEndpoint {
            api_key: "secondary-api-key".to_string(),
            url: format!("{}/extra", secondary.url()),
            is_reliable: true,
        }],
        use_compression: false,
        compression_level: 0,
        flush_timeout: Duration::from_secs(5),
    };

    let result = LogFlusher::new(config, build_client(), handle)
        .flush()
        .await;

    assert!(
        result,
        "primary succeeded — additional endpoint failure must not affect return value"
    );
}

// ── Network intake (LogServer) ────────────────────────────────────────────────

/// Bind :0 to get a free port, drop the listener, then start LogServer on that
/// port.  Returns the base URL ("http://127.0.0.1:<port>").
async fn start_log_server(handle: datadog_log_agent::AggregatorHandle) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind :0");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);

    let server = LogServer::new(
        LogServerConfig {
            host: "127.0.0.1".into(),
            port,
        },
        handle,
    );
    tokio::spawn(server.serve());
    sleep(Duration::from_millis(50)).await; // allow server to bind
    format!("http://127.0.0.1:{port}")
}

/// Full network-intake pipeline: HTTP POST → LogServer → AggregatorService →
/// LogFlusher → mockito backend.  This mirrors what serverless-compat does
/// when DD_LOGS_ENABLED=true.
#[tokio::test]
async fn test_server_to_flusher_full_pipeline() {
    let mut backend = Server::new_async().await;
    let mock = backend
        .mock("POST", "/logs")
        .match_header("DD-API-KEY", "test-api-key")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    tokio::spawn(svc.run());

    let base_url = start_log_server(handle.clone()).await;

    // External adapter POSTs a log entry over HTTP (as serverless-compat extension would).
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_url}/v1/input"))
        .json(&serde_json::json!([{
            "message":  "invocation start",
            "timestamp": 1_700_000_000_000_i64,
            "ddsource": "lambda",
            "service":  "my-fn",
            "ddtags":   "env:prod"
        }]))
        .send()
        .await
        .expect("POST to log server");

    assert_eq!(resp.status(), 200, "log server should accept the entry");

    // Flush everything accumulated in the aggregator to the mock backend.
    let result = LogFlusher::new(opw_config(&backend.url()), build_client(), handle)
        .flush()
        .await;

    assert!(result, "flush should return true on 200");
    mock.assert_async().await;
}

/// Multiple concurrent HTTP clients can POST entries simultaneously; all
/// entries must arrive in the aggregator before flushing.
#[tokio::test]
async fn test_server_concurrent_clients_all_entries_arrive() {
    let mut backend = Server::new_async().await;
    let mock = backend
        .mock("POST", "/logs")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let (svc, handle) = AggregatorService::new();
    tokio::spawn(svc.run());

    let base_url = start_log_server(handle.clone()).await;

    // Five concurrent producers each POST one entry.
    const N: usize = 5;
    let mut tasks = Vec::with_capacity(N);
    for i in 0..N {
        let url = format!("{base_url}/v1/input");
        tasks.push(tokio::spawn(async move {
            reqwest::Client::new()
                .post(&url)
                .json(&serde_json::json!([{
                    "message":  format!("entry-{i}"),
                    "timestamp": i as i64
                }]))
                .send()
                .await
                .expect("concurrent POST")
                .status()
        }));
    }

    for task in tasks {
        let status = task.await.expect("task");
        assert_eq!(status, 200);
    }

    // All N entries must be present in the aggregator.
    let batches = handle.get_batches().await.expect("get_batches");
    let total: usize = batches
        .iter()
        .map(|b| {
            let arr: serde_json::Value = serde_json::from_slice(b).unwrap();
            arr.as_array().unwrap().len()
        })
        .sum();
    assert_eq!(total, N, "all {N} concurrent entries must be aggregated");

    // Re-insert the drained entries so we have something to flush.
    let (svc2, handle2) = AggregatorService::new();
    tokio::spawn(svc2.run());
    handle2
        .insert_batch(vec![entry("placeholder")])
        .expect("insert");
    let result = LogFlusher::new(opw_config(&backend.url()), build_client(), handle2)
        .flush()
        .await;
    assert!(result);
    mock.assert_async().await;
}

/// A malformed POST (invalid JSON) must return 400 and must not prevent the
/// server from processing subsequent valid requests.
#[tokio::test]
async fn test_server_invalid_request_does_not_block_subsequent_valid_requests() {
    let (svc, handle) = AggregatorService::new();
    tokio::spawn(svc.run());

    let base_url = start_log_server(handle.clone()).await;
    let client = reqwest::Client::new();
    let url = format!("{base_url}/v1/input");

    // Bad JSON → 400
    let bad = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body("not-json-at-all")
        .send()
        .await
        .expect("bad POST");
    assert_eq!(bad.status(), 400);

    // Valid entry immediately after → 200 and entry reaches aggregator
    let good = client
        .post(&url)
        .json(&serde_json::json!([{"message": "after-error", "timestamp": 1_i64}]))
        .send()
        .await
        .expect("good POST");
    assert_eq!(good.status(), 200);

    let batches = handle.get_batches().await.expect("get_batches");
    let total: usize = batches
        .iter()
        .map(|b| {
            let arr: serde_json::Value = serde_json::from_slice(b).unwrap();
            arr.as_array().unwrap().len()
        })
        .sum();
    assert_eq!(total, 1, "only the valid entry should be in the aggregator");
}
