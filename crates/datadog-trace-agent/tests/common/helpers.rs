// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Helper functions for integration tests

use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use libdd_common::hyper_migration;
use libdd_trace_utils::test_utils::create_test_json_span;
use std::time::{Duration, UNIX_EPOCH};
use tokio::time::timeout;

/// Create a simple test trace payload as msgpack bytes
pub fn create_test_trace_payload() -> Vec<u8> {
    let start = UNIX_EPOCH.elapsed().unwrap().as_nanos() as i64;
    let json_span = create_test_json_span(11, 222, 0, start, false);
    rmp_serde::to_vec(&vec![vec![json_span]]).expect("Failed to serialize test trace")
}

/// Send an HTTP request over TCP and return the response
pub async fn send_tcp_request(
    port: u16,
    uri: &str,
    method: &str,
    body: Option<Vec<u8>>,
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

    let response = if let Some(body_data) = body {
        let body_len = body_data.len();
        request_builder = request_builder.header("Content-Length", body_len.to_string());
        let request = request_builder.body(hyper_migration::Body::from(body_data))?;
        timeout(Duration::from_secs(2), sender.send_request(request)).await??
    } else {
        let request = request_builder.body(hyper_migration::Body::empty())?;
        timeout(Duration::from_secs(2), sender.send_request(request)).await??
    };

    Ok(response)
}

#[cfg(windows)]
/// Send an HTTP request over named pipe and return the response
pub async fn send_named_pipe_request(
    pipe_name: &str,
    uri: &str,
    method: &str,
    body: Option<Vec<u8>>,
) -> Result<Response<hyper::body::Incoming>, Box<dyn std::error::Error>> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let client = timeout(Duration::from_secs(2), ClientOptions::new().open(pipe_name)).await??;

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
        let request = request_builder.body(hyper_migration::Body::from(body_data))?;
        timeout(Duration::from_secs(2), sender.send_request(request)).await??
    } else {
        let request = request_builder.body(hyper_migration::Body::empty())?;
        timeout(Duration::from_secs(2), sender.send_request(request)).await??
    };

    Ok(response)
}
