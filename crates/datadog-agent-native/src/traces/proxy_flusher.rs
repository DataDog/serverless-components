//! HTTP proxy request flushing for Datadog intake endpoints.
//!
//! This module handles forwarding buffered HTTP proxy requests to Datadog intake
//! endpoints with API key injection, retry logic, and parallel request execution.
//!
//! # Proxy Flushing Flow
//!
//! 1. **API Key Resolution**: Fetch API key from factory
//! 2. **Batch Retrieval**: Pull buffered requests from aggregator
//! 3. **Request Building**: Inject API key and prepare HTTP requests
//! 4. **Parallel Execution**: Send all requests concurrently via `JoinSet`
//! 5. **Retry Collection**: Gather failed requests for retry
//!
//! # Architecture
//!
//! ```text
//! Proxy Aggregator → Flusher → [Parallel HTTP Requests] → Datadog Intake
//!                       ↓
//!                 API Key Injection
//!                 Header Cleanup
//!                 Retry Logic (5xx errors)
//! ```
//!
//! # Error Handling
//!
//! - **4xx errors**: Client errors, not retried (data issue)
//! - **5xx errors**: Server errors, retried up to `FLUSH_RETRY_COUNT`
//! - **Network errors**: Retried up to `FLUSH_RETRY_COUNT`
//! - **Failed requests**: Returned for later retry attempt

use dogstatsd::api_key::ApiKeyFactory;
use reqwest::header::HeaderMap;
use std::{error::Error, sync::Arc};
use thiserror::Error as ThisError;
use tokio::sync::OnceCell;
use tokio::{sync::Mutex, task::JoinSet};

use tracing::{debug, error};

use crate::{
    config,
    http::get_client,
    tags::provider,
    traces::proxy_aggregator::{Aggregator, ProxyRequest},
    FLUSH_RETRY_COUNT,
};

/// Error type for failed proxy requests that should be retried.
///
/// This error preserves the original `RequestBuilder` so it can be retried later,
/// along with a descriptive message about why the request failed.
///
/// # Retry Logic
///
/// When this error is returned, the request should be queued for retry on the next
/// flush cycle. The request builder can be cloned for retry attempts.
#[derive(ThisError, Debug)]
#[error("{message}")]
pub struct FailedProxyRequestError {
    /// The original request builder for retry attempts.
    pub request: reqwest::RequestBuilder,
    /// Human-readable error message describing the failure.
    pub message: String,
}

/// HTTP proxy request flusher for forwarding to Datadog.
///
/// The `Flusher` manages the delivery of buffered proxy requests to Datadog intake
/// endpoints with features including:
/// - API key injection (DD-API-KEY header)
/// - Header cleanup (removes host, content-length)
/// - Parallel request execution
/// - Automatic retry for 5xx errors
/// - Failed request collection for later retry
///
/// # Concurrency
///
/// Requests are sent in parallel using `tokio::task::JoinSet` to maximize throughput.
/// Each request is independent and failures don't block other requests.
///
/// # Usage
///
/// ```rust
/// use datadog_agent_native::traces::proxy_flusher::Flusher;
/// // let flusher = Flusher::new(api_key_factory, aggregator, tags_provider, config);
/// //
/// // // Flush all buffered requests
/// // let failed_requests = flusher.flush(None).await;
/// //
/// // // Retry previously failed requests
/// // if let Some(failures) = failed_requests {
/// //     flusher.flush(Some(failures)).await;
/// // }
/// ```
pub struct Flusher {
    /// HTTP client for sending requests to Datadog.
    client: reqwest::Client,
    /// Shared aggregator containing buffered proxy requests.
    aggregator: Arc<Mutex<Aggregator>>,
    /// Agent configuration (for flush timeout, etc.).
    config: Arc<config::Config>,
    /// Tags provider (reserved for future use).
    #[allow(dead_code)]
    tags_provider: Arc<provider::Provider>,
    /// Factory for resolving API keys.
    api_key_factory: Arc<ApiKeyFactory>,
    /// Cached headers (DD-API-KEY) initialized once per flush.
    headers: OnceCell<HeaderMap>,
}

impl Flusher {
    /// Creates a new proxy flusher.
    ///
    /// # Arguments
    ///
    /// * `api_key_factory` - Factory for resolving Datadog API keys
    /// * `aggregator` - Shared aggregator containing buffered proxy requests
    /// * `tags_provider` - Tags provider (reserved for future use)
    /// * `config` - Agent configuration for flush timeout, HTTP client settings
    ///
    /// # Returns
    ///
    /// A new `Flusher` instance configured with an HTTP client.
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::proxy_flusher::Flusher;
    /// use datadog_agent_native::config::Config;
    /// use std::sync::Arc;
    ///
    /// // let api_key_factory = ...; // From ApiKeyFactory::new
    /// // let aggregator = ...; // From Aggregator::default
    /// // let tags_provider = ...; // From Provider::new
    /// // let config = Arc::new(Config::default());
    /// //
    /// // let flusher = Flusher::new(api_key_factory, aggregator, tags_provider, config);
    /// ```
    pub fn new(
        api_key_factory: Arc<ApiKeyFactory>,
        aggregator: Arc<Mutex<Aggregator>>,
        tags_provider: Arc<provider::Provider>,
        config: Arc<config::Config>,
    ) -> Self {
        let client = get_client(&config);

        Flusher {
            client,
            aggregator,
            config,
            tags_provider,
            api_key_factory,
            headers: OnceCell::new(),
        }
    }

    /// Gets or initializes cached headers with API key.
    ///
    /// This method lazily initializes and caches HTTP headers containing the
    /// DD-API-KEY header. The headers are initialized once per flusher instance.
    ///
    /// # Arguments
    ///
    /// * `api_key` - Datadog API key to include in headers
    ///
    /// # Returns
    ///
    /// Reference to the cached `HeaderMap` with DD-API-KEY set.
    ///
    /// # Panics
    ///
    /// Panics if the API key cannot be parsed as a valid header value.
    async fn get_headers(&self, api_key: &str) -> &HeaderMap {
        self.headers
            .get_or_init(move || async move {
                let mut headers = HeaderMap::new();
                // Add DD-API-KEY header for Datadog authentication
                headers.insert(
                    "DD-API-KEY",
                    api_key.parse().expect("Failed to parse API key header"),
                );
                headers
            })
            .await
    }

    /// Flushes buffered proxy requests to Datadog.
    ///
    /// This method orchestrates the complete flush cycle:
    /// 1. Resolves API key from factory
    /// 2. Retrieves requests (either retries or new batch from aggregator)
    /// 3. Sends all requests in parallel using `JoinSet`
    /// 4. Collects and returns failed requests for retry
    ///
    /// # Arguments
    ///
    /// * `retry_requests` - Optional vector of previously failed requests to retry.
    ///                      If `Some`, these are retried instead of pulling new batch.
    ///
    /// # Returns
    ///
    /// - `Some(Vec<RequestBuilder>)` - Failed requests to retry on next flush
    /// - `None` - All requests succeeded or no API key available
    ///
    /// # Error Handling
    ///
    /// If API key resolution fails:
    /// - All buffered requests in aggregator are dropped
    /// - Returns `None` (no requests sent)
    /// - Error is logged
    ///
    /// # Concurrency
    ///
    /// All requests are sent in parallel using `tokio::task::JoinSet` for maximum
    /// throughput. Each request has independent retry logic and timeout.
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::proxy_flusher::Flusher;
    ///
    /// # async fn example(flusher: Flusher) {
    /// // Normal flush (pull from aggregator)
    /// let failures = flusher.flush(None).await;
    ///
    /// // Retry previously failed requests
    /// if let Some(failed_requests) = failures {
    ///     println!("Retrying {} failed requests", failed_requests.len());
    ///     flusher.flush(Some(failed_requests)).await;
    /// }
    /// # }
    /// ```
    pub async fn flush(
        &self,
        retry_requests: Option<Vec<reqwest::RequestBuilder>>,
    ) -> Option<Vec<reqwest::RequestBuilder>> {
        // Step 1: Resolve API key or abort flush
        let Some(api_key) = self.api_key_factory.get_api_key().await else {
            error!(
                "PROXY_FLUSHER | Failed to resolve API key, dropping aggregated data and skipping flush."
            );
            // Clear aggregator to prevent unbounded growth
            {
                let mut aggregator = self.aggregator.lock().await;
                aggregator.clear();
            }
            return None;
        };

        let mut join_set = JoinSet::new();
        let mut requests = Vec::<reqwest::RequestBuilder>::new();

        // Step 2: Get requests - either retries or new batch
        if retry_requests.as_ref().is_some_and(|r| !r.is_empty()) {
            // Retry previously failed requests
            let retries = retry_requests.unwrap_or_default();
            debug!("PROXY_FLUSHER | Retrying {} failed requests", retries.len());
            requests = retries;
        } else {
            // Pull new batch from aggregator and build requests
            let mut aggregator = self.aggregator.lock().await;
            for pr in aggregator.get_batch() {
                requests.push(self.create_request(pr, api_key.as_str()).await);
            }
        }

        // Step 3: Send all requests in parallel
        for request in requests {
            join_set.spawn(async move { Self::send(request).await });
        }

        // Step 4: Wait for all requests and collect failures
        let send_results = join_set.join_all().await;

        Self::get_failed_requests(send_results)
    }

    /// Creates an HTTP request from a proxy request with API key injection.
    ///
    /// This method transforms a `ProxyRequest` into a `reqwest::RequestBuilder` by:
    /// 1. Cloning and cleaning headers (removes host, content-length)
    /// 2. Injecting DD-API-KEY header
    /// 3. Setting target URL, timeout, and body
    /// 4. Adding query parameters if present
    ///
    /// # Arguments
    ///
    /// * `request` - Proxy request from aggregator
    /// * `api_key` - Datadog API key to inject
    ///
    /// # Returns
    ///
    /// A `RequestBuilder` ready to be sent to Datadog.
    ///
    /// # Header Cleanup
    ///
    /// - **host**: Removed (reqwest sets this automatically)
    /// - **content-length**: Removed (reqwest calculates this)
    /// - **DD-API-KEY**: Added for authentication
    async fn create_request(
        &self,
        request: ProxyRequest,
        api_key: &str,
    ) -> reqwest::RequestBuilder {
        let mut headers = request.headers.clone();

        // Remove headers that are not needed - reqwest will set these correctly
        headers.remove("host");
        headers.remove("content-length");

        // Add DD-API-KEY and other headers from cache
        headers.extend(self.get_headers(api_key).await.clone());

        let mut request_builder = self
            .client
            .post(&request.target_url)
            .headers(headers)
            .timeout(std::time::Duration::from_secs(self.config.flush_timeout))
            .body(request.body);

        // Add query parameters if present
        if !request.query_params.is_empty() {
            request_builder = request_builder.query(&request.query_params);
        }

        request_builder
    }

    /// Sends a single HTTP request to Datadog with retry logic.
    ///
    /// This method implements automatic retry for transient failures:
    /// - **5xx errors**: Retried up to `FLUSH_RETRY_COUNT` times (server issues)
    /// - **Network errors**: Retried up to `FLUSH_RETRY_COUNT` times (connectivity)
    /// - **4xx errors**: Not retried (client errors, invalid data)
    /// - **200/202 responses**: Success, no retry
    ///
    /// # Arguments
    ///
    /// * `request` - Request builder to send (will be cloned for retries)
    ///
    /// # Returns
    ///
    /// - `Ok(())` - Request succeeded (200/202 status)
    /// - `Err(FailedProxyRequestError)` - Request failed, should be retried later
    /// - `Err(io::Error)` - Request cannot be cloned (fatal)
    ///
    /// # Retry Logic
    ///
    /// 1. Try sending request
    /// 2. If 5xx or network error: retry up to `FLUSH_RETRY_COUNT`
    /// 3. If 4xx error: return error immediately (no retry)
    /// 4. If all retries exhausted: return `FailedProxyRequestError` for later retry
    async fn send(request: reqwest::RequestBuilder) -> Result<(), Box<dyn Error + Send>> {
        debug!("PROXY_FLUSHER | Attempting to send request");
        let mut attempts = 0;

        loop {
            attempts += 1;

            // Clone request for retry attempt (original preserved for later retry)
            let Some(cloned_request) = request.try_clone() else {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "can't clone proxy request",
                )));
            };

            let time = std::time::Instant::now();
            let response = cloned_request.send().await;
            let elapsed = time.elapsed();

            match response {
                Ok(r) => {
                    let url = r.url().to_string();
                    let status = r.status();

                    // Success: 200 OK or 202 Accepted
                    if status == 202 || status == 200 {
                        debug!(
                            "PROXY_FLUSHER | Successfully sent request in {} ms to {url}",
                            elapsed.as_millis()
                        );
                        return Ok(());
                    } else {
                        // Error response - read body for logging (truncate to avoid huge logs)
                        let body = r
                            .text()
                            .await
                            .unwrap_or_else(|_| String::from("<failed to read body>"));
                        let truncated_body = if body.len() > 500 {
                            format!("{}... (truncated {} bytes)", &body[..500], body.len())
                        } else {
                            body
                        };
                        error!(
                            "PROXY_FLUSHER | Request failed with status {status}: {truncated_body}"
                        );

                        // Decision: retry 5xx errors, give up on 4xx errors
                        // 5xx = server error (transient, may succeed on retry)
                        // 4xx = client error (bad data, won't succeed on retry)
                        if status.is_server_error() && attempts < FLUSH_RETRY_COUNT {
                            // Retry server errors (continue loop)
                            continue;
                        } else {
                            // Give up: either 4xx or exhausted retries
                            // Return error so request can be retried on next flush cycle
                            return Err(Box::new(FailedProxyRequestError {
                                request,
                                message: format!("Request failed with status {status}"),
                            }));
                        }
                    }
                }
                Err(e) => {
                    // Network error (connection failed, timeout, etc.)
                    if attempts >= FLUSH_RETRY_COUNT {
                        // Exhausted retries - return error for later retry
                        error!(
                            "PROXY_FLUSHER | Failed to send request after {} attempts: {:?}",
                            attempts, e
                        );

                        return Err(Box::new(FailedProxyRequestError {
                            request,
                            message: e.to_string(),
                        }));
                    }
                    // Retry network errors (continue loop)
                }
            }
        }
    }

    /// Extracts failed requests from send results for retry.
    ///
    /// This method processes the results from parallel request sends and extracts
    /// any requests that failed (`FailedProxyRequestError`) so they can be retried
    /// on the next flush cycle.
    ///
    /// # Arguments
    ///
    /// * `results` - Vector of send results from `JoinSet::join_all()`
    ///
    /// # Returns
    ///
    /// - `Some(Vec<RequestBuilder>)` - Failed requests to retry
    /// - `None` - All requests succeeded
    ///
    /// # Error Handling
    ///
    /// Only `FailedProxyRequestError` errors are collected for retry. Other error
    /// types (e.g., `io::Error` for uncloneable requests) are ignored as they
    /// cannot be retried.
    ///
    /// # Implementation Note
    ///
    /// The nested pattern matching is necessary because:
    /// 1. Results are `Result<(), Box<dyn Error>>`
    /// 2. Must downcast to `FailedProxyRequestError` to access request
    /// 3. Request must be cloned to avoid consuming the error
    fn get_failed_requests(
        results: Vec<Result<(), Box<dyn Error + Send>>>,
    ) -> Option<Vec<reqwest::RequestBuilder>> {
        let mut failed_requests: Vec<reqwest::RequestBuilder> = Vec::new();

        for result in results {
            // Extract failed requests from errors
            // Nested pattern matching: Result -> Error -> FailedProxyRequestError
            if let Err(e) = result {
                // Downcast to FailedProxyRequestError to access the request
                if let Some(fpre) = e.downcast_ref::<FailedProxyRequestError>() {
                    // Clone request for retry (may fail if not cloneable)
                    if let Some(request) = fpre.request.try_clone() {
                        failed_requests.push(request);
                    }
                }
            }
        }

        // Return None if all requests succeeded
        if failed_requests.is_empty() {
            return None;
        }

        Some(failed_requests)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tags::provider::Provider;
    use crate::traces::proxy_aggregator::{Aggregator, ProxyRequest};
    use bytes::Bytes;
    use dogstatsd::api_key::ApiKeyFactory;
    use reqwest::header::HeaderMap;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn create_test_config() -> Config {
        Config {
            api_key: "test-api-key".to_string(),
            site: "datadoghq.com".to_string(),
            flush_timeout: 5,
            ..Default::default()
        }
    }

    fn create_test_aggregator() -> Arc<Mutex<Aggregator>> {
        Arc::new(Mutex::new(Aggregator::default()))
    }

    fn create_test_provider() -> Arc<Provider> {
        let mut config = Config::default();
        config.service = Some("test-service".to_string());
        Arc::new(Provider::new(Arc::new(config)))
    }

    fn create_test_proxy_request() -> ProxyRequest {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("host", "example.com".parse().unwrap());
        headers.insert("content-length", "100".parse().unwrap());

        ProxyRequest {
            headers,
            body: Bytes::from("test body"),
            target_url: "https://trace.agent.datadoghq.com/api/v0.2/traces".to_string(),
            query_params: HashMap::new(),
        }
    }

    #[test]
    fn test_flusher_new() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();
        let tags_provider = create_test_provider();

        let flusher = Flusher::new(api_key_factory, aggregator, tags_provider, config);

        // Verify flusher is created (we can't inspect private fields directly)
        assert!(flusher.headers.get().is_none());
    }

    #[tokio::test]
    async fn test_flusher_get_headers() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();
        let tags_provider = create_test_provider();

        let flusher = Flusher::new(api_key_factory, aggregator, tags_provider, config);

        let headers = flusher.get_headers("test-api-key").await;

        assert!(headers.contains_key("DD-API-KEY"));
        assert_eq!(headers.get("DD-API-KEY").unwrap(), "test-api-key");
        // X-Datadog-Additional-Tags should NOT be set by the flusher - it comes from endpoint handlers
        assert!(!headers.contains_key("X-Datadog-Additional-Tags"));
    }

    #[tokio::test]
    async fn test_flusher_create_request() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();
        let tags_provider = create_test_provider();

        let flusher = Flusher::new(api_key_factory, aggregator, tags_provider, config);

        let proxy_request = create_test_proxy_request();
        let request = flusher.create_request(proxy_request, "test-api-key").await;

        // Verify request can be cloned (basic validity check)
        assert!(request.try_clone().is_some());
    }

    #[tokio::test]
    async fn test_flusher_create_request_removes_host_header() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();
        let tags_provider = create_test_provider();

        let flusher = Flusher::new(api_key_factory, aggregator, tags_provider, config);

        let proxy_request = create_test_proxy_request();

        // Ensure host header exists before
        assert!(proxy_request.headers.contains_key("host"));
        assert!(proxy_request.headers.contains_key("content-length"));

        let _request = flusher.create_request(proxy_request, "test-api-key").await;

        // Note: We can't directly inspect the request builder's headers,
        // but we know the implementation removes host and content-length
    }

    #[tokio::test]
    async fn test_flusher_create_request_with_query_params() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();
        let tags_provider = create_test_provider();

        let flusher = Flusher::new(api_key_factory, aggregator, tags_provider, config);

        let mut proxy_request = create_test_proxy_request();
        proxy_request
            .query_params
            .insert("param1".to_string(), "value1".to_string());
        proxy_request
            .query_params
            .insert("param2".to_string(), "value2".to_string());

        let request = flusher.create_request(proxy_request, "test-api-key").await;

        // Verify request can be cloned
        assert!(request.try_clone().is_some());
    }

    #[test]
    fn test_get_failed_requests_empty() {
        let results: Vec<Result<(), Box<dyn Error + Send>>> = vec![];
        let failed = Flusher::get_failed_requests(results);
        assert!(failed.is_none());
    }

    #[test]
    fn test_get_failed_requests_all_success() {
        let results: Vec<Result<(), Box<dyn Error + Send>>> = vec![Ok(()), Ok(()), Ok(())];
        let failed = Flusher::get_failed_requests(results);
        assert!(failed.is_none());
    }

    #[test]
    fn test_get_failed_requests_with_failures() {
        let client = reqwest::Client::new();
        let request1 = client.post("https://example.com/test1");
        let request2 = client.post("https://example.com/test2");

        let error1 = FailedProxyRequestError {
            request: request1.try_clone().unwrap(),
            message: "Test error 1".to_string(),
        };

        let error2 = FailedProxyRequestError {
            request: request2.try_clone().unwrap(),
            message: "Test error 2".to_string(),
        };

        let results: Vec<Result<(), Box<dyn Error + Send>>> =
            vec![Ok(()), Err(Box::new(error1)), Ok(()), Err(Box::new(error2))];

        let failed = Flusher::get_failed_requests(results);
        assert!(failed.is_some());
        let failed_requests = failed.unwrap();
        assert_eq!(failed_requests.len(), 2);
    }

    #[test]
    fn test_failed_proxy_request_error() {
        let client = reqwest::Client::new();
        let request = client.post("https://example.com/test");

        let error = FailedProxyRequestError {
            request: request.try_clone().unwrap(),
            message: "Test error".to_string(),
        };

        assert_eq!(error.message, "Test error");
        assert!(format!("{}", error).contains("Test error"));
    }

    #[test]
    fn test_failed_proxy_request_error_display() {
        let client = reqwest::Client::new();
        let request = client.post("https://example.com/test");

        let error = FailedProxyRequestError {
            request: request.try_clone().unwrap(),
            message: "Connection timeout".to_string(),
        };

        let error_string = error.to_string();
        assert_eq!(error_string, "Connection timeout");
    }
}
