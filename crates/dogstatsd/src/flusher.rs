// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::aggregator_service::AggregatorHandle;
use crate::api_key::ApiKeyFactory;
use crate::datadog::{DdApi, MetricsIntakeUrlPrefix, RetryStrategy};
use reqwest::{Response, StatusCode};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;
use tracing::{debug, error};
use zstd::zstd_safe::CompressionLevel;

#[derive(Clone)]
pub struct Flusher {
    // Allow accepting a future so the API key resolution is deferred until the flush happens
    api_key_factory: Arc<ApiKeyFactory>,
    metrics_intake_url_prefix: MetricsIntakeUrlPrefix,
    https_proxy: Option<String>,
    ca_cert_path: Option<String>,
    timeout: Duration,
    retry_strategy: RetryStrategy,
    aggregator_handle: AggregatorHandle,
    dd_api: OnceCell<Option<DdApi>>,
    compression_level: CompressionLevel,
}

pub struct FlusherConfig {
    pub api_key_factory: Arc<ApiKeyFactory>,
    pub aggregator_handle: AggregatorHandle,
    pub metrics_intake_url_prefix: MetricsIntakeUrlPrefix,
    pub https_proxy: Option<String>,
    pub ca_cert_path: Option<String>,
    pub timeout: Duration,
    pub retry_strategy: RetryStrategy,
    pub compression_level: CompressionLevel,
}

impl Flusher {
    pub fn new(config: FlusherConfig) -> Self {
        Flusher {
            api_key_factory: Arc::clone(&config.api_key_factory),
            metrics_intake_url_prefix: config.metrics_intake_url_prefix,
            https_proxy: config.https_proxy,
            ca_cert_path: config.ca_cert_path,
            timeout: config.timeout,
            retry_strategy: config.retry_strategy,
            aggregator_handle: config.aggregator_handle,
            compression_level: config.compression_level,
            dd_api: OnceCell::new(),
        }
    }

    async fn get_dd_api(&self) -> &Option<DdApi> {
        self.dd_api
            .get_or_init(|| async {
                let api_key = self.api_key_factory.get_api_key().await;
                match api_key {
                    Some(api_key) => Some(DdApi::new(
                        api_key.to_string(),
                        self.metrics_intake_url_prefix.clone(),
                        self.https_proxy.clone(),
                        self.ca_cert_path.clone(),
                        self.timeout,
                        self.retry_strategy.clone(),
                        self.compression_level,
                    )),
                    None => {
                        error!("Failed to create dd_api: failed to get API key");
                        None
                    }
                }
            })
            .await
    }

    /// Flush metrics from the aggregator
    pub async fn flush(
        &self,
    ) -> Option<(
        Vec<crate::datadog::Series>,
        Vec<datadog_protos::metrics::SketchPayload>,
    )> {
        // Request flush through the channel - no lock needed!
        let response = match self.aggregator_handle.flush().await {
            Ok(response) => response,
            Err(e) => {
                error!("Failed to flush aggregator: {}", e);
                return Some((Vec::new(), Vec::new()));
            }
        };

        self.flush_metrics(response.series, response.distributions)
            .await
    }

    /// Flush given batch of metrics
    pub async fn flush_metrics(
        &self,
        series: Vec<crate::datadog::Series>,
        distributions: Vec<datadog_protos::metrics::SketchPayload>,
    ) -> Option<(
        Vec<crate::datadog::Series>,
        Vec<datadog_protos::metrics::SketchPayload>,
    )> {
        let n_series = series.len();
        let n_distributions = distributions.len();

        // Early return if there are no metrics to flush
        if n_series == 0 && n_distributions == 0 {
            return None;
        }

        debug!("Flushing {n_series} series and {n_distributions} distributions");

        // Save copies for potential error returns
        let series_copy = series.clone();
        let distributions_copy = distributions.clone();

        let dd_api = match self.get_dd_api().await {
            None => {
                error!("Failed to flush metrics: failed to create dd_api");
                return Some((series_copy, distributions_copy));
            }
            Some(dd_api) => dd_api,
        };

        let dd_api_clone = dd_api.clone();
        let series_handle = tokio::spawn(async move {
            let mut failed = Vec::new();
            let mut had_shipping_error = false;
            for a_batch in series {
                let (continue_shipping, should_retry) =
                    should_try_next_batch(dd_api_clone.ship_series(&a_batch).await).await;
                if should_retry {
                    failed.push(a_batch);
                    had_shipping_error = true;
                }
                if !continue_shipping {
                    break;
                }
            }
            (failed, had_shipping_error)
        });

        let dd_api_clone = dd_api.clone();
        let distributions_handle = tokio::spawn(async move {
            let mut failed = Vec::new();
            let mut had_shipping_error = false;
            for a_batch in distributions {
                let (continue_shipping, should_retry) =
                    should_try_next_batch(dd_api_clone.ship_distributions(&a_batch).await).await;
                if should_retry {
                    failed.push(a_batch);
                    had_shipping_error = true;
                }
                if !continue_shipping {
                    break;
                }
            }
            (failed, had_shipping_error)
        });

        match tokio::try_join!(series_handle, distributions_handle) {
            Ok(((series_failed, series_had_error), (sketches_failed, sketches_had_error))) => {
                if series_failed.is_empty() && sketches_failed.is_empty() {
                    debug!("Successfully flushed {n_series} series and {n_distributions} distributions");
                    None // Return None to indicate success
                } else if series_had_error || sketches_had_error {
                    // Only return the metrics if there was an actual shipping error
                    error!("Failed to flush some metrics due to shipping errors: {} series and {} sketches", 
                        series_failed.len(), sketches_failed.len());
                    // Return the failed metrics for potential retry
                    Some((series_failed, sketches_failed))
                } else {
                    debug!("Some metrics were not sent but no errors occurred");
                    None // No shipping errors, so don't return metrics for retry
                }
            }
            Err(err) => {
                error!("Failed to flush metrics: {err}");
                // Return all metrics in case of join error for potential retry
                Some((series_copy, distributions_copy))
            }
        }
    }
}

pub enum ShippingError {
    Payload(String),
    Destination(Option<StatusCode>, String),
}

/// Returns a tuple (continue_to_next_batch, should_retry_this_batch)
async fn should_try_next_batch(resp: Result<Response, ShippingError>) -> (bool, bool) {
    match resp {
        Ok(resp_payload) => match resp_payload.status() {
            StatusCode::ACCEPTED => (true, false), // Success, continue to next batch, no need to retry
            unexpected_status_code => {
                // Check if the status code indicates a permanent error (4xx) or a temporary error (5xx)
                let is_permanent_error =
                    unexpected_status_code.as_u16() >= 400 && unexpected_status_code.as_u16() < 500;

                error!(
                    "{}: Failed to push to API: {:?}",
                    unexpected_status_code,
                    resp_payload.text().await.unwrap_or_default()
                );

                if is_permanent_error {
                    (true, false) // Permanent error, continue to next batch but don't retry
                } else {
                    (false, true) // Temporary error, don't continue to next batch and mark for retry
                }
            }
        },
        Err(ShippingError::Payload(msg)) => {
            error!("Failed to prepare payload. Data dropped: {}", msg);
            (true, false) // Payload error, continue to next batch but don't retry (data is malformed)
        }
        Err(ShippingError::Destination(sc, msg)) => {
            // Check if status code indicates a permanent error
            let is_permanent_error =
                sc.is_some_and(|code| code.as_u16() >= 400 && code.as_u16() < 500);

            error!("Error shipping data: {:?} {}", sc, msg);

            if is_permanent_error {
                (false, false) // Permanent destination error, don't continue and don't retry
            } else {
                (false, true) // Temporary error, don't continue and mark for retry
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator_service::AggregatorService;
    use crate::constants::CONTEXTS;
    use crate::datadog::{DdDdUrl, MetricsIntakeUrlPrefix, MetricsIntakeUrlPrefixOverride};
    use crate::metric::SortedTags;

    #[tokio::test]
    async fn test_flush_metrics_empty_returns_none() {
        use std::pin::Pin;

        // Create an aggregator service for the flusher
        let (service, handle) = AggregatorService::new(
            SortedTags::parse("test:value").expect("failed to parse tags"),
            CONTEXTS,
        )
        .expect("failed to create aggregator service");

        // Spawn the service
        tokio::spawn(service.run());

        // Create an API key factory with a resolver that panics if called
        // This proves that get_dd_api() is never invoked when there are no metrics
        let api_key_factory = ApiKeyFactory::new_from_resolver(
            Arc::new(|| {
                Box::pin(async {
                    panic!("API key resolver should not be called for empty metrics!");
                    #[allow(unreachable_code)]
                    {
                        None
                    }
                })
                    as Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
            }),
            None,
        );

        let flusher = Flusher::new(FlusherConfig {
            api_key_factory: Arc::new(api_key_factory),
            aggregator_handle: handle,
            metrics_intake_url_prefix: MetricsIntakeUrlPrefix::new(
                None,
                MetricsIntakeUrlPrefixOverride::maybe_new(
                    None,
                    Some(
                        DdDdUrl::new("http://localhost:8080".to_string())
                            .expect("failed to create URL"),
                    ),
                ),
            )
            .expect("failed to create URL"),
            https_proxy: None,
            ca_cert_path: None,
            timeout: Duration::from_secs(5),
            retry_strategy: RetryStrategy::Immediate(1),
            compression_level: CompressionLevel::try_from(6)
                .expect("failed to create compression level"),
        });

        // Test with empty vectors
        let result = flusher.flush_metrics(Vec::new(), Vec::new()).await;

        // Should return None when there are no metrics to flush
        // If get_dd_api() was called, the test would panic from the resolver
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_flush_metrics_with_series_only() {
        use crate::aggregator::Aggregator;
        use crate::metric::parse;

        // Create an aggregator service for the flusher
        let (service, handle) = AggregatorService::new(
            SortedTags::parse("test:value").expect("failed to parse tags"),
            CONTEXTS,
        )
        .expect("failed to create aggregator service");

        // Spawn the service
        tokio::spawn(service.run());

        let api_key_factory = ApiKeyFactory::new("test-api-key");

        let flusher = Flusher::new(FlusherConfig {
            api_key_factory: Arc::new(api_key_factory),
            aggregator_handle: handle,
            metrics_intake_url_prefix: MetricsIntakeUrlPrefix::new(
                None,
                MetricsIntakeUrlPrefixOverride::maybe_new(
                    None,
                    Some(
                        DdDdUrl::new("http://localhost:8080".to_string())
                            .expect("failed to create URL"),
                    ),
                ),
            )
            .expect("failed to create URL"),
            https_proxy: None,
            ca_cert_path: None,
            timeout: Duration::from_secs(5),
            retry_strategy: RetryStrategy::Immediate(1),
            compression_level: CompressionLevel::try_from(6)
                .expect("failed to create compression level"),
        });

        // Create a series with actual metrics
        let mut aggregator = Aggregator::new(SortedTags::parse("test:value").unwrap(), 1)
            .expect("failed to create aggregator");
        let metric = parse("test:1|c").expect("failed to parse metric");
        aggregator.insert(metric).expect("failed to insert metric");
        let series = vec![aggregator.to_series()];

        // Test with series but empty distributions
        let result = flusher.flush_metrics(series, Vec::new()).await;

        // The request will fail with 404 (no mock server), which is a permanent 4xx error.
        // Permanent errors are not retried, so None is returned (success from retry perspective).
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_flush_metrics_with_distributions_only() {
        // Create an aggregator service for the flusher
        let (service, handle) = AggregatorService::new(
            SortedTags::parse("test:value").expect("failed to parse tags"),
            CONTEXTS,
        )
        .expect("failed to create aggregator service");

        // Spawn the service
        tokio::spawn(service.run());

        let api_key_factory = ApiKeyFactory::new("test-api-key");

        let flusher = Flusher::new(FlusherConfig {
            api_key_factory: Arc::new(api_key_factory),
            aggregator_handle: handle,
            metrics_intake_url_prefix: MetricsIntakeUrlPrefix::new(
                None,
                MetricsIntakeUrlPrefixOverride::maybe_new(
                    None,
                    Some(
                        DdDdUrl::new("http://localhost:8080".to_string())
                            .expect("failed to create URL"),
                    ),
                ),
            )
            .expect("failed to create URL"),
            https_proxy: None,
            ca_cert_path: None,
            timeout: Duration::from_secs(5),
            retry_strategy: RetryStrategy::Immediate(1),
            compression_level: CompressionLevel::try_from(6)
                .expect("failed to create compression level"),
        });

        // Create a distribution sketch payload
        let sketch = datadog_protos::metrics::SketchPayload::default();
        let distributions = vec![sketch];

        // Test with distributions but empty series
        let result = flusher.flush_metrics(Vec::new(), distributions).await;

        // The request will fail with 404 (no mock server), which is a permanent 4xx error.
        // Permanent errors are not retried, so None is returned (success from retry perspective).
        assert!(result.is_none());
    }
}
