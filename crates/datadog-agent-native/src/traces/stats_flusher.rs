// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Stats payload flushing to Datadog trace stats intake.
//!
//! This module handles the delivery of aggregated APM statistics to Datadog's
//! trace stats intake endpoint. It retrieves batched stats from the aggregator,
//! serializes them, and sends them via HTTP to Datadog.
//!
//! # Stats Flushing Flow
//!
//! 1. **Batch Retrieval**: Pull size-limited batches from aggregator
//! 2. **Payload Construction**: Combine multiple stats into single payload
//! 3. **Serialization**: Serialize payload to msgpack format
//! 4. **HTTP Delivery**: POST to Datadog trace stats intake
//! 5. **Iteration**: Continue until aggregator is empty
//!
//! # Architecture
//!
//! ```text
//! StatsAggregator → StatsFlusher → [Serialize] → HTTP POST → Datadog Stats Intake
//!                       ↓
//!                 Batches (~3MB each)
//!                 API Key Injection
//!                 Endpoint Caching
//! ```
//!
//! # Endpoint
//!
//! Stats are sent to: `https://trace.agent.{site}/api/v0.2/stats`
//! where `{site}` is configured (e.g., `datadoghq.com`, `datadoghq.eu`)
//!
//! # Error Handling
//!
//! - **Serialization failures**: Stats batch is dropped, error logged
//! - **API key missing**: Flush is skipped, error logged
//! - **Network errors**: Error logged, batch is lost (no retry at this level)

use async_trait::async_trait;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;

use crate::config;
use crate::traces::stats_aggregator::StatsAggregator;
use crate::traces::S_TO_MS;
use datadog_trace_protobuf::pb;
use datadog_trace_utils::{config_utils::trace_stats_url, stats_utils};
use ddcommon::Endpoint;
use dogstatsd::api_key::ApiKeyFactory;
use tracing::{debug, error};

/// Trait for flushing aggregated stats to Datadog.
///
/// This trait defines the interface for stats flushers, which are responsible
/// for retrieving batched stats from an aggregator and delivering them to
/// Datadog's trace stats intake endpoint.
///
/// # Implementations
///
/// - `ServerlessStatsFlusher`: Serverless-optimized implementation
///
/// # Usage Pattern
///
/// 1. Create flusher with `new()`
/// 2. Periodically call `flush(false)` to send completed stats
/// 3. On shutdown, call `flush(true)` to force-flush remaining stats
#[async_trait]
pub trait StatsFlusher {
    /// Creates a new stats flusher.
    ///
    /// # Arguments
    ///
    /// * `api_key_factory` - Factory for resolving Datadog API keys
    /// * `aggregator` - Shared aggregator containing batched stats
    /// * `config` - Agent configuration for site, timeout, etc.
    fn new(
        api_key_factory: Arc<ApiKeyFactory>,
        aggregator: Arc<Mutex<StatsAggregator>>,
        config: Arc<config::Config>,
    ) -> Self
    where
        Self: Sized;

    /// Sends a batch of stats payloads to Datadog.
    ///
    /// This method handles:
    /// - Payload construction (combining multiple `ClientStatsPayload`)
    /// - Serialization to msgpack
    /// - HTTP POST to trace stats intake
    ///
    /// # Arguments
    ///
    /// * `traces` - Vector of stats payloads to send
    async fn send(&self, traces: Vec<pb::ClientStatsPayload>);

    /// Flushes all available stats from the aggregator.
    ///
    /// Retrieves batches from the aggregator and sends them until the
    /// aggregator is empty. Each batch is sent via `send()`.
    ///
    /// # Arguments
    ///
    /// * `force_flush` - If `true`, flush all buckets including incomplete ones.
    ///                   If `false`, only flush completed buckets.
    async fn flush(&self, force_flush: bool);
}

/// Serverless-optimized stats flusher for APM statistics.
///
/// The `ServerlessStatsFlusher` implements the `StatsFlusher` trait for serverless
/// environments where stats need to be sent efficiently with minimal overhead.
///
/// # Features
///
/// - **Endpoint Caching**: Datadog endpoint is initialized once and reused
/// - **Batched Sending**: Retrieves size-limited batches from aggregator
/// - **Msgpack Serialization**: Efficient binary format for stats payloads
/// - **Iterative Flushing**: Continues until aggregator is empty
///
/// # Cloning
///
/// The flusher is `Clone` to allow multiple references while maintaining shared
/// state via `Arc` wrappers.
///
/// # Example
///
/// ```rust
/// use datadog_agent_native::traces::stats_flusher::{StatsFlusher, ServerlessStatsFlusher};
/// use std::sync::Arc;
///
/// # async fn example() {
/// // let api_key_factory = ...; // From ApiKeyFactory::new
/// // let aggregator = ...; // From StatsAggregator::new_with_concentrator
/// // let config = Arc::new(Config::default());
/// //
/// // let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);
/// //
/// // // Flush completed stats
/// // flusher.flush(false).await;
/// //
/// // // Force flush on shutdown
/// // flusher.flush(true).await;
/// # }
/// ```
#[allow(clippy::module_name_repetitions)]
#[derive(Clone)]
pub struct ServerlessStatsFlusher {
    /// Shared aggregator containing batched stats payloads.
    aggregator: Arc<Mutex<StatsAggregator>>,
    /// Agent configuration (site, timeout, etc.).
    config: Arc<config::Config>,
    /// Factory for resolving Datadog API keys.
    api_key_factory: Arc<ApiKeyFactory>,
    /// Cached Datadog endpoint (initialized once on first flush).
    endpoint: OnceCell<Endpoint>,
}

#[async_trait]
impl StatsFlusher for ServerlessStatsFlusher {
    fn new(
        api_key_factory: Arc<ApiKeyFactory>,
        aggregator: Arc<Mutex<StatsAggregator>>,
        config: Arc<config::Config>,
    ) -> Self {
        ServerlessStatsFlusher {
            aggregator,
            config,
            api_key_factory,
            endpoint: OnceCell::new(),
        }
    }

    async fn send(&self, stats: Vec<pb::ClientStatsPayload>) {
        // Early return if no stats to send
        if stats.is_empty() {
            return;
        }

        // Resolve API key or abort send
        let Some(api_key) = self.api_key_factory.get_api_key().await else {
            error!("Skipping flushing stats: Failed to resolve API key");
            return;
        };

        // Lazily initialize and cache Datadog endpoint
        let api_key_clone = api_key.to_string();
        let endpoint = self
            .endpoint
            .get_or_init({
                move || async move {
                    // Build stats intake URL based on configured site
                    let stats_url = trace_stats_url(&self.config.site);
                    Endpoint {
                        url: hyper::Uri::from_str(&stats_url)
                            .expect("can't make URI from stats url, exiting"),
                        api_key: Some(api_key_clone.into()),
                        timeout_ms: self.config.flush_timeout * S_TO_MS,
                        test_token: None,
                    }
                }
            })
            .await;

        debug!("Flushing {} stats", stats.len());

        // Step 1: Construct combined stats payload from individual payloads
        let stats_payload = stats_utils::construct_stats_payload(stats);

        // Step 2: Serialize payload to msgpack format
        let serialized_stats_payload = match stats_utils::serialize_stats_payload(stats_payload) {
            Ok(res) => res,
            Err(err) => {
                // Serialization failure - drop stats batch
                error!("Failed to serialize stats payload, dropping stats: {err}");
                return;
            }
        };

        let stats_url = trace_stats_url(&self.config.site);

        // Step 3: Send serialized payload via HTTP POST
        let start = std::time::Instant::now();
        let resp =
            stats_utils::send_stats_payload(serialized_stats_payload, endpoint, api_key.as_str())
                .await;
        let elapsed = start.elapsed();

        // Log results
        debug!(
            "Stats request to {} took {} ms",
            stats_url,
            elapsed.as_millis()
        );
        match resp {
            Ok(()) => debug!("Successfully flushed stats"),
            Err(e) => {
                // Network or server error - stats batch is lost
                error!("Error sending stats: {e:?}");
            }
        };
    }

    async fn flush(&self, force_flush: bool) {
        // Lock aggregator for the duration of flush
        let mut guard = self.aggregator.lock().await;

        // Iteratively retrieve and send batches until aggregator is empty
        let mut stats = guard.get_batch(force_flush).await;
        while !stats.is_empty() {
            // Send current batch
            self.send(stats).await;

            // Get next batch (or empty vec if aggregator is now empty)
            stats = guard.get_batch(force_flush).await;
        }
        // Aggregator is empty - flush complete
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::traces::stats_concentrator_service::StatsConcentratorService;
    use datadog_trace_protobuf::pb;
    use dogstatsd::api_key::ApiKeyFactory;
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

    fn create_test_aggregator() -> Arc<Mutex<StatsAggregator>> {
        let config = Arc::new(Config::default());
        let (_, concentrator) = StatsConcentratorService::new(config);
        Arc::new(Mutex::new(StatsAggregator::new_with_concentrator(
            concentrator,
        )))
    }

    fn create_test_stats_payload() -> pb::ClientStatsPayload {
        pb::ClientStatsPayload {
            hostname: "test-hostname".to_string(),
            env: "test-env".to_string(),
            version: "1.0.0".to_string(),
            stats: vec![],
            lang: "rust".to_string(),
            tracer_version: "1.0.0".to_string(),
            runtime_id: "test-runtime-id".to_string(),
            sequence: 1,
            agent_aggregation: "test-aggregation".to_string(),
            service: "test-service".to_string(),
            container_id: "test-container".to_string(),
            tags: vec!["env:test".to_string(), "version:1.0.0".to_string()],
            git_commit_sha: "abc123".to_string(),
            image_tag: "latest".to_string(),
            process_tags: "process:test".to_string(),
            process_tags_hash: 12345,
        }
    }

    #[test]
    fn test_serverless_stats_flusher_new() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        // Verify flusher is created with OnceCell not initialized
        assert!(flusher.endpoint.get().is_none());
    }

    #[test]
    fn test_serverless_stats_flusher_clone() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        // Test that ServerlessStatsFlusher can be cloned
        let cloned_flusher = flusher.clone();
        assert!(cloned_flusher.endpoint.get().is_none());
    }

    #[tokio::test]
    async fn test_send_empty_stats() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        // Send empty stats - should return immediately without error
        flusher.send(vec![]).await;

        // Verify endpoint was not initialized
        assert!(flusher.endpoint.get().is_none());
    }

    #[tokio::test]
    async fn test_send_with_api_key() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-api-key-123"));
        let aggregator = create_test_aggregator();

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        let stats = vec![create_test_stats_payload()];

        // Note: This will attempt to send to Datadog, which will fail in tests
        // but we're testing the logic flow, not the network call
        flusher.send(stats).await;

        // Verify endpoint was initialized
        assert!(flusher.endpoint.get().is_some());
    }

    #[tokio::test]
    async fn test_send_multiple_stats_payloads() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-api-key"));
        let aggregator = create_test_aggregator();

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        let mut payload1 = create_test_stats_payload();
        payload1.sequence = 1;
        let mut payload2 = create_test_stats_payload();
        payload2.sequence = 2;
        let mut payload3 = create_test_stats_payload();
        payload3.sequence = 3;

        let stats = vec![payload1, payload2, payload3];

        flusher.send(stats).await;

        // Verify endpoint was initialized
        assert!(flusher.endpoint.get().is_some());
    }

    #[tokio::test]
    async fn test_send_with_different_sites() {
        let sites = vec![
            "datadoghq.com",
            "datadoghq.eu",
            "us3.datadoghq.com",
            "us5.datadoghq.com",
            "ap1.datadoghq.com",
        ];

        for site in sites {
            let mut config = create_test_config();
            config.site = site.to_string();
            let config = Arc::new(config);
            let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
            let aggregator = create_test_aggregator();

            let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

            let stats = vec![create_test_stats_payload()];
            flusher.send(stats).await;

            // Verify endpoint was initialized for each site
            assert!(flusher.endpoint.get().is_some());
        }
    }

    #[tokio::test]
    async fn test_send_with_custom_timeout() {
        let mut config = create_test_config();
        config.flush_timeout = 10;
        let config = Arc::new(config);
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        let stats = vec![create_test_stats_payload()];
        flusher.send(stats).await;

        // Verify endpoint was initialized with custom timeout
        let endpoint = flusher.endpoint.get();
        assert!(endpoint.is_some());
        if let Some(ep) = endpoint {
            assert_eq!(ep.timeout_ms, 10 * S_TO_MS);
        }
    }

    #[tokio::test]
    async fn test_flush_empty_aggregator() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        // Flush with empty aggregator - should complete without error
        flusher.flush(false).await;

        // Endpoint should not be initialized since no stats were sent
        assert!(flusher.endpoint.get().is_none());
    }

    #[tokio::test]
    async fn test_flush_with_stats_in_aggregator() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        // Add some stats to the aggregator
        {
            let mut agg = aggregator.lock().await;
            agg.add(create_test_stats_payload());
        }

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        // Flush the stats
        flusher.flush(false).await;

        // Verify endpoint was initialized
        assert!(flusher.endpoint.get().is_some());
    }

    #[tokio::test]
    async fn test_flush_multiple_batches() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        // Add multiple stats to the aggregator
        {
            let mut agg = aggregator.lock().await;
            for i in 0..5 {
                let mut payload = create_test_stats_payload();
                payload.sequence = i;
                agg.add(payload);
            }
        }

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        // Flush all stats
        flusher.flush(false).await;

        // Verify endpoint was initialized
        assert!(flusher.endpoint.get().is_some());

        // Verify aggregator is empty after flush
        {
            let mut agg = flusher.aggregator.lock().await;
            let remaining = agg.get_batch(false).await;
            assert!(remaining.is_empty());
        }
    }

    #[tokio::test]
    async fn test_flush_with_force_flush_true() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        // Add stats to the aggregator
        {
            let mut agg = aggregator.lock().await;
            agg.add(create_test_stats_payload());
        }

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        // Flush with force_flush = true
        flusher.flush(true).await;

        // Verify endpoint was initialized
        assert!(flusher.endpoint.get().is_some());
    }

    #[tokio::test]
    async fn test_flush_with_force_flush_false() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        // Add stats to the aggregator
        {
            let mut agg = aggregator.lock().await;
            agg.add(create_test_stats_payload());
        }

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        // Flush with force_flush = false
        flusher.flush(false).await;

        // Verify endpoint was initialized
        assert!(flusher.endpoint.get().is_some());
    }

    #[tokio::test]
    async fn test_endpoint_cached_after_first_send() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        // First send
        flusher.send(vec![create_test_stats_payload()]).await;
        let endpoint1 = flusher.endpoint.get();
        assert!(endpoint1.is_some());

        // Second send should use cached endpoint
        flusher.send(vec![create_test_stats_payload()]).await;
        let endpoint2 = flusher.endpoint.get();
        assert!(endpoint2.is_some());

        // Endpoints should be the same instance (cached)
        assert!(std::ptr::eq(endpoint1.unwrap(), endpoint2.unwrap()));
    }

    #[tokio::test]
    async fn test_send_with_stats_containing_different_tags() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        let mut payload = create_test_stats_payload();
        payload.tags = vec![
            "env:production".to_string(),
            "service:web".to_string(),
            "version:2.0.0".to_string(),
            "region:us-east-1".to_string(),
        ];

        flusher.send(vec![payload]).await;

        // Verify endpoint was initialized
        assert!(flusher.endpoint.get().is_some());
    }

    #[tokio::test]
    async fn test_send_with_empty_tags() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        let mut payload = create_test_stats_payload();
        payload.tags = vec![];

        flusher.send(vec![payload]).await;

        // Verify endpoint was initialized
        assert!(flusher.endpoint.get().is_some());
    }

    #[tokio::test]
    async fn test_flush_clears_aggregator() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        // Add stats
        {
            let mut agg = aggregator.lock().await;
            for i in 0..3 {
                let mut payload = create_test_stats_payload();
                payload.sequence = i;
                agg.add(payload);
            }
        }

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator.clone(), config);

        // Flush all stats
        flusher.flush(false).await;

        // Verify aggregator is empty
        {
            let mut agg = aggregator.lock().await;
            let batch = agg.get_batch(false).await;
            assert!(batch.is_empty());
        }
    }

    #[test]
    fn test_stats_flusher_trait_new() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        // Test that we can create via trait method
        let _flusher: ServerlessStatsFlusher =
            StatsFlusher::new(api_key_factory, aggregator, config);
    }

    #[tokio::test]
    async fn test_concurrent_flushes() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        // Add stats
        {
            let mut agg = aggregator.lock().await;
            for i in 0..10 {
                let mut payload = create_test_stats_payload();
                payload.sequence = i;
                agg.add(payload);
            }
        }

        let flusher = Arc::new(ServerlessStatsFlusher::new(
            api_key_factory,
            aggregator,
            config,
        ));

        // Spawn multiple concurrent flush operations
        let mut handles = vec![];
        for _ in 0..3 {
            let flusher_clone = flusher.clone();
            let handle = tokio::spawn(async move {
                flusher_clone.flush(false).await;
            });
            handles.push(handle);
        }

        // Wait for all flushes to complete
        for handle in handles {
            handle.await.expect("Flush task panicked");
        }

        // Verify endpoint was initialized
        assert!(flusher.endpoint.get().is_some());
    }

    #[tokio::test]
    async fn test_send_respects_api_key_from_factory() {
        // Test with different API keys
        let api_keys = vec![
            "short-key",
            "very-long-api-key-with-many-characters-0123456789",
            "key-with-special-chars-!@#$%",
        ];

        for api_key in api_keys {
            let config = Arc::new(create_test_config());
            let api_key_factory = Arc::new(ApiKeyFactory::new(api_key));
            let aggregator = create_test_aggregator();

            let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

            flusher.send(vec![create_test_stats_payload()]).await;

            // Verify endpoint was initialized
            assert!(flusher.endpoint.get().is_some());
        }
    }

    #[tokio::test]
    async fn test_send_with_large_payload() {
        let config = Arc::new(create_test_config());
        let api_key_factory = Arc::new(ApiKeyFactory::new("test-key"));
        let aggregator = create_test_aggregator();

        let flusher = ServerlessStatsFlusher::new(api_key_factory, aggregator, config);

        // Create a payload with many tags
        let mut payload = create_test_stats_payload();
        for i in 0..100 {
            payload.tags.push(format!("tag{}:value{}", i, i));
        }

        flusher.send(vec![payload]).await;

        // Verify endpoint was initialized
        assert!(flusher.endpoint.get().is_some());
    }
}
