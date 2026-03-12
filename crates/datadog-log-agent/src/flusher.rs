// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::io::Write as _;

use futures::future::join_all;
use reqwest::Client;
use tracing::{debug, error, warn};
use zstd::stream::write::Encoder;

use crate::aggregator::AggregatorHandle;
use crate::config::{FlusherMode, LogFlusherConfig};
use crate::errors::FlushError;

/// Maximum number of send attempts before giving up on a batch.
const MAX_FLUSH_ATTEMPTS: u32 = 3;

/// Drains log batches from an [`AggregatorHandle`] and ships them to Datadog.
#[derive(Clone)]
pub struct LogFlusher {
    config: LogFlusherConfig,
    client: Client,
    aggregator_handle: AggregatorHandle,
}

impl LogFlusher {
    /// Create a new flusher.
    ///
    /// The `client` **must** be built via
    /// [`datadog_fips::reqwest_adapter::create_reqwest_client_builder`] to ensure
    /// FIPS-compliant TLS. Never use `reqwest::Client::builder()` directly.
    pub fn new(
        config: LogFlusherConfig,
        client: Client,
        aggregator_handle: AggregatorHandle,
    ) -> Self {
        Self {
            config,
            client,
            aggregator_handle,
        }
    }

    /// Drain the aggregator, ship all pending batches to Datadog, and redrive any
    /// builders that failed transiently in the previous invocation.
    ///
    /// # Arguments
    ///
    /// * `retry_requests` — builders returned by a previous `flush` call that
    ///   exhausted their per-invocation retry budget. They are re-sent before
    ///   draining new batches from the aggregator.
    ///
    /// # Returns
    ///
    /// A vec of `RequestBuilder`s that still failed after all in-call retries.
    /// The caller should pass these back on the next invocation to re-attempt
    /// delivery. An empty vec means every batch was delivered successfully
    /// (or encountered a permanent error and was dropped — those are logged).
    ///
    /// Failures on additional endpoints are logged as warnings but their
    /// builders are not included in the returned vec (best-effort delivery).
    pub async fn flush(
        &self,
        retry_requests: Vec<reqwest::RequestBuilder>,
    ) -> Vec<reqwest::RequestBuilder> {
        let mut failed: Vec<reqwest::RequestBuilder> = Vec::new();

        // Redrive builders that failed transiently in the previous invocation.
        if !retry_requests.is_empty() {
            debug!(
                "redriving {} log builder(s) from previous flush",
                retry_requests.len()
            );
        }
        let retry_futures = retry_requests.into_iter().map(|builder| async move {
            match self.send_with_retry(builder).await {
                Ok(()) => None,
                Err(b) => Some(b),
            }
        });
        for result in join_all(retry_futures).await {
            if let Some(b) = result {
                failed.push(b);
            }
        }

        // Drain new batches from the aggregator.
        let batches = match self.aggregator_handle.get_batches().await {
            Ok(b) => b,
            Err(e) => {
                error!("failed to retrieve log batches from aggregator: {e}");
                return failed;
            }
        };

        if batches.is_empty() {
            debug!("no log batches to flush");
            return failed;
        }

        debug!("flushing {} log batch(es)", batches.len());

        let (primary_url, use_compression) = self.resolve_endpoint();

        let batch_futures = batches.iter().map(|batch| {
            let primary_url = primary_url.clone();
            async move {
                // Primary endpoint — failures are tracked for cross-invocation retry.
                let primary_result = self
                    .ship_batch(batch, &primary_url, use_compression, &self.config.api_key)
                    .await;

                // Additional endpoints — best-effort; failures are only logged.
                let extra_futures = self.config.additional_endpoints.iter().map(|endpoint| {
                    let url = endpoint.url.clone();
                    let api_key = endpoint.api_key.clone();
                    async move {
                        if let Err(_) = self
                            .ship_batch(batch, &url, use_compression, &api_key)
                            .await
                        {
                            warn!(
                                "failed to ship log batch to additional endpoint {url} after all retries"
                            );
                        }
                    }
                });
                join_all(extra_futures).await;

                primary_result
            }
        });

        for result in join_all(batch_futures).await {
            if let Err(b) = result {
                failed.push(b);
            }
        }

        failed
    }

    fn resolve_endpoint(&self) -> (String, bool) {
        match &self.config.mode {
            FlusherMode::Datadog => {
                let url = format!("https://http-intake.logs.{}/api/v2/logs", self.config.site);
                (url, self.config.use_compression)
            }
            FlusherMode::ObservabilityPipelinesWorker { url } => {
                // OPW does not support compression
                (url.clone(), false)
            }
        }
    }

    async fn ship_batch(
        &self,
        batch: &[u8],
        url: &str,
        compress: bool,
        api_key: &str,
    ) -> Result<(), reqwest::RequestBuilder> {
        let (body, content_encoding) = if compress {
            match compress_zstd(batch, self.config.compression_level) {
                Ok(compressed) => (compressed, Some("zstd")),
                Err(e) => {
                    warn!("failed to compress log batch, sending uncompressed: {e}");
                    (batch.to_vec(), None)
                }
            }
        } else {
            (batch.to_vec(), None)
        };

        let mut req = self
            .client
            .post(url)
            .timeout(self.config.flush_timeout)
            .header("DD-API-KEY", api_key)
            .header("Content-Type", "application/json");

        if matches!(self.config.mode, FlusherMode::Datadog) {
            req = req.header("DD-PROTOCOL", "agent-json");
        }

        if let Some(enc) = content_encoding {
            req = req.header("Content-Encoding", enc);
        }

        let req = req.body(body);
        self.send_with_retry(req).await
    }

    /// Send `builder`, retrying transient failures up to `MAX_FLUSH_ATTEMPTS`.
    ///
    /// # Returns
    ///
    /// * `Ok(())` — success **or** a permanent error (no point retrying; already
    ///   logged at `warn!`).
    /// * `Err(builder)` — all attempts exhausted on a transient error. The
    ///   original builder is returned so the caller can retry it next invocation.
    async fn send_with_retry(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<(), reqwest::RequestBuilder> {
        let mut attempts: u32 = 0;

        loop {
            attempts += 1;

            let cloned = match builder.try_clone() {
                Some(b) => b,
                None => {
                    // Streaming body — can't clone, can't retry.
                    warn!("log batch request is not cloneable; dropping batch");
                    return Ok(());
                }
            };

            match cloned.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    // Drain the body so the underlying TCP connection is
                    // returned to the pool rather than held in CLOSE_WAIT.
                    let _ = resp.bytes().await;

                    if status.is_success() {
                        debug!("log batch accepted: {status}");
                        return Ok(());
                    }

                    // Retryable 4xx: treat like transient server errors and
                    // fall through to the retry loop below.
                    // 408 = Request Timeout (transient network condition)
                    // 425 = Too Early (TLS 0-RTT replay rejection)
                    // 429 = Too Many Requests (intake rate-limiting)
                    //
                    // TODO: for 429, parse the `Retry-After` response header
                    // and sleep for the indicated duration before retrying
                    // instead of retrying immediately, to avoid hammering the
                    // intake endpoint while it is still rate-limiting us.
                    let retryable_4xx = matches!(status.as_u16(), 408 | 425 | 429);

                    // Permanent client errors — stop immediately, do not retry.
                    if status.as_u16() >= 400 && status.as_u16() < 500 && !retryable_4xx {
                        warn!("permanent error from logs intake: {status}; dropping batch");
                        return Ok(());
                    }

                    // Transient server errors — fall through to retry.
                    warn!(
                        "transient error from logs intake: {status} (attempt {attempts}/{MAX_FLUSH_ATTEMPTS})"
                    );
                }
                Err(e) => {
                    warn!(
                        "network error sending log batch (attempt {attempts}/{MAX_FLUSH_ATTEMPTS}): {e}"
                    );
                }
            }

            if attempts >= MAX_FLUSH_ATTEMPTS {
                warn!("log batch failed after {attempts} attempts; will retry next flush");
                return Err(builder);
            }
        }
    }
}

fn compress_zstd(data: &[u8], level: i32) -> Result<Vec<u8>, FlushError> {
    let mut encoder =
        Encoder::new(Vec::new(), level).map_err(|e| FlushError::Compression(e.to_string()))?;
    encoder
        .write_all(data)
        .map_err(|e| FlushError::Compression(e.to_string()))?;
    encoder
        .finish()
        .map_err(|e| FlushError::Compression(e.to_string()))
}

#[cfg(test)]
// Tests use plain reqwest client to connect to local mock server
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::aggregator::AggregatorService;
    use crate::config::{FlusherMode, LogFlusherConfig};
    use crate::log_entry::LogEntry;
    use crate::logs_additional_endpoint::LogsAdditionalEndpoint;
    use mockito::Matcher;
    use std::time::Duration;

    fn make_entry(msg: &str) -> LogEntry {
        LogEntry::from_message(msg, 1_700_000_000_000)
    }

    fn config_for_mock(mock_url: &str) -> LogFlusherConfig {
        // Use OPW mode pointing at the mock server to avoid HTTPS
        LogFlusherConfig {
            api_key: "test-api-key".to_string(),
            site: "datadoghq.com".to_string(),
            mode: FlusherMode::ObservabilityPipelinesWorker {
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
            mode: FlusherMode::Datadog,
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
            .ship_batch(batch, &url, false, "test-api-key")
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
            mode: FlusherMode::ObservabilityPipelinesWorker { url: opw_url },
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
            mode: FlusherMode::ObservabilityPipelinesWorker {
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
            mode: FlusherMode::ObservabilityPipelinesWorker {
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
            mode: FlusherMode::ObservabilityPipelinesWorker {
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
}
