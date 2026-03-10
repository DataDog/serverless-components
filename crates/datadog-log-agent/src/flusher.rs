// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::io::Write as _;

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

    /// Drain the aggregator and ship all pending batches to Datadog.
    ///
    /// Returns `true` if all batches were shipped successfully (or if there was
    /// nothing to ship). Returns `false` if any batch failed after all retries.
    ///
    /// Failures on additional endpoints are logged but do not affect the returned value.
    pub async fn flush(&self) -> bool {
        let batches = match self.aggregator_handle.get_batches().await {
            Ok(b) => b,
            Err(e) => {
                error!("failed to retrieve log batches from aggregator: {e}");
                return false;
            }
        };

        if batches.is_empty() {
            debug!("no log batches to flush");
            return true;
        }

        debug!("flushing {} log batch(es)", batches.len());

        let (primary_url, use_compression) = self.resolve_endpoint();
        let mut all_ok = true;

        for batch in &batches {
            if let Err(e) = self.ship_batch(batch, &primary_url, use_compression).await {
                error!("failed to ship log batch to primary endpoint: {e}");
                all_ok = false;
            }

            for extra_url in &self.config.additional_endpoints {
                if let Err(e) = self.ship_batch(batch, extra_url, use_compression).await {
                    warn!("failed to ship log batch to additional endpoint {extra_url}: {e}");
                }
            }
        }

        all_ok
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

    async fn ship_batch(&self, batch: &[u8], url: &str, compress: bool) -> Result<(), FlushError> {
        let (body, content_encoding) = if compress {
            let compressed = compress_zstd(batch, self.config.compression_level)?;
            (compressed, Some("zstd"))
        } else {
            (batch.to_vec(), None)
        };

        let mut req = self
            .client
            .post(url)
            .timeout(self.config.flush_timeout)
            .header("DD-API-KEY", &self.config.api_key)
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

    async fn send_with_retry(&self, builder: reqwest::RequestBuilder) -> Result<(), FlushError> {
        let mut attempts: u32 = 0;

        loop {
            attempts += 1;

            let cloned = match builder.try_clone() {
                Some(b) => b,
                None => {
                    return Err(FlushError::Request(
                        "failed to clone request builder".to_string(),
                    ));
                }
            };

            match cloned.send().await {
                Ok(resp) => {
                    let status = resp.status();

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

                    // Permanent client errors — stop immediately, do not retry
                    if status.as_u16() >= 400 && status.as_u16() < 500 && !retryable_4xx {
                        warn!("permanent error from logs intake: {status}");
                        return Err(FlushError::PermanentError {
                            status: status.as_u16(),
                        });
                    }

                    // Transient server errors — fall through to retry
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
                return Err(FlushError::MaxRetriesExceeded { attempts });
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
    use mockito::Matcher;
    use std::time::Duration;

    fn make_entry(msg: &str) -> LogEntry {
        LogEntry::new(msg, 1_700_000_000_000)
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

        assert!(flusher.flush().await, "empty flush should succeed");
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
            .ship_batch(batch, &url, false)
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
        let result = flusher.flush().await;
        assert!(result, "OPW flush should return true on 200");
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
        let result = flusher.flush().await;
        assert!(!result, "403 should cause flush() to return false");
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
        let result = flusher.flush().await;
        assert!(result, "should succeed after 429 retry");
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
        let result = flusher.flush().await;
        assert!(result, "should succeed on second attempt");
    }
}
