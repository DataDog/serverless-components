//! Log flushing to Datadog intake endpoints with compression and retry logic.
//!
//! This module handles the final stage of log delivery: compressing batched logs
//! and sending them to Datadog via HTTP POST with automatic retry on transient failures.
//!
//! # Architecture
//!
//! ```text
//!   Aggregator
//!       │
//!       v
//!   ┌─────────────┐
//!   │ Get Batches │ (JSON arrays)
//!   └──────┬──────┘
//!          │
//!          v
//!   ┌─────────────┐
//!   │  Compress   │ (zstd)
//!   └──────┬──────┘
//!          │
//!          v
//!   ┌─────────────┐
//!   │ HTTP POST   │ (parallel)
//!   └──────┬──────┘
//!          │
//!          v
//!   ┌─────────────┐
//!   │   Retry?    │ (5xx/network errors)
//!   └─────────────┘
//! ```
//!
//! # Features
//!
//! - **Compression**: Optional zstd compression (configurable level)
//! - **Parallel flushing**: Multiple endpoints flushed concurrently
//! - **Retry logic**: Automatic retry for 5xx and network errors
//! - **API key injection**: Adds DD-API-KEY header to requests
//! - **Multiple endpoints**: Supports additional backup endpoints

use crate::config;
use crate::http::get_client;
use crate::logs::aggregator_service::AggregatorHandle;
use crate::FLUSH_RETRY_COUNT;
use dogstatsd::api_key::ApiKeyFactory;
use futures::future::join_all;
use hyper::StatusCode;
use reqwest::header::HeaderMap;
use std::error::Error;
use std::time::Instant;
use std::{io::Write, sync::Arc};
use thiserror::Error as ThisError;
use tokio::{sync::OnceCell, task::JoinSet};
use tracing::{debug, error};
use zstd::stream::write::Encoder;

/// Error returned when a request fails after all retry attempts.
///
/// Contains the original request builder so it can be retried later.
#[derive(ThisError, Debug)]
#[error("{message}")]
pub struct FailedRequestError {
    /// The original request that failed (can be retried).
    pub request: reqwest::RequestBuilder,
    /// Error message describing the failure.
    pub message: String,
}

/// Flusher for a single Datadog logs endpoint.
///
/// Handles HTTP POST requests with compression, retry logic, and header management.
#[derive(Debug, Clone)]
pub struct Flusher {
    /// HTTP client for sending requests.
    client: reqwest::Client,
    /// Base endpoint URL (e.g., "https://http-intake.logs.datadoghq.com").
    endpoint: String,
    /// Agent configuration (compression, timeouts, etc.).
    config: Arc<config::Config>,
    /// Factory for retrieving API keys.
    api_key_factory: Arc<ApiKeyFactory>,
    /// Cached headers (initialized on first use).
    headers: OnceCell<HeaderMap>,
}

impl Flusher {
    #[must_use]
    pub fn new(
        api_key_factory: Arc<ApiKeyFactory>,
        endpoint: String,
        config: Arc<config::Config>,
    ) -> Self {
        let client = get_client(&config);
        Flusher {
            client,
            endpoint,
            config,
            api_key_factory,
            headers: OnceCell::new(),
        }
    }

    pub async fn flush(&self, batches: Option<Arc<Vec<Vec<u8>>>>) -> Vec<reqwest::RequestBuilder> {
        let Some(api_key) = self.api_key_factory.get_api_key().await else {
            error!("LOGS | Skipping flushing: Failed to resolve API key");
            return vec![];
        };

        let mut set = JoinSet::new();

        if let Some(logs_batches) = batches {
            for batch in logs_batches.iter() {
                if batch.is_empty() {
                    continue;
                }
                let req = self.create_request(batch.clone(), api_key.as_str()).await;
                set.spawn(async move { Self::send(req).await });
            }
        }

        let mut failed_requests = Vec::new();
        for result in set.join_all().await {
            if let Err(e) = result {
                debug!("LOGS | Failed to join task: {}", e);
                continue;
            }

            // At this point we know the task completed successfully,
            // but the send operation itself may have failed
            if let Err(e) = result {
                if let Some(failed_req_err) = e.downcast_ref::<FailedRequestError>() {
                    // Clone the request from our custom error
                    failed_requests.push(
                        failed_req_err
                            .request
                            .try_clone()
                            .expect("should be able to clone request"),
                    );
                    debug!("LOGS | Failed to send request after retries, will retry later");
                } else {
                    debug!("LOGS | Failed to send request: {}", e);
                }
            }
        }
        failed_requests
    }

    async fn create_request(&self, data: Vec<u8>, api_key: &str) -> reqwest::RequestBuilder {
        let url = if self.config.observability_pipelines_worker_logs_enabled {
            self.endpoint.clone()
        } else {
            format!("{}/api/v2/logs", self.endpoint)
        };
        let headers = self.get_headers(api_key).await;
        self.client
            .post(&url)
            .timeout(std::time::Duration::from_secs(self.config.flush_timeout))
            .headers(headers.clone())
            .body(data)
    }

    async fn send(req: reqwest::RequestBuilder) -> Result<(), Box<dyn Error + Send>> {
        let mut attempts = 0;

        loop {
            let time = Instant::now();
            attempts += 1;
            let Some(cloned_req) = req.try_clone() else {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "can't clone",
                )));
            };
            let resp = cloned_req.send().await;
            let elapsed = time.elapsed();

            match resp {
                Ok(resp) => {
                    let status = resp.status();
                    // Don't read response body unless needed - saves memory and CPU
                    if status == StatusCode::FORBIDDEN {
                        // Access denied. Stop retrying.
                        error!(
                            "LOGS | Request was denied by Datadog: Access denied. Please verify that your API key is valid."
                        );
                        return Ok(());
                    }
                    if status.is_success() {
                        return Ok(());
                    }
                }
                Err(e) => {
                    if attempts >= FLUSH_RETRY_COUNT {
                        // After 3 failed attempts, return the original request for later retry
                        // Create a custom error that can be downcast to get the RequestBuilder
                        error!(
                            "LOGS | Failed to send request after {} ms and {} attempts: {:?}",
                            elapsed.as_millis(),
                            attempts,
                            e
                        );
                        return Err(Box::new(FailedRequestError {
                            request: req,
                            message: format!("LOGS | Failed after {attempts} attempts: {e}"),
                        }));
                    }
                }
            }
        }
    }

    async fn get_headers(&self, api_key: &str) -> &HeaderMap {
        self.headers
            .get_or_init(move || async move {
                let mut headers = HeaderMap::new();
                headers.insert(
                    "DD-API-KEY",
                    api_key.parse().expect("failed to parse header"),
                );
                if !self.config.observability_pipelines_worker_logs_enabled {
                    headers.insert(
                        "DD-PROTOCOL",
                        "agent-json".parse().expect("failed to parse header"),
                    );
                }
                headers.insert(
                    "Content-Type",
                    "application/json".parse().expect("failed to parse header"),
                );

                if self.config.logs_config_use_compression
                    && !self.config.observability_pipelines_worker_logs_enabled
                {
                    headers.insert(
                        "Content-Encoding",
                        "zstd".parse().expect("failed to parse header"),
                    );
                }
                headers
            })
            .await
    }
}

/// Coordinator for flushing logs to multiple Datadog endpoints.
///
/// Manages multiple flushers (primary + additional endpoints), retrieves
/// batches from the aggregator, compresses them, and sends to all endpoints
/// in parallel.
///
/// # Multiple Endpoints
///
/// Supports sending logs to multiple endpoints simultaneously:
/// - Primary endpoint (from config)
/// - Additional endpoints (for backup/multi-region)
///
/// All endpoints are flushed concurrently for maximum throughput.
#[allow(clippy::module_name_repetitions)]
#[derive(Clone)]
pub struct LogsFlusher {
    /// Agent configuration.
    config: Arc<config::Config>,
    /// List of flushers (one per endpoint).
    pub flushers: Vec<Flusher>,
    /// Handle to retrieve logs from aggregator.
    aggregator_handle: AggregatorHandle,
}

impl LogsFlusher {
    pub fn new(
        api_key_factory: Arc<ApiKeyFactory>,
        aggregator_handle: AggregatorHandle,
        config: Arc<config::Config>,
    ) -> Self {
        let mut flushers = Vec::new();

        let endpoint = if config.observability_pipelines_worker_logs_enabled {
            if config.observability_pipelines_worker_logs_url.is_empty() {
                error!("LOGS | Observability Pipelines Worker URL is empty");
            }
            config.observability_pipelines_worker_logs_url.clone()
        } else {
            config.logs_config_logs_dd_url.clone()
        };

        // Create primary flusher
        flushers.push(Flusher::new(
            Arc::clone(&api_key_factory),
            endpoint,
            config.clone(),
        ));

        // Create flushers for additional endpoints
        for endpoint in &config.logs_config_additional_endpoints {
            let endpoint_url = format!("https://{}:{}", endpoint.host, endpoint.port);
            let additional_api_key_factory =
                Arc::new(ApiKeyFactory::new(endpoint.api_key.clone().as_str()));
            flushers.push(Flusher::new(
                additional_api_key_factory,
                endpoint_url,
                config.clone(),
            ));
        }

        LogsFlusher {
            config,
            flushers,
            aggregator_handle,
        }
    }

    pub async fn flush(
        &self,
        retry_request: Option<reqwest::RequestBuilder>,
    ) -> Vec<reqwest::RequestBuilder> {
        let mut failed_requests = Vec::new();

        // If retry_request is provided, only process that request
        if let Some(req) = retry_request {
            if let Some(req_clone) = req.try_clone() {
                if let Err(e) = Flusher::send(req_clone).await {
                    if let Some(failed_req_err) = e.downcast_ref::<FailedRequestError>() {
                        failed_requests.push(
                            failed_req_err
                                .request
                                .try_clone()
                                .expect("should be able to clone request"),
                        );
                    }
                }
            }
        } else {
            let logs_batches = Arc::new({
                match self.aggregator_handle.get_batches().await {
                    Ok(batches) => batches
                        .into_iter()
                        .map(|batch| self.compress(batch))
                        .collect(),
                    Err(e) => {
                        debug!("Failed to flush from aggregator: {}", e);
                        Vec::new()
                    }
                }
            });

            // Send batches to each flusher
            let futures = self.flushers.iter().map(|flusher| {
                let batches = Arc::clone(&logs_batches);
                let flusher = flusher.clone();
                async move { flusher.flush(Some(batches)).await }
            });

            let results = join_all(futures).await;
            for failed in results {
                failed_requests.extend(failed);
            }
        }
        failed_requests
    }

    fn compress(&self, data: Vec<u8>) -> Vec<u8> {
        if !self.config.logs_config_use_compression
            || self.config.observability_pipelines_worker_logs_enabled
        {
            return data;
        }

        match self.encode(&data) {
            Ok(compressed_data) => compressed_data,
            Err(e) => {
                debug!("LOGS | Failed to compress data: {}", e);
                data
            }
        }
    }

    fn encode(&self, data: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut encoder = Encoder::new(Vec::new(), self.config.logs_config_compression_level)?;
        encoder.write_all(data)?;
        encoder.finish().map_err(|e| Box::new(e) as Box<dyn Error>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::logs::aggregator_service::AggregatorHandle;
    use dogstatsd::api_key::ApiKeyFactory;
    use std::sync::Arc;

    fn create_test_config() -> Config {
        Config {
            api_key: "test-api-key".to_string(),
            logs_config_logs_dd_url: "https://logs.example.com".to_string(),
            logs_config_use_compression: false,
            logs_config_compression_level: 3,
            observability_pipelines_worker_logs_enabled: false,
            observability_pipelines_worker_logs_url: String::new(),
            flush_timeout: 5,
            ..Default::default()
        }
    }

    fn create_test_aggregator_handle() -> AggregatorHandle {
        let (_service, handle) = crate::logs::aggregator_service::AggregatorService::default();
        handle
    }

    #[test]
    fn test_flusher_new() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let endpoint = "https://example.com".to_string();

        let flusher = Flusher::new(api_key_factory, endpoint.clone(), config);

        assert_eq!(flusher.endpoint, endpoint);
    }

    #[tokio::test]
    async fn test_flusher_create_request_normal_mode() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let endpoint = "https://example.com".to_string();

        let flusher = Flusher::new(api_key_factory, endpoint, config);

        let data = b"test log data".to_vec();
        let request = flusher.create_request(data, "test-api-key").await;

        // Request is created successfully (we can't inspect the URL directly from RequestBuilder)
        // but we can verify it doesn't panic
        assert!(request.try_clone().is_some());
    }

    #[tokio::test]
    async fn test_flusher_create_request_opw_mode() {
        let mut config = create_test_config();
        config.observability_pipelines_worker_logs_enabled = true;
        config.observability_pipelines_worker_logs_url = "https://opw.example.com".to_string();

        let config = Arc::new(config);
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let endpoint = "https://opw.example.com".to_string();

        let flusher = Flusher::new(api_key_factory, endpoint, config);

        let data = b"test log data".to_vec();
        let request = flusher.create_request(data, "test-api-key").await;

        assert!(request.try_clone().is_some());
    }

    #[tokio::test]
    async fn test_flusher_get_headers_without_compression() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let endpoint = "https://example.com".to_string();

        let flusher = Flusher::new(api_key_factory, endpoint, config);

        let headers = flusher.get_headers("test-api-key").await;

        assert!(headers.contains_key("DD-API-KEY"));
        assert!(headers.contains_key("DD-PROTOCOL"));
        assert!(headers.contains_key("Content-Type"));
        assert!(!headers.contains_key("Content-Encoding"));
    }

    #[tokio::test]
    async fn test_flusher_get_headers_with_compression() {
        let mut config = create_test_config();
        config.logs_config_use_compression = true;

        let config = Arc::new(config);
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let endpoint = "https://example.com".to_string();

        let flusher = Flusher::new(api_key_factory, endpoint, config);

        let headers = flusher.get_headers("test-api-key").await;

        assert!(headers.contains_key("DD-API-KEY"));
        assert!(headers.contains_key("Content-Encoding"));
        assert_eq!(headers.get("Content-Encoding").unwrap(), "zstd");
    }

    #[tokio::test]
    async fn test_flusher_get_headers_opw_mode() {
        let mut config = create_test_config();
        config.observability_pipelines_worker_logs_enabled = true;
        config.logs_config_use_compression = true;

        let config = Arc::new(config);
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let endpoint = "https://example.com".to_string();

        let flusher = Flusher::new(api_key_factory, endpoint, config);

        let headers = flusher.get_headers("test-api-key").await;

        // OPW mode should not have DD-PROTOCOL or Content-Encoding
        assert!(headers.contains_key("DD-API-KEY"));
        assert!(!headers.contains_key("DD-PROTOCOL"));
        assert!(!headers.contains_key("Content-Encoding"));
    }

    #[test]
    fn test_logs_flusher_new_basic() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator_handle = create_test_aggregator_handle();

        let logs_flusher = LogsFlusher::new(api_key_factory, aggregator_handle, config);

        // Should have exactly 1 flusher (primary endpoint)
        assert_eq!(logs_flusher.flushers.len(), 1);
    }

    #[test]
    fn test_logs_flusher_new_with_additional_endpoints() {
        let mut config = create_test_config();
        config.logs_config_additional_endpoints = vec![
            crate::config::logs_additional_endpoints::LogsAdditionalEndpoint {
                api_key: "second-key".to_string(),
                host: "logs2.example.com".to_string(),
                port: 443,
                is_reliable: false,
            },
            crate::config::logs_additional_endpoints::LogsAdditionalEndpoint {
                api_key: "third-key".to_string(),
                host: "logs3.example.com".to_string(),
                port: 443,
                is_reliable: false,
            },
        ];

        let config = Arc::new(config);
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator_handle = create_test_aggregator_handle();

        let logs_flusher = LogsFlusher::new(api_key_factory, aggregator_handle, config);

        // Should have 3 flushers (1 primary + 2 additional)
        assert_eq!(logs_flusher.flushers.len(), 3);
    }

    #[test]
    fn test_logs_flusher_compress_disabled() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator_handle = create_test_aggregator_handle();

        let logs_flusher = LogsFlusher::new(api_key_factory, aggregator_handle, config);

        let data = b"test log data".to_vec();
        let compressed = logs_flusher.compress(data.clone());

        // Compression disabled, should return original data
        assert_eq!(compressed, data);
    }

    #[test]
    fn test_logs_flusher_compress_enabled() {
        let mut config = create_test_config();
        config.logs_config_use_compression = true;

        let config = Arc::new(config);
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator_handle = create_test_aggregator_handle();

        let logs_flusher = LogsFlusher::new(api_key_factory, aggregator_handle, config);

        let data = b"test log data that should be compressed".to_vec();
        let compressed = logs_flusher.compress(data.clone());

        // Compression enabled, result should be different (and likely smaller for large data)
        assert_ne!(compressed, data);
    }

    #[test]
    fn test_logs_flusher_compress_opw_mode() {
        let mut config = create_test_config();
        config.logs_config_use_compression = true;
        config.observability_pipelines_worker_logs_enabled = true;

        let config = Arc::new(config);
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator_handle = create_test_aggregator_handle();

        let logs_flusher = LogsFlusher::new(api_key_factory, aggregator_handle, config);

        let data = b"test log data".to_vec();
        let compressed = logs_flusher.compress(data.clone());

        // OPW mode disables compression even if config says enabled
        assert_eq!(compressed, data);
    }

    #[test]
    fn test_logs_flusher_encode() {
        let mut config = create_test_config();
        config.logs_config_use_compression = true;
        config.logs_config_compression_level = 3;

        let config = Arc::new(config);
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator_handle = create_test_aggregator_handle();

        let logs_flusher = LogsFlusher::new(api_key_factory, aggregator_handle, config);

        let data = b"test log data for compression";
        let result = logs_flusher.encode(data);

        assert!(result.is_ok());
        let compressed = result.unwrap();
        assert!(!compressed.is_empty());

        // Verify we can decompress it back
        let mut decoder = zstd::stream::read::Decoder::new(&compressed[..]).unwrap();
        let mut decompressed = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut decompressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_failed_request_error() {
        let client = reqwest::Client::new();
        let request = client.post("https://example.com/test");

        let error = FailedRequestError {
            request: request.try_clone().unwrap(),
            message: "Test error".to_string(),
        };

        assert_eq!(error.message, "Test error");
        assert!(format!("{}", error).contains("Test error"));
    }
}
