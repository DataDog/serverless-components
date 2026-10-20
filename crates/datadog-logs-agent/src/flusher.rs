// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::future::Future;
use std::io::Write as _;
use std::pin::Pin;
use std::sync::Arc;

use futures::future::join_all;
use reqwest::Client;
use tracing::{debug, error, warn};
use zstd::stream::write::Encoder;

use crate::aggregator::AggregatorHandle;
use crate::config::{Destination, LogFlusherConfig};
use crate::errors::FlushError;

/// Maximum number of send attempts before giving up on a batch.
const MAX_FLUSH_ATTEMPTS: u32 = 3;

/// Returns the API key for the primary endpoint, or `None` if it cannot be resolved.
pub type ApiKeyResolver =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync>;

/// Drains log batches from an [`AggregatorHandle`] and ships them to Datadog.
#[derive(Clone)]
pub struct LogFlusher {
    config: LogFlusherConfig,
    client: Client,
    aggregator_handle: AggregatorHandle,
    api_key_resolver: Option<ApiKeyResolver>,
    datadog_intake_url: Option<String>,
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
            api_key_resolver: None,
            datadog_intake_url: None,
        }
    }

    /// Get the primary endpoint's API key from `resolver` on every flush instead of using
    /// `LogFlusherConfig::api_key`. If it returns `None`, the primary endpoint is skipped
    /// for that flush (its batches are dropped, with one `error!`); additional endpoints
    /// still use their own keys.
    #[must_use]
    pub fn with_api_key_resolver(mut self, resolver: ApiKeyResolver) -> Self {
        self.api_key_resolver = Some(resolver);
        self
    }

    /// In `Destination::Datadog` mode, send to `url` (full URL, including the path) instead
    /// of `https://http-intake.logs.{site}/api/v2/logs`. Ignored in Observability Pipelines
    /// Worker mode.
    #[must_use]
    pub fn with_datadog_intake_url(mut self, url: impl Into<String>) -> Self {
        self.datadog_intake_url = Some(url.into());
        self
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
    /// (or encountered a permanent error, or the primary API key could not be
    /// resolved, and was dropped — those are logged).
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
        let retry_futures = retry_requests
            .into_iter()
            .map(|builder| async move { self.send_with_retry(builder).await.err() });
        for b in join_all(retry_futures).await.into_iter().flatten() {
            failed.push(*b);
        }

        // Drain new batches from the aggregator.
        let batches = match self.aggregator_handle.get_batches().await {
            Ok(b) => b,
            Err(e) => {
                error!("failed to retrieve log batches from aggregator: {e}");
                return failed;
            }
        };

        let primary_api_key = match &self.api_key_resolver {
            None => Some(self.config.api_key.clone()),
            Some(resolve) => {
                let key = resolve().await;
                if key.is_none() {
                    error!(
                        "failed to resolve the logs API key; skipping the primary endpoint for this flush"
                    );
                }
                key
            }
        };

        if batches.is_empty() {
            debug!("no log batches to flush");
            return failed;
        }

        debug!("flushing {} log batch(es)", batches.len());

        let (primary_url, primary_use_compression) = self.resolve_endpoint();

        let batch_futures = batches.iter().map(|batch| {
            let primary_url = primary_url.clone();
            let primary_api_key = primary_api_key.as_deref();
            async move {
                // Primary endpoint — failures are tracked for cross-invocation retry.
                let is_primary_datadog = matches!(self.config.mode, Destination::Datadog);
                let primary_result = match primary_api_key {
                    Some(api_key) => {
                        self.ship_batch(batch, &primary_url, primary_use_compression, api_key, is_primary_datadog)
                            .await
                    }
                    None => Ok(()),
                };

                // Additional endpoints — best-effort; failures are only logged.
                // Additional endpoints are always Datadog intakes regardless of the
                // primary destination, so DD-PROTOCOL: agent-json is always required.
                // They use config.use_compression independently of the primary: OPW
                // primaries disable compression for themselves but Datadog extras
                // should still honour DD_LOGS_CONFIG_USE_COMPRESSION.
                let extra_futures = self.config.additional_endpoints.iter().map(|endpoint| {
                    let url = endpoint.url.clone();
                    let api_key = endpoint.api_key.clone();
                    async move {
                        if self
                            .ship_batch(batch, &url, self.config.use_compression, &api_key, true)
                            .await
                            .is_err()
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
                failed.push(*b);
            }
        }

        failed
    }

    fn resolve_endpoint(&self) -> (String, bool) {
        match &self.config.mode {
            Destination::Datadog => {
                let url = self.datadog_intake_url.clone().unwrap_or_else(|| {
                    format!("https://http-intake.logs.{}/api/v2/logs", self.config.site)
                });
                (url, self.config.use_compression)
            }
            Destination::ObservabilityPipelinesWorker { url } => {
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
        is_datadog_intake: bool,
    ) -> Result<(), Box<reqwest::RequestBuilder>> {
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

        if is_datadog_intake {
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
    /// * `Err(Box<builder>)` — all attempts exhausted on a transient error. The
    ///   original builder is returned (boxed) so the caller can retry it next invocation.
    async fn send_with_retry(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<(), Box<reqwest::RequestBuilder>> {
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
                return Err(Box::new(builder));
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
mod tests;
