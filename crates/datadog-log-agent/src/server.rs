// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! HTTP intake server for the log agent.
//!
//! [`LogServer`] listens on a TCP port and accepts `POST /v1/input` requests
//! whose body is a JSON array of [`crate::LogEntry`] values.  Entries are
//! forwarded to the shared [`crate::AggregatorHandle`] for batching and
//! eventual flushing.
//!
//! # Usage (network intake — serverless-compat)
//! ```ignore
//! let (service, handle) = AggregatorService::new();
//! tokio::spawn(service.run());
//!
//! let server = LogServer::new(
//!     LogServerConfig { host: "0.0.0.0".into(), port: 8080 },
//!     handle.clone(),
//! );
//! tokio::spawn(server.serve());
//!
//! let flusher = LogFlusher::new(config, client, handle);
//! // flush periodically …
//! ```
//!
//! # Direct intake (bottlecap — unchanged)
//! ```ignore
//! // bottlecap never uses LogServer; it calls handle.insert_batch() directly.
//! handle.insert_batch(entries).expect("insert");
//! ```

use http_body_util::BodyExt as _;
use hyper::body::{Body as _, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tracing::{debug, error, warn};

use crate::aggregator::AggregatorHandle;
use crate::log_entry::LogEntry;

const LOG_INTAKE_PATH: &str = "/v1/input";
/// Maximum accepted request body size in bytes (4 MiB). Requests larger than
/// this are rejected with 413 before the body is read into memory.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Configuration for the [`LogServer`] HTTP intake listener.
#[derive(Debug, Clone)]
pub struct LogServerConfig {
    /// Interface to bind (e.g. `"0.0.0.0"` or `"127.0.0.1"`).
    pub host: String,
    /// TCP port to listen on.
    pub port: u16,
}

/// HTTP server that receives log entries over the network and forwards them to
/// a running [`AggregatorHandle`].
///
/// Create with [`LogServer::new`], then call [`LogServer::serve`] inside a
/// `tokio::spawn` — it runs forever until the process exits.
pub struct LogServer {
    config: LogServerConfig,
    handle: AggregatorHandle,
}

impl LogServer {
    /// Create a new server. Does **not** bind the port until [`serve`](Self::serve) is called.
    pub fn new(config: LogServerConfig, handle: AggregatorHandle) -> Self {
        Self { config, handle }
    }

    /// Bind the configured port and serve HTTP/1 requests indefinitely.
    ///
    /// This is an `async fn` meant to be run inside `tokio::spawn`.
    /// It only returns if binding fails; otherwise it loops forever.
    pub async fn serve(self) {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => {
                let actual = l.local_addr().map_or(addr.clone(), |a| a.to_string());
                debug!("log server listening on {actual}");
                l
            }
            Err(e) => {
                error!("log server failed to bind {addr}: {e}");
                return;
            }
        };

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    warn!("log server accept error: {e}");
                    continue;
                }
            };

            debug!("log server: connection from {peer}");
            let handle = self.handle.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc = service_fn(move |req: Request<Incoming>| {
                    let handle = handle.clone();
                    async move { handle_request(req, handle).await }
                });
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, svc)
                    .await
                {
                    debug!("log server: connection error: {e}");
                }
            });
        }
    }
}

/// Handle a single HTTP request: route, parse body, insert into aggregator.
async fn handle_request(
    req: Request<Incoming>,
    handle: AggregatorHandle,
) -> Result<Response<String>, std::convert::Infallible> {
    if req.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body("method not allowed".to_string())
            .unwrap_or_default());
    }
    if req.uri().path() != LOG_INTAKE_PATH {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("not found".to_string())
            .unwrap_or_default());
    }

    // Collect body with size guard
    let upper = req.body().size_hint().upper().unwrap_or(u64::MAX) as usize;
    if upper > MAX_BODY_BYTES {
        return Ok(Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .body("payload too large".to_string())
            .unwrap_or_default());
    }
    let bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            warn!("log server: failed to read request body: {e}");
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body("failed to read body".to_string())
                .unwrap_or_default());
        }
    };
    if bytes.len() > MAX_BODY_BYTES {
        return Ok(Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .body("payload too large".to_string())
            .unwrap_or_default());
    }

    let entries: Vec<LogEntry> = match serde_json::from_slice(&bytes) {
        Ok(e) => e,
        Err(e) => {
            warn!("log server: failed to parse log entries: {e}");
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(format!("invalid JSON: {e}"))
                .unwrap_or_default());
        }
    };

    if entries.is_empty() {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .body("ok".to_string())
            .unwrap_or_default());
    }

    debug!("log server: received {} entries", entries.len());

    if let Err(e) = handle.insert_batch(entries) {
        error!("log server: failed to insert batch: {e}");
        return Ok(Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body("aggregator unavailable".to_string())
            .unwrap_or_default());
    }

    Ok(Response::builder()
        .status(StatusCode::OK)
        .body("ok".to_string())
        .unwrap_or_default())
}

#[cfg(test)]
// Tests use plain reqwest; FIPS client not needed for local loopback
#[allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::aggregator::AggregatorService;
    use tokio::time::{sleep, Duration};

    /// Bind `:0`, record the OS-assigned port, drop the listener, then start
    /// `LogServer` on that port. Returns the base URL.
    async fn start_test_server(handle: AggregatorHandle) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let server = LogServer::new(
            LogServerConfig {
                host: "127.0.0.1".into(),
                port,
            },
            handle,
        );
        tokio::spawn(server.serve());
        sleep(Duration::from_millis(50)).await;
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn test_post_valid_entries_returns_200_and_batch_inserted() {
        let (service, handle) = AggregatorService::new();
        tokio::spawn(service.run());

        let base_url = start_test_server(handle.clone()).await;
        let client = reqwest::Client::new();

        let entries = vec![
            serde_json::json!({"message": "hello", "timestamp": 1_700_000_000_000_i64}),
            serde_json::json!({"message": "world", "timestamp": 1_700_000_001_000_i64}),
        ];

        let resp = client
            .post(format!("{base_url}/v1/input"))
            .json(&entries)
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), 200);

        let batches = handle.get_batches().await.expect("get_batches");
        assert_eq!(batches.len(), 1, "should have one batch");
        let arr: serde_json::Value = serde_json::from_slice(&batches[0]).unwrap();
        assert_eq!(arr.as_array().unwrap().len(), 2);
        assert_eq!(arr[0]["message"], "hello");
    }

    #[tokio::test]
    async fn test_post_malformed_json_returns_400() {
        let (service, handle) = AggregatorService::new();
        tokio::spawn(service.run());

        let base_url = start_test_server(handle).await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("{base_url}/v1/input"))
            .header("Content-Type", "application/json")
            .body("not-json")
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn test_get_request_returns_405() {
        let (service, handle) = AggregatorService::new();
        tokio::spawn(service.run());

        let base_url = start_test_server(handle).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("{base_url}/v1/input"))
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), 405);
    }

    #[tokio::test]
    async fn test_wrong_path_returns_404() {
        let (service, handle) = AggregatorService::new();
        tokio::spawn(service.run());

        let base_url = start_test_server(handle).await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("{base_url}/wrong/path"))
            .json(&serde_json::json!([{"message": "x", "timestamp": 0_i64}]))
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn test_post_empty_array_returns_200_no_batch() {
        let (service, handle) = AggregatorService::new();
        tokio::spawn(service.run());

        let base_url = start_test_server(handle.clone()).await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("{base_url}/v1/input"))
            .json(&serde_json::json!([]))
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), 200);

        let batches = handle.get_batches().await.expect("get_batches");
        assert!(batches.is_empty(), "empty POST should insert nothing");
    }

    /// A request whose Content-Length header exceeds MAX_BODY_BYTES must be
    /// rejected with 413 before any body bytes are read.
    #[tokio::test]
    async fn test_oversized_content_length_returns_413() {
        let (service, handle) = AggregatorService::new();
        tokio::spawn(service.run());

        let base_url = start_test_server(handle).await;
        let client = reqwest::Client::new();

        // Real body is tiny; Content-Length is lying about the size.
        // hyper populates size_hint().upper() from Content-Length, so the
        // server hits the early-rejection check without buffering 4 MiB.
        let fake_large_size = MAX_BODY_BYTES + 1;
        let resp = client
            .post(format!("{base_url}/v1/input"))
            .header("Content-Type", "application/json")
            .header("Content-Length", fake_large_size.to_string())
            .body("[]")
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), 413);
    }

    /// All optional LogEntry fields (hostname, service, ddsource, ddtags,
    /// status) and arbitrary attributes must survive the HTTP round-trip
    /// through the server and appear intact in the aggregated batch.
    #[tokio::test]
    async fn test_full_log_entry_fields_preserved_through_http() {
        let (service, handle) = AggregatorService::new();
        tokio::spawn(service.run());

        let base_url = start_test_server(handle.clone()).await;
        let client = reqwest::Client::new();

        let payload = serde_json::json!([{
            "message":   "lambda invoked",
            "timestamp": 1_700_000_002_000_i64,
            "hostname":  "arn:aws:lambda:us-east-1:123:function:my-fn",
            "service":   "my-fn",
            "ddsource":  "lambda",
            "ddtags":    "env:prod,version:1.0",
            "status":    "info",
            "lambda": {
                "arn":        "arn:aws:lambda:us-east-1:123:function:my-fn",
                "request_id": "req-abc-123"
            }
        }]);

        let resp = client
            .post(format!("{base_url}/v1/input"))
            .json(&payload)
            .send()
            .await
            .expect("request failed");

        assert_eq!(resp.status(), 200);

        let batches = handle.get_batches().await.expect("get_batches");
        assert_eq!(batches.len(), 1);
        let arr: serde_json::Value = serde_json::from_slice(&batches[0]).unwrap();
        let entry = &arr[0];

        assert_eq!(entry["message"], "lambda invoked");
        assert_eq!(
            entry["hostname"],
            "arn:aws:lambda:us-east-1:123:function:my-fn"
        );
        assert_eq!(entry["service"], "my-fn");
        assert_eq!(entry["ddsource"], "lambda");
        assert_eq!(entry["ddtags"], "env:prod,version:1.0");
        assert_eq!(entry["status"], "info");
        // Flattened attributes must appear at the top level
        assert_eq!(entry["lambda"]["request_id"], "req-abc-123");
    }

    /// Two sequential POST requests must both accumulate in the aggregator
    /// before `get_batches` drains them.
    #[tokio::test]
    async fn test_sequential_posts_accumulate_in_aggregator() {
        let (service, handle) = AggregatorService::new();
        tokio::spawn(service.run());

        let base_url = start_test_server(handle.clone()).await;
        let client = reqwest::Client::new();

        // First request
        client
            .post(format!("{base_url}/v1/input"))
            .json(&serde_json::json!([{"message": "first", "timestamp": 1_i64}]))
            .send()
            .await
            .expect("first request failed");

        // Second request
        client
            .post(format!("{base_url}/v1/input"))
            .json(&serde_json::json!([{"message": "second", "timestamp": 2_i64}]))
            .send()
            .await
            .expect("second request failed");

        let batches = handle.get_batches().await.expect("get_batches");
        // Both entries land in the same aggregator; batch count depends on
        // the aggregator's internal sizing, but total entries must be 2.
        let total_entries: usize = batches
            .iter()
            .map(|b| {
                let arr: serde_json::Value = serde_json::from_slice(b).unwrap();
                arr.as_array().unwrap().len()
            })
            .sum();
        assert_eq!(total_entries, 2, "both entries should be in the aggregator");
    }
}
