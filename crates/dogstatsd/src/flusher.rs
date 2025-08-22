// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::aggregator::Aggregator;
use crate::api_key::ApiKeyFactory;
use crate::datadog::{DdApi, MetricsIntakeUrlPrefix, RetryStrategy};
use reqwest::{Response, StatusCode};
use std::sync::{Arc, Mutex};
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
    timeout: Duration,
    retry_strategy: RetryStrategy,
    aggregator: Arc<Mutex<Aggregator>>,
    dd_api: OnceCell<Option<DdApi>>,
    compression_level: CompressionLevel,
}

pub struct FlusherConfig {
    pub api_key_factory: Arc<ApiKeyFactory>,
    pub aggregator: Arc<Mutex<Aggregator>>,
    pub metrics_intake_url_prefix: MetricsIntakeUrlPrefix,
    pub https_proxy: Option<String>,
    pub timeout: Duration,
    pub retry_strategy: RetryStrategy,
    pub compression_level: CompressionLevel,
}

#[allow(clippy::await_holding_lock)]
impl Flusher {
    pub fn new(config: FlusherConfig) -> Self {
        Flusher {
            api_key_factory: Arc::clone(&config.api_key_factory),
            metrics_intake_url_prefix: config.metrics_intake_url_prefix,
            https_proxy: config.https_proxy,
            timeout: config.timeout,
            retry_strategy: config.retry_strategy,
            aggregator: config.aggregator,
            compression_level: config.compression_level,
            dd_api: OnceCell::new(),
        }
    }

    async fn get_dd_api(&mut self) -> &Option<DdApi> {
        self.dd_api
            .get_or_init(|| async {
                let api_key = self.api_key_factory.get_api_key().await;
                match api_key {
                    Some(api_key) => Some(DdApi::new(
                        api_key.to_string(),
                        self.metrics_intake_url_prefix.clone(),
                        self.https_proxy.clone(),
                        self.timeout,
                        self.retry_strategy.clone(),
                        self.compression_level.clone(),
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
        &mut self,
    ) -> Option<(
        Vec<crate::datadog::Series>,
        Vec<datadog_protos::metrics::SketchPayload>,
    )> {
        let (series, distributions) = {
            #[allow(clippy::expect_used)]
            let mut aggregator = self.aggregator.lock().expect("lock poisoned");
            (
                aggregator.consume_metrics(),
                aggregator.consume_distributions(),
            )
        };
        self.flush_metrics(series, distributions).await
    }

    /// Flush given batch of metrics
    pub async fn flush_metrics(
        &mut self,
        series: Vec<crate::datadog::Series>,
        distributions: Vec<datadog_protos::metrics::SketchPayload>,
    ) -> Option<(
        Vec<crate::datadog::Series>,
        Vec<datadog_protos::metrics::SketchPayload>,
    )> {
        let n_series = series.len();
        let n_distributions = distributions.len();

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
                sc.map_or(false, |code| code.as_u16() >= 400 && code.as_u16() < 500);

            error!("Error shipping data: {:?} {}", sc, msg);

            if is_permanent_error {
                (false, false) // Permanent destination error, don't continue and don't retry
            } else {
                (false, true) // Temporary error, don't continue and mark for retry
            }
        }
    }
}
