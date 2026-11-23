// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Helper functions for integration tests

use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use libdd_common::http_common;
use libdd_trace_protobuf::pb;
use libdd_trace_utils::test_utils::create_test_json_span;
use serde_json::json;
use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH};
use tokio::time::timeout;

/// Create a simple test trace payload as msgpack bytes. Pass `Some(name)` to
/// override the default service ("test-service"); dual-transport tests use
/// distinct service names to distinguish payloads that flow through
/// different listeners but share a single flusher pipeline.
pub fn create_test_trace_payload(service: Option<&str>) -> Vec<u8> {
    let start = UNIX_EPOCH.elapsed().unwrap().as_nanos() as i64;
    let mut span = create_test_json_span(11, 222, 0, start, false);
    if let Some(name) = service {
        span["service"] = serde_json::Value::String(name.into());
        span["meta"]["service"] = serde_json::Value::String(name.into());
    }
    rmp_serde::to_vec(&vec![vec![span]]).expect("Failed to serialize test trace")
}

/// One v0.5 span: service, name, resource, trace ID, span ID, parent ID, start, duration, error,
/// meta, metrics and type, with every string an index into the payload's string table.
type V05Span = (
    u32,
    u32,
    u32,
    u64,
    u64,
    u64,
    i64,
    i64,
    i32,
    HashMap<u32, u32>,
    HashMap<u32, f64>,
    u32,
);

/// Create a msgpack v0.5 payload with two kept traces: spans `first` and `second` in one trace,
/// and span `third` in the other.
pub fn create_test_v05_trace_payload() -> Vec<u8> {
    let strings = [
        "",
        "test-service",
        "first",
        "second",
        "third",
        "test-resource",
        "web",
        "_sampling_priority_v1",
    ];
    let start = UNIX_EPOCH.elapsed().unwrap().as_nanos() as i64;
    let kept = HashMap::from([(7, 1.0)]);
    let span = |name, trace_id, span_id, parent_id, metrics: &HashMap<u32, f64>| -> V05Span {
        (
            1,
            name,
            5,
            trace_id,
            span_id,
            parent_id,
            start,
            5,
            0,
            HashMap::new(),
            metrics.clone(),
            6,
        )
    };
    let traces = vec![
        vec![
            span(2, 1, 10, 0, &kept),
            span(3, 1, 11, 10, &HashMap::new()),
        ],
        vec![span(4, 2, 20, 0, &kept)],
    ];
    rmp_serde::to_vec(&(strings, traces)).expect("Failed to serialize v0.5 test trace")
}

/// Create a msgpack v0.7 payload from a Python tracer with two traces: spans `first` and
/// `second` in a chunk with priority 2, origin `synthetics` and the dropped flag, and span
/// `third` in a chunk without a priority, whose span has priority 1.
pub fn create_test_v07_trace_payload() -> Vec<u8> {
    let start = UNIX_EPOCH.elapsed().unwrap().as_nanos() as i64;
    let span = |name: &str, trace_id, span_id, parent_id| {
        let mut span = create_test_json_span(trace_id, span_id, parent_id, start, false);
        span["name"] = json!(name);
        span
    };
    let mut third = span("third", 2, 20, 0);
    third["metrics"]["_sampling_priority_v1"] = json!(1.0);
    let payload = json!({
        "language_name": "python",
        "chunks": [
            {
                "priority": 2,
                "origin": "synthetics",
                "dropped_trace": true,
                "spans": [span("first", 1, 10, 0), span("second", 1, 11, 10)],
            },
            {"priority": -128, "spans": [third]},
        ],
    });
    rmp_serde::to_vec(&payload).expect("Failed to serialize v0.7 test trace")
}

/// Create a trace payload with a root span and two non-top-level, non-measured child spans:
/// - a `"server"` child, which is eligible for stats only via `span_kinds_stats_computed`
/// - an `"internal"` child, which is never eligible regardless of configuration
pub fn create_trace_with_span_kind_children_payload() -> Vec<u8> {
    let start = UNIX_EPOCH.elapsed().unwrap().as_nanos() as i64;
    let root = create_test_json_span(300, 301, 0, start, false);
    let mut server_child = create_test_json_span(300, 302, 301, start, false);
    server_child["name"] = json!("server_op");
    server_child["meta"]["span.kind"] = json!("server");
    let mut internal_child = create_test_json_span(300, 303, 301, start, false);
    internal_child["name"] = json!("internal_op");
    internal_child["meta"]["span.kind"] = json!("internal");
    rmp_serde::to_vec(&vec![vec![root, server_child, internal_child]])
        .expect("Failed to serialize span kind children trace")
}

/// Create a trace payload with a single client span that carries a `peer.service` peer tag.
/// The span is a trace root (parent_id == 0) so it is both top-level and eligible due to span.kind.
pub fn create_client_span_with_peer_tag_payload() -> Vec<u8> {
    let start = UNIX_EPOCH.elapsed().unwrap().as_nanos() as i64;
    let mut span = create_test_json_span(400, 401, 0, start, false);
    span["meta"]["span.kind"] = json!("client");
    span["meta"]["peer.service"] = json!("my-db");
    rmp_serde::to_vec(&vec![vec![span]]).expect("Failed to serialize client peer tag trace")
}

/// Build a raw, uncompressed, msgpack `ClientStatsPayload` body as sent by a tracer directly
/// to `/v0.6/stats`. `service` is used to distinguish tracer computed stats from agent computed stats.
pub fn create_test_client_stats_payload(service: &str) -> Vec<u8> {
    let payload = pb::ClientStatsPayload {
        service: service.to_string(),
        stats: vec![pb::ClientStatsBucket {
            start: 0,
            duration: 10_000_000_000,
            stats: vec![],
            agent_time_shift: 0,
        }],
        ..Default::default()
    };
    rmp_serde::to_vec_named(&payload).expect("Failed to serialize client stats payload")
}

/// Send an HTTP request over TCP and return the response
pub async fn send_tcp_request(
    port: u16,
    uri: &str,
    method: &str,
    body: Option<Vec<u8>>,
    additional_headers: &[(&str, &str)],
) -> Result<Response<hyper::body::Incoming>, Box<dyn std::error::Error>> {
    let stream = timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)),
    )
    .await??;

    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut request_builder = Request::builder()
        .uri(uri)
        .method(method)
        .header("Content-Type", "application/msgpack");

    for (name, value) in additional_headers {
        request_builder = request_builder.header(*name, *value);
    }

    let response = if let Some(body_data) = body {
        let body_len = body_data.len();
        request_builder = request_builder.header("Content-Length", body_len.to_string());
        let request = request_builder.body(http_common::Body::from(body_data))?;
        timeout(Duration::from_secs(2), sender.send_request(request)).await??
    } else {
        let request = request_builder.body(http_common::Body::empty())?;
        timeout(Duration::from_secs(2), sender.send_request(request)).await??
    };

    Ok(response)
}

#[cfg(all(windows, feature = "windows-pipes"))]
/// Send an HTTP request over named pipe and return the response
pub async fn send_named_pipe_request(
    pipe_name: &str,
    uri: &str,
    method: &str,
    body: Option<Vec<u8>>,
) -> Result<Response<hyper::body::Incoming>, Box<dyn std::error::Error>> {
    use tokio::net::windows::named_pipe::ClientOptions;

    // Note: open() is synchronous on Windows, not async
    let client = ClientOptions::new().open(pipe_name)?;

    let io = TokioIo::new(client);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut request_builder = Request::builder()
        .uri(uri)
        .method(method)
        .header("Content-Type", "application/msgpack");

    let response = if let Some(body_data) = body {
        let body_len = body_data.len();
        request_builder = request_builder.header("Content-Length", body_len.to_string());
        let request = request_builder.body(http_common::Body::from(body_data))?;
        timeout(Duration::from_secs(2), sender.send_request(request)).await??
    } else {
        let request = request_builder.body(http_common::Body::empty())?;
        timeout(Duration::from_secs(2), sender.send_request(request)).await??
    };

    Ok(response)
}
